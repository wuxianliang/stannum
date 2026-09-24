// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! A term's tuple locations in heap order.
//!
//! Two body layouts share one stream header; the builder emits whichever is
//! smaller. Either may carry score bounds, which a term's postings do and the
//! document table does not: a table with one entry per block of
//! `BLOCK_POSTINGS` postings, or, when the stream is one block, a single
//! term bound without the per-block fields.
//!
//! ```text
//! stream  := form u8, count varint, [bounds_len varint, table | term_bound], body
//!            form bits: 1 grouped (else sparse), 2 table follows, 4 term bound follows
//! sparse  := (block_delta varint, offset varint)*        first block absolute
//! grouped := group_count varint, group*
//! group   := gid varint, count varint, page_bitmap[32], body_len varint, body
//!            gid is absolute for the first group, then (delta - 1)
//! body    := page*  one per set bit of page_bitmap, ascending
//! page    := 0x00, n varint, offset u16le * n     (n <= LIST_MAX)
//!          | 0x01, tuple_bitmap[37]               (bit offset-1 set)
//! table   := entry * ceil(count / BLOCK_POSTINGS)
//! entry   := term_bound,
//!            last_block varint (delta from the previous entry), last_offset varint,
//!            [sparse: start varint, byte offset into body, delta from the previous entry]
//! term_bound := buckets varint (bit b set: bucket b occurs in the block),
//!            min_len varint per set bit ascending
//! ```
//!
//! `LSG4` keeps the envelope, body layouts and table framing, and widens the
//! bound payload by a field dimension (RFC §5.4):
//!
//! ```text
//! field_bound := field_mask varint (bit i set: field i occurs in the block),
//!            per set field bit, ascending:
//!            bucket_mask varint (bit b set: bucket b occurs in field i),
//!            min_len varint per set bit ascending — the shortest length of
//!            field i among the block's postings carrying field i at bucket b
//! ```
//!
//! The decoded form ([`FieldBlockBound`]) preserves those per-`(field,
//! bucket)` minima; the per-field aggregates the frozen bound formula uses
//! are derived from them.
//!
//! Groups cover 256 consecutive heap blocks, so intersecting two grouped terms
//! can skip whole groups and whole pages without decoding offsets. Every group
//! and page carries its count, so `seek` maintains the ordinal of the current
//! posting, which is how the parallel payload stream is addressed.
//!
//! A bound describes a run of postings: for every term-frequency bucket that
//! occurs in the run, the shortest document it occurs in. A BM25 contribution
//! never grows with the document length, so the best score in the run under
//! any parameters is the best of those (bucket, length) pairs: a ranked scan
//! bounds the run exactly without decoding it. A table entry's last location
//! tells it the range of tuples the bound covers, and sparse streams also
//! record where each block starts, so their `seek` jumps over whole blocks
//! through the table. A stream of at most `BLOCK_POSTINGS` postings is one
//! block, so `LSG3` writers store only the term bound (form bit 4): its last
//! location is the stream's last posting, which a cursor finds by walking the
//! stream once when first asked. `LSG2` writers stored the full table for
//! such streams (form bit 2); both are still read.

use crate::reader::Reader;
use crate::segment::Format;
use crate::set::Cursor;
use crate::tf_bucket::BUCKET_COUNT;
use crate::tid::MAX_OFFSET;
use crate::{Error, Result, Tid, varint};

pub const GROUP_BLOCKS: u32 = 256;
/// Postings per score-bound block.
pub const BLOCK_POSTINGS: u32 = 128;
const PAGE_BITMAP_BYTES: usize = 32;
/// 296 bits, enough for `MAX_OFFSET` (291) one-based offsets.
const TUPLE_BITMAP_BYTES: usize = 37;
/// A list of this many `u16` offsets is no larger than a tuple bitmap.
pub const LIST_MAX: usize = 18;
const FORM_SPARSE: u8 = 0;
const FORM_GROUPED: u8 = 1;
/// Set on a form byte when a bounds table precedes the body.
const FORM_BOUNDED: u8 = 2;
/// Set on a form byte when a single term bound precedes the body; the stream
/// then holds at most `BLOCK_POSTINGS` postings.
const FORM_TERM_BOUND: u8 = 4;
/// The form bits that describe bounds rather than the body layout.
const FORM_FLAGS: u8 = FORM_BOUNDED | FORM_TERM_BOUND;
const TAG_LIST: u8 = 0;
const TAG_BITMAP: u8 = 1;

/// Score bounds over one block of field-aware (`LSG4`) postings.
///
/// Shape reconciliation (RFC §5.4): the frozen payload stores one `min_len`
/// per **set bucket of every set field** — a `field_mask` varint, then per
/// set field bit a `bucket_mask` varint and one `min_len` varint per set
/// bucket. The RFC's type sketch instead lists `min_doc_length: [u32; 16]`,
/// a per-field aggregate. A decoded type that keeps only the per-field
/// minimum is lossy: it cannot be re-encoded or merged without inventing
/// per-bucket minima the bytes never carried, and a field-scoped bound
/// needs each selected field's length floor *at that field's own buckets* —
/// flattening is exactly the representation bug that breaks the bound's
/// safety proof (RFC §10 retains the per-`(field, bucket)` minima for later
/// WAND tightening). The type therefore carries the faithful matrix
/// `min_doc_length[field][bucket]`, and the frozen evaluation formula's
/// per-field aggregate is derived by [`FieldBlockBound::field_min_doc_length`];
/// `max_tf_bucket` stays per field, matching both the payload's masks and
/// the frozen formula.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldBlockBound {
    /// The segment header's field count, `1..=16`.
    pub field_count: u8,
    /// Bit i set: field i occurs in the block.
    pub present_fields: u16,
    /// Per field: the highest occurring bucket (0 when the field is absent) —
    /// the highest set bucket of that field's `min_doc_length` row.
    pub max_tf_bucket: [u8; 16],
    /// Per `(field, bucket)`: the shortest length of that field among the
    /// block's postings carrying the field at that bucket. `u32::MAX` marks
    /// an absent pair, mirroring the absent marker the payload rejects.
    pub min_doc_length: [[u32; 16]; 16],
    /// The block's last posting; every posting of the block is at or before it.
    pub last: Tid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
// The size gap is inherent: the faithful per-(field, bucket) matrix is a
// kilobyte where the single-bucket bound is 72 bytes. Boxing the variant
// would deviate from the RFC's frozen sketch; phase-2 WAND holds one at a
// time, so the copy cost is bounded.
#[allow(clippy::large_enum_variant)]
pub enum ScoreBound {
    Single(BlockBound),
    Fields(FieldBlockBound),
}

impl FieldBlockBound {
    /// The bound over postings given as `(field, bucket, field length)`,
    /// ending at `last`. Callers validate fields and buckets (see
    /// [`PostingsBuilder::push_scored_fields`]); out-of-range inputs are
    /// skipped, which can only loosen the bound, never tighten it. A length
    /// of `u32::MAX` is recorded one shorter — as [`BlockBound::over`] does —
    /// so the value can mark absent pairs.
    pub fn over(postings: &[(u8, u8, u32)], last: Tid, field_count: u8) -> Self {
        let field_count = field_count.min(16);
        let mut out = Self {
            field_count,
            present_fields: 0,
            max_tf_bucket: [0; 16],
            min_doc_length: [[u32::MAX; 16]; 16],
            last,
        };
        for &(field, bucket, len) in postings {
            if field >= field_count || field >= 16 || bucket >= BUCKET_COUNT as u8 {
                continue;
            }
            let i = usize::from(field);
            out.present_fields |= 1 << field;
            out.max_tf_bucket[i] = out.max_tf_bucket[i].max(bucket);
            out.min_doc_length[i][usize::from(bucket)] =
                out.min_doc_length[i][usize::from(bucket)].min(len.min(u32::MAX - 1));
        }
        out
    }

    /// The frozen evaluation formula's per-field aggregate: the shortest
    /// length over the field's set buckets (`u32::MAX` when the field is
    /// absent from the block).
    pub fn field_min_doc_length(&self, field: u8) -> u32 {
        self.min_doc_length
            .get(usize::from(field))
            .map_or(u32::MAX, |row| {
                row.iter().copied().min().unwrap_or(u32::MAX)
            })
    }

    /// Every occurring `(field, bucket)` pair with its minimum, ascending by
    /// field then bucket.
    pub fn pairs(&self) -> impl Iterator<Item = (u8, u8, u32)> + '_ {
        (0u16..16)
            .filter(|&field| self.present_fields & (1 << field) != 0)
            .flat_map(move |field| {
                (0usize..BUCKET_COUNT)
                    .filter(move |&bucket| {
                        self.min_doc_length[usize::from(field)][bucket] != u32::MAX
                    })
                    .map(move |bucket| {
                        (
                            field as u8,
                            bucket as u8,
                            self.min_doc_length[usize::from(field)][bucket],
                        )
                    })
            })
    }

    /// The looser of two bounds: unioned fields, per-`(field, bucket)`
    /// minima, per-field maxima, and the later last location — covering both
    /// blocks. The sibling of [`BlockBound::merge`].
    pub fn merge(&self, other: &Self) -> Self {
        let mut out = self.clone();
        out.present_fields |= other.present_fields;
        out.field_count = out.field_count.max(other.field_count);
        for field in 0..16 {
            out.max_tf_bucket[field] = out.max_tf_bucket[field].max(other.max_tf_bucket[field]);
            for bucket in 0..BUCKET_COUNT {
                out.min_doc_length[field][bucket] =
                    out.min_doc_length[field][bucket].min(other.min_doc_length[field][bucket]);
            }
        }
        out.last = out.last.max(other.last);
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockBound {
    /// Per term-frequency bucket, the shortest document in the block with
    /// that bucket; `u32::MAX` where the bucket does not occur.
    pub min_len: [u32; BUCKET_COUNT],
    /// The block's last posting; every posting of the block is at or before it.
    pub last: Tid,
}

impl BlockBound {
    /// The buckets that occur in the block with their shortest document.
    pub fn buckets(&self) -> impl Iterator<Item = (u8, u32)> + '_ {
        self.min_len
            .iter()
            .enumerate()
            .filter(|(_, len)| **len != u32::MAX)
            .map(|(bucket, len)| (bucket as u8, *len))
    }

    /// Largest bucket in the block.
    pub fn max_tf_bucket(&self) -> u8 {
        self.buckets().map(|(bucket, _)| bucket).max().unwrap_or(0)
    }

    /// Shortest document in the block.
    pub fn shortest(&self) -> u32 {
        self.min_len.iter().copied().min().unwrap_or(u32::MAX)
    }

    /// The tighter of two bounds' minima per bucket, covering both blocks.
    pub fn merge(&self, other: &Self) -> Self {
        let mut min_len = self.min_len;
        for (mine, theirs) in min_len.iter_mut().zip(&other.min_len) {
            *mine = (*mine).min(*theirs);
        }
        Self {
            min_len,
            last: self.last.max(other.last),
        }
    }

    /// The bound over postings given as (bucket, document length), ending at
    /// `last`. A length of `u32::MAX` is recorded one shorter, which only
    /// loosens the bound, so the value can mark absent buckets.
    pub fn over(postings: &[(u8, u32)], last: Tid) -> Self {
        let mut min_len = [u32::MAX; BUCKET_COUNT];
        for (bucket, len) in postings {
            let slot = &mut min_len[usize::from(*bucket)];
            *slot = (*slot).min((*len).min(u32::MAX - 1));
        }
        Self { min_len, last }
    }
}

