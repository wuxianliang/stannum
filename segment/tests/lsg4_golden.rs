// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! LSG4 golden vectors (RFC §7): frozen blobs plus JSON manifests, decoded
//! here by a **std-only independent decoder** that shares no code with the
//! segment crate and was written against the RFC's text — so a writer and
//! reader that agree on a wrong layout cannot both pass.
//!
//! `tests/fixtures/lsg4/<case>.segment` is the blob; `<case>.json` is the
//! manifest (`"schema": "stannum.lsg4-golden/1"`). Valid cases must decode to
//! the manifest's expected structure; `invalid` cases must fail with the
//! manifest's error class, which names the specific RFC validation rule.
//!
//! The blobs are generated once by the `#[ignore]`d `generate` test at the
//! bottom — the only code here that links the segment crate — and then frozen;
//! CI never regenerates them. Any byte change is a deliberate RFC amendment.

use std::fmt::Write as _;
use std::path::PathBuf;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lsg4");

/// One fixture document: `(field, term, position)` tokens in TID order.
type FixtureDocuments = Vec<(segment::Tid, Vec<(u8, String, u32)>)>;

// ---------------------------------------------------------------------------
// A minimal JSON reader, enough for the manifests' canonical form.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn str(&self) -> &str {
        match self {
            Json::Str(s) => s,
            _ => panic!("expected a string, got {self:?}"),
        }
    }
    fn num(&self) -> f64 {
        match self {
            Json::Num(n) => *n,
            _ => panic!("expected a number, got {self:?}"),
        }
    }
    fn integer(&self) -> u64 {
        self.num() as u64
    }
    fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(items) => items,
            _ => panic!("expected an array, got {self:?}"),
        }
    }
}

fn parse_json(text: &str) -> Json {
    let bytes = text.as_bytes();
    let mut at = 0usize;
    let value = json_value(bytes, &mut at);
    skip_space(bytes, &mut at);
    assert_eq!(at, bytes.len(), "trailing JSON bytes");
    value
}

fn skip_space(bytes: &[u8], at: &mut usize) {
    while *at < bytes.len() && bytes[*at].is_ascii_whitespace() {
        *at += 1;
    }
}

fn json_value(bytes: &[u8], at: &mut usize) -> Json {
    skip_space(bytes, at);
    match bytes.get(*at) {
        Some(b'{') => {
            *at += 1;
            let mut pairs = Vec::new();
            skip_space(bytes, at);
            if bytes.get(*at) == Some(&b'}') {
                *at += 1;
                return Json::Obj(pairs);
            }
            loop {
                skip_space(bytes, at);
                let key = match json_value(bytes, at) {
                    Json::Str(key) => key,
                    other => panic!("object key is not a string: {other:?}"),
                };
                skip_space(bytes, at);
                assert_eq!(bytes[*at], b':', "expected ':'");
                *at += 1;
                let value = json_value(bytes, at);
                pairs.push((key, value));
                skip_space(bytes, at);
                match bytes.get(*at) {
                    Some(b',') => *at += 1,
                    Some(b'}') => {
                        *at += 1;
                        return Json::Obj(pairs);
                    }
                    _ => panic!("expected ',' or '}}' in object"),
                }
            }
        }
        Some(b'[') => {
            *at += 1;
            let mut items = Vec::new();
            skip_space(bytes, at);
            if bytes.get(*at) == Some(&b']') {
                *at += 1;
                return Json::Arr(items);
            }
            loop {
                items.push(json_value(bytes, at));
                skip_space(bytes, at);
                match bytes.get(*at) {
                    Some(b',') => *at += 1,
                    Some(b']') => {
                        *at += 1;
                        return Json::Arr(items);
                    }
                    _ => panic!("expected ',' or ']' in array"),
                }
            }
        }
        Some(b'"') => {
            *at += 1;
            let mut out = String::new();
            loop {
                match bytes[*at] {
                    b'"' => {
                        *at += 1;
                        return Json::Str(out);
                    }
                    b'\\' => {
                        *at += 1;
                        match bytes[*at] {
                            b'"' => out.push('"'),
                            b'\\' => out.push('\\'),
                            b'/' => out.push('/'),
                            other => panic!("unsupported escape \\{}", other as char),
                        }
                        *at += 1;
                    }
                    byte => {
                        // Raw UTF-8: find the full sequence.
                        let len = utf8_len(byte);
                        out.push_str(std::str::from_utf8(&bytes[*at..*at + len]).expect("UTF-8"));
                        *at += len;
                    }
                }
            }
        }
        Some(b't') => {
            expect(bytes, at, "true");
            Json::Bool(true)
        }
        Some(b'f') => {
            expect(bytes, at, "false");
            Json::Bool(false)
        }
        Some(b'n') => {
            expect(bytes, at, "null");
            Json::Null
        }
        Some(_) => {
            let start = *at;
            while *at < bytes.len()
                && (bytes[*at].is_ascii_digit()
                    || matches!(bytes[*at], b'-' | b'+' | b'.' | b'e' | b'E'))
            {
                *at += 1;
            }
            Json::Num(
                std::str::from_utf8(&bytes[start..*at])
                    .expect("number text")
                    .parse()
                    .expect("number"),
            )
        }
        None => panic!("JSON input ends early"),
    }
}

fn utf8_len(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn expect(bytes: &[u8], at: &mut usize, word: &str) {
    assert_eq!(
        &bytes[*at..*at + word.len()],
        word.as_bytes(),
        "JSON literal"
    );
    *at += word.len();
}

// ---------------------------------------------------------------------------
// The independent decoder. Everything below decodes from the RFC's text:
// §5.1 header, §5.2 lengths, §5.3 payload, §5.4 postings bounds, §5.5
// dictionary. Errors are prefixed with the rule that rejects the input.
// ---------------------------------------------------------------------------

const BLOCK_POSTINGS: usize = 128;
const BLOCK_TERMS: usize = 64;
const SKIP_INTERVAL: usize = 32;
const GROUP_BLOCKS: usize = 256;
const LIST_MAX: usize = 18;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Tid {
    block: u32,
    offset: u16,
}

#[derive(Clone, Debug, PartialEq)]
struct FieldHit {
    field: u8,
    bucket: u8,
    positions: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq)]
struct DecodedTerm {
    term: String,
    df: u32,
    max_tf_bucket: u8,
    postings: Vec<(Tid, Vec<FieldHit>)>,
    /// Per block of `BLOCK_POSTINGS` postings: the decoded field bound.
    bounds: Vec<DecodedBound>,
}

/// A decoded field bound: present fields and the per-(field, bucket) minima,
/// `u32::MAX` where absent, plus the block's last posting.
#[derive(Clone, Debug, PartialEq)]
struct DecodedBound {
    present_fields: u16,
    min_doc_length: [[u32; 16]; 16],
    last: Tid,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Decoded {
    header_len: usize,
    doc_count: u32,
    total_length: u64,
    field_count: u8,
    field_totals: Vec<u64>,
    documents: Vec<(Tid, Vec<u32>)>,
    terms: Vec<DecodedTerm>,
}

struct Bytes<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn u8(&mut self) -> Result<u8, String> {
        let byte = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| "truncated blob".to_owned())?;
        self.at += 1;
        Ok(byte)
    }
    fn u16(&mut self) -> Result<u16, String> {
        let lo = self.u8()? as u16;
        let hi = self.u8()? as u16;
        Ok(lo | (hi << 8))
    }
    fn u32(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for shift in [0, 8, 16, 24] {
            value |= (self.u8()? as u32) << shift;
        }
        Ok(value)
    }
    fn u64(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in [0, 8, 16, 24, 32, 40, 48, 56] {
            value |= (self.u8()? as u64) << shift;
        }
        Ok(value)
    }
    fn varint(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..=63).step_by(7) {
            let byte = self.u8()?;
            if shift == 63 && byte > 1 {
                return Err("varint exceeds 64 bits".into());
            }
            value |= u64::from(byte & 127) << shift;
            if byte & 128 == 0 {
                return Ok(value);
            }
        }
        Err("varint exceeds 64 bits".into())
    }
    /// A `u32`-bounded varint, as every length field in the format is.
    fn varint_u32(&mut self) -> Result<u32, String> {
        u32::try_from(self.varint()?).map_err(|_| "value exceeds 32 bits".to_owned())
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| "truncated blob".to_owned())?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }
    fn skip(&mut self, len: usize) -> Result<(), String> {
        self.take(len).map(|_| ())
    }
}

