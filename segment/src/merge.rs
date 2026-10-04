// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Validated direct posting merges, independent of PostgreSQL publication.
//!
//! Inputs are borrowed complete blobs with per-input dead sets. All inputs,
//! including dead documents, are verified before metadata is reused. No index
//! page is written.
//! A fold's budgeted PostgreSQL merges retain the metadata lock; every other
//! merge runs on owned snapshots unlocked and revalidates before publication.
//! Page allocation, WAL and publication remain the caller’s responsibility.
use crate::dictionary::{DictionaryBuilder, Extent, TermEntry};
use crate::docs::TidCursor;
use crate::payload::PayloadBuilder;
use crate::segment::Segment;
use crate::set::Cursor;
use crate::{Error, Result, Tid};
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap};

/// Pairing a blob and its dead set prevents mismatched parallel input arrays.
#[derive(Clone, Copy)]
pub struct MergeInput<'a> {
    pub bytes: &'a [u8],
    pub dead: &'a BTreeSet<Tid>,
}

/// Admission/output limits, not a peak-memory or elapsed-time guarantee.
/// Verification and existing codecs allocate temporary data. Cancellation runs
/// between input validations, documents, terms and members; a single validation
/// or codec call is not interruptible. Callers must bound individual input sizes.
#[derive(Clone, Copy, Debug)]
pub struct MergeLimits {
    pub max_inputs: usize,
    pub max_input_bytes: usize,
    pub max_documents: usize,
    pub max_output_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error(transparent)]
    Codec(#[from] Error),
    #[error("invalid merge input {index}: {detail}")]
    InvalidInput { index: usize, detail: String },
    #[error("merge limit exceeded: {0}")]
    Limit(&'static str),
    #[error("merge canceled")]
    Canceled,
    #[error("cannot allocate merge output")]
    Allocation,
}

/// Validate and merge immutable segments, filtering each source's dead tuples.
/// Duplicate live CTIDs are errors; dead CTID reuse in another input is allowed.
/// On error/cancellation no output is returned, and inputs are never modified.
/// Resource exhaustion in existing infallible codec allocations is not converted
/// to `MergeError`; callers must enforce their own memory budget as well.
pub fn merge(
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
    checkpoint: impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Vec<u8>, MergeError> {
    merge_inner(inputs, limits, checkpoint)
}

fn check(actual: usize, limit: usize, name: &'static str) -> std::result::Result<(), MergeError> {
    if actual > limit {
        Err(MergeError::Limit(name))
    } else {
        Ok(())
    }
}

fn merge_field_count(segments: &[Segment<'_>]) -> Result<u8> {
    let mut found: Option<u8> = None;
    for segment in segments {
        let Some(trailer) = segment.trailer() else {
            continue;
        };
        if trailer.version != crate::trailer::VERSION {
            // A withdrawn v1 trailer must be classified stale before any
            // merge sees it, not fail incidentally inside a channel decode.
            return Err(Error::Corrupt("STNF version"));
        }
        match found {
            None => found = Some(trailer.field_count),
            Some(prev) if prev == trailer.field_count => {}
            Some(_) => return Err(Error::Corrupt("STNF field_count")),
        }
    }
    Ok(found.unwrap_or(1))
}

/// Apply the same admission and complete input validation used by direct merges.
/// Alternative executors must additionally reject duplicate live TIDs, enforce
/// output limits and provide checkpoints while constructing their output.
pub fn validate_inputs(
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
    mut checkpoint: impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<(), MergeError> {
    checkpoint()?;
    check(inputs.len(), limits.max_inputs, "input count")?;
    let mut input_bytes = 0usize;
    let mut input_docs = 0usize;
    // Admission precedes the whole-segment verifier and merge allocations.
    for input in inputs {
        input_bytes = input_bytes
            .checked_add(input.bytes.len())
            .ok_or(MergeError::Limit("input bytes"))?;
        check(
            input_bytes,
            limits.max_input_bytes.min(u32::MAX as usize),
            "input bytes",
        )?;
        let parsed = Segment::parse(input.bytes)?;
        check(
            input.dead.len(),
            parsed.document_count() as usize,
            "dead tuple count",
        )?;
        input_docs = input_docs
            .checked_add(parsed.document_count() as usize)
            .ok_or(MergeError::Limit("documents"))?;
        check(
            input_docs,
            limits.max_documents.min(u32::MAX as usize),
            "documents",
        )?;
    }
    for (index, input) in inputs.iter().enumerate() {
        checkpoint()?;
        let report = crate::verify::verify_segment(input.bytes);
        if let Some(finding) = report.findings.first() {
            return Err(MergeError::InvalidInput {
                index,
                detail: finding.to_string(),
            });
        }
        if input
            .dead
            .iter()
            .any(|tid| report.documents.binary_search(tid).is_err())
        {
            return Err(MergeError::InvalidInput {
                index,
                detail: "dead tuple absent from input document table".into(),
            });
        }
        checkpoint()?;
    }
    Ok(())
}

// Dead documents have already been fully validated. Advance past them outside
// the cross-input heap; only live candidates need ordering against other
// sources. Complete validation proved source membership; the map contains
// every live document. A missing/mismatched owner therefore denotes this
// source's dead occurrence, including a TID reused live by another source.
fn skip_dead(
    documents: &mut TidCursor<'_>,
    payload: &mut crate::payload::PayloadCursor<'_>,
    live_lengths: &HashMap<Tid, (u32, usize, u32)>,
    source: usize,
    checkpoint: &mut impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Option<(u32, u32)>, MergeError> {
    while let Some(tid) = documents.current() {
        if let Some(&(length, owner, ordinal)) = live_lengths.get(&tid)
            && owner == source
        {
            return Ok(Some((length, ordinal)));
        }
        checkpoint()?;
        payload.skip_entry()?;
        documents.advance()?;
    }
    Ok(None)
}

fn merge_inner(
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
    mut checkpoint: impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Vec<u8>, MergeError> {
    validate_inputs(inputs, limits, &mut checkpoint)?;
    let segments = inputs
        .iter()
        .map(|input| Segment::parse(input.bytes))
        .collect::<Result<Vec<_>>>()?;
    let field_count = merge_field_count(&segments)?;
    let mut docs = segments
        .iter()
        .map(Segment::documents)
        .collect::<Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::new();
    for (i, cursor) in docs.iter().enumerate() {
        if let Some(tid) = cursor.current() {
            heap.push(Reverse((tid, i)));
        }
    }
    let mut live_lengths = HashMap::new();
    let mut page_documents = Vec::new();
    let mut length_bytes = Vec::new();
    let mut class_bytes = Vec::new();
    let mut total_length = 0u64;
    while let Some(Reverse((tid, i))) = heap.pop() {
        checkpoint()?;
        if !inputs[i].dead.contains(&tid) {
            let len = segments[i].length_at(docs[i].ordinal())?;
            let ordinal = u32::try_from(live_lengths.len())
                .map_err(|_| MergeError::Limit("document count"))?;
            page_documents.push(tid);
            if live_lengths.insert(tid, (len, i, ordinal)).is_some() {
                return Err(Error::Unordered.into());
            }
            length_bytes.extend_from_slice(&len.to_le_bytes());
            class_bytes.push(crate::length_class::class_of(len));
            total_length = total_length
                .checked_add(u64::from(len))
                .ok_or(MergeError::Limit("total document length"))?;
        }
        docs[i].advance()?;
        if let Some(tid) = docs[i].current() {
            heap.push(Reverse((tid, i)));
        }
    }
    drop(docs);
    let mut sidecar = if field_count >= crate::trailer::MIN_FIELD_COUNT {
        Some(crate::trailer::Tables::new(
            field_count,
            u32::try_from(live_lengths.len()).map_err(|_| MergeError::Limit("document count"))?,
        )?)
    } else {
        None
    };
    let mut dictionaries = segments
        .iter()
        .map(|s| Ok(s.dictionary()?.iter()))
        .collect::<Result<Vec<_>>>()?;
    let mut entries = vec![None; segments.len()];
    let mut terms = BinaryHeap::new();
    for (i, iter) in dictionaries.iter_mut().enumerate() {
        if let Some(item) = iter.next() {
            let (term, entry) = item?;
            entries[i] = Some(entry);
            terms.push(Reverse((term, i)));
        }
    }
    let mut dictionary = DictionaryBuilder::default();
    let mut ordinals_area = Vec::new();
    let mut payload_area = Vec::new();
    let mut ordinals = Vec::new();
    let mut scores: Vec<(u8, u32)> = Vec::new();
    let mut positions = Vec::new();
    let mut pos_lens = Vec::new();
    let mut term_inputs = Vec::new();
    let mut cursors = Vec::new();
    let mut postings_heap = BinaryHeap::new();
    while let Some(Reverse((term, first))) = terms.pop() {
        checkpoint()?;
        term_inputs.clear();
        term_inputs.push(first);
        while terms.peek().is_some_and(|Reverse((next, _))| next == &term) {
            // The heap was just peeked and no intervening operation can empty it.
            term_inputs.push(terms.pop().expect("peeked term exists").0.1);
        }
        if field_count >= crate::trailer::MIN_FIELD_COUNT {
            merge_fielded_term(
                &segments,
                &mut term_inputs,
                &mut entries,
                &mut dictionary,
                &mut ordinals_area,
                &mut payload_area,
                &live_lengths,
                sidecar.as_mut(),
                field_count,
                &term,
                limits.max_output_bytes,
                &mut checkpoint,
            )?;
            for &i in &term_inputs {
                if let Some(item) = dictionaries[i].next() {
                    let (term, entry) = item?;
                    entries[i] = Some(entry);
                    terms.push(Reverse((term, i)));
                }
            }
            continue;
        }
        cursors.clear();
        postings_heap.clear();
        for &i in &term_inputs {
            let resolved =
                segments[i].resolve(entries[i].take().expect("each queued term owns an entry"))?;
            let mut documents = resolved.cursor()?;
            if resolved.df() == 1
                && documents.current().is_some_and(|tid| {
                    live_lengths
                        .get(&tid)
                        .is_none_or(|&(_, owner, _)| owner != i)
                })
            {
                // This fully validated source term has no surviving document.
                // No payload cursor will be consumed for it.
                checkpoint()?;
                continue;
            }
            let mut payload = resolved.payload()?.cursor();
            let length = skip_dead(
                &mut documents,
                &mut payload,
                &live_lengths,
                i,
                &mut checkpoint,
            )?;
            if let Some(tid) = documents.current() {
                postings_heap.push(Reverse((tid, cursors.len())));
            }
            cursors.push((i, documents, payload, length));
        }
        let mut payload = PayloadBuilder::default();
        let mut count = 0u32;
        let mut max_bucket = 0;
        ordinals.clear();
        scores.clear();
        pos_lens.clear();
        while let Some(Reverse((_, c))) = postings_heap.pop() {
            checkpoint()?;
            let (i, cursor, positions_cursor, length) = &mut cursors[c];
            positions.clear();
            positions_cursor.next_into(&mut positions)?;
            let bucket = cursor
                .bucket()
                .ok_or(Error::Corrupt("term member without a bucket"))?;
            let (len, ordinal) = length.ok_or(Error::Corrupt("posting missing document"))?;
            ordinals.push(ordinal);
            scores.push((bucket, len));
            payload.push(&positions)?;
            pos_lens.push(positions.len() as u32);
            count = count
                .checked_add(1)
                .ok_or(MergeError::Limit("postings count"))?;
            max_bucket = max_bucket.max(bucket);
            cursor.advance()?;
            *length = skip_dead(cursor, positions_cursor, &live_lengths, *i, &mut checkpoint)?;
            if let Some(tid) = cursor.current() {
                postings_heap.push(Reverse((tid, c)));
            }
        }
        if count != 0 {
            let ordinal_bytes = crate::ordinals::encode_scored(&ordinals, &scores);
            let payload_bytes = payload.finish();
            check(
                ordinals_area
                    .len()
                    .checked_add(payload_area.len())
                    .and_then(|n| n.checked_add(ordinal_bytes.len()))
                    .and_then(|n| n.checked_add(payload_bytes.len()))
                    .ok_or(MergeError::Limit("output bytes"))?,
                limits.max_output_bytes,
                "output bytes",
            )?;
            dictionary.push(
                &term,
                TermEntry {
                    df: count,
                    max_tf_bucket: max_bucket,
                    ordinals: Extent {
                        offset: ordinals_area.len() as u64,
                        len: u32::try_from(ordinal_bytes.len())
                            .map_err(|_| MergeError::Limit("ordinal extent"))?,
                    },
                    payload: Extent {
                        offset: payload_area.len() as u64,
                        len: u32::try_from(payload_bytes.len())
                            .map_err(|_| MergeError::Limit("payload extent"))?,
                    },
                },
            )?;
            ordinals_area.extend_from_slice(&ordinal_bytes);
            payload_area.extend_from_slice(&payload_bytes);
            if let Some(tables) = sidecar.as_mut()
                && let Ok(Some((field, token))) = crate::trailer::inspect_stored_term(&term)
            {
                for (&ordinal, &n) in ordinals.iter().zip(&pos_lens) {
                    tables.add(field, &token, ordinal, n)?;
                }
            }
        }
        for &i in &term_inputs {
            if let Some(item) = dictionaries[i].next() {
                let (term, entry) = item?;
                entries[i] = Some(entry);
                terms.push(Reverse((term, i)));
            }
        }
    }
    let dictionary = dictionary.finish();
    let offsets = crate::docs::offsets(page_documents.iter().copied());
    let pages = crate::docs::page_table(page_documents.iter().copied());
    let header = crate::segment::blob_header(
        live_lengths.len() as u32,
        total_length,
        dictionary.len(),
        ordinals_area.len(),
        payload_area.len(),
        pages.len(),
    );
    drop(page_documents);
    drop(live_lengths);
    let mut out = assemble(
        [
            header,
            dictionary,
            ordinals_area,
            payload_area,
            offsets,
            length_bytes,
            class_bytes,
            pages,
        ],
        limits.max_output_bytes,
    )?;
    if let Some(tables) = sidecar {
        let trailer = tables.encode()?;
        let total = out
            .len()
            .checked_add(trailer.len())
            .ok_or(MergeError::Limit("output bytes"))?;
        check(total, limits.max_output_bytes, "output bytes")?;
        out.try_reserve_exact(trailer.len())
            .map_err(|_| MergeError::Allocation)?;
        out.extend_from_slice(&trailer);
    }
    checkpoint()?;
    check(out.len(), limits.max_output_bytes, "output bytes")?;
    Ok(out)
}

/// Merges one surface token's per-field channel streams across inputs
/// (design §1.3/§7): each field's stock streams merge independently, the
/// union is recounted into `TermEntry.df` — channel dfs are never added —
/// and the channel directory is rewritten. Norms are recounted from the
/// live postings only.
#[allow(clippy::too_many_arguments)]
fn merge_fielded_term(
    segments: &[Segment<'_>],
    term_inputs: &mut [usize],
    entries: &mut [Option<TermEntry>],
    dictionary: &mut DictionaryBuilder,
    ordinals_area: &mut Vec<u8>,
    payload_area: &mut Vec<u8>,
    live_lengths: &HashMap<Tid, (u32, usize, u32)>,
    mut sidecar: Option<&mut crate::trailer::Tables>,
    field_count: u8,
    term: &str,
    max_output_bytes: usize,
    checkpoint: &mut impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<(), MergeError> {
    struct ChannelCursor<'a> {
        field: u8,
        input: usize,
        documents: crate::docs::TidCursor<'a>,
        payload: crate::payload::PayloadCursor<'a>,
        input_ordinal: u32,
        new_ordinal: u32,
    }
    let mut channel_cursors: Vec<ChannelCursor<'_>> = Vec::new();
    for &i in term_inputs.iter() {
        let entry = entries[i].take().expect("each queued term owns an entry");
        let resolved = segments[i].resolve(entry)?;
        for (field, child) in resolved.channels(field_count)? {
            let mut documents = child.cursor()?;
            let mut payload = child.payload()?.cursor();
            if child.df() == 1
                && documents.current().is_some_and(|tid| {
                    live_lengths
                        .get(&tid)
                        .is_none_or(|&(_, owner, _)| owner != i)
                })
            {
                // This fully validated channel has no surviving document.
                // No payload cursor will be consumed for it.
                checkpoint()?;
                continue;
            }
            let Some((_, new_ordinal)) =
                skip_dead(&mut documents, &mut payload, live_lengths, i, checkpoint)?
            else {
                continue;
            };
            let input_ordinal = documents
                .ordinal()
                .ok_or(Error::Corrupt("posting missing document"))?;
            channel_cursors.push(ChannelCursor {
                field,
                input: i,
                documents,
                payload,
                input_ordinal,
                new_ordinal,
            });
        }
    }
    channel_cursors.sort_by_key(|cursor| (cursor.field, cursor.input));
    let mut union = BTreeSet::new();
    let mut max_bucket = 0u8;
    let mut streams: Vec<crate::channels::FieldStreams> = Vec::new();
    let mut ordinals = Vec::new();
    let mut scores: Vec<(u8, u32)> = Vec::new();
    let mut positions = Vec::new();
    let at = 0;
    while at < channel_cursors.len() {
        let field = channel_cursors[at].field;
        let end = at
            + channel_cursors[at..]
                .iter()
                .position(|cursor| cursor.field != field)
                .unwrap_or(channel_cursors.len() - at);
        let mut group: Vec<ChannelCursor<'_>> = channel_cursors.drain(at..end).collect();
        let mut heap = BinaryHeap::new();
        for (index, cursor) in group.iter_mut().enumerate() {
            if let Some(tid) = cursor.documents.current() {
                heap.push(Reverse((tid, index)));
            }
        }
        let mut payload = PayloadBuilder::default();
        ordinals.clear();
        scores.clear();
        while let Some(Reverse((_, index))) = heap.pop() {
            checkpoint()?;
            let cursor = &mut group[index];
            positions.clear();
            cursor.payload.next_into(&mut positions)?;
            let bucket = cursor
                .documents
                .bucket()
                .ok_or(Error::Corrupt("term member without a bucket"))?;
            let field_length = segments[cursor.input]
                .trailer()
                .and_then(|trailer| trailer.row(cursor.input_ordinal, cursor.field))
                .ok_or(Error::Corrupt("STNF norms"))?;
            ordinals.push(cursor.new_ordinal);
            scores.push((bucket, field_length));
            payload.push(&positions)?;
            let tf = positions.len() as u32;
            if let Some(tables) = sidecar.as_deref_mut() {
                tables.add(field, term, cursor.new_ordinal, tf)?;
            }
            union.insert(cursor.new_ordinal);
            max_bucket = max_bucket.max(bucket);
            let input = cursor.input;
            cursor.documents.advance()?;
            let live = skip_dead(
                &mut cursor.documents,
                &mut cursor.payload,
                live_lengths,
                input,
                checkpoint,
            )?;
            if let Some((_, new_ordinal)) = live {
                cursor.input_ordinal = cursor
                    .documents
                    .ordinal()
                    .ok_or(Error::Corrupt("posting missing document"))?;
                cursor.new_ordinal = new_ordinal;
            }
            if let Some(tid) = cursor.documents.current() {
                heap.push(Reverse((tid, index)));
            }
        }
        if !ordinals.is_empty() {
            streams.push(crate::channels::FieldStreams {
                field,
                ordinals: crate::ordinals::encode_scored(&ordinals, &scores),
                payload: payload.finish(),
            });
        }
    }
    if union.is_empty() {
        return Ok(());
    }
    let encoded = crate::channels::encode(field_count, &streams).map_err(MergeError::Codec)?;
    check(
        ordinals_area
            .len()
            .checked_add(payload_area.len())
            .and_then(|n| n.checked_add(encoded.ordinals.len()))
            .and_then(|n| n.checked_add(encoded.payload.len()))
            .ok_or(MergeError::Limit("output bytes"))?,
        max_output_bytes,
        "output bytes",
    )?;
    dictionary
        .push(
            term,
            TermEntry {
                df: union.len() as u32,
                max_tf_bucket: max_bucket,
                ordinals: Extent {
                    offset: ordinals_area.len() as u64,
                    len: u32::try_from(encoded.ordinals.len())
                        .map_err(|_| MergeError::Limit("ordinal extent"))?,
                },
                payload: Extent {
                    offset: payload_area.len() as u64,
                    len: u32::try_from(encoded.payload.len())
                        .map_err(|_| MergeError::Limit("payload extent"))?,
                },
            },
        )
        .map_err(MergeError::Codec)?;
    ordinals_area.extend_from_slice(&encoded.ordinals);
    payload_area.extend_from_slice(&encoded.payload);
    Ok(())
}

/// Preserve the byte layout while reusing the largest allocation. Reserving a
/// separate output would retain a second complete copy of the encoded areas.
fn assemble(mut parts: [Vec<u8>; 8], limit: usize) -> std::result::Result<Vec<u8>, MergeError> {
    let mut offsets = [0; 8];
    let mut total = 0usize;
    for (offset, part) in offsets.iter_mut().zip(&parts) {
        *offset = total;
        total = total
            .checked_add(part.len())
            .ok_or(MergeError::Limit("output bytes"))?;
    }
    check(total, limit, "output bytes")?;
    let largest = (0..parts.len())
        .max_by_key(|&i| parts[i].capacity())
        .unwrap();
    let mut out = std::mem::take(&mut parts[largest]);
    let old_len = out.len();
    out.try_reserve_exact(total - old_len)
        .map_err(|_| MergeError::Allocation)?;
    out.resize(total, 0);
    out.copy_within(0..old_len, offsets[largest]);
    for (part, offset) in parts.into_iter().zip(offsets) {
        out[offset..offset + part.len()].copy_from_slice(&part);
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::segment::SegmentBuilder;

    fn limits() -> MergeLimits {
        MergeLimits {
            max_inputs: 128,
            max_input_bytes: 64 << 20,
            max_documents: 100_000,
            max_output_bytes: 64 << 20,
        }
    }

    fn inputs<'a>(blobs: &'a [Vec<u8>], dead: &'a [BTreeSet<Tid>]) -> Vec<MergeInput<'a>> {
        blobs
            .iter()
            .zip(dead)
            .map(|(bytes, dead)| MergeInput { bytes, dead })
            .collect()
    }

    /// `inputs` segments of `docs` documents over `terms` terms with up to
    /// `repeat` occurrences each; `interleaved` spreads each input's
    /// documents across the heap, and every `deletion`th document (when
    /// nonzero) is dead.
    pub(crate) fn fixture(
        inputs: usize,
        docs: u32,
        terms: u32,
        repeat: u32,
        interleaved: bool,
        deletion: u32,
    ) -> (Vec<Vec<u8>>, Vec<BTreeSet<Tid>>) {
        let mut blobs = Vec::new();
        let mut dead = Vec::new();
        for input in 0..inputs {
            let mut builder = SegmentBuilder::default();
            let mut gone = BTreeSet::new();
            for d in 0..docs {
                let n = if interleaved {
                    d * inputs as u32 + input as u32
                } else {
                    input as u32 * docs + d
                };
                let tid = Tid::new(n / 5, (n % 5 + 1) as u16).unwrap();
                let mut position = 0u32;
                let mut tokens = Vec::new();
                for t in 0..terms {
                    if (n + t) % 3 == 0 {
                        continue;
                    }
                    for _ in 0..(1 + (n + t) % repeat.max(1)) {
                        position += 1;
                        tokens.push((format!("t{t:03}"), position));
                    }
                }
                if tokens.is_empty() {
                    tokens.push(("t000".to_owned(), 1));
                }
                builder
                    .add_document(tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
                    .unwrap();
                if deletion != 0 && d % deletion == 0 {
                    gone.insert(tid);
                }
            }
            blobs.push(builder.finish());
            dead.push(gone);
        }
        (blobs, dead)
    }

    /// The merge by way of forward records, which the segment builder
    /// assembles independently of the streaming merge.
    fn reference(blobs: &[Vec<u8>], dead: &[BTreeSet<Tid>]) -> Result<Vec<u8>> {
        let mut builder = SegmentBuilder::default();
        let mut field_count = 1u8;
        for bytes in blobs {
            if let Some(trailer) = Segment::parse(bytes)?.trailer() {
                if field_count == 1 {
                    field_count = trailer.field_count;
                } else if field_count != trailer.field_count {
                    return Err(Error::Corrupt("STNF field_count"));
                }
            }
        }
        builder.set_field_count(field_count)?;
        for (bytes, dead) in blobs.iter().zip(dead) {
            for record in Segment::parse(bytes)?.records(|tid| dead.contains(&tid))? {
                builder.add_record(&record)?;
            }
        }
        Ok(builder.finish())
    }

    #[test]
    fn assembly_preserves_order_and_reuses_every_possible_area() {
        for largest in 0..8 {
            for empty_mask in 0..256 {
                let mut parts: [Vec<u8>; 8] = std::array::from_fn(|i| {
                    if empty_mask & (1 << i) != 0 {
                        Vec::new()
                    } else {
                        vec![i as u8 + 1; i * 3 + 1]
                    }
                });
                let expected = parts.concat();
                parts[largest].reserve_exact(256);
                let allocation = parts[largest].as_ptr();
                let actual = assemble(parts, expected.len()).unwrap();
                assert_eq!(actual, expected);
                assert_eq!(actual.as_ptr(), allocation);
            }
        }
        // Eight parts of 1..=8 bytes: the limit is the exact total.
        let parts = std::array::from_fn(|i| vec![i as u8; i + 1]);
        assert!(matches!(
            assemble(parts, 35),
            Err(MergeError::Limit("output bytes"))
        ));
        let parts = std::array::from_fn(|i| vec![i as u8; i + 1]);
        assert_eq!(assemble(parts.clone(), 36).unwrap(), parts.concat());
    }

    #[test]
    fn validated_merge_matches_reference() {
        for interleaved in [false, true] {
            for deletion in [0, 1, 7] {
                let (blobs, dead) = fixture(4, 65, 40, 67, interleaved, deletion);
                let merged = merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap();
                assert_eq!(merged, reference(&blobs, &dead).unwrap());
                assert!(crate::verify::verify_segment(&merged).is_clean());
            }
        }
        let empty = merge(&[], limits(), || Ok(())).unwrap();
        assert_eq!(empty, reference(&[], &[]).unwrap());
    }

    #[test]
    fn limits_reject_without_returning_partial_output() {
        let (blobs, dead) = fixture(2, 3, 4, 2, true, 0);
        let input = inputs(&blobs, &dead);
        let exact = merge(&input, limits(), || Ok(())).unwrap();
        for (limited, expected) in [
            (
                MergeLimits {
                    max_inputs: 1,
                    ..limits()
                },
                "input count",
            ),
            (
                MergeLimits {
                    max_input_bytes: blobs.iter().map(Vec::len).sum::<usize>() - 1,
                    ..limits()
                },
                "input bytes",
            ),
            (
                MergeLimits {
                    max_documents: 5,
                    ..limits()
                },
                "documents",
            ),
            (
                MergeLimits {
                    max_output_bytes: exact.len() - 1,
                    ..limits()
                },
                "output bytes",
            ),
        ] {
            assert!(
                matches!(merge(&input,limited,|| Ok(())),Err(MergeError::Limit(n)) if n == expected)
            );
        }
        assert_eq!(
            merge(
                &input,
                MergeLimits {
                    max_inputs: 2,
                    max_input_bytes: blobs.iter().map(Vec::len).sum(),
                    max_documents: 6,
                    max_output_bytes: exact.len()
                },
                || Ok(())
            )
            .unwrap(),
            exact
        );
    }

    #[test]
    fn cancellation_at_every_checkpoint_is_atomic() {
        for deletion in [0, 1, 2] {
            let (blobs, dead) = fixture(2, 3, 4, 2, true, deletion);
            let original = blobs.clone();
            let input = inputs(&blobs, &dead);
            let mut calls = 0;
            merge(&input, limits(), || {
                calls += 1;
                Ok(())
            })
            .unwrap();
            for stop in 1..=calls {
                let mut at = 0;
                assert!(matches!(
                    merge(&input, limits(), || {
                        at += 1;
                        if at == stop {
                            Err(MergeError::Canceled)
                        } else {
                            Ok(())
                        }
                    }),
                    Err(MergeError::Canceled)
                ));
            }
            assert_eq!(blobs, original);
            assert_eq!(
                merge(&input, limits(), || Ok(())).unwrap(),
                reference(&blobs, &dead).unwrap()
            );
        }
    }

    #[test]
    fn rejects_unknown_dead_tids_and_duplicate_live_tids() {
        let (mut blobs, mut dead) = fixture(1, 1, 4, 2, true, 0);
        dead[0].insert(Tid::new(42, 1).unwrap());
        assert!(matches!(
            merge(&inputs(&blobs, &dead), limits(), || Ok(())),
            Err(MergeError::InvalidInput { index: 0, .. })
        ));
        dead[0].clear();
        blobs.push(blobs[0].clone());
        dead.push(BTreeSet::new());
        assert!(matches!(
            merge(&inputs(&blobs, &dead), limits(), || Ok(())),
            Err(MergeError::Codec(Error::Unordered))
        ));
        dead[0].insert(Tid::new(0, 1).unwrap());
        assert_eq!(
            merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap(),
            reference(&blobs, &dead).unwrap()
        );
    }

    #[test]
    fn live_source_ownership_distinguishes_reused_tids_in_shared_terms() {
        let (mut blobs, _) = fixture(1, 3, 4, 2, true, 0);
        blobs.push(blobs[0].clone());
        let all_dead = crate::set::collect(Segment::parse(&blobs[0]).unwrap().documents().unwrap())
            .unwrap()
            .into_iter()
            .collect::<BTreeSet<_>>();
        for dead in [
            [all_dead.clone(), BTreeSet::new()],
            [BTreeSet::new(), all_dead.clone()],
        ] {
            assert_eq!(
                merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap(),
                reference(&blobs, &dead).unwrap()
            );
        }
    }

    fn fielded_segment(field_count: u8, docs: &[(Tid, &[&str])]) -> Vec<u8> {
        let mut builder = SegmentBuilder::default();
        builder.set_field_count(field_count).unwrap();
        for (tid, columns) in docs {
            builder.begin_fielded_document(*tid).unwrap();
            for (field, text) in columns.iter().enumerate() {
                let mut by_term: std::collections::BTreeMap<&str, Vec<u32>> =
                    std::collections::BTreeMap::new();
                let mut len = 0u32;
                for (i, word) in text.split_whitespace().enumerate() {
                    len += 1;
                    by_term.entry(word).or_default().push(i as u32 + 1);
                }
                if len == 0 {
                    continue;
                }
                for (word, positions) in by_term {
                    builder
                        .add_occurrence(word, field as u8, &positions, len)
                        .unwrap();
                }
            }
        }
        builder.finish()
    }

    /// A leftover 4.3–4.6 v1 segment: stock extents carrying fielded-key
    /// terms under a v1 trailer. Built by splicing, since no writer emits it.
    fn legacy_v1_segment(docs: &[(Tid, &[&str])]) -> Vec<u8> {
        legacy_v1_segment_with(2, docs)
    }

    fn legacy_v1_segment_with(field_count: u8, docs: &[(Tid, &[&str])]) -> Vec<u8> {
        let mut builder = SegmentBuilder::default();
        for (tid, columns) in docs {
            let mut tokens = Vec::new();
            let mut position = 0u32;
            for (field, text) in columns.iter().enumerate() {
                for word in text.split_whitespace() {
                    position += 1;
                    tokens.push((format!("~{field:x}~{word}"), position));
                }
            }
            builder
                .add_document(*tid, tokens.iter().map(|(t, p)| (t.as_str(), *p)))
                .unwrap();
        }
        let stock = builder.finish();
        // Rows and df from the texts directly: parsing the stock blob would
        // trip the fielded-key-without-trailer guard before the splice.
        let mut rows = vec![0u32; usize::from(field_count) * docs.len()];
        let mut df: std::collections::BTreeMap<String, u64> = Default::default();
        for (o, (_, columns)) in docs.iter().enumerate() {
            let mut seen = std::collections::BTreeSet::new();
            for (f, text) in columns.iter().enumerate() {
                let len = text.split_whitespace().count() as u32;
                rows[o * usize::from(field_count) + f] = len;
                for word in text.split_whitespace() {
                    seen.insert(word);
                }
            }
            for word in seen {
                *df.entry(word.to_owned()).or_insert(0) += 1;
            }
        }
        let mut totals = vec![0u64; usize::from(field_count)];
        for row in rows.chunks_exact(usize::from(field_count)) {
            for (total, cell) in totals.iter_mut().zip(row) {
                *total += u64::from(*cell);
            }
        }
        let df_rows: Vec<(String, u64)> = df.into_iter().collect();
        let df_refs: Vec<(&str, u64)> = df_rows.iter().map(|(t, d)| (t.as_str(), *d)).collect();
        let trailer = crate::trailer::encode_v1(&totals, &rows, &df_refs).unwrap();
        let mut out = stock;
        out.extend_from_slice(&trailer);
        out
    }

    #[test]
    fn merge_recounts_df_from_live_channels_not_input_dfs() {
        let tid1 = Tid::new(0, 1).unwrap();
        let tid2 = Tid::new(0, 2).unwrap();
        // A: tid1 foo in BOTH fields (live); tid2 foo in field 0 (dead in A).
        let a = fielded_segment(2, &[(tid1, &["foo", "foo"]), (tid2, &["foo", ""])]);
        // B: tid2 foo in field 0, live (dead-CTID reuse is allowed).
        let b = fielded_segment(2, &[(tid2, &["foo", ""])]);
        let a_parse = Segment::parse(&a).unwrap();
        let b_parse = Segment::parse(&b).unwrap();
        let a_foo = a_parse.term("foo").unwrap().unwrap();
        let b_foo = b_parse.term("foo").unwrap().unwrap();
        assert_eq!(a_foo.df(), 2, "input A counts the dead ordinal");
        assert_eq!(b_foo.df(), 1);
        assert_eq!(
            a_foo.df() + b_foo.df(),
            3,
            "the sum the merge must not write"
        );

        let dead = [BTreeSet::from([tid2]), BTreeSet::new()];
        let blobs = [a, b];
        let merged = merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap();
        let segment = Segment::parse(&merged).unwrap();
        let foo = segment.term("foo").unwrap().unwrap();
        assert_eq!(
            foo.df(),
            2,
            "union over live docs, not the sum of channel or input dfs"
        );
        let channels = foo.channels(2).unwrap();
        let per_field: Vec<(u8, u32)> = channels
            .iter()
            .map(|(field, child)| (*field, child.df()))
            .collect();
        assert_eq!(
            per_field,
            vec![(0, 2), (1, 1)],
            "field dfs sum to 3 ≠ union 2"
        );
        let trailer = segment.trailer().unwrap();
        assert_eq!(trailer.version, 2);
        assert_eq!(trailer.row(0, 0), Some(1), "tid1 title length");
        assert_eq!(trailer.row(0, 1), Some(1), "tid1 body length");
        assert_eq!(trailer.rows.len(), 4, "two live docs × two fields");
        assert!(trailer.df_agg.is_empty());
        assert!(crate::verify::verify_segment(&merged).is_clean());
    }

    #[test]
    fn merge_recounts_union_df_when_fields_overlap() {
        // A token posting in both fields of one document counts once.
        let tid1 = Tid::new(0, 1).unwrap();
        let tid2 = Tid::new(0, 2).unwrap();
        let tid3 = Tid::new(0, 3).unwrap();
        let a = fielded_segment(
            2,
            &[
                (tid1, &["needle", "needle"]),
                (tid2, &["needle", "pad"]),
                (tid3, &["", "needle"]),
            ],
        );
        let merged = merge(&inputs(&[a], &[BTreeSet::new()]), limits(), || Ok(())).unwrap();
        let segment = Segment::parse(&merged).unwrap();
        let needle = segment.term("needle").unwrap().unwrap();
        assert_eq!(needle.df(), 3, "union df == 3, not 4");
        let channels = needle.channels(2).unwrap();
        let per_field: Vec<(u8, u32)> = channels
            .iter()
            .map(|(field, child)| (*field, child.df()))
            .collect();
        assert_eq!(per_field, vec![(0, 2), (1, 2)]);
        assert_eq!(per_field.iter().map(|(_, df)| df).sum::<u32>(), 4);
        assert!(crate::verify::verify_segment(&merged).is_clean());
    }

    #[test]
    fn merge_all_dead_multi_column_keeps_empty_trailer() {
        let tid = Tid::new(0, 1).unwrap();
        let blob = fielded_segment(2, &[(tid, &["keep", "gone"])]);
        let dead = [BTreeSet::from([tid])];
        let blobs = [blob];
        let merged = merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap();
        let segment = Segment::parse(&merged).unwrap();
        assert_eq!(segment.document_count(), 0);
        let trailer = segment.trailer().unwrap();
        assert_eq!(trailer.version, 2);
        assert_eq!(trailer.field_count, 2);
        assert!(trailer.rows.is_empty());
        assert!(crate::verify::verify_segment(&merged).is_clean());
    }

    #[test]
    fn merge_still_rejects_legacy_v1_encoded_term_segments() {
        let tid1 = Tid::new(0, 1).unwrap();
        let tid2 = Tid::new(0, 2).unwrap();
        let a = legacy_v1_segment(&[(tid1, &["foo", "foo"]), (tid2, &["foo"])]);
        let b = legacy_v1_segment(&[(tid2, &["foo"])]);
        let dead = [BTreeSet::from([tid2]), BTreeSet::new()];
        let blobs = [a, b];
        // Input validation refuses the v1 extents before any merge logic:
        // multi-column verify requires FCH1, and the field-count gate inside
        // merge would refuse the withdrawn version besides.
        let rejected = merge(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap_err();
        assert!(
            rejected.to_string().contains("channel magic"),
            "v1 extents carry no FCH1 directory: {rejected}"
        );
    }
}