/// Accumulates strictly increasing tuple locations for one term.
#[derive(Default, Debug)]
pub struct PostingsBuilder {
    tids: Vec<Tid>,
    /// Term-frequency bucket and document length per posting, when every
    /// posting was pushed with [`PostingsBuilder::push_scored`].
    scores: Vec<(u8, u32)>,
    /// Field, bucket and that field's length per posting, when every posting
    /// was pushed with [`PostingsBuilder::push_scored_fields`].
    field_scores: Vec<Vec<(u8, u8, u32)>>,
    /// The field count every field-scored entry agreed on. The encoded
    /// bytes carry only masks, but the writer and the header must agree on
    /// the count: it is what the reader validates the masks against, and a
    /// term hitting only high fields must not shrink it.
    field_count: Option<u8>,
}

impl PostingsBuilder {
    pub fn push(&mut self, tid: Tid) -> Result<()> {
        Tid::new(tid.block, tid.offset)?;
        if self.tids.last().is_some_and(|last| *last >= tid) {
            return Err(Error::Unordered);
        }
        self.tids.push(tid);
        Ok(())
    }

    /// Pushes a posting with the inputs of its score, so the stream carries
    /// block bounds. A stream mixing `push` and `push_scored` carries none.
    pub fn push_scored(&mut self, tid: Tid, tf_bucket: u8, doc_len: u32) -> Result<()> {
        if tf_bucket > crate::payload::MAX_TF_BUCKET {
            return Err(Error::InvalidTfBucket);
        }
        self.push(tid)?;
        self.scores.push((tf_bucket, doc_len));
        Ok(())
    }

    /// Pushes a posting with the inputs of its field-aware score, so an
    /// `LSG4` stream carries field bounds (RFC §5.4). `fields` holds one
    /// `(field, bucket)` pair per field the term hits in this document, in
    /// ascending field order; `field_lens` is the document's full per-field
    /// length row, whose length fixes the field count every push must agree
    /// on. A stream mixing this with `push`/`push_scored` carries no bounds.
    pub fn push_scored_fields(
        &mut self,
        tid: Tid,
        fields: &[(u8, u8)],
        field_lens: &[u32],
    ) -> Result<()> {
        let Ok(field_count) = u8::try_from(field_lens.len()) else {
            return Err(Error::Corrupt("field score inputs"));
        };
        if field_count == 0
            || field_count > 16
            || fields.is_empty()
            || fields.len() > field_lens.len()
            || self.field_count.is_some_and(|count| count != field_count)
        {
            return Err(Error::Corrupt("field score inputs"));
        }
        let mut previous = None;
        let mut owned = Vec::with_capacity(fields.len());
        for &(field, bucket) in fields {
            if usize::from(field) >= field_lens.len()
                || bucket > crate::payload::MAX_TF_BUCKET
                || previous.is_some_and(|p| p >= field)
            {
                return Err(Error::Corrupt("field score inputs"));
            }
            previous = Some(field);
            owned.push((field, bucket, field_lens[usize::from(field)]));
        }
        self.push(tid)?;
        self.field_count = Some(field_count);
        self.field_scores.push(owned);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.tids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tids.is_empty()
    }

    /// Encodes with whichever form is smaller for this list.
    pub fn finish(self) -> Vec<u8> {
        self.finish_as(Format::CURRENT)
    }

    /// Encodes in the layout of an earlier format, for compatibility tests:
    /// `LSG1` streams carry no bounds and `LSG2` streams a table however few
    /// blocks they have.
    pub(crate) fn finish_as(self, format: Format) -> Vec<u8> {
        if format == Format::Lsg4
            && !self.tids.is_empty()
            && let Some(field_count) = self.field_count
            && self.field_scores.len() == self.tids.len()
        {
            let sparse = encode_fields(&self.tids, &self.field_scores, field_count, true);
            let grouped = encode_fields(&self.tids, &self.field_scores, field_count, false);
            return if sparse.len() <= grouped.len() {
                sparse
            } else {
                grouped
            };
        }
        let scores =
            (format.has_bounds() && !self.tids.is_empty() && self.scores.len() == self.tids.len())
                .then_some(self.scores.as_slice());
        let sparse = encode_sparse(&self.tids, scores, format);
        let grouped = encode_grouped(&self.tids, scores, format);
        if sparse.len() <= grouped.len() {
            sparse
        } else {
            grouped
        }
    }
}

/// Encodes one field term bound (RFC §5.4): the field mask, then per set
/// field bit a bucket mask and the shortest field length per set bucket,
/// both ascending. The bound's `last` is not part of these bytes — the table
/// framing (or the term-bound walk) supplies it.
fn encode_field_term_bound(out: &mut Vec<u8>, bound: &FieldBlockBound) {
    varint::put(out, u64::from(bound.present_fields));
    for field in 0..bound.field_count {
        if bound.present_fields & (1 << field) == 0 {
            continue;
        }
        let mut bucket_mask = 0u64;
        for bucket in 0..BUCKET_COUNT {
            if bound.min_doc_length[usize::from(field)][bucket] != u32::MAX {
                bucket_mask |= 1 << bucket;
            }
        }
        varint::put(out, bucket_mask);
        for bucket in 0..BUCKET_COUNT {
            if bucket_mask & (1 << bucket) != 0 {
                varint::put(
                    out,
                    u64::from(bound.min_doc_length[usize::from(field)][bucket]),
                );
            }
        }
    }
}

/// Encodes the `LSG4` bounds table: per block, the field term bound over its
/// postings, the last location and, for sparse streams, where the block
/// starts. The framing is the unchanged `LSG3` shape.
fn encode_field_bounds(
    tids: &[Tid],
    scores: &[Vec<(u8, u8, u32)>],
    field_count: u8,
    starts: Option<&[usize]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous_block = 0u32;
    let mut previous_start = 0usize;
    let blocks = tids
        .chunks(BLOCK_POSTINGS as usize)
        .zip(scores.chunks(BLOCK_POSTINGS as usize));
    for (index, (block, block_scores)) in blocks.enumerate() {
        let flattened: Vec<_> = block_scores
            .iter()
            .flat_map(|v| v.iter().copied())
            .collect();
        encode_field_term_bound(
            &mut out,
            &FieldBlockBound::over(&flattened, block[block.len() - 1], field_count),
        );
        let last = block[block.len() - 1];
        varint::put(&mut out, u64::from(last.block - previous_block));
        varint::put(&mut out, u64::from(last.offset));
        previous_block = last.block;
        if let Some(starts) = starts {
            varint::put(&mut out, (starts[index] - previous_start) as u64);
            previous_start = starts[index];
        }
    }
    out
}

/// The field bounds a scored stream carries, in the envelope's two shapes:
/// a single term bound when the stream is one block, else the table. The
/// field-aware mirror of `encode_scored`.
fn encode_field_scored(
    tids: &[Tid],
    scores: &[Vec<(u8, u8, u32)>],
    field_count: u8,
    starts: Option<&[usize]>,
) -> (u8, Vec<u8>) {
    if tids.len() <= BLOCK_POSTINGS as usize {
        let mut out = Vec::new();
        let flattened: Vec<_> = scores.iter().flat_map(|v| v.iter().copied()).collect();
        encode_field_term_bound(
            &mut out,
            &FieldBlockBound::over(&flattened, tids[tids.len() - 1], field_count),
        );
        (FORM_TERM_BOUND, out)
    } else {
        (
            FORM_BOUNDED,
            encode_field_bounds(tids, scores, field_count, starts),
        )
    }
}

/// Encodes a field-scored stream's body — shared by both layouts — returning
/// the body bytes and, for the sparse layout, where each block starts.
fn encode_fields_body(tids: &[Tid], sparse: bool) -> (Vec<u8>, Vec<usize>) {
    if sparse {
        let mut body = Vec::new();
        let mut starts = Vec::new();
        let mut last_block = 0u32;
        for (ordinal, tid) in tids.iter().enumerate() {
            if (ordinal as u32).is_multiple_of(BLOCK_POSTINGS) {
                starts.push(body.len());
            }
            varint::put(&mut body, u64::from(tid.block - last_block));
            varint::put(&mut body, u64::from(tid.offset));
            last_block = tid.block;
        }
        (body, starts)
    } else {
        let mut body = Vec::new();
        write_grouped_body(&mut body, tids);
        (body, Vec::new())
    }
}

/// Encodes a field-scored stream in one body layout; the caller picks the
/// smaller of the two, as the fieldless builder does.
fn encode_fields(
    tids: &[Tid],
    scores: &[Vec<(u8, u8, u32)>],
    field_count: u8,
    sparse: bool,
) -> Vec<u8> {
    let (body, starts) = encode_fields_body(tids, sparse);
    let bounds = encode_field_scored(tids, scores, field_count, sparse.then_some(&starts));
    let mut out = encode_header(
        if sparse { FORM_SPARSE } else { FORM_GROUPED },
        tids.len(),
        Some(&bounds),
    );
    out.extend_from_slice(&body);
    out
}

fn encode_term_bound(out: &mut Vec<u8>, bound: &BlockBound) {
    let buckets = bound
        .buckets()
        .fold(0u64, |mask, (bucket, _)| mask | 1 << bucket);
    varint::put(out, buckets);
    for (_, len) in bound.buckets() {
        varint::put(out, u64::from(len));
    }
}

/// Encodes the bounds table: per block, the bound over the score inputs, the
/// last location and, for sparse streams, where the block starts.
fn encode_bounds(tids: &[Tid], scores: &[(u8, u32)], starts: Option<&[usize]>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous_block = 0u32;
    let mut previous_start = 0usize;
    let blocks = tids
        .chunks(BLOCK_POSTINGS as usize)
        .zip(scores.chunks(BLOCK_POSTINGS as usize));
    for (index, (block, block_scores)) in blocks.enumerate() {
        let last = block[block.len() - 1];
        encode_term_bound(&mut out, &BlockBound::over(block_scores, last));
        varint::put(&mut out, u64::from(last.block - previous_block));
        varint::put(&mut out, u64::from(last.offset));
        previous_block = last.block;
        if let Some(starts) = starts {
            varint::put(&mut out, (starts[index] - previous_start) as u64);
            previous_start = starts[index];
        }
    }
    out
}

/// The bounds a scored stream carries: a single term bound when it is one
/// block, else the table. Returns the form bit and the encoded bytes.
fn encode_scored(
    tids: &[Tid],
    scores: &[(u8, u32)],
    starts: Option<&[usize]>,
    format: Format,
) -> (u8, Vec<u8>) {
    if format >= Format::Lsg3 && tids.len() <= BLOCK_POSTINGS as usize {
        let mut out = Vec::new();
        encode_term_bound(&mut out, &BlockBound::over(scores, tids[tids.len() - 1]));
        (FORM_TERM_BOUND, out)
    } else {
        (FORM_BOUNDED, encode_bounds(tids, scores, starts))
    }
}

fn encode_header(form: u8, count: usize, bounds: Option<&(u8, Vec<u8>)>) -> Vec<u8> {
    let mut out = vec![bounds.map_or(form, |(bit, _)| form | bit)];
    varint::put(&mut out, count as u64);
    if let Some((bit, bounds)) = bounds {
        if *bit == FORM_BOUNDED {
            varint::put(&mut out, bounds.len() as u64);
        }
        out.extend_from_slice(bounds);
    }
    out
}

fn encode_sparse(tids: &[Tid], scores: Option<&[(u8, u32)]>, format: Format) -> Vec<u8> {
    let mut body = Vec::new();
    let mut starts = Vec::new();
    let mut last_block = 0u32;
    for (ordinal, tid) in tids.iter().enumerate() {
        if (ordinal as u32).is_multiple_of(BLOCK_POSTINGS) {
            starts.push(body.len());
        }
        varint::put(&mut body, u64::from(tid.block - last_block));
        varint::put(&mut body, u64::from(tid.offset));
        last_block = tid.block;
    }
    let bounds = scores.map(|scores| encode_scored(tids, scores, Some(&starts), format));
    let mut out = encode_header(FORM_SPARSE, tids.len(), bounds.as_ref());
    out.extend_from_slice(&body);
    out
}