/// Decodes one encoded position list (first absolute, then delta − 1),
/// validating rule 5 (count) and rule 6 (strictly increasing, no overflow).
fn positions(reader: &mut Bytes<'_>) -> Result<Vec<u32>, String> {
    let count = reader.varint_u32()?;
    if count == 0 {
        return Err("payload rule 5: position count is zero".into());
    }
    let mut out = Vec::with_capacity(count.min(4096) as usize);
    let mut position = reader.varint_u32()?;
    out.push(position);
    for _ in 1..count {
        let delta = reader.varint_u32()?;
        position = position
            .checked_add(delta)
            .and_then(|p| p.checked_add(1))
            .ok_or_else(|| "payload rule 6: position overflow".to_owned())?;
        if position <= out[out.len() - 1] {
            return Err("payload rule 6: positions not strictly increasing".into());
        }
        out.push(position);
    }
    Ok(out)
}

/// One LSG4 payload entry: `field_hit_count` groups of packed byte +
/// positions (RFC §5.3). The full rule set is enforced here, including the
/// bucket-quantization cross-check of rule 8.
fn field_entry(reader: &mut Bytes<'_>, field_count: u8) -> Result<Vec<FieldHit>, String> {
    let hit_count = reader.varint_u32()?;
    if hit_count == 0 {
        return Err("payload rule 1: field hit count is zero".into());
    }
    if hit_count > u32::from(field_count) {
        return Err("payload rule 1: field hit count exceeds field count".into());
    }
    let mut fields = Vec::with_capacity(hit_count as usize);
    for _ in 0..hit_count {
        let packed = reader.u8()?;
        let field = packed >> 4;
        let bucket = packed & 0x0f;
        if field >= field_count {
            return Err("payload rule 3: field id at or beyond field count".into());
        }
        if fields
            .last()
            .is_some_and(|hit: &FieldHit| hit.field >= field)
        {
            return Err("payload rule 2: field ids not strictly increasing".into());
        }
        let hits = positions(reader)?;
        if bucket_of(hits.len()) != bucket {
            return Err("payload rule 8: bucket disagrees with position count".into());
        }
        fields.push(FieldHit {
            field,
            bucket,
            positions: hits,
        });
    }
    Ok(fields)
}

/// The production tf quantization boundaries (tf_bucket.rs), embedded here so
/// the cross-check of payload rule 8 does not depend on the crate.
const REPRESENTATIVE_COUNTS: [u32; 16] = [
    1, 2, 3, 5, 10, 20, 41, 85, 177, 371, 777, 1626, 3405, 7132, 14938, 31288,
];

fn bucket_of(count: usize) -> u8 {
    let positive = (count as u32).max(1);
    (REPRESENTATIVE_COUNTS.partition_point(|&start| start <= positive) as u8) - 1
}

/// One LSG4 term bound (RFC §5.4): field mask, then per set field a bucket
/// mask and one min_len per set bucket. `last` is stamped by the caller.
fn field_bound(reader: &mut Bytes<'_>, field_count: u8) -> Result<DecodedBound, String> {
    let field_mask = reader.varint_u32()?;
    if field_mask == 0 {
        return Err("bound rule 1: field mask is empty".into());
    }
    if field_count == 0 || field_count > 16 || field_mask >> field_count != 0 {
        return Err("bound rule 2: field id at or beyond field count".into());
    }
    let mut bound = DecodedBound {
        present_fields: field_mask as u16,
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
        if bucket_mask == 0 {
            return Err("bound rule 3: bucket mask is empty".into());
        }
        if bucket_mask >> 16 != 0 {
            return Err("bound rule 3: bucket id beyond the bucket range".into());
        }
        for bucket in 0..16usize {
            if bucket_mask & (1 << bucket) == 0 {
                continue;
            }
            let len = reader.varint_u32()?;
            if len == u32::MAX {
                return Err("bound rule 4: min_len is the absent marker".into());
            }
            bound.min_doc_length[field as usize][bucket] = len;
        }
    }
    Ok(bound)
}

/// Decodes the postings stream of one term: the envelope, the bounds in the
/// stream's layout (term bound or table, RFC §5.4 rules 5-6), and the body's
/// locations. Returns the postings, their count and the decoded bounds.
fn postings_stream(bytes: &[u8], field_count: u8) -> Result<(Vec<Tid>, Vec<DecodedBound>), String> {
    let mut reader = Bytes::new(bytes);
    let form = reader.u8()?;
    if form > 7 || form & 6 == 6 {
        return Err("postings: unknown form byte".into());
    }
    let count = reader.varint_u32()?;
    let grouped = form & 1 != 0;
    let compact = form & 4 != 0;
    let mut bounds = Vec::new();
    let body_at;
    if form & 2 != 0 {
        // A bounds table precedes the body; decode every entry with the
        // framing rules (5) and the bound payload rules (1-4).
        let len = reader.varint_u32()? as usize;
        body_at = reader.at + len;
        let mut table = Bytes::new(reader.take(len)?);
        let mut previous_last: Option<Tid> = None;
        let mut previous_start: Option<usize> = None;
        for _ in 0..count.div_ceil(BLOCK_POSTINGS as u32) {
            let mut bound = field_bound(&mut table, field_count)?;
            let block = previous_last
                .map_or(0, |last| last.block)
                .checked_add(table.varint_u32()?)
                .ok_or("bound rule 5: block delta overflows")?;
            let offset = table
                .varint_u32()?
                .try_into()
                .map_err(|_| "bound rule 5: invalid offset")?;
            let last = Tid { block, offset };
            if previous_last.is_some_and(|previous| previous >= last) {
                return Err("bound rule 5: block bounds not increasing".into());
            }
            if !grouped {
                let start = previous_start.unwrap_or(0) + table.varint()? as usize;
                if start >= bytes.len() - body_at
                    || previous_start.is_some_and(|previous| previous >= start)
                {
                    return Err("bound rule 5: block start beyond body".into());
                }
                previous_start = Some(start);
            }
            previous_last = Some(last);
            bound.last = last;
            bounds.push(bound);
        }
        if table.at != table.bytes.len() {
            return Err("bound rule 5: trailing table bytes".into());
        }
    } else if compact {
        if count == 0 || count > BLOCK_POSTINGS as u32 {
            return Err("bound rule 6: term bound on a stream of several blocks".into());
        }
        let mut bound = field_bound(&mut reader, field_count)?;
        body_at = reader.at;
        bound.last = Tid {
            block: 0,
            offset: 0,
        }; // filled from the body below
        bounds.push(bound);
    } else {
        body_at = reader.at;
    }
    // The body: sparse deltas or grouped pages, both to strictly increasing
    // locations totalling `count`.
    let mut reader = Bytes::new(&bytes[body_at..]);
    let tids = if grouped {
        let groups = reader.varint_u32()?;
        let mut tids = Vec::new();
        let mut seen = 0u32;
        let mut previous_gid: Option<u32> = None;
        for _ in 0..groups {
            let raw = reader.varint_u32()?;
            let gid = match previous_gid {
                None => raw,
                Some(previous) => previous + raw + 1,
            };
            previous_gid = Some(gid);
            let group_count = reader.varint_u32()?;
            if group_count == 0 {
                return Err("postings: empty group".into());
            }
            let bitmap = reader.take(32)?;
            let body_len = reader.varint_u32()? as usize;
            let group_end = reader.at + body_len;
            if group_end > bytes.len() - body_at {
                return Err("postings: group body beyond stream".into());
            }
            for bit in 0..GROUP_BLOCKS {
                if bitmap[bit / 8] & (1 << (bit % 8)) == 0 {
                    continue;
                }
                let block = gid * GROUP_BLOCKS as u32 + bit as u32;
                match reader.u8()? {
                    0 => {
                        let n = reader.varint_u32()?;
                        if n == 0 || n as usize > LIST_MAX {
                            return Err("postings: offset list length".into());
                        }
                        let mut previous = 0u16;
                        for _ in 0..n {
                            let offset = reader.u16()?;
                            if offset == 0 || offset <= previous {
                                return Err("postings: offset list not increasing".into());
                            }
                            previous = offset;
                            tids.push(Tid { block, offset });
                            seen += 1;
                        }
                    }
                    1 => {
                        for (byte, bits) in reader.take(37)?.iter().enumerate() {
                            for bit in 0..8 {
                                if bits & (1 << bit) != 0 {
                                    tids.push(Tid {
                                        block,
                                        offset: (byte * 8 + bit + 1) as u16,
                                    });
                                    seen += 1;
                                }
                            }
                        }
                    }
                    _ => return Err("postings: unknown page tag".into()),
                }
            }
            if reader.at != group_end {
                return Err("postings: group body length mismatch".into());
            }
        }
        if seen != count {
            return Err("postings: posting count mismatch".into());
        }
        tids
    } else {
        let mut tids = Vec::new();
        let mut last_block = 0u32;
        for _ in 0..count {
            let block = last_block + reader.varint_u32()?;
            let offset = reader
                .varint_u32()?
                .try_into()
                .map_err(|_| "postings: invalid offset")?;
            let tid = Tid { block, offset };
            if tids.last().is_some_and(|previous| *previous >= tid) {
                return Err("postings: sparse postings not increasing".into());
            }
            last_block = block;
            tids.push(tid);
        }
        tids
    };
    if tids.len() != count as usize {
        return Err("postings: posting count mismatch".into());
    }
    if reader.at != reader.bytes.len() {
        return Err("postings: trailing body bytes".into());
    }
    // A term bound's last location is the stream's last posting.
    if compact {
        bounds[0].last = *tids.last().ok_or("postings: empty but bounded")?;
    }
    Ok((tids, bounds))
}

/// Decodes a complete LSG4 blob per the RFC. Every area is consumed exactly.
fn decode_lsg4(blob: &[u8]) -> Result<Decoded, String> {
    if blob.len() < 4 {
        return Err("header: blob too short for a magic".into());
    }
    if &blob[..4] != b"LSG4" {
        return Err("header: magic is not LSG4".into());
    }
    let mut reader = Bytes::new(blob);
    reader.skip(4)?;
    // §5.1: layout_revision MUST be 1 (fail closed).
    let revision = reader.varint_u32()?;
    if revision != 1 {
        return Err("header: layout_revision is not 1".into());
    }
    let doc_count = reader.varint_u32()?;
    let total_length = reader.varint()?;
    let field_count = reader.varint_u32()? as u8;
    if field_count == 0 || field_count > 16 {
        return Err("header: field_count outside 1..=16".into());
    }
    let mut field_totals = Vec::with_capacity(usize::from(field_count));
    for _ in 0..field_count {
        field_totals.push(reader.u64()?);
    }
    if field_totals
        .iter()
        .try_fold(0u64, |sum, n| sum.checked_add(*n))
        != Some(total_length)
    {
        return Err("header: field totals do not sum to total_length".into());
    }
    let dictionary_len = reader.varint_u32()? as usize;
    let postings_len = reader.varint_u32()? as usize;
    let payload_len = reader.varint_u32()? as usize;
    let docs_len = reader.varint_u32()? as usize;
    let header_len = reader.at;
    let dictionary_at = header_len;
    let postings_at = dictionary_at + dictionary_len;
    let payload_at = postings_at + postings_len;
    let docs_at = payload_at + payload_len;
    let lengths_at = docs_at + docs_len;
    if lengths_at + doc_count as usize * usize::from(field_count) * 4 != blob.len() {
        return Err("header: lengths extent disagrees with the blob length".into());
    }

    // §5.2 + the document table: the docs postings stream and doc-major rows.
    let (documents, _) = postings_stream(&blob[docs_at..docs_at + docs_len], field_count)?;
    let mut rows = Vec::with_capacity(documents.len());
    let mut lengths = Bytes::new(&blob[lengths_at..]);
    for tid in &documents {
        let mut row = Vec::with_capacity(usize::from(field_count));
        for _ in 0..field_count {
            row.push(lengths.u32()?);
        }
        rows.push((*tid, row));
    }
    if documents.len() != doc_count as usize {
        return Err("header: doc_count disagrees with the document table".into());
    }
    let lengths_of = |tid: Tid| -> Option<&[u32]> {
        rows.binary_search_by_key(&tid, |(row_tid, _)| *row_tid)
            .ok()
            .map(|index| rows[index].1.as_slice())
    };

    // §5.5 dictionary (the LSG3 entry layout, verbatim).
    let mut dictionary = Bytes::new(&blob[dictionary_at..dictionary_at + dictionary_len]);
    let term_count = dictionary.varint_u32()?;
    let block_count = dictionary.varint_u32()?;
    if block_count as usize != (term_count as usize).div_ceil(BLOCK_TERMS) {
        return Err("dictionary: block count".into());
    }
    let index_len = dictionary.varint_u32()? as usize;
    let index = dictionary.take(index_len)?;
    let mut index_reader = Bytes::new(index);
    let mut previous_first = Vec::new();
    let mut previous_offset: Option<usize> = None;
    for _ in 0..block_count {
        let offset = index_reader.varint_u32()? as usize;
        let len = index_reader.varint_u32()? as usize;
        let first = index_reader.take(len)?.to_vec();
        if previous_first.as_slice() >= first.as_slice()
            || previous_offset.is_some_and(|previous| offset <= previous)
        {
            return Err("dictionary: index order".into());
        }
        previous_first = first;
        previous_offset = Some(offset);
    }
    if index_reader.at != index.len() {
        return Err("dictionary: index length".into());
    }
    let blocks_len = dictionary_len - index_len - dictionary.at + header_len;
    let _ = blocks_len;

    let mut decoded = Decoded {
        header_len,
        doc_count,
        total_length,
        field_count,
        field_totals: field_totals.clone(),
        documents: rows.clone(),
        terms: Vec::new(),
    };
    let mut postings_end = 0usize;
    let mut payload_end = 0usize;
    let mut previous_term = Vec::new();
    for _ in 0..term_count {
        let shared = dictionary.varint_u32()? as usize;
        let suffix_len = dictionary.varint_u32()? as usize;
        if shared > previous_term.len() {
            return Err("dictionary: shared prefix".into());
        }
        let suffix = dictionary.take(suffix_len)?;
        let mut term = previous_term[..shared].to_vec();
        term.extend_from_slice(suffix);
        if term.is_empty() || previous_term.as_slice() >= term.as_slice() {
            return Err("dictionary: term order".into());
        }
        previous_term = term.clone();
        let packed = dictionary.varint_u32()?;
        let df = packed >> 4;
        let max_tf_bucket = (packed & 15) as u8;
        let postings_gap = zigzag(dictionary.varint()?);
        let postings_offset = usize::try_from(postings_end as i64 + postings_gap)
            .map_err(|_| "dictionary: extent gap".to_string())?;
        let postings_extent_len = dictionary.varint_u32()? as usize;
        let payload_gap = zigzag(dictionary.varint()?);
        let payload_offset = usize::try_from(payload_end as i64 + payload_gap)
            .map_err(|_| "dictionary: extent gap".to_string())?;
        let payload_extent_len = dictionary.varint_u32()? as usize;
        let postings_extent_end = postings_offset + postings_extent_len;
        let payload_extent_end = payload_offset + payload_extent_len;
        if postings_extent_end > postings_len || payload_extent_end > payload_len {
            return Err("dictionary: extent beyond area".into());
        }
        postings_end = postings_extent_end;
        payload_end = payload_extent_end;

        // §5.4: postings with field bounds, re-derived from the decoded
        // payload and length rows.
        let (tids, bounds) = postings_stream(
            &blob[postings_at + postings_offset..postings_at + postings_extent_end],
            field_count,
        )?;
        if tids.len() as u32 != df {
            return Err("dictionary: df disagrees with the postings count".into());
        }
        // §5.3: the payload stream, LSG3 skip framing with LSG4 entries.
        let mut payload =
            Bytes::new(&blob[payload_at + payload_offset..payload_at + payload_extent_end]);
        let entry_count = payload.varint_u32()?;
        if entry_count != df {
            return Err("payload: entry count disagrees with df".into());
        }
        let slots = (entry_count as usize)
            .div_ceil(SKIP_INTERVAL)
            .saturating_sub(1);
        let mut skips = Vec::with_capacity(slots);
        for _ in 0..slots {
            skips.push(payload.u32()? as usize);
        }
        let data_at = payload.at;
        if let Some(&last) = skips.last()
            && last >= payload.bytes.len() - data_at
        {
            return Err("payload: skip offset beyond the data area".into());
        }
        let mut postings = Vec::with_capacity(df as usize);
        for _ in 0..entry_count {
            let hits = field_entry(&mut payload, field_count)?;
            postings.push((
                Tid {
                    block: 0,
                    offset: 0,
                },
                hits,
            ));
        }
        for (index, tid) in tids.iter().enumerate() {
            postings[index].0 = *tid;
        }
        if payload.at != payload.bytes.len() {
            return Err("payload rule 7: data area not consumed exactly".into());
        }
        // The bounds must be what the postings and length rows imply.
        if !bounds.is_empty() {
            let mut expected = [[u32::MAX; 16]; 16];
            let mut present = 0u16;
            for (block, chunk) in tids.chunks(BLOCK_POSTINGS).enumerate() {
                for tid in chunk {
                    let hits = &postings
                        .iter()
                        .find(|(posting_tid, _)| posting_tid == tid)
                        .expect("postings align with tids")
                        .1;
                    let row = lengths_of(*tid).ok_or("payload: posting not a document")?;
                    for hit in hits {
                        present |= 1 << hit.field;
                        let slot = &mut expected[usize::from(hit.field)][usize::from(hit.bucket)];
                        *slot = (*slot).min(row[usize::from(hit.field)]);
                    }
                }
                if bounds[block].present_fields != present
                    || bounds[block].min_doc_length != expected
                    || bounds[block].last != chunk[chunk.len() - 1]
                {
                    return Err("bound: disagrees with postings and lengths".into());
                }
                expected = [[u32::MAX; 16]; 16];
                present = 0;
            }
        }
        let observed_max = postings
            .iter()
            .flat_map(|(_, hits)| hits.iter().map(|hit| hit.bucket))
            .max()
            .unwrap_or(0);
        if observed_max != max_tf_bucket {
            return Err("dictionary: max_tf_bucket disagrees with the payload".into());
        }
        decoded.terms.push(DecodedTerm {
            term: String::from_utf8(term).map_err(|_| "dictionary: term UTF-8".to_string())?,
            df,
            max_tf_bucket,
            postings,
            bounds,
        });
    }
    if dictionary.at != dictionary_len {
        return Err("dictionary: blocks area not consumed exactly".into());
    }
    if postings_end != postings_len || payload_end != payload_len {
        return Err("dictionary: areas not consumed exactly".into());
    }
    Ok(decoded)
}