fn encode_grouped(tids: &[Tid], scores: Option<&[(u8, u32)]>, format: Format) -> Vec<u8> {
    let bounds = scores.map(|scores| encode_scored(tids, scores, None, format));
    let mut out = encode_header(FORM_GROUPED, tids.len(), bounds.as_ref());
    write_grouped_body(&mut out, tids);
    out
}

/// Writes the grouped body: page-bitmap groups covering 256 blocks each.
fn write_grouped_body(out: &mut Vec<u8>, tids: &[Tid]) {
    let groups = tids.chunk_by(|a, b| a.group() == b.group());
    varint::put(out, groups.clone().count() as u64);
    let mut previous_gid: Option<u32> = None;
    for group in groups {
        let gid = group[0].group();
        match previous_gid {
            None => varint::put(out, u64::from(gid)),
            Some(previous) => varint::put(out, u64::from(gid - previous - 1)),
        }
        previous_gid = Some(gid);
        varint::put(out, group.len() as u64);
        let mut page_bitmap = [0u8; PAGE_BITMAP_BYTES];
        let mut body = Vec::new();
        for page in group.chunk_by(|a, b| a.block == b.block) {
            let bit = page[0].page_bit();
            page_bitmap[usize::from(bit / 8)] |= 1 << (bit % 8);
            if page.len() <= LIST_MAX {
                body.push(TAG_LIST);
                varint::put(&mut body, page.len() as u64);
                for tid in page {
                    body.extend_from_slice(&tid.offset.to_le_bytes());
                }
            } else {
                body.push(TAG_BITMAP);
                let mut tuple_bitmap = [0u8; TUPLE_BITMAP_BYTES];
                for tid in page {
                    let bit = usize::from(tid.offset - 1);
                    tuple_bitmap[bit / 8] |= 1 << (bit % 8);
                }
                body.extend_from_slice(&tuple_bitmap);
            }
        }
        out.extend_from_slice(&page_bitmap);
        varint::put(out, body.len() as u64);
        out.extend_from_slice(&body);
    }
}

/// A parsed stream header. Parsing reads only the header; bodies are validated
/// as cursors traverse them.
#[derive(Clone, Copy, Debug)]
pub struct Postings<'a> {
    bytes: &'a [u8],
    form: u8,
    count: u32,
    /// Where the bounds table is, when the stream carries one.
    bounds: Option<(usize, usize)>,
    body_at: usize,
    /// The segment header's field count when this is an `LSG4` stream;
    /// bounds are decoded and validated field-aware.
    fields: Option<u8>,
}

impl<'a> Postings<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        Self::parse_format(bytes, Format::CURRENT)
    }

    pub fn parse_format(bytes: &'a [u8], format: Format) -> Result<Self> {
        if format == Format::Lsg4 {
            // Field bounds need the header's field count; without it a legacy
            // decode could misparse field-bound bytes as an LSG3 term bound.
            return Err(Error::Corrupt("postings format"));
        }
        Self::parse_inner(bytes, None)
    }

    /// Parses an `LSG4` stream whose segment header declares `field_count`
    /// fields. The envelope is the unchanged `LSG3` shape; the term bound and
    /// bounds table carry the RFC §5.4 field layout and are validated against
    /// the complete rule set as they are decoded.
    pub fn parse_fields(bytes: &'a [u8], field_count: u8) -> Result<Self> {
        if field_count == 0 || field_count > 16 {
            return Err(Error::Corrupt("segment field count"));
        }
        Self::parse_inner(bytes, Some(field_count))
    }

    fn parse_inner(bytes: &'a [u8], fields: Option<u8>) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        let form = reader.u8()?;
        let layout = form & !FORM_FLAGS;
        if (layout != FORM_SPARSE && layout != FORM_GROUPED)
            || (form & FORM_BOUNDED != 0 && form & FORM_TERM_BOUND != 0)
        {
            return Err(Error::Corrupt("unknown postings form"));
        }
        let count = reader.varint_u32()?;
        let bounds = if form & FORM_BOUNDED != 0 {
            let len = reader.varint_u32()? as usize;
            let at = reader.position();
            reader.skip(len)?;
            Some((at, len))
        } else if form & FORM_TERM_BOUND != 0 {
            if count == 0 || count > BLOCK_POSTINGS {
                return Err(Error::Corrupt("term bound on a stream of several blocks"));
            }
            let at = reader.position();
            match fields {
                None => {
                    decode_term_bound(&mut reader)?;
                }
                Some(field_count) => {
                    decode_field_term_bound(&mut reader, field_count)?;
                }
            }
            Some((at, reader.position() - at))
        } else {
            None
        };
        Ok(Self {
            bytes,
            form,
            count,
            bounds,
            body_at: reader.position(),
            fields,
        })
    }

    /// Number of postings, from the header.
    pub const fn count(&self) -> u32 {
        self.count
    }

    pub const fn is_grouped(&self) -> bool {
        self.form & !FORM_FLAGS == FORM_GROUPED
    }

    /// True when the stream carries per-block score bounds.
    pub const fn has_bounds(&self) -> bool {
        self.bounds.is_some()
    }

    /// True when the bounds are a single term bound rather than a table.
    pub const fn has_term_bound(&self) -> bool {
        self.form & FORM_TERM_BOUND != 0
    }

    /// Bytes of the bounds table, for size accounting.
    pub const fn bounds_len(&self) -> usize {
        match self.bounds {
            Some((_, len)) => len,
            None => 0,
        }
    }

    /// Where the bounds table sits in the stream, when it carries one.
    /// Test and maintenance code uses this to aim corruption at a bound.
    pub const fn bounds_position(&self) -> Option<(usize, usize)> {
        self.bounds
    }

    /// Bytes of the body after the header and bounds table.
    pub const fn body_len(&self) -> usize {
        self.bytes.len() - self.body_at
    }

    fn bounds_table(&self) -> Option<Bounds<'a>> {
        self.bounds.map(|(at, len)| Bounds {
            reader: Reader::new(&self.bytes[at..at + len]),
            blocks: self.count.div_ceil(BLOCK_POSTINGS),
            starts: if self.is_grouped() || self.has_term_bound() {
                None
            } else {
                Some(Vec::new())
            },
            body_len: self.bytes.len() - self.body_at,
            entries: Vec::new(),
            field_entries: Vec::new(),
            stream: *self,
            compact: self.has_term_bound(),
            term_last: None,
            fields: self.fields,
        })
    }

    fn grouped(&self) -> Result<GroupedCursor<'a>> {
        let mut reader = Reader::at(self.bytes, self.body_at);
        let groups_left = reader.varint_u32()?;
        Ok(GroupedCursor {
            reader,
            total: self.count,
            groups_left,
            previous_gid: None,
            gid: 0,
            group_count: 0,
            page_bitmap: [0; PAGE_BITMAP_BYTES],
            body_end: 0,
            next_bit: 0,
            group_consumed: 0,
            block: 0,
            offsets: Vec::new(),
            index: 0,
            current: None,
            ordinal: 0,
            seen: 0,
            bounds: None,
        })
    }

    /// Whether masks amortize their fixed five-word cost. Grouped encoding
    /// alone is not enough: it can also win for many sparsely occupied pages.
    /// Read group headers only, skipping offset bodies. Four tuples per page
    /// is a conservative crossover for the bulk count path.
    pub fn prefers_pages(&self) -> Result<bool> {
        if !self.is_grouped() {
            return Ok(false);
        }
        let mut groups = self.grouped()?;
        if u64::from(self.count) >= u64::from(groups.groups_left) * u64::from(GROUP_BLOCKS) * 4 {
            return Ok(self.count != 0);
        }
        let mut pages = 0u64;
        while groups.enter_group()? {
            pages += groups
                .page_bitmap
                .iter()
                .map(|b| u64::from(b.count_ones()))
                .sum::<u64>();
            groups.reader.seek(groups.body_end)?;
        }
        Ok(pages != 0 && u64::from(self.count) >= pages * 4)
    }

    /// Reads dense pages as bitmaps without expanding them into tuple IDs.
    pub fn pages(&self) -> Result<Box<dyn crate::pages::Cursor + 'a>> {
        if self.is_grouped() {
            let mut pages = GroupedPages {
                inner: self.grouped()?,
                current: None,
            };
            crate::pages::Cursor::advance(&mut pages)?;
            Ok(Box::new(pages))
        } else {
            Ok(Box::new(crate::pages::Rows::new(self.cursor_with(false)?)?))
        }
    }

    pub fn cursor(&self) -> Result<PostingsCursor<'a>> {
        self.cursor_with(true)
    }

    /// A cursor, with or without its bounds attached.
    fn cursor_with(&self, with_bounds: bool) -> Result<PostingsCursor<'a>> {
        let bounds = if with_bounds {
            self.bounds_table()
        } else {
            None
        };
        let mut cursor = if self.is_grouped() {
            let mut grouped = self.grouped()?;
            grouped.bounds = bounds;
            PostingsCursor::Grouped(grouped)
        } else {
            PostingsCursor::Sparse(SparseCursor {
                reader: Reader::at(self.bytes, self.body_at),
                body_at: self.body_at,
                total: self.count,
                remaining: self.count,
                last_block: 0,
                current: None,
                ordinal: 0,
                bounds,
            })
        };
        cursor.start()?;
        Ok(cursor)
    }

    /// Decodes every posting. Fails on the first malformed byte.
    pub fn to_vec(&self) -> Result<Vec<Tid>> {
        let mut cursor = self.cursor()?;
        // The count is untrusted until the stream has been decoded.
        // Bound speculative allocation by bytes actually present.
        let mut out = Vec::with_capacity((self.count as usize).min(self.bytes.len()));
        while let Some(tid) = cursor.current() {
            out.push(tid);
            cursor.advance()?;
        }
        if out.len() != self.count as usize {
            return Err(Error::Corrupt("posting count mismatch"));
        }
        Ok(out)
    }
}