fn zigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// The LSG3 twin of the decoder, for the bit-equality vector: the shared
/// envelope with the fieldless header, payload entries and term bound.
fn decode_lsg3(blob: &[u8]) -> Result<Decoded, String> {
    if &blob[..4] != b"LSG3" {
        return Err("header: magic is not LSG3".into());
    }
    let mut reader = Bytes::new(blob);
    reader.skip(4)?;
    let doc_count = reader.varint_u32()?;
    let total_length = reader.varint()?;
    let dictionary_len = reader.varint_u32()? as usize;
    let postings_len = reader.varint_u32()? as usize;
    let payload_len = reader.varint_u32()? as usize;
    let docs_len = reader.varint_u32()? as usize;
    let dictionary_at = reader.at;
    let postings_at = dictionary_at + dictionary_len;
    let payload_at = postings_at + postings_len;
    let docs_at = payload_at + payload_len;
    let lengths_at = docs_at + docs_len;
    if lengths_at + doc_count as usize * 4 != blob.len() {
        return Err("header: lengths extent".into());
    }
    let (documents, _) = postings_stream_lsg3(&blob[docs_at..docs_at + docs_len])?;
    let mut rows = Vec::with_capacity(documents.len());
    let mut lengths = Bytes::new(&blob[lengths_at..]);
    for tid in &documents {
        rows.push((*tid, vec![lengths.u32()?]));
    }
    let mut dictionary = Bytes::new(&blob[dictionary_at..dictionary_at + dictionary_len]);
    let term_count = dictionary.varint_u32()?;
    let _block_count = dictionary.varint_u32()?;
    let index_len = dictionary.varint_u32()? as usize;
    dictionary.skip(index_len)?;
    let mut decoded = Decoded {
        header_len: dictionary_at,
        doc_count,
        total_length,
        field_count: 1,
        field_totals: vec![total_length],
        documents: rows.clone(),
        terms: Vec::new(),
    };
    let mut postings_end = 0usize;
    let mut payload_end = 0usize;
    let mut previous_term = Vec::new();
    for _ in 0..term_count {
        let shared = dictionary.varint_u32()? as usize;
        let suffix_len = dictionary.varint_u32()? as usize;
        let suffix = dictionary.take(suffix_len)?;
        let mut term = previous_term[..shared].to_vec();
        term.extend_from_slice(suffix);
        previous_term = term.clone();
        let packed = dictionary.varint_u32()?;
        let df = packed >> 4;
        let max_tf_bucket = (packed & 15) as u8;
        let postings_gap = zigzag(dictionary.varint()?);
        let postings_offset = (postings_end as i64 + postings_gap) as usize;
        let postings_extent_len = dictionary.varint_u32()? as usize;
        let payload_gap = zigzag(dictionary.varint()?);
        let payload_offset = (payload_end as i64 + payload_gap) as usize;
        let payload_extent_len = dictionary.varint_u32()? as usize;
        postings_end = postings_offset + postings_extent_len;
        payload_end = payload_offset + payload_extent_len;
        let (tids, _) =
            postings_stream_lsg3(&blob[postings_at + postings_offset..postings_at + postings_end])?;
        let mut payload = Bytes::new(&blob[payload_at + payload_offset..payload_at + payload_end]);
        let entry_count = payload.varint_u32()?;
        let slots = (entry_count as usize)
            .div_ceil(SKIP_INTERVAL)
            .saturating_sub(1);
        for _ in 0..slots {
            payload.u32()?;
        }
        let mut postings = Vec::new();
        for _ in 0..entry_count {
            let bucket = payload.u8()?;
            let hits = positions(&mut payload)?;
            postings.push((
                Tid {
                    block: 0,
                    offset: 0,
                },
                vec![FieldHit {
                    field: 0,
                    bucket,
                    positions: hits,
                }],
            ));
        }
        for (index, tid) in tids.iter().enumerate() {
            postings[index].0 = *tid;
        }
        decoded.terms.push(DecodedTerm {
            term: String::from_utf8(term).map_err(|_| "dictionary: term UTF-8".to_string())?,
            df,
            max_tf_bucket,
            postings,
            bounds: Vec::new(),
        });
    }
    Ok(decoded)
}

/// The LSG3 postings envelope: like the LSG4 one but the term bound is the
/// bucket-mask + min_len payload of the older format.
fn postings_stream_lsg3(bytes: &[u8]) -> Result<(Vec<Tid>, Vec<DecodedBound>), String> {
    let mut reader = Bytes::new(bytes);
    let form = reader.u8()?;
    if form > 7 || form & 6 == 6 {
        return Err("postings: unknown form byte".into());
    }
    let count = reader.varint_u32()?;
    let grouped = form & 1 != 0;
    let compact = form & 4 != 0;
    let mut bounds = Vec::new();
    if form & 2 != 0 {
        let len = reader.varint_u32()? as usize;
        let mut table = Bytes::new(reader.take(len)?);
        for _ in 0..count.div_ceil(BLOCK_POSTINGS as u32) {
            let buckets = table.varint_u32()?;
            if buckets == 0 || buckets >> 16 != 0 {
                return Err("bound: bucket mask".into());
            }
            for bucket in 0..16 {
                if buckets & (1 << bucket) != 0 && table.varint_u32()? == u32::MAX {
                    return Err("bound: min_len".into());
                }
            }
            table.varint_u32()?;
            table.varint_u32()?;
            if !grouped {
                table.varint()?;
            }
        }
    } else if compact {
        let buckets = reader.varint_u32()?;
        if buckets == 0 || buckets >> 16 != 0 {
            return Err("bound: bucket mask".into());
        }
        for bucket in 0..16 {
            if buckets & (1 << bucket) != 0 {
                reader.varint_u32()?;
            }
        }
        bounds.push(DecodedBound {
            present_fields: 1,
            min_doc_length: [[u32::MAX; 16]; 16],
            last: Tid {
                block: 0,
                offset: 0,
            },
        });
    }
    let body_at = reader.at;
    let mut reader = Bytes::new(&bytes[body_at..]);
    let tids = if grouped {
        let groups = reader.varint_u32()?;
        let mut tids = Vec::new();
        let mut previous_gid: Option<u32> = None;
        for _ in 0..groups {
            let raw = reader.varint_u32()?;
            let gid = match previous_gid {
                None => raw,
                Some(previous) => previous + raw + 1,
            };
            previous_gid = Some(gid);
            let group_count = reader.varint_u32()?;
            let bitmap = reader.take(32)?;
            let body_len = reader.varint_u32()? as usize;
            let group_end = reader.at + body_len;
            for bit in 0..GROUP_BLOCKS {
                if bitmap[bit / 8] & (1 << (bit % 8)) == 0 {
                    continue;
                }
                let block = gid * GROUP_BLOCKS as u32 + bit as u32;
                match reader.u8()? {
                    0 => {
                        let n = reader.varint_u32()?;
                        for _ in 0..n {
                            let offset = reader.u16()?;
                            tids.push(Tid { block, offset });
                        }
                    }
                    1 => {
                        for (byte, bits) in reader.take(37)?.iter().enumerate() {
                            for bit in 0..8 {
                                if bits & (1 << bit) != 0 {
                                    tids.push(Tid {
                                        block,
                                        offset: (byte * 8 + bit + 1) as u16,
                                    });
                                }
                            }
                        }
                    }
                    _ => return Err("postings: unknown page tag".into()),
                }
            }
            let _ = group_end;
            let _ = group_count;
        }
        if tids.len() as u32 != count {
            return Err("postings: posting count mismatch".into());
        }
        tids
    } else {
        let mut tids = Vec::new();
        let mut last_block = 0u32;
        for _ in 0..count {
            let block = last_block + reader.varint_u32()?;
            let offset = reader.varint_u32()? as u16;
            tids.push(Tid { block, offset });
            last_block = block;
        }
        tids
    };
    if !bounds.is_empty() {
        bounds[0].last = *tids.last().ok_or("postings: empty but bounded")?;
    }
    Ok((tids, bounds))
}

// ---------------------------------------------------------------------------
// Manifest comparison.
// ---------------------------------------------------------------------------

fn manifest_tid(value: &Json) -> Tid {
    let pair = value.arr();
    Tid {
        block: pair[0].integer() as u32,
        offset: pair[1].integer() as u16,
    }
}

fn manifest_hit(value: &Json) -> FieldHit {
    FieldHit {
        field: value.get("field").unwrap().integer() as u8,
        bucket: value.get("bucket").unwrap().integer() as u8,
        positions: value
            .get("positions")
            .unwrap()
            .arr()
            .iter()
            .map(Json::integer)
            .map(|p| p as u32)
            .collect(),
    }
}

fn compare(decoded: &Decoded, manifest: &Json, what: &str) {
    let expected = manifest.get("expected").expect("expected section");
    assert_eq!(
        decoded.doc_count,
        expected.get("doc_count").unwrap().integer() as u32,
        "{what}: doc_count"
    );
    assert_eq!(
        decoded.total_length,
        expected.get("total_length").unwrap().integer(),
        "{what}: total_length"
    );
    let totals: Vec<u64> = expected
        .get("field_total")
        .unwrap()
        .arr()
        .iter()
        .map(Json::integer)
        .collect();
    assert_eq!(decoded.field_totals, totals, "{what}: field_total");
    let documents = expected.get("documents").unwrap().arr();
    assert_eq!(
        decoded.documents.len(),
        documents.len(),
        "{what}: document count"
    );
    for (row, want) in decoded.documents.iter().zip(documents) {
        assert_eq!(row.0, manifest_tid(want.get("tid").unwrap()), "{what}: tid");
        let lengths: Vec<u32> = want
            .get("lengths")
            .unwrap()
            .arr()
            .iter()
            .map(|n| n.integer() as u32)
            .collect();
        assert_eq!(row.1, lengths, "{what}: lengths of {}", describe(row.0));
    }
    let terms = expected.get("terms").unwrap().arr();
    assert_eq!(decoded.terms.len(), terms.len(), "{what}: term count");
    for (term, want) in decoded.terms.iter().zip(terms) {
        assert_eq!(term.term, want.get("term").unwrap().str(), "{what}: term");
        assert_eq!(
            term.df,
            want.get("df").unwrap().integer() as u32,
            "{what}: df"
        );
        assert_eq!(
            term.max_tf_bucket,
            want.get("max_tf_bucket").unwrap().integer() as u8,
            "{what}: max_tf_bucket"
        );
        let postings = want.get("postings").unwrap().arr();
        assert_eq!(
            term.postings.len(),
            postings.len(),
            "{what}: postings of {:?}",
            term.term
        );
        for ((tid, hits), want) in term.postings.iter().zip(postings) {
            assert_eq!(
                *tid,
                manifest_tid(want.get("tid").unwrap()),
                "{what}: posting"
            );
            let fields: Vec<FieldHit> = want
                .get("fields")
                .unwrap()
                .arr()
                .iter()
                .map(manifest_hit)
                .collect();
            assert_eq!(
                *hits,
                fields,
                "{what}: fields of {:?} at {}",
                term.term,
                describe(*tid)
            );
        }
    }
}

fn describe(tid: Tid) -> String {
    let mut out = String::new();
    let _ = write!(out, "({},{})", tid.block, tid.offset);
    out
}

fn fixtures() -> PathBuf {
    PathBuf::from(FIXTURES)
}

fn load(name: &str) -> (Vec<u8>, Json) {
    let text = std::fs::read_to_string(fixtures().join(format!("{name}.json")))
        .unwrap_or_else(|error| panic!("missing manifest {name}.json: {error}"));
    (load_blob(name), parse_json(&text))
}

fn load_blob(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(format!("{name}.segment")))
        .unwrap_or_else(|error| panic!("missing fixture {name}.segment: {error}"))
}

#[test]
fn golden_vectors_decode_to_their_manifests() {
    let mut valid = 0;
    let mut invalid = 0;
    for entry in std::fs::read_dir(fixtures()).expect("fixtures directory") {
        let path = entry.expect("directory entry").path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        // The weights-alt manifest revalidates another case's blob; the LSG3
        // fixtures have their own dedicated tests below.
        let manifest = parse_json(&std::fs::read_to_string(&path).expect("manifest text"));
        if let Some("LSG3") = manifest.get("format").map(Json::str) {
            continue;
        }
        if name.ends_with("_alt") {
            let blob = load_blob(name.trim_end_matches("_alt"));
            check_alt(&blob, &manifest, &name);
            valid += 1;
            continue;
        }
        let blob = load_blob(&name);
        assert_eq!(
            manifest.get("schema").unwrap().str(),
            "stannum.lsg4-golden/1",
            "{name}: schema"
        );
        match manifest.get("kind").unwrap().str() {
            "valid" => {
                let decoded = decode_lsg4(&blob)
                    .unwrap_or_else(|error| panic!("{name}: decode failed: {error}"));
                compare(&decoded, &manifest, &name);
                // Weights never enter the bytes: the manifest's weights are
                // free to disagree with any other manifest of the same blob.
                valid += 1;
            }
            "invalid" => {
                let expected = manifest.get("expected_error").unwrap().str();
                match decode_lsg4(&blob) {
                    Err(error) => assert!(
                        error.contains(expected),
                        "{name}: error {error:?} does not match {expected:?}"
                    ),
                    Ok(_) => panic!("{name}: expected {expected:?}, decoded cleanly"),
                }
                invalid += 1;
            }
            other => panic!("{name}: unknown kind {other}"),
        }
    }
    assert!(valid >= 8, "{valid} valid vectors; the minimum is 8");
    assert!(
        invalid >= 15,
        "{invalid} invalid vectors; the minimum is 15"
    );
}

fn check_alt(blob: &[u8], manifest: &Json, name: &str) {
    let decoded =
        decode_lsg4(blob).unwrap_or_else(|error| panic!("{name}: decode failed: {error}"));
    compare(&decoded, manifest, name);
}