/// Positioned at one posting (or exhausted), with that posting's ordinal.
#[derive(Clone, Debug)]
pub enum PostingsCursor<'a> {
    Sparse(SparseCursor<'a>),
    Grouped(GroupedCursor<'a>),
}

impl<'a> PostingsCursor<'a> {
    fn start(&mut self) -> Result<()> {
        match self {
            Self::Sparse(cursor) => cursor.load_next(),
            Self::Grouped(cursor) => cursor.open_page_at_or_after(0),
        }
    }

    /// Zero-based index of the current posting within its stream.
    pub fn ordinal(&self) -> u32 {
        match self {
            Self::Sparse(cursor) => cursor.ordinal,
            Self::Grouped(cursor) => cursor.ordinal,
        }
    }

    /// Ordinal of `tid` if present. Leaves the cursor at or after `tid`.
    pub fn rank(&mut self, tid: Tid) -> Result<Option<u32>> {
        self.seek(tid)?;
        Ok((self.current() == Some(tid)).then(|| self.ordinal()))
    }

    fn bounds_mut(&mut self) -> Option<&mut Bounds<'a>> {
        match self {
            Self::Sparse(cursor) => cursor.bounds.as_mut(),
            Self::Grouped(cursor) => cursor.bounds.as_mut(),
        }
    }

    /// True when the stream carries per-block score bounds.
    pub fn has_bounds(&self) -> bool {
        match self {
            Self::Sparse(cursor) => cursor.bounds.is_some(),
            Self::Grouped(cursor) => cursor.bounds.is_some(),
        }
    }

    /// Bounds of the block holding the first posting at or after `target`
    /// that the cursor has not passed: the current block when `target` is at
    /// or before the current posting. `None` when no such posting exists or
    /// the stream carries no bounds. The cursor does not move.
    pub fn bound_at(&mut self, target: Tid) -> Result<Option<BlockBound>> {
        let Some(current) = self.current() else {
            return Ok(None);
        };
        let target = target.max(current);
        let mut block = self.ordinal() / BLOCK_POSTINGS;
        self.resolve_term_last()?;
        let Some(bounds) = self.bounds_mut() else {
            return Ok(None);
        };
        loop {
            let Some(entry) = bounds.entry(block)? else {
                return Ok(None);
            };
            if entry.last >= target {
                return Ok(Some(entry));
            }
            block += 1;
        }
    }

    /// Every block's bounds, in order; empty when the stream carries none.
    pub fn block_bounds(&mut self) -> Result<Vec<BlockBound>> {
        let mut all = Vec::new();
        self.block_bounds_into(&mut all)?;
        Ok(all)
    }

    /// Reuse caller scratch while checking and decoding every block bound.
    /// Scratch is cleared first; on error it can contain a decoded prefix.
    pub(crate) fn block_bounds_into(&mut self, all: &mut Vec<BlockBound>) -> Result<()> {
        all.clear();
        self.resolve_term_last()?;
        let Some(bounds) = self.bounds_mut() else {
            return Ok(());
        };
        // A corrupt count can imply millions of bounds in a tiny stream.
        for block in 0..bounds.blocks {
            all.push(bounds.entry(block)?.expect("block index is in range"));
        }
        Ok(())
    }

    /// Bounds of the block holding the first posting at or after `target` on
    /// a field-aware (`LSG4`) stream; `None` when no such posting exists or
    /// the stream carries no bounds. The field-aware mirror of
    /// [`PostingsCursor::bound_at`]; the cursor does not move.
    pub fn field_bound_at(&mut self, target: Tid) -> Result<Option<FieldBlockBound>> {
        let Some(current) = self.current() else {
            return Ok(None);
        };
        let target = target.max(current);
        let mut block = self.ordinal() / BLOCK_POSTINGS;
        self.resolve_term_last()?;
        let Some(bounds) = self.bounds_mut() else {
            return Ok(None);
        };
        loop {
            let Some(entry) = bounds.field_entry(block)? else {
                return Ok(None);
            };
            if entry.last >= target {
                return Ok(Some(entry));
            }
            block += 1;
        }
    }

    /// Every block's field bounds, in order; empty when the stream carries
    /// none.
    pub fn field_block_bounds(&mut self) -> Result<Vec<FieldBlockBound>> {
        let mut all = Vec::new();
        self.field_block_bounds_into(&mut all)?;
        Ok(all)
    }

    /// Reuse caller scratch while checking and decoding every field block
    /// bound. Scratch is cleared first; on error it can contain a prefix.
    pub(crate) fn field_block_bounds_into(&mut self, all: &mut Vec<FieldBlockBound>) -> Result<()> {
        all.clear();
        self.resolve_term_last()?;
        let Some(bounds) = self.bounds_mut() else {
            return Ok(());
        };
        for block in 0..bounds.blocks {
            all.push(bounds.field_entry(block)?.expect("block index is in range"));
        }
        Ok(())
    }

    /// A term bound stores no last location: find the stream's last posting
    /// by walking a fresh cursor once, so the bound covers exactly the
    /// stream. The walk is short, since such a stream is one block.
    fn resolve_term_last(&mut self) -> Result<()> {
        let stream = match self.bounds_mut() {
            Some(bounds) if bounds.compact && bounds.term_last.is_none() => bounds.stream,
            _ => return Ok(()),
        };
        let mut probe = stream.cursor_with(false)?;
        let mut last = None;
        while let Some(current) = probe.current() {
            last = Some(current);
            probe.advance()?;
        }
        let last = last.ok_or(Error::Corrupt("term bound on an empty stream"))?;
        self.bounds_mut().expect("checked above").term_last = Some(last);
        Ok(())
    }
}

impl Cursor for PostingsCursor<'_> {
    fn current(&self) -> Option<Tid> {
        match self {
            Self::Sparse(cursor) => cursor.current,
            Self::Grouped(cursor) => cursor.current,
        }
    }

    fn advance(&mut self) -> Result<()> {
        match self {
            Self::Sparse(cursor) => {
                if cursor.current.is_some() {
                    cursor.ordinal += 1;
                    cursor.load_next()?;
                }
                Ok(())
            }
            Self::Grouped(cursor) => cursor.advance(),
        }
    }

    fn seek(&mut self, target: Tid) -> Result<()> {
        match self {
            Self::Sparse(cursor) => cursor.seek(target),
            Self::Grouped(cursor) => cursor.seek(target),
        }
    }
}

/// Decodes a term bound: the bucket mask and the shortest document per
/// bucket that occurs.
fn decode_term_bound(reader: &mut Reader<'_>) -> Result<[u32; BUCKET_COUNT]> {
    let buckets = reader.varint_u32()?;
    if buckets == 0 || buckets >> BUCKET_COUNT != 0 {
        return Err(Error::Corrupt("block bound buckets"));
    }
    let mut min_len = [u32::MAX; BUCKET_COUNT];
    for (bucket, len) in min_len.iter_mut().enumerate() {
        if buckets & (1 << bucket) != 0 {
            *len = reader.varint_u32()?;
            if *len == u32::MAX {
                return Err(Error::Corrupt("block bound length"));
            }
        }
    }
    Ok(min_len)
}

/// Decodes an `LSG4` term bound (RFC §5.4), validating the complete rule
/// set: a non-empty field mask within `field_count`, per set field a
/// non-empty bucket mask within `BUCKET_COUNT`, and a `min_len` below
/// `u32::MAX` for every set `(field, bucket)` pair. The returned bound's
/// `last` is a placeholder: these bytes carry no location, and the caller
/// stamps the block's real last posting.
fn decode_field_term_bound(reader: &mut Reader<'_>, field_count: u8) -> Result<FieldBlockBound> {
    let field_mask = reader.varint_u32()?;
    if field_count == 0 || field_count > 16 || field_mask == 0 || field_mask >> field_count != 0 {
        return Err(Error::Corrupt("field bound mask"));
    }
    let mut bound = FieldBlockBound {
        field_count,
        present_fields: field_mask as u16,
        max_tf_bucket: [0; 16],
        min_doc_length: [[u32::MAX; 16]; 16],
        last: Tid {
            block: 0,
            offset: 1,
        },
    };
    for field in 0..16u32 {
        if field_mask & (1 << field) == 0 {
            continue;
        }
        let bucket_mask = reader.varint_u32()?;
        if bucket_mask == 0 || bucket_mask >> BUCKET_COUNT != 0 {
            return Err(Error::Corrupt("field bound buckets"));
        }
        for bucket in 0..BUCKET_COUNT {
            if bucket_mask & (1 << bucket) == 0 {
                continue;
            }
            let len = reader.varint_u32()?;
            if len == u32::MAX {
                return Err(Error::Corrupt("field bound length"));
            }
            bound.min_doc_length[field as usize][bucket] = len;
            bound.max_tf_bucket[field as usize] =
                bound.max_tf_bucket[field as usize].max(bucket as u8);
        }
    }
    Ok(bound)
}

/// The bounds table, decoded one entry at a time as the cursor moves.
#[derive(Clone, Debug)]
struct Bounds<'a> {
    /// Positioned at the next undecoded entry.
    reader: Reader<'a>,
    blocks: u32,
    /// Sparse streams with a table only: byte offset of each block's first
    /// posting.
    starts: Option<Vec<usize>>,
    body_len: usize,
    entries: Vec<BlockBound>,
    /// Field-aware (`LSG4`) bounds, when the stream is one; `entries` stays
    /// empty then, and vice versa.
    field_entries: Vec<FieldBlockBound>,
    /// The stream, so a term bound's last location can be found.
    stream: Postings<'a>,
    /// True for a term bound: one entry, without a stored last location.
    compact: bool,
    /// A term bound's last location once found (see
    /// [`PostingsCursor::resolve_term_last`]).
    term_last: Option<Tid>,
    /// The segment header's field count for `LSG4` streams.
    fields: Option<u8>,
}