#[test]
fn one_field_lsg4_is_logically_equal_to_its_lsg3_twin() {
    let (lsg4, _) = load("one_field_bit_equality");
    let (lsg3, _) = load("one_field_bit_equality_lsg3");
    let four = decode_lsg4(&lsg4).expect("LSG4 twin decodes");
    let three = decode_lsg3(&lsg3).expect("LSG3 twin decodes");
    assert_eq!(four.doc_count, three.doc_count);
    assert_eq!(four.total_length, three.total_length);
    assert_eq!(four.field_count, 1);
    assert_eq!(four.documents, three.documents);
    // The bound payloads differ by format (field-masked vs bucket-masked);
    // what must be identical is everything the scorer consumes.
    let strip = |mut decoded: Decoded| {
        decoded.header_len = 0; // the headers differ by format, of course
        for term in &mut decoded.terms {
            term.bounds.clear();
        }
        decoded
    };
    assert_eq!(strip(four), strip(three), "the twins differ logically");
}

#[test]
fn single_column_builds_stay_lsg3() {
    let (blob, manifest) = load("single_column_lsg3");
    assert_eq!(&blob[..4], b"LSG3");
    assert_eq!(manifest.get("format").unwrap().str(), "LSG3");
    assert!(decode_lsg4(&blob).is_err());
}

#[test]
fn sixteen_field_header_fits_the_extended_head_probe() {
    let (blob, manifest) = load("sixteen_fields");
    let header_len = decode_lsg4(&blob)
        .unwrap_or_else(|error| panic!("sixteen_fields: {error}"))
        .header_len;
    // The 16 x u64 field totals alone (128 bytes) push the header past the
    // 64-byte probe, which is exactly why LSG4 re-reads a 256-byte head; the
    // RFC's 169-byte worst case additionally assumes maximal varints.
    assert!(header_len > 64, "header is only {header_len} bytes");
    assert!(header_len <= 256, "header is {header_len} bytes");
    assert_eq!(manifest.get("kind").unwrap().str(), "valid");
}

// ---------------------------------------------------------------------------
// The blessed generator. Ignored: run by hand to (re)create the fixtures;
// a regeneration is a deliberate RFC amendment, never routine.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod generator {
    use super::*;

    fn write_manifest(name: &str, manifest: &str) {
        std::fs::write(fixtures().join(format!("{name}.json")), manifest).unwrap();
    }

    fn write_blob(name: &str, blob: &[u8]) {
        std::fs::write(fixtures().join(format!("{name}.segment")), blob).unwrap();
    }

    /// Expected values derived from the INPUT documents, never from reading
    /// the writer's output.
    fn expected_json(documents: &FixtureDocuments, field_count: u8) -> String {
        use std::collections::BTreeMap;
        /// Per term, per document: its `(field, positions)` groups.
        type TermPostings = BTreeMap<segment::Tid, Vec<(u8, Vec<u32>)>>;
        let mut rows: BTreeMap<segment::Tid, Vec<u32>> = BTreeMap::new();
        let mut terms: BTreeMap<String, TermPostings> = BTreeMap::new();
        for (tid, tokens) in documents.iter().filter(|(_, tokens)| !tokens.is_empty()) {
            let mut row = vec![0u32; usize::from(field_count)];
            let mut per_term: BTreeMap<(&str, u8), Vec<u32>> = BTreeMap::new();
            for (field, term, position) in tokens {
                row[usize::from(*field)] += 1;
                per_term
                    .entry((term.as_str(), *field))
                    .or_default()
                    .push(*position);
            }
            rows.insert(*tid, row);
            for ((term, field), positions) in per_term {
                terms
                    .entry(term.to_owned())
                    .or_default()
                    .entry(*tid)
                    .or_default()
                    .push((field, positions));
            }
        }
        let total: u64 = rows
            .values()
            .map(|row| row.iter().sum::<u32>() as u64)
            .sum();
        let field_total: Vec<u64> = (0..field_count)
            .map(|field| {
                rows.values()
                    .map(|row| u64::from(row[usize::from(field)]))
                    .sum()
            })
            .collect();
        let mut documents_json = String::new();
        for (tid, row) in &rows {
            let lengths = row
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                documents_json,
                "        {{\"tid\": [{}, {}], \"lengths\": [{lengths}]}},",
                tid.block, tid.offset
            );
        }
        documents_json.pop(); // trailing newline
        documents_json.pop(); // trailing comma
        let mut terms_json = String::new();
        for (term, postings) in &terms {
            let df = postings.len();
            let mut postings_json = String::new();
            let mut max_bucket = 0u8;
            for (tid, hits) in postings {
                let mut fields_json = String::new();
                for (field, positions) in hits {
                    let bucket = bucket_of(positions.len());
                    max_bucket = max_bucket.max(bucket);
                    let positions = positions
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = writeln!(
                        fields_json,
                        "              {{\"field\": {field}, \"bucket\": {bucket}, \"positions\": [{positions}]}},"
                    );
                }
                fields_json.pop();
                fields_json.pop();
                let _ = writeln!(
                    postings_json,
                    "          {{\"tid\": [{}, {}], \"fields\": [\n{fields_json}\n          ]}},",
                    tid.block, tid.offset
                );
            }
            postings_json.pop();
            postings_json.pop();
            let _ = writeln!(
                terms_json,
                "      {{\"term\": {term:?}, \"df\": {df}, \"max_tf_bucket\": {max_bucket}, \"postings\": [\n{postings_json}\n      ]}},"
            );
        }
        terms_json.pop();
        terms_json.pop();
        let totals = field_total
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "  \"expected\": {{\n    \"doc_count\": {},\n    \"total_length\": {total},\n    \"field_total\": [{totals}],\n    \"documents\": [\n{documents_json}\n    ],\n    \"terms\": [\n{terms_json}\n    ]\n  }}",
            rows.len()
        )
    }

    fn manifest(
        case: &str,
        kind: &str,
        fields: &[(&str, f32)],
        documents: &FixtureDocuments,
        field_count: u8,
    ) -> String {
        let fields = fields
            .iter()
            .map(|(name, weight)| format!("    {{\"name\": {name:?}, \"weight\": {weight}}}"))
            .collect::<Vec<_>>()
            .join(",\n");
        let mut documents_json = String::new();
        for (tid, tokens) in documents {
            let mut tokens_json = String::new();
            for (field, term, position) in tokens {
                let _ = writeln!(
                    tokens_json,
                    "        {{\"field\": {field}, \"term\": {term:?}, \"position\": {position}}},"
                );
            }
            tokens_json.pop();
            tokens_json.pop();
            let _ = writeln!(
                documents_json,
                "    {{\"tid\": [{}, {}], \"tokens\": [\n{tokens_json}\n    ]}},",
                tid.block, tid.offset
            );
        }
        documents_json.pop();
        documents_json.pop();
        format!(
            "{{\n  \"schema\": \"stannum.lsg4-golden/1\",\n  \"case\": {case:?},\n  \"kind\": {kind:?},\n  \"generator\": {{\"git\": \"db68b07+\", \"tool\": \"SegmentBuilder::add_document_fields + finish_fields\"}},\n  \"format\": \"LSG4\",\n  \"layout_revision\": 1,\n  \"fields\": [\n{fields}\n  ],\n  \"documents\": [\n{documents_json}\n  ],\n{expected}\n}}\n",
            expected = expected_json(documents, field_count)
        )
    }

    fn invalid_manifest(case: &str, expected_error: &str) -> String {
        format!(
            "{{\n  \"schema\": \"stannum.lsg4-golden/1\",\n  \"case\": {case:?},\n  \"kind\": \"invalid\",\n  \"generator\": {{\"git\": \"db68b07+\", \"tool\": \"byte edits over corrupt_base\"}},\n  \"format\": \"LSG4\",\n  \"layout_revision\": 1,\n  \"expected_error\": {expected_error:?}\n}}\n"
        )
    }

    fn build(field_count: u8, documents: &FixtureDocuments) -> Vec<u8> {
        use segment::segment::SegmentBuilder;
        let mut builder = SegmentBuilder::with_field_count(field_count);
        for (tid, tokens) in documents {
            builder
                .add_document_fields(
                    *tid,
                    field_count,
                    tokens
                        .iter()
                        .map(|(field, term, position)| (*field, term.as_str(), *position)),
                )
                .unwrap();
        }
        builder.finish_fields()
    }

    fn tid(block: u32, offset: u16) -> segment::Tid {
        segment::Tid::new(block, offset).unwrap()
    }

    #[test]
    #[ignore = "regenerates the frozen golden vectors; run by hand only"]
    fn generate() {
        std::fs::create_dir_all(fixtures()).unwrap();

        // two_fields_basic: one document, a term in both fields with both
        // position sequences starting at 0 (RFC §7's own example shape).
        let documents = vec![(
            tid(0, 1),
            vec![
                (0u8, "数据库".to_owned(), 0u32),
                (1, "数据库".to_owned(), 0),
                (1, "索引".to_owned(), 1),
            ],
        )];
        write_blob("two_fields_basic", &build(2, &documents));
        write_manifest(
            "two_fields_basic",
            &manifest(
                "two_fields_basic",
                "valid",
                &[("title", 3.0), ("body", 1.0)],
                &documents,
                2,
            ),
        );

        // three_fields_weights: three fields, distinct weights. The same blob
        // is revalidated under a second manifest with different weights —
        // field names and weights live only in the meta trailer.
        let mut documents = Vec::new();
        for i in 0..5u32 {
            let mut tokens = Vec::new();
            let mut position = [0u32; 3];
            for (field, word) in [(0u8, "apple"), (1, "banana"), (2, "cherry")] {
                for _ in 0..=(i % 3) + u32::from(field) {
                    position[usize::from(field)] += 1;
                    tokens.push((field, word.to_owned(), position[usize::from(field)]));
                }
            }
            documents.push((tid(i * 3, (i % 3 + 1) as u16), tokens));
        }
        let blob = build(3, &documents);
        write_blob("three_fields_weights", &blob);
        write_manifest(
            "three_fields_weights",
            &manifest(
                "three_fields_weights",
                "valid",
                &[("title", 2.5), ("abstract", 1.5), ("body", 1.0)],
                &documents,
                3,
            ),
        );
        write_manifest(
            "three_fields_weights_alt",
            &manifest(
                "three_fields_weights",
                "valid",
                &[("heading", 9.0), ("summary", 0.5), ("text", 3.25)],
                &documents,
                3,
            ),
        );

        // sixteen_fields: the MAX_FIELDS boundary; a term hitting every field.
        let mut documents = Vec::new();
        for i in 0..3u32 {
            let mut tokens = Vec::new();
            let mut position = [0u32; 16];
            for field in 0..16u8 {
                for _ in 0..=i + u32::from(field.min(2)) {
                    position[usize::from(field)] += 1;
                    tokens.push((field, "omni".to_owned(), position[usize::from(field)]));
                }
                position[usize::from(field)] += 1;
                tokens.push((field, format!("f{field}"), position[usize::from(field)]));
            }
            documents.push((tid(i, (i + 1) as u16), tokens));
        }
        write_blob("sixteen_fields", &build(16, &documents));
        write_manifest(
            "sixteen_fields",
            &manifest(
                "sixteen_fields",
                "valid",
                &(0..16)
                    .map(|f| (Box::leak(format!("col{f}").into_boxed_str()) as &str, 1.0))
                    .collect::<Vec<_>>(),
                &documents,
                16,
            ),
        );

        // bounds_table: more than BLOCK_POSTINGS postings for one term across
        // several fields and buckets, so the stream carries a bounds table.
        let mut documents = Vec::new();
        for i in 0..300u32 {
            let mut tokens = Vec::new();
            let mut position = [0u32; 3];
            for field in 0..3u8 {
                for _ in 0..=(i % 5) + u32::from(field) {
                    position[usize::from(field)] += 1;
                    tokens.push((field, "common".to_owned(), position[usize::from(field)]));
                }
            }
            documents.push((tid(i / 4, (i % 4 + 1) as u16), tokens));
        }
        write_blob("bounds_table", &build(3, &documents));
        write_manifest(
            "bounds_table",
            &manifest(
                "bounds_table",
                "valid",
                &[("title", 2.0), ("body", 1.0), ("footer", 0.5)],
                &documents,
                3,
            ),
        );

        // skip_table: one term with more than SKIP_INTERVAL postings, so the
        // payload carries a skip table.
        let mut documents = Vec::new();
        for i in 0..40u32 {
            let mut tokens = Vec::new();
            for k in 0..3 {
                tokens.push((0u8, "skipme".to_owned(), k));
                tokens.push((1, "skipme".to_owned(), k));
            }
            documents.push((tid(i, 1), tokens));
        }
        write_blob("skip_table", &build(2, &documents));
        write_manifest(
            "skip_table",
            &manifest(
                "skip_table",
                "valid",
                &[("title", 1.0), ("body", 1.0)],
                &documents,
                2,
            ),
        );

        // empty_fields_omitted: a document whose every field is empty is not
        // recorded at all.
        let documents = vec![
            (tid(0, 1), vec![(0u8, "kept".to_owned(), 0u32)]),
            (tid(0, 2), Vec::new()),
        ];
        write_blob("empty_fields_omitted", &build(2, &documents));
        write_manifest(
            "empty_fields_omitted",
            &manifest(
                "empty_fields_omitted",
                "valid",
                &[("title", 1.0), ("body", 1.0)],
                &documents,
                2,
            ),
        );

        // one_field_bit_equality: a one-field LSG4 blob beside the LSG3 blob
        // of the same tokens. LSG4 field_count == 1 is valid on disk (RFC
        // §5.8); the product write path never produces it.
        let mut tokens = Vec::new();
        for i in 0..50u32 {
            tokens.push((0u8, "shared".to_owned(), i));
        }
        tokens.push((0, "once".to_owned(), 50));
        let documents = vec![(tid(0, 1), tokens)];
        write_blob("one_field_bit_equality", &build(1, &documents));
        write_manifest(
            "one_field_bit_equality",
            &manifest(
                "one_field_bit_equality",
                "valid",
                &[("only", 1.0)],
                &documents,
                1,
            ),
        );
        let mut lsg3 = segment::segment::SegmentBuilder::default();
        let (tid0, flat): (segment::Tid, Vec<(u8, String, u32)>) = documents[0].clone();
        let flat: Vec<(String, u32)> = flat
            .iter()
            .map(|(_, term, position)| (term.clone(), *position))
            .collect();
        lsg3.add_document(
            tid0,
            flat.iter()
                .map(|(term, position)| (term.as_str(), *position)),
        )
        .unwrap();
        let lsg3 = lsg3.finish();
        assert_eq!(&lsg3[..4], b"LSG3");
        write_blob("one_field_bit_equality_lsg3", &lsg3);
        write_manifest("one_field_bit_equality_lsg3", &invalid_manifest_shim());

        // single_column_lsg3: the product write path (add_document +
        // finish) stays LSG3 forever.
        let mut builder = segment::segment::SegmentBuilder::default();
        builder
            .add_document(tid(0, 1), [("single", 1u32), ("column", 2)])
            .unwrap();
        let blob = builder.finish();
        assert_eq!(&blob[..4], b"LSG3");
        write_blob("single_column_lsg3", &blob);
        write_manifest(
            "single_column_lsg3",
            "{\n  \"schema\": \"stannum.lsg4-golden/1\",\n  \"case\": \"single_column_lsg3\",\n  \"kind\": \"valid\",\n  \"generator\": {\"git\": \"db68b07+\", \"tool\": \"SegmentBuilder::add_document + finish\"},\n  \"format\": \"LSG3\",\n  \"layout_revision\": null,\n  \"fields\": [],\n  \"documents\": [],\n  \"expected\": {}\n}\n",
        );

        // The corruption matrix, as byte edits over corrupt_base.
        let base_documents = vec![
            (
                tid(0, 1),
                vec![
                    (0u8, "alpha".to_owned(), 0u32),
                    (1, "alpha".to_owned(), 0),
                    (1, "beta".to_owned(), 1),
                ],
            ),
            (
                tid(0, 2),
                vec![
                    (0, "alpha".to_owned(), 0),
                    (0, "beta".to_owned(), 1),
                    (1, "gamma".to_owned(), 0),
                ],
            ),
            (
                tid(1, 1),
                vec![
                    (0, "gamma".to_owned(), 0),
                    (0, "gamma".to_owned(), 1),
                    (1, "alpha".to_owned(), 0),
                ],
            ),
        ];
        let base = build(2, &base_documents);
        let edits = corruption_edits(&base, 2);
        for (name, blob, expected_error) in edits {
            write_blob(name.as_str(), blob.as_slice());
            write_manifest(
                name.as_str(),
                &invalid_manifest(name.as_str(), expected_error.as_str()),
            );
        }
    }

    fn invalid_manifest_shim() -> String {
        // The LSG3 twin carries no LSG4 expectations of its own; the
        // bit-equality test reads it directly.
        "{\n  \"schema\": \"stannum.lsg4-golden/1\",\n  \"case\": \"one_field_bit_equality_lsg3\",\n  \"kind\": \"twin\",\n  \"generator\": {\"git\": \"db68b07+\", \"tool\": \"SegmentBuilder::add_document + finish\"},\n  \"format\": \"LSG3\",\n  \"layout_revision\": null,\n  \"fields\": [],\n  \"documents\": [],\n  \"expected\": {}\n}\n".to_string()
    }

    /// Byte edits over the base blob, each provoking exactly one named rule.
    fn corruption_edits(base: &[u8], field_count: u8) -> Vec<(String, Vec<u8>, String)> {
        use segment::segment::Segment;
        let segment = Segment::parse(base).unwrap();
        let sections = segment.sections();

        let mut bad_magic = base.to_vec();
        bad_magic[0] = b'X';
        let mut revision = base.to_vec();
        revision[4] = 2;
        let mut field_count_17 = base.to_vec();
        // Header: magic(4) + revision(1) + doc_count + total_length varints.
        let mut at = 5;
        let walk = |bytes: &[u8], at: &mut usize| -> u64 {
            let mut value = 0u64;
            let mut shift = 0;
            loop {
                let byte = bytes[*at];
                value |= u64::from(byte & 127) << shift;
                *at += 1;
                shift += 7;
                if byte & 128 == 0 {
                    return value;
                }
            }
        };
        walk(base, &mut at); // doc_count
        walk(base, &mut at); // total_length
        let field_count_at = at;
        field_count_17[field_count_at] = 17;
        let mut lengths_invariant = base.to_vec();
        lengths_invariant.extend_from_slice(&[0, 0, 0, 0]);
        let totals_at = field_count_at + 1;
        let mut totals_mismatch = base.to_vec();
        totals_mismatch[totals_at] ^= 1;

        // Locate the first term's payload data area for entry-level edits.
        let dictionary = segment.dictionary().unwrap();
        let (term, entry) = dictionary.iter().next().unwrap().unwrap();
        assert_eq!(term, "alpha");
        let payload_at = sections.header
            + sections.dictionary
            + sections.postings
            + entry.payload.offset as usize;
        let payload = &base[payload_at..payload_at + entry.payload.len as usize];
        let entries = segment.resolve(entry).unwrap().payload().unwrap();
        let data_at = payload_at + entries.skip_table_len() + {
            // count varint
            let mut at = 0;
            walk(payload, &mut at);
            at
        };

        let mut hit_count_zero = base.to_vec();
        hit_count_zero[data_at] = 0;
        let mut hit_count_over = base.to_vec();
        hit_count_over[data_at] = field_count + 1;

        // Group layout of entry 0: hit_count, then per group packed +
        // positions. alpha's entry 0 has two groups (fields 0 and 1).
        let mut at = data_at;
        let hit_count = walk(base, &mut at);
        assert_eq!(hit_count, 2);
        let packed_at = at;
        let mut field_ids_flat = base.to_vec();
        // Make the second group's field nibble equal the first's.
        let first_packed = base[packed_at];
        let second_group_at = {
            let mut probe = at + 1; // past the first packed byte
            let _ = walk(base, &mut probe); // n
            let n = walk_count_positions(base, packed_at);
            let _ = n;
            probe
        };
        let _ = second_group_at;
        // Walk the first group's positions to the second packed byte.
        let mut probe = packed_at + 1;
        let n = walk(base, &mut probe) as usize;
        probe += varint_len_of_list(base, probe, n);
        let second_packed_at = probe;
        field_ids_flat[second_packed_at] = first_packed;

        // position_count zero: patch the first group's n varint.
        let mut position_count_zero = base.to_vec();
        position_count_zero[packed_at + 1] = 0;

        // bucket mismatch: flip the first group's bucket nibble.
        let mut bucket_mismatch = base.to_vec();
        let packed = base[packed_at];
        let bucket = packed & 0x0f;
        bucket_mismatch[packed_at] = (packed & 0xf0) | ((bucket + 1) % 16);

        // Bounds edits on alpha's postings term bound (one block).
        let postings_at = sections.header + sections.dictionary + entry.postings.offset as usize;
        let mut pwalk = postings_at + 1; // form byte
        let _ = walk(base, &mut pwalk); // count
        let field_mask_at = pwalk;
        let mut field_mask_zero = base.to_vec();
        field_mask_zero[field_mask_at] = 0;
        let mut field_mask_beyond = base.to_vec();
        field_mask_beyond[field_mask_at] = 1 << field_count;
        let bucket_mask_at = field_mask_at + 1; // field mask is one byte here
        let mut bucket_mask_zero = base.to_vec();
        bucket_mask_zero[bucket_mask_at] = 0;
        let mut min_len_at = bucket_mask_at + 1; // bucket mask is one byte
        while base[min_len_at] == 0x80 {
            min_len_at += 1;
        }
        let mut min_len_absent = base.to_vec();
        put_varint(&mut min_len_absent, min_len_at, u64::from(u32::MAX));

        let truncated = base[..base.len() - 3].to_vec();

        vec![
            (
                "corrupt_bad_magic".into(),
                bad_magic,
                "header: magic".into(),
            ),
            (
                "corrupt_revision_two".into(),
                revision,
                "header: layout_revision".into(),
            ),
            (
                "corrupt_field_count_17".into(),
                field_count_17,
                "header: field_count".into(),
            ),
            (
                "corrupt_lengths_invariant".into(),
                lengths_invariant,
                "header: lengths extent".into(),
            ),
            (
                "corrupt_field_totals_mismatch".into(),
                totals_mismatch,
                "header: field totals".into(),
            ),
            (
                "corrupt_hit_count_zero".into(),
                hit_count_zero,
                "payload rule 1: field hit count is zero".into(),
            ),
            (
                "corrupt_hit_count_over".into(),
                hit_count_over,
                "payload rule 1: field hit count exceeds".into(),
            ),
            (
                "corrupt_field_ids_not_increasing".into(),
                field_ids_flat,
                "payload rule 2: field ids not strictly increasing".into(),
            ),
            (
                "corrupt_position_count_zero".into(),
                position_count_zero,
                "payload rule 5: position count is zero".into(),
            ),
            (
                "corrupt_bucket_mismatch".into(),
                bucket_mismatch,
                "payload rule 8: bucket disagrees".into(),
            ),
            (
                "corrupt_field_mask_zero".into(),
                field_mask_zero,
                "bound rule 1: field mask is empty".into(),
            ),
            (
                "corrupt_field_mask_beyond".into(),
                field_mask_beyond,
                "bound rule 2: field id at or beyond".into(),
            ),
            (
                "corrupt_bucket_mask_zero".into(),
                bucket_mask_zero,
                "bound rule 3: bucket mask is empty".into(),
            ),
            (
                "corrupt_min_len_absent_marker".into(),
                min_len_absent,
                "bound rule 4: min_len is the absent marker".into(),
            ),
            (
                "corrupt_truncated".into(),
                truncated,
                // The tail cut lands in the length rows, so the lengths
                // invariant fires first; any truncation is caught somewhere.
                "header: lengths extent".into(),
            ),
        ]
    }

    fn walk_count_positions(_base: &[u8], _at: usize) -> u64 {
        0
    }

    /// The encoded byte length of an `n`-position list starting at `at`.
    fn varint_len_of_list(base: &[u8], at: usize, n: usize) -> usize {
        // first absolute + (n - 1) deltas; each varint's length is counted
        // from its bytes.
        let mut len = 0;
        let mut probe = at;
        for _ in 0..n {
            loop {
                let byte = base[probe];
                probe += 1;
                len += 1;
                if byte & 128 == 0 {
                    break;
                }
            }
        }
        len
    }

    fn put_varint(bytes: &mut [u8], at: usize, mut value: u64) {
        let mut at = at;
        loop {
            let byte = (value & 127) as u8;
            value >>= 7;
            if value == 0 {
                bytes[at] = byte;
                return;
            }
            bytes[at] = byte | 128;
            at += 1;
        }
    }
}