impl Bounds<'_> {
    /// Decodes up to and including entry `block`; `None` when out of range.
    /// Errors on a field-aware stream: a [`BlockBound`] cannot represent a
    /// field bound (use [`Bounds::field_entry`]).
    fn entry(&mut self, block: u32) -> Result<Option<BlockBound>> {
        if self.fields.is_some() {
            return Err(Error::Corrupt("postings bound form"));
        }
        if block >= self.blocks {
            return Ok(None);
        }
        while self.entries.len() <= block as usize {
            self.decode()?;
        }
        Ok(Some(self.entries[block as usize]))
    }

    /// The field-aware mirror of [`Bounds::entry`]; errors on an `LSG1-3`
    /// stream.
    fn field_entry(&mut self, block: u32) -> Result<Option<FieldBlockBound>> {
        if self.fields.is_none() {
            return Err(Error::Corrupt("postings bound form"));
        }
        if block >= self.blocks {
            return Ok(None);
        }
        while self.field_entries.len() <= block as usize {
            self.decode()?;
        }
        Ok(Some(self.field_entries[block as usize].clone()))
    }

    /// The block's last posting location, whichever bound shape the stream
    /// carries. The sparse seek path's block-skip step needs only this.
    fn last_of(&mut self, block: u32) -> Result<Option<Tid>> {
        match self.fields {
            None => Ok(self.entry(block)?.map(|entry| entry.last)),
            Some(_) => Ok(self.field_entry(block)?.map(|entry| entry.last)),
        }
    }

    fn decode(&mut self) -> Result<()> {
        let (min_len, field_bound) = match self.fields {
            None => (Some(decode_term_bound(&mut self.reader)?), None),
            Some(field_count) => (
                None,
                Some(decode_field_term_bound(&mut self.reader, field_count)?),
            ),
        };
        let previous_last = match self.fields {
            None => self.entries.last().map(|entry| entry.last),
            Some(_) => self.field_entries.last().map(|entry| entry.last),
        };
        let last = if self.compact {
            self.term_last
                .expect("a term bound's last location is resolved before decoding")
        } else {
            let block = previous_last
                .map_or(0, |last| last.block)
                .checked_add(self.reader.varint_u32()?)
                .ok_or(Error::Corrupt("block overflow"))?;
            let offset = u16::try_from(self.reader.varint_u32()?).map_err(|_| Error::InvalidTid)?;
            let last = Tid::new(block, offset)?;
            if previous_last.is_some_and(|previous| previous >= last) {
                return Err(Error::Corrupt("block bounds not increasing"));
            }
            if let Some(starts) = self.starts.as_mut() {
                let previous_start = starts.last().copied();
                let start = previous_start
                    .unwrap_or(0)
                    .checked_add(self.reader.varint()? as usize)
                    .filter(|start| *start < self.body_len)
                    .ok_or(Error::Corrupt("block start beyond body"))?;
                if previous_start.is_some_and(|previous| previous >= start) {
                    return Err(Error::Corrupt("block starts not increasing"));
                }
                starts.push(start);
            }
            last
        };
        match (min_len, field_bound) {
            (Some(min_len), None) => self.entries.push(BlockBound { min_len, last }),
            (None, Some(bound)) => {
                self.field_entries.push(FieldBlockBound { last, ..bound });
            }
            _ => unreachable!("exactly one bound shape is decoded"),
        }
        let decoded = if self.fields.is_some() {
            self.field_entries.len()
        } else {
            self.entries.len()
        };
        if decoded == self.blocks as usize && self.reader.remaining() != 0 {
            return Err(Error::Corrupt("block bounds length"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SparseCursor<'a> {
    reader: Reader<'a>,
    body_at: usize,
    total: u32,
    remaining: u32,
    last_block: u32,
    current: Option<Tid>,
    ordinal: u32,
    bounds: Option<Bounds<'a>>,
}

impl SparseCursor<'_> {
    fn seek(&mut self, target: Tid) -> Result<()> {
        if self.bounds.as_ref().is_some_and(|b| b.starts.is_some()) {
            self.skip_blocks_before(target)?;
        }
        while self.current.is_some_and(|current| current < target) {
            self.ordinal += 1;
            self.load_next()?;
        }
        Ok(())
    }

    /// Jumps over whole blocks whose last posting is before `target`.
    fn skip_blocks_before(&mut self, target: Tid) -> Result<()> {
        while self.current.is_some() {
            let bounds = self.bounds.as_mut().expect("bounded stream");
            let block = self.ordinal / BLOCK_POSTINGS;
            let last = bounds
                .last_of(block)?
                .ok_or(Error::Corrupt("posting beyond block bounds"))?;
            if last >= target {
                return Ok(());
            }
            if bounds.last_of(block + 1)?.is_none() {
                self.current = None;
                self.remaining = 0;
                self.ordinal = self.total;
                return Ok(());
            }
            let start =
                bounds.starts.as_ref().expect("sparse bounds track starts")[block as usize + 1];
            self.reader.seek(self.body_at + start)?;
            self.last_block = last.block;
            self.ordinal = (block + 1) * BLOCK_POSTINGS;
            self.remaining = self.total - self.ordinal;
            self.current = Some(last);
            self.load_next()?;
        }
        Ok(())
    }

    fn load_next(&mut self) -> Result<()> {
        if self.remaining == 0 {
            self.current = None;
            return Ok(());
        }
        let delta = self.reader.varint_u32()?;
        let block = self
            .last_block
            .checked_add(delta)
            .ok_or(Error::Corrupt("block overflow"))?;
        let offset = self.reader.varint_u32()?;
        let offset = u16::try_from(offset).map_err(|_| Error::InvalidTid)?;
        let tid = Tid::new(block, offset)?;
        if self.current.is_some_and(|current| current >= tid) {
            return Err(Error::Corrupt("sparse postings not increasing"));
        }
        self.last_block = block;
        self.remaining -= 1;
        self.current = Some(tid);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct GroupedCursor<'a> {
    reader: Reader<'a>,
    total: u32,
    groups_left: u32,
    previous_gid: Option<u32>,
    gid: u32,
    group_count: u32,
    page_bitmap: [u8; PAGE_BITMAP_BYTES],
    body_end: usize,
    /// Next page bit to examine within the current group.
    next_bit: u16,
    /// Postings of this group in pages already passed (decoded or skipped).
    group_consumed: u32,
    block: u32,
    offsets: Vec<u16>,
    index: usize,
    current: Option<Tid>,
    ordinal: u32,
    /// Total postings accounted for across finished groups; checked at the end.
    seen: u32,
    bounds: Option<Bounds<'a>>,
}

struct GroupedPages<'a> {
    inner: GroupedCursor<'a>,
    current: Option<crate::pages::Page>,
}

impl crate::pages::Cursor for GroupedPages<'_> {
    fn current(&self) -> Option<crate::pages::Page> {
        self.current
    }
    fn advance(&mut self) -> Result<()> {
        let c = &mut self.inner;
        self.current = None;
        loop {
            if c.previous_gid.is_some() {
                if let Some(bit) = c.next_set_bit(c.next_bit) {
                    let offsets = c.decode_offsets()?;
                    c.next_bit = bit + 1;
                    c.group_consumed = c
                        .group_consumed
                        .checked_add(offsets.count())
                        .ok_or(Error::Corrupt("posting count overflow"))?;
                    let block = c.gid * GROUP_BLOCKS + u32::from(bit);
                    Tid::new(block, 1)?;
                    self.current = Some(crate::pages::Page { block, offsets });
                    return Ok(());
                }
                c.finish_group()?;
            }
            if !c.enter_group()? {
                if c.seen != c.total {
                    return Err(Error::Corrupt("posting count mismatch"));
                }
                // An exhausted cursor must remain exhausted on further advances.
                c.previous_gid = None;
                return Ok(());
            }
        }
    }
    fn seek(&mut self, block: u32) -> Result<()> {
        if self.current.is_none_or(|page| page.block >= block) {
            return Ok(());
        }
        let c = &mut self.inner;
        let target_gid = block / GROUP_BLOCKS;
        // Skip complete group bodies by their stored length/count.
        while c.gid < target_gid {
            c.reader.seek(c.body_end)?;
            c.group_consumed = c.group_count;
            c.finish_group()?;
            if !c.enter_group()? {
                if c.seen != c.total {
                    return Err(Error::Corrupt("posting count mismatch"));
                }
                c.previous_gid = None;
                self.current = None;
                return Ok(());
            }
        }
        if c.gid == target_gid {
            c.skip_pages_before((block % GROUP_BLOCKS) as u16)?;
        }
        self.advance()
    }
}

impl<'a> GroupedCursor<'a> {
    /// Reads the next group header. Returns false when no groups remain.
    fn enter_group(&mut self) -> Result<bool> {
        if self.groups_left == 0 {
            return Ok(false);
        }
        self.groups_left -= 1;
        let raw = self.reader.varint_u32()?;
        self.gid = match self.previous_gid {
            None => raw,
            Some(previous) => previous
                .checked_add(raw)
                .and_then(|gid| gid.checked_add(1))
                .ok_or(Error::Corrupt("group id overflow"))?,
        };
        if u64::from(self.gid) * u64::from(GROUP_BLOCKS) > u64::from(crate::tid::MAX_BLOCK) {
            return Err(Error::Corrupt("group beyond block range"));
        }
        self.previous_gid = Some(self.gid);
        self.group_count = self.reader.varint_u32()?;
        if self.group_count == 0 {
            return Err(Error::Corrupt("empty group"));
        }
        self.page_bitmap
            .copy_from_slice(self.reader.take(PAGE_BITMAP_BYTES)?);
        let body_len = self.reader.varint()? as usize;
        self.body_end = self
            .reader
            .position()
            .checked_add(body_len)
            .filter(|end| *end <= self.reader.position() + self.reader.remaining())
            .ok_or(Error::Truncated)?;
        self.next_bit = 0;
        self.group_consumed = 0;
        Ok(true)
    }

    fn finish_group(&mut self) -> Result<()> {
        if self.group_consumed != self.group_count {
            return Err(Error::Corrupt("group count mismatch"));
        }
        if self.reader.position() != self.body_end {
            return Err(Error::Corrupt("group body length mismatch"));
        }
        self.seen = self
            .seen
            .checked_add(self.group_count)
            .ok_or(Error::Corrupt("posting count overflow"))?;
        Ok(())
    }

    fn next_set_bit(&self, from: u16) -> Option<u16> {
        (from..GROUP_BLOCKS as u16)
            .find(|bit| self.page_bitmap[usize::from(bit / 8)] & (1 << (bit % 8)) != 0)
    }

    fn page_reader(&self) -> Result<Reader<'a>> {
        if self.reader.position() >= self.body_end {
            return Err(Error::Corrupt("page beyond group body"));
        }
        Ok(self.reader)
    }

    /// Number of postings on the page at the read position, skipping it.
    fn skip_page(&mut self) -> Result<u32> {
        let mut reader = self.page_reader()?;
        let n = match reader.u8()? {
            TAG_LIST => {
                let n = reader.varint_u32()?;
                if n == 0 || n as usize > LIST_MAX {
                    return Err(Error::Corrupt("offset list length"));
                }
                reader.skip(n as usize * 2)?;
                n
            }
            TAG_BITMAP => {
                let bitmap = reader.take(TUPLE_BITMAP_BYTES)?;
                bitmap.iter().map(|byte| byte.count_ones()).sum()
            }
            _ => return Err(Error::Corrupt("unknown page tag")),
        };
        if reader.position() > self.body_end {
            return Err(Error::Corrupt("page beyond group body"));
        }
        self.reader = reader;
        Ok(n)
    }

    fn decode_offsets(&mut self) -> Result<crate::pages::Offsets> {
        let mut reader = self.page_reader()?;
        let mut offsets = crate::pages::Offsets::default();
        match reader.u8()? {
            TAG_LIST => {
                let n = reader.varint_u32()?;
                if n == 0 || n as usize > LIST_MAX {
                    return Err(Error::Corrupt("offset list length"));
                }
                let mut previous = 0;
                for _ in 0..n {
                    let offset = reader.u16_le()?;
                    if offset == 0 || offset > MAX_OFFSET || offset <= previous {
                        return Err(Error::Corrupt("offset list not increasing"));
                    }
                    offsets.insert(offset);
                    previous = offset;
                }
            }
            TAG_BITMAP => {
                offsets = crate::pages::Offsets::from_bitmap(reader.take(TUPLE_BITMAP_BYTES)?)?;
                if offsets.is_empty() {
                    return Err(Error::Corrupt("empty tuple bitmap"));
                }
            }
            _ => return Err(Error::Corrupt("unknown page tag")),
        }
        if reader.position() > self.body_end {
            return Err(Error::Corrupt("page beyond group body"));
        }
        self.reader = reader;
        Ok(offsets)
    }

    fn decode_page(&mut self) -> Result<()> {
        let offsets = self.decode_offsets()?;
        self.offsets.clear();
        self.offsets.extend(offsets.iter());
        Ok(())
    }

    /// Positions on the first posting of the first present page whose bit is at
    /// least `bit` in the current group, moving into later groups as needed.
    fn open_page_at_or_after(&mut self, mut bit: u16) -> Result<()> {
        loop {
            // `previous_gid` is set once any group has been entered.
            if self.previous_gid.is_some() {
                if let Some(found) = self.next_set_bit(bit) {
                    self.decode_page()?;
                    self.block = self.gid * GROUP_BLOCKS + u32::from(found);
                    self.index = 0;
                    self.next_bit = found + 1;
                    self.current = Some(Tid {
                        block: self.block,
                        offset: self.offsets[0],
                    });
                    return Ok(());
                }
                self.finish_group()?;
            }
            if !self.enter_group()? {
                if self.seen != self.total {
                    return Err(Error::Corrupt("posting count mismatch"));
                }
                self.current = None;
                return Ok(());
            }
            bit = 0;
        }
    }

    /// Skips pages whose bit is below `bit`, accounting for their postings.
    fn skip_pages_before(&mut self, bit: u16) -> Result<()> {
        while let Some(found) = self.next_set_bit(self.next_bit) {
            if found >= bit {
                return Ok(());
            }
            let n = self.skip_page()?;
            self.group_consumed += n;
            self.ordinal += n;
            self.next_bit = found + 1;
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<()> {
        if self.current.is_none() {
            return Ok(());
        }
        self.index += 1;
        self.ordinal += 1;
        if self.index < self.offsets.len() {
            self.current = Some(Tid {
                block: self.block,
                offset: self.offsets[self.index],
            });
            return Ok(());
        }
        self.group_consumed += self.offsets.len() as u32;
        self.open_page_at_or_after(self.next_bit)
    }

    fn seek(&mut self, target: Tid) -> Result<()> {
        let Some(current) = self.current else {
            return Ok(());
        };
        if current >= target {
            return Ok(());
        }
        if target.block == self.block {
            let skipped =
                self.offsets[self.index..].partition_point(|offset| *offset < target.offset);
            self.index += skipped;
            self.ordinal += skipped as u32;
            if self.index < self.offsets.len() {
                self.current = Some(Tid {
                    block: self.block,
                    offset: self.offsets[self.index],
                });
                return Ok(());
            }
            self.group_consumed += self.offsets.len() as u32;
            return self.open_page_at_or_after(self.next_bit);
        }
        // Leave the current page entirely.
        let rest = (self.offsets.len() - self.index) as u32;
        self.ordinal += rest;
        self.group_consumed += self.offsets.len() as u32;
        if target.group() > self.gid {
            // Skip the remainder of this group and any whole groups before the target's.
            self.ordinal += self
                .group_count
                .checked_sub(self.group_consumed)
                .ok_or(Error::Corrupt("group count mismatch"))?;
            self.group_consumed = self.group_count;
            self.reader.seek(self.body_end)?;
            loop {
                self.finish_group()?;
                if !self.enter_group()? {
                    if self.seen != self.total {
                        return Err(Error::Corrupt("posting count mismatch"));
                    }
                    self.current = None;
                    return Ok(());
                }
                if self.gid >= target.group() {
                    break;
                }
                self.ordinal += self.group_count;
                self.group_consumed = self.group_count;
                self.reader.seek(self.body_end)?;
            }
            if self.gid > target.group() {
                return self.open_page_at_or_after(0);
            }
        }
        // Same group as the target: skip pages before its block.
        self.skip_pages_before(target.page_bit())?;
        self.open_page_at_or_after(target.page_bit())?;
        if self
            .current
            .is_some_and(|current| current.block == target.block)
        {
            let skipped = self
                .offsets
                .partition_point(|offset| *offset < target.offset);
            self.index = skipped;
            self.ordinal += skipped as u32;
            if self.index < self.offsets.len() {
                self.current = Some(Tid {
                    block: self.block,
                    offset: self.offsets[self.index],
                });
            } else {
                self.group_consumed += self.offsets.len() as u32;
                self.open_page_at_or_after(self.next_bit)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(block: u32, offset: u16) -> Tid {
        Tid::new(block, offset).unwrap()
    }

    fn build(tids: &[Tid]) -> Vec<u8> {
        let mut builder = PostingsBuilder::default();
        for tid in tids {
            builder.push(*tid).unwrap();
        }
        builder.finish()
    }

    /// Bucket and length derived from the location, so tests can predict them.
    fn score_of(tid: Tid) -> (u8, u32) {
        (
            (tid.block % 16) as u8,
            10 + (tid.block * 7 + u32::from(tid.offset)) % 50,
        )
    }

    fn build_scored(tids: &[Tid]) -> Vec<u8> {
        let mut builder = PostingsBuilder::default();
        for tid in tids {
            let (bucket, len) = score_of(*tid);
            builder.push_scored(*tid, bucket, len).unwrap();
        }
        builder.finish()
    }

    fn expected_bounds(tids: &[Tid]) -> Vec<BlockBound> {
        tids.chunks(BLOCK_POSTINGS as usize)
            .map(|block| {
                let scores: Vec<(u8, u32)> = block.iter().map(|t| score_of(*t)).collect();
                BlockBound::over(&scores, block[block.len() - 1])
            })
            .collect()
    }

    #[test]
    fn block_bound_reports_per_bucket_minima() {
        let bound = BlockBound::over(&[(3, 50), (0, 7), (3, 20), (15, 9)], tid(9, 9));
        assert_eq!(
            bound.buckets().collect::<Vec<_>>(),
            [(0, 7), (3, 20), (15, 9)]
        );
        assert_eq!(bound.max_tf_bucket(), 15);
        assert_eq!(bound.shortest(), 7);
        let other = BlockBound::over(&[(3, 10), (1, 3)], tid(4, 1));
        let merged = bound.merge(&other);
        assert_eq!(
            merged.buckets().collect::<Vec<_>>(),
            [(0, 7), (1, 3), (3, 10), (15, 9)]
        );
        assert_eq!(merged.last, tid(9, 9));
    }

    #[test]
    fn empty_list_round_trips() {
        let bytes = build(&[]);
        let postings = Postings::parse(&bytes).unwrap();
        assert_eq!(postings.count(), 0);
        assert!(!postings.has_bounds());
        assert_eq!(postings.to_vec().unwrap(), Vec::<Tid>::new());
        let mut cursor = postings.cursor().unwrap();
        assert_eq!(cursor.current(), None);
        cursor.seek(tid(5, 5)).unwrap();
        assert_eq!(cursor.current(), None);
        assert!(!cursor.has_bounds());
        assert_eq!(cursor.bound_at(tid(1, 1)).unwrap(), None);
        assert!(cursor.block_bounds().unwrap().is_empty());
    }

    #[test]
    fn builder_rejects_out_of_order_and_invalid_tids() {
        let mut builder = PostingsBuilder::default();
        builder.push(tid(3, 3)).unwrap();
        assert_eq!(builder.push(tid(3, 3)), Err(Error::Unordered));
        assert_eq!(builder.push(tid(2, 9)), Err(Error::Unordered));
        assert_eq!(
            builder.push(Tid {
                block: 4,
                offset: 0
            }),
            Err(Error::InvalidTid)
        );
        assert_eq!(
            builder.push_scored(tid(5, 1), 16, 1),
            Err(Error::InvalidTfBucket)
        );
    }

    #[test]
    fn dense_lists_choose_grouped_form_and_sparse_lists_do_not() {
        let rare: Vec<Tid> = (0..5).map(|i| tid(i * 100_000, 1)).collect();
        let dense: Vec<Tid> = (0..2000)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        assert!(!Postings::parse(&build(&rare)).unwrap().is_grouped());
        assert!(Postings::parse(&build(&dense)).unwrap().is_grouped());
        assert_eq!(
            Postings::parse(&build(&rare)).unwrap().to_vec().unwrap(),
            rare
        );
        assert_eq!(
            Postings::parse(&build(&dense)).unwrap().to_vec().unwrap(),
            dense
        );
        // Bounds do not change the choice of layout or the decoded postings.
        assert!(!Postings::parse(&build_scored(&rare)).unwrap().is_grouped());
        assert!(Postings::parse(&build_scored(&dense)).unwrap().is_grouped());
        assert_eq!(
            Postings::parse(&build_scored(&dense))
                .unwrap()
                .to_vec()
                .unwrap(),
            dense
        );
    }

    #[test]
    fn one_block_streams_carry_a_term_bound_in_both_layouts() {
        // Dense enough to be grouped, and a thinned copy that is sparse.
        let dense: Vec<Tid> = (0..BLOCK_POSTINGS)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        let sparse: Vec<Tid> = dense
            .iter()
            .step_by(3)
            .map(|t| tid(t.block * 500, t.offset))
            .collect();
        for (tids, grouped) in [(dense, true), (sparse, false)] {
            let bytes = build_scored(&tids);
            let postings = Postings::parse(&bytes).unwrap();
            assert_eq!(postings.is_grouped(), grouped);
            assert!(postings.has_bounds());
            assert!(postings.has_term_bound());
            assert_eq!(postings.to_vec().unwrap(), tids);
            // The bound is the block bound, with the stream's last posting.
            let expected = expected_bounds(&tids);
            assert_eq!(expected.len(), 1);
            let mut cursor = postings.cursor().unwrap();
            assert_eq!(cursor.block_bounds().unwrap(), expected);
            let mut cursor = postings.cursor().unwrap();
            for target in [tids[0], tids[tids.len() / 2], tids[tids.len() - 1]] {
                assert_eq!(cursor.bound_at(target).unwrap(), Some(expected[0]));
            }
            let last = tids[tids.len() - 1];
            let past = Tid {
                block: last.block,
                offset: last.offset + 1,
            };
            assert_eq!(cursor.bound_at(past).unwrap(), None);
            // A cursor moved past the middle still reports the one block,
            // and one that was exhausted reports nothing.
            cursor.seek(tids[tids.len() / 2]).unwrap();
            assert_eq!(cursor.bound_at(tids[0]).unwrap(), Some(expected[0]));
            assert_eq!(cursor.ordinal() as usize, tids.len() / 2);
            cursor.seek(past).unwrap();
            assert_eq!(cursor.current(), None);
            assert_eq!(cursor.bound_at(tids[0]).unwrap(), None);
            // The term bound is smaller than the table it replaces.
            let with_table = {
                let mut builder = PostingsBuilder::default();
                for tid in &tids {
                    let (bucket, len) = score_of(*tid);
                    builder.push_scored(*tid, bucket, len).unwrap();
                }
                builder.finish_as(Format::Lsg2)
            };
            assert!(bytes.len() < with_table.len());
            let old = Postings::parse(&with_table).unwrap();
            assert!(!old.has_term_bound() && old.has_bounds());
            assert_eq!(old.is_grouped(), grouped);
            assert_eq!(old.cursor().unwrap().block_bounds().unwrap(), expected);
        }
        // One posting more than a block gets the table.
        let spill: Vec<Tid> = (0..=BLOCK_POSTINGS).map(|i| tid(i * 7, 1)).collect();
        let bytes = build_scored(&spill);
        let postings = Postings::parse(&bytes).unwrap();
        assert!(postings.has_bounds() && !postings.has_term_bound());
        assert_eq!(postings.cursor().unwrap().block_bounds().unwrap().len(), 2);
    }

    #[test]
    fn bounds_describe_each_block_in_both_layouts() {
        let sparse: Vec<Tid> = (0..1000).map(|i| tid(i * 37, (i % 3 + 1) as u16)).collect();
        let dense: Vec<Tid> = (0..3000)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        for tids in [sparse, dense] {
            let bytes = build_scored(&tids);
            let postings = Postings::parse(&bytes).unwrap();
            assert!(postings.has_bounds());
            assert_eq!(postings.to_vec().unwrap(), tids);
            let mut cursor = postings.cursor().unwrap();
            assert!(cursor.has_bounds());
            let expected = expected_bounds(&tids);
            assert_eq!(cursor.block_bounds().unwrap(), expected);
            // A fresh cursor decodes entries on demand through `bound_at`.
            let mut cursor = postings.cursor().unwrap();
            for (i, block) in tids.chunks(BLOCK_POSTINGS as usize).enumerate() {
                for target in [block[0], block[block.len() / 2], block[block.len() - 1]] {
                    assert_eq!(cursor.bound_at(target).unwrap(), Some(expected[i]));
                }
                // A location just past a block's last posting resolves to the next block.
                let past = Tid {
                    block: block[block.len() - 1].block,
                    offset: block[block.len() - 1].offset + 1,
                };
                assert_eq!(cursor.bound_at(past).unwrap(), expected.get(i + 1).copied());
            }
            // Targets behind the cursor report the current block.
            cursor.seek(tids[200]).unwrap();
            assert_eq!(cursor.bound_at(tids[0]).unwrap(), Some(expected[1]));
            cursor.seek(tid(u32::MAX - 1, 1)).unwrap();
            assert_eq!(cursor.current(), None);
            assert_eq!(cursor.bound_at(tids[0]).unwrap(), None);
        }
    }

    #[test]
    fn sparse_seek_jumps_blocks_with_correct_ordinals() {
        let tids: Vec<Tid> = (0..1000).map(|i| tid(i * 37, (i % 3 + 1) as u16)).collect();
        let bytes = build_scored(&tids);
        let postings = Postings::parse(&bytes).unwrap();
        assert!(!postings.is_grouped());
        for (expected_ordinal, target) in tids.iter().enumerate().step_by(7) {
            let mut cursor = postings.cursor().unwrap();
            cursor.seek(*target).unwrap();
            assert_eq!(cursor.current(), Some(*target));
            assert_eq!(cursor.ordinal() as usize, expected_ordinal);
            let bumped = Tid {
                block: target.block,
                offset: target.offset + 1,
            };
            cursor.seek(bumped).unwrap();
            assert_eq!(cursor.current(), tids.get(expected_ordinal + 1).copied());
            // Walking on from a jump decodes the rest of the stream intact.
            let mut rest = Vec::new();
            while let Some(current) = cursor.current() {
                rest.push(current);
                cursor.advance().unwrap();
            }
            assert_eq!(rest, tids[expected_ordinal + 1..]);
        }
        let mut cursor = postings.cursor().unwrap();
        cursor.seek(tid(37 * 999 + 1, 1)).unwrap();
        assert_eq!(cursor.current(), None);
        assert_eq!(cursor.ordinal(), 1000);
    }

    #[test]
    fn grouped_seek_skips_groups_and_pages_with_correct_ordinals() {
        // Three groups: 0, 3 and 4; a bitmap page and list pages in each.
        let mut tids = Vec::new();
        for block in [0u32, 7, 255, 768, 770, 1024, 1279] {
            let per_page = if block % 2 == 0 { 40 } else { 3 };
            for offset in 1..=per_page {
                tids.push(tid(block, offset));
            }
        }
        for bytes in [build(&tids), build_scored(&tids)] {
            let postings = Postings::parse(&bytes).unwrap();
            assert!(postings.is_grouped());
            for (expected_ordinal, target) in tids.iter().enumerate() {
                let mut cursor = postings.cursor().unwrap();
                cursor.seek(*target).unwrap();
                assert_eq!(cursor.current(), Some(*target));
                assert_eq!(cursor.ordinal() as usize, expected_ordinal, "{target:?}");
                // Seeking to a location just past the target lands on the successor.
                let mut cursor = postings.cursor().unwrap();
                let bumped = Tid {
                    block: target.block,
                    offset: target.offset + 1,
                };
                cursor.seek(bumped).unwrap();
                let successor = tids.iter().find(|t| **t >= bumped).copied();
                assert_eq!(cursor.current(), successor, "successor of {target:?}");
                if successor.is_some() {
                    assert_eq!(cursor.ordinal() as usize, expected_ordinal + 1);
                }
            }
            // Seeking into an absent group between present ones.
            let mut cursor = postings.cursor().unwrap();
            cursor.seek(tid(300, 1)).unwrap();
            assert_eq!(cursor.current(), Some(tid(768, 1)));
            assert_eq!(cursor.rank(tid(768, 1)).unwrap(), Some(46));
            assert_eq!(cursor.rank(tid(769, 1)).unwrap(), None);
            assert_eq!(cursor.current(), Some(tid(770, 1)));
            cursor.seek(tid(9_999, 1)).unwrap();
            assert_eq!(cursor.current(), None);
        }
    }

    #[test]
    fn corrupt_streams_are_reported_not_trusted() {
        let dense: Vec<Tid> = (0..2000)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        let mut bytes = build(&dense);
        assert!(Postings::parse(&[]).is_err());
        assert!(Postings::parse(&[9]).is_err());
        // Truncation anywhere in the body surfaces as an error, never a short list.
        for cut in [bytes.len() - 1, bytes.len() / 2, 3] {
            let result = Postings::parse(&bytes[..cut]).and_then(|p| p.to_vec());
            assert!(result.is_err(), "cut at {cut}");
        }
        // A tampered page tag: the first page of the first group.
        let mut header = Reader::new(&bytes);
        header.u8().unwrap();
        for _ in 0..4 {
            header.varint().unwrap();
        }
        header.skip(PAGE_BITMAP_BYTES).unwrap();
        header.varint().unwrap();
        let tag_at = header.position();
        assert_eq!(bytes[tag_at], TAG_BITMAP);
        bytes[tag_at] = 7;
        assert!(Postings::parse(&bytes).unwrap().to_vec().is_err());
    }

    #[test]
    fn bounds_scratch_is_replaced_after_scans_and_errors() {
        let mut scratch = Vec::new();
        for count in [500, 1, 50] {
            let tids: Vec<_> = (0..count).map(|i| tid(i * 37, 1)).collect();
            let bytes = build_scored(&tids);
            let mut cursor = Postings::parse(&bytes).unwrap().cursor().unwrap();
            while cursor.current().is_some() {
                cursor.advance().unwrap();
            }
            cursor.block_bounds_into(&mut scratch).unwrap();
            assert_eq!(scratch, expected_bounds(&tids));
        }
        let tids: Vec<_> = (0..500).map(|i| tid(i * 37, 1)).collect();
        let mut corrupted = build_scored(&tids);
        let at = Postings::parse(&corrupted).unwrap().bounds.unwrap().0;
        corrupted[at] = 0;
        let mut cursor = Postings::parse(&corrupted).unwrap().cursor().unwrap();
        assert!(cursor.block_bounds_into(&mut scratch).is_err());
        assert!(scratch.is_empty());
        scratch = expected_bounds(&tids);
        let unbounded = build(&tids);
        Postings::parse(&unbounded)
            .unwrap()
            .cursor()
            .unwrap()
            .block_bounds_into(&mut scratch)
            .unwrap();
        assert!(scratch.is_empty());
    }

    #[test]
    fn corrupt_term_bounds_are_reported_not_trusted() {
        let tids: Vec<Tid> = (0..50).map(|i| tid(i * 37, 1)).collect();
        let bytes = build_scored(&tids);
        let postings = Postings::parse(&bytes).unwrap();
        let (at, len) = postings.bounds.unwrap();
        // An empty bucket set is impossible for a non-empty stream.
        let mut tampered = bytes.clone();
        tampered[at] = 0;
        assert!(Postings::parse(&tampered).is_err());
        // A term bound on a stream of several blocks is not a layout.
        let mut tampered = bytes.clone();
        tampered[1] = 200;
        assert!(Postings::parse(&tampered).is_err());
        // Both bound bits at once are not a layout either.
        let mut tampered = bytes.clone();
        tampered[0] |= FORM_BOUNDED;
        assert!(Postings::parse(&tampered).is_err());
        // A truncated bound fails to parse.
        assert!(Postings::parse(&bytes[..at + len - 1]).is_err());
        // A body that ends early is caught when the last posting is sought.
        let truncated = &bytes[..bytes.len() - 1];
        assert!(
            Postings::parse(truncated)
                .and_then(|p| p.cursor()?.block_bounds())
                .is_err()
        );
    }

    #[test]
    fn corrupt_bounds_are_reported_not_trusted() {
        let tids: Vec<Tid> = (0..500).map(|i| tid(i * 37, 1)).collect();
        let bytes = build_scored(&tids);
        let postings = Postings::parse(&bytes).unwrap();
        let (bounds_at, bounds_len) = postings.bounds.unwrap();
        // An empty bucket set is impossible for a non-empty block.
        let mut tampered = bytes.clone();
        tampered[bounds_at] = 0;
        let parsed = Postings::parse(&tampered).unwrap();
        assert!(parsed.cursor().unwrap().block_bounds().is_err());
        // Bounds that are not increasing: copy the first entry over the second.
        let mut cursor = postings.cursor().unwrap();
        let first = cursor.bound_at(tids[0]).unwrap().unwrap();
        assert_eq!(first.last, tids[127]);
        let mut tampered = bytes.clone();
        let entry_len = bounds_len / 4;
        tampered.copy_within(bounds_at..bounds_at + entry_len, bounds_at + entry_len);
        let parsed = Postings::parse(&tampered).unwrap();
        let mut cursor = parsed.cursor().unwrap();
        assert!(cursor.bound_at(tids[200]).is_err() || cursor.seek(tids[300]).is_err());
        // A truncated table fails to parse or to decode, never yields bounds.
        let truncated = &bytes[..bounds_at + bounds_len - 1];
        assert!(
            Postings::parse(truncated)
                .and_then(|p| p.cursor()?.block_bounds())
                .is_err()
        );
    }

    /// Field-scored inputs per posting: `(field, bucket)` pairs and the full
    /// per-field length row, mirroring what `finish_fields` pushes.
    fn field_score_of(i: usize) -> (Vec<(u8, u8)>, Vec<u32>) {
        let fields = match i % 3 {
            0 => vec![(0u8, (i % 16) as u8)],
            1 => vec![(0, 2), (1, (i % 5) as u8)],
            _ => vec![(0, (i % 7) as u8), (1, 3), (2, 1)],
        };
        let lens = vec![
            10 + (i % 13) as u32,
            20 + (i % 29) as u32,
            30 + (i % 31) as u32,
        ];
        (fields, lens)
    }

    fn build_scored_fields(tids: &[Tid]) -> Vec<u8> {
        let mut builder = PostingsBuilder::default();
        for tid in tids {
            let (fields, lens) = field_score_of(usize::from(tid.offset) + tid.block as usize);
            builder.push_scored_fields(*tid, &fields, &lens).unwrap();
        }
        builder.finish_as(Format::Lsg4)
    }

    fn expected_field_bounds(tids: &[Tid]) -> Vec<FieldBlockBound> {
        tids.chunks(BLOCK_POSTINGS as usize)
            .map(|block| {
                let scores: Vec<(u8, u8, u32)> = block
                    .iter()
                    .flat_map(|tid| {
                        let (fields, lens) =
                            field_score_of(usize::from(tid.offset) + tid.block as usize);
                        fields
                            .iter()
                            .map(|&(field, bucket)| (field, bucket, lens[usize::from(field)]))
                            .collect::<Vec<_>>()
                    })
                    .collect();
                FieldBlockBound::over(&scores, block[block.len() - 1], 3)
            })
            .collect()
    }

    #[test]
    fn field_block_bound_preserves_per_field_per_bucket_minima() {
        // The regression shape the RFC's type sketch hides: a field whose
        // shortest length at bucket 3 differs from its shortest at bucket 0.
        // A per-field flattening would keep only one of the two.
        let bound = FieldBlockBound::over(&[(0, 3, 50), (0, 0, 7), (1, 3, 40)], tid(9, 9), 2);
        assert_eq!(bound.field_count, 2);
        assert_eq!(bound.present_fields, 0b11);
        assert_eq!(
            bound.max_tf_bucket,
            [3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            bound.pairs().collect::<Vec<_>>(),
            [(0, 0, 7), (0, 3, 50), (1, 3, 40)]
        );
        // The frozen formula's per-field aggregate is the min over set buckets.
        assert_eq!(bound.field_min_doc_length(0), 7);
        assert_eq!(bound.field_min_doc_length(1), 40);
        assert_eq!(bound.field_min_doc_length(2), u32::MAX);
        // Merging keeps every per-bucket minimum distinct.
        let other = FieldBlockBound::over(&[(0, 3, 10)], tid(4, 1), 2);
        let merged = bound.merge(&other);
        assert_eq!(
            merged.pairs().collect::<Vec<_>>(),
            [(0, 0, 7), (0, 3, 10), (1, 3, 40)]
        );
        assert_eq!(merged.last, tid(9, 9));
        assert_eq!(merged.field_min_doc_length(0), 7);
        // A length of u32::MAX is clamped one shorter so it cannot pose as
        // the absent marker.
        let clamped = FieldBlockBound::over(&[(0, 0, u32::MAX)], tid(0, 1), 1);
        assert_eq!(clamped.pairs().collect::<Vec<_>>(), [(0, 0, u32::MAX - 1)]);
    }

    #[test]
    fn one_block_field_streams_carry_a_field_term_bound_in_both_layouts() {
        let dense: Vec<Tid> = (0..BLOCK_POSTINGS)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        let sparse: Vec<Tid> = dense
            .iter()
            .step_by(3)
            .map(|t| tid(t.block * 500, t.offset))
            .collect();
        for (tids, grouped) in [(dense, true), (sparse, false)] {
            let bytes = build_scored_fields(&tids);
            let postings = Postings::parse_fields(&bytes, 3).unwrap();
            assert_eq!(postings.is_grouped(), grouped);
            assert!(postings.has_bounds());
            assert!(postings.has_term_bound());
            assert_eq!(postings.to_vec().unwrap(), tids);
            let expected = expected_field_bounds(&tids);
            assert_eq!(expected.len(), 1);
            let mut cursor = postings.cursor().unwrap();
            assert_eq!(cursor.field_block_bounds().unwrap(), expected);
            let mut cursor = postings.cursor().unwrap();
            for target in [tids[0], tids[tids.len() / 2], tids[tids.len() - 1]] {
                assert_eq!(
                    cursor.field_bound_at(target).unwrap(),
                    Some(expected[0].clone())
                );
            }
            let last = tids[tids.len() - 1];
            let past = Tid {
                block: last.block,
                offset: last.offset + 1,
            };
            assert_eq!(cursor.field_bound_at(past).unwrap(), None);
            // Legacy bound access fails closed on a field stream.
            assert_eq!(
                cursor.block_bounds(),
                Err(Error::Corrupt("postings bound form"))
            );
            // The field-aware constructor is the only parse path; the
            // format-taking one refuses LSG4.
            assert!(Postings::parse_format(&bytes, Format::Lsg4).is_err());
        }
        // One posting more than a block gets the table.
        let spill: Vec<Tid> = (0..=BLOCK_POSTINGS).map(|i| tid(i * 7, 1)).collect();
        let spill_bytes = build_scored_fields(&spill);
        let postings = Postings::parse_fields(&spill_bytes, 3).unwrap();
        assert!(postings.has_bounds() && !postings.has_term_bound());
        let bounds = postings.cursor().unwrap().field_block_bounds().unwrap();
        assert_eq!(bounds.len(), 2);
        assert_eq!(bounds, expected_field_bounds(&spill));
    }

    #[test]
    fn field_bounds_describe_each_block_and_seeks_agree() {
        let sparse: Vec<Tid> = (0..1000).map(|i| tid(i * 37, (i % 3 + 1) as u16)).collect();
        let dense: Vec<Tid> = (0..3000)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        for tids in [sparse, dense] {
            let bytes = build_scored_fields(&tids);
            let postings = Postings::parse_fields(&bytes, 3).unwrap();
            assert!(postings.has_bounds());
            assert_eq!(postings.to_vec().unwrap(), tids);
            let expected = expected_field_bounds(&tids);
            let mut cursor = postings.cursor().unwrap();
            assert_eq!(cursor.field_block_bounds().unwrap(), expected);
            // On-demand decode through `field_bound_at`, plus seeks that jump
            // blocks through the table like the legacy cursor does.
            let mut cursor = postings.cursor().unwrap();
            for (i, block) in tids.chunks(BLOCK_POSTINGS as usize).enumerate() {
                for target in [block[0], block[block.len() / 2], block[block.len() - 1]] {
                    assert_eq!(
                        cursor.field_bound_at(target).unwrap(),
                        Some(expected[i].clone())
                    );
                }
                let past = Tid {
                    block: block[block.len() - 1].block,
                    offset: block[block.len() - 1].offset + 1,
                };
                assert_eq!(
                    cursor.field_bound_at(past).unwrap(),
                    expected.get(i + 1).cloned()
                );
            }
            let mut cursor = postings.cursor().unwrap();
            cursor.seek(tids[700]).unwrap();
            assert_eq!(cursor.current(), Some(tids[700]));
            assert_eq!(cursor.ordinal() as usize, 700);
            assert_eq!(
                cursor.field_bound_at(tids[700]).unwrap(),
                Some(expected[700 / BLOCK_POSTINGS as usize].clone())
            );
            cursor.seek(tid(u32::MAX - 1, 1)).unwrap();
            assert_eq!(cursor.current(), None);
            assert_eq!(cursor.field_bound_at(tids[0]).unwrap(), None);
        }
    }

    #[test]
    fn corrupt_field_bounds_are_reported_not_trusted() {
        // Term-bound rules fail at parse (the bound is decoded eagerly);
        // table rules fail when the table is decoded.
        let one_block: Vec<Tid> = (0..BLOCK_POSTINGS)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        let compact = build_scored_fields(&one_block);
        let (at, len) = Postings::parse_fields(&compact, 3).unwrap().bounds.unwrap();
        // Rule 1: an empty field mask.
        let mut tampered = compact.clone();
        tampered[at] = 0;
        assert_eq!(
            Postings::parse_fields(&tampered, 3).unwrap_err(),
            Error::Corrupt("field bound mask")
        );
        // Rule 2: a field bit at or beyond field_count (3).
        let mut tampered = compact.clone();
        tampered[at] = 1 << 3;
        assert_eq!(
            Postings::parse_fields(&tampered, 3).unwrap_err(),
            Error::Corrupt("field bound mask")
        );
        // Rule 2 again through the header's count: the same bytes with a
        // smaller declared field count must not decode either.
        assert!(Postings::parse_fields(&compact, 2).is_err());
        // Rule 6: a term bound on a stream of several blocks.
        let multi: Vec<Tid> = (0..=BLOCK_POSTINGS).map(|i| tid(i * 7, 1)).collect();
        let table = build_scored_fields(&multi);
        let mut tampered = table.clone();
        tampered[0] = FORM_TERM_BOUND;
        assert!(Postings::parse_fields(&tampered, 3).is_err());
        // A truncated term bound fails to parse.
        assert!(Postings::parse_fields(&compact[..at + len - 1], 3).is_err());

        let postings = Postings::parse_fields(&table, 3).unwrap();
        let (bounds_at, bounds_len) = postings.bounds.unwrap();
        let decode = |bytes: &[u8]| {
            Postings::parse_fields(bytes, 3).and_then(|p| p.cursor()?.field_block_bounds())
        };
        // Rule 3a: an empty bucket mask for a present field. The field mask
        // is one byte (0b111), so the first bucket mask follows directly.
        let mut tampered = table.clone();
        tampered[bounds_at + 1] = 0;
        assert!(decode(&tampered).is_err());
        // Rule 3b: a bucket bit beyond BUCKET_COUNT.
        let mut tampered = table.clone();
        put_varint_over(&mut tampered, bounds_at + 1, 1 << BUCKET_COUNT);
        assert!(decode(&tampered).is_err());
        // Rule 4: an absent-marker min_len on the first set bucket.
        let mut probe = Reader::new(&table[bounds_at..]);
        probe.varint().unwrap();
        probe.varint().unwrap();
        let min_len_at = bounds_at + probe.position();
        let mut tampered = table.clone();
        put_varint_over(&mut tampered, min_len_at, u64::from(u32::MAX));
        assert_eq!(
            decode(&tampered).unwrap_err(),
            Error::Corrupt("field bound length")
        );
        // Rule 5: a non-increasing table endpoint (copy entry 0 over entry 1).
        let mut tampered = table.clone();
        let entry_len = bounds_len / 2;
        tampered.copy_within(bounds_at..bounds_at + entry_len, bounds_at + entry_len);
        let parsed = Postings::parse_fields(&tampered, 3).unwrap();
        let mut cursor = parsed.cursor().unwrap();
        assert!(cursor.field_bound_at(multi[100]).is_err() || cursor.seek(multi[128]).is_err());
        // A truncated table fails to decode, never yields bounds.
        assert!(
            Postings::parse_fields(&table[..bounds_at + bounds_len - 1], 3)
                .and_then(|p| p.cursor()?.field_block_bounds())
                .is_err()
        );
    }

    /// Overwrites `at` onward with `value`'s varint bytes without shifting
    /// the tail; corruption tests only need the decoder to trip on the new
    /// bytes, and the rule fires before anything past them is read.
    fn put_varint_over(bytes: &mut [u8], at: usize, value: u64) {
        let mut encoded = Vec::new();
        varint::put(&mut encoded, value);
        bytes[at..at + encoded.len()].copy_from_slice(&encoded);
    }

    #[test]
    fn push_scored_fields_validates_its_inputs() {
        let mut builder = PostingsBuilder::default();
        let lens = [10u32, 20, 30];
        builder
            .push_scored_fields(tid(0, 1), &[(0, 0), (2, 1)], &lens)
            .unwrap();
        // Empty field list, unknown field, descending fields, duplicate field.
        assert!(builder.push_scored_fields(tid(0, 2), &[], &lens).is_err());
        assert!(
            builder
                .push_scored_fields(tid(0, 2), &[(3, 0)], &lens)
                .is_err()
        );
        assert!(
            builder
                .push_scored_fields(tid(0, 2), &[(2, 0), (1, 0)], &lens)
                .is_err()
        );
        assert!(
            builder
                .push_scored_fields(tid(0, 2), &[(1, 0), (1, 1)], &lens)
                .is_err()
        );
        // Bucket out of range, field count disagreement, zero fields.
        assert!(
            builder
                .push_scored_fields(tid(0, 2), &[(0, 16)], &lens)
                .is_err()
        );
        assert!(
            builder
                .push_scored_fields(tid(0, 2), &[(0, 0)], &[1, 2])
                .is_err()
        );
        assert!(
            builder
                .push_scored_fields(tid(0, 2), &[(0, 0)], &[])
                .is_err()
        );
        assert!(
            builder
                .push_scored_fields(tid(0, 2), &[(0, 0)], &[0; 17])
                .is_err()
        );
        // Out-of-order TIDs still fail through `push`.
        assert_eq!(
            builder.push_scored_fields(tid(0, 1), &[(0, 0)], &lens),
            Err(Error::Unordered)
        );
        let bytes = builder.finish_as(Format::Lsg4);
        assert_eq!(
            Postings::parse_fields(&bytes, 3).unwrap().to_vec().unwrap(),
            [tid(0, 1)]
        );
    }

    #[test]
    fn sixteen_field_bounds_round_trip_in_both_layouts() {
        let lens: Vec<u32> = (1u32..=16).map(|f| f * 3).collect();
        let dense: Vec<Tid> = (0..BLOCK_POSTINGS)
            .map(|i| tid(i / 40, (i % 40 + 1) as u16))
            .collect();
        let sparse: Vec<Tid> = (BLOCK_POSTINGS..BLOCK_POSTINGS * 3)
            .map(|i| tid(i * 91, (i % 7 + 1) as u16))
            .collect();
        for tids in [dense, sparse] {
            let mut builder = PostingsBuilder::default();
            for tid in &tids {
                // Every field occurs, with buckets spanning the range.
                let fields: Vec<(u8, u8)> = (0..16u8)
                    .map(|f| (f, ((usize::from(tid.offset) + usize::from(f)) % 16) as u8))
                    .collect();
                builder.push_scored_fields(*tid, &fields, &lens).unwrap();
            }
            let bytes = builder.finish_as(Format::Lsg4);
            let postings = Postings::parse_fields(&bytes, 16).unwrap();
            let scores: Vec<(u8, u8, u32)> = tids
                .iter()
                .flat_map(|tid| {
                    (0..16u8).map(|f| {
                        (
                            f,
                            ((usize::from(tid.offset) + usize::from(f)) % 16) as u8,
                            lens[usize::from(f)],
                        )
                    })
                })
                .collect();
            let mut cursor = postings.cursor().unwrap();
            let expected: Vec<FieldBlockBound> = tids
                .chunks(BLOCK_POSTINGS as usize)
                .zip(scores.chunks(BLOCK_POSTINGS as usize * 16))
                .map(|(block, scores)| FieldBlockBound::over(scores, block[block.len() - 1], 16))
                .collect();
            assert_eq!(cursor.field_block_bounds().unwrap(), expected);
            for bound in &expected {
                assert_eq!(bound.present_fields, u16::MAX);
            }
        }
    }
}
