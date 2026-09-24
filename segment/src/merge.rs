// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Validated direct posting merges, independent of PostgreSQL publication.
//!
//! Inputs are borrowed complete blobs with per-input dead sets. All inputs,
//! including dead documents, are verified before metadata is reused. The output
//! uses the current format; old formats remain readable. No index page is written.
//! Foreground PostgreSQL merges retain the metadata lock; VACUUM merges owned
//! snapshots unlocked and revalidates before publication. Page allocation, WAL
//! and publication remain the caller’s responsibility.
use crate::dictionary::{DictionaryBuilder, Extent, TermEntry};
use crate::payload::PayloadBuilder;
use crate::postings::PostingsBuilder;
#[cfg(test)]
use crate::segment::SegmentBuilder;
use crate::segment::{AreaFetch, Format, Segment};
use crate::set::Cursor;
use crate::{Error, Result, Tid, varint};
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
/// between input validations, documents, terms and postings; a single validation
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
    #[error("merge cancelled")]
    Cancelled,
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
    merge_as(inputs, limits, Format::CURRENT, checkpoint)
}

/// Validate and merge field-aware (`LSG4`) segments, preserving per-field
/// lengths and field-tagged payload groups. Every input must be `LSG4` with
/// the same field count: an index never mixes `LSG3` and `LSG4` segments,
/// and a merge across the two is refused with a clear error instead of
/// silently flattening the field dimension. Merging zero inputs yields an
/// empty `LSG4` segment of one field (valid on disk, RFC §5.8).
pub fn merge_fields(
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
    checkpoint: impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Vec<u8>, MergeError> {
    merge_as(inputs, limits, Format::Lsg4, checkpoint)
}

fn check(actual: usize, limit: usize, name: &'static str) -> std::result::Result<(), MergeError> {
    if actual > limit {
        Err(MergeError::Limit(name))
    } else {
        Ok(())
    }
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
        for finding in &report.findings {
            let legacy_notice = finding.severity == crate::verify::Severity::Warning
                && finding.location == "header"
                && finding.message
                    == "LSG1 segment: ranked scans over it score every candidate; REINDEX to upgrade";
            if !legacy_notice {
                return Err(MergeError::InvalidInput {
                    index,
                    detail: finding.to_string(),
                });
            }
        }
        let mut document_at = 0;
        if input.dead.iter().any(|tid| {
            crate::verify::ordered_rank(&report.documents, &mut document_at, *tid).is_none()
        }) {
            return Err(MergeError::InvalidInput {
                index,
                detail: "dead tuple absent from input document table".into(),
            });
        }
        checkpoint()?;
    }
    Ok(())
}

// Dead postings have already been fully validated. Advance them outside the
// cross-input heap; only live candidates need ordering against other sources.
// Complete validation proved source membership; the map contains every live
// document. A missing/mismatched owner therefore denotes this source's dead
// occurrence, including a TID reused live by another source.
fn skip_dead(
    postings: &mut crate::postings::PostingsCursor<'_>,
    payload: &mut crate::payload::PayloadCursor<'_>,
    live_lengths: &HashMap<Tid, (u32, usize)>,
    source: usize,
    checkpoint: &mut impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Option<u32>, MergeError> {
    while let Some(tid) = postings.current() {
        if let Some(&(length, owner)) = live_lengths.get(&tid)
            && owner == source
        {
            return Ok(Some(length));
        }
        checkpoint()?;
        payload.next_bucket()?;
        postings.advance()?;
    }
    Ok(None)
}

/// The field-aware sibling of [`skip_dead`]: live documents carry their full
/// per-field length row, and dead `LSG4` payload entries are skipped through
/// the validating field decoder, not the single-bucket one.
fn skip_dead_fields(
    postings: &mut crate::postings::PostingsCursor<'_>,
    payload: &mut crate::payload::PayloadCursor<'_>,
    live_rows: &HashMap<Tid, (Vec<u32>, usize)>,
    source: usize,
    checkpoint: &mut impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Option<Vec<u32>>, MergeError> {
    while let Some(tid) = postings.current() {
        if let Some((row, owner)) = live_rows.get(&tid)
            && *owner == source
        {
            return Ok(Some(row.clone()));
        }
        checkpoint()?;
        payload.skip_fields()?;
        postings.advance()?;
    }
    Ok(None)
}

fn merge_as(
    inputs: &[MergeInput<'_>],
    limits: MergeLimits,
    format: Format,
    mut checkpoint: impl FnMut() -> std::result::Result<(), MergeError>,
) -> std::result::Result<Vec<u8>, MergeError> {
    validate_inputs(inputs, limits, &mut checkpoint)?;
    let segments = inputs
        .iter()
        .map(|input| Segment::parse(input.bytes))
        .collect::<Result<Vec<_>>>()?;
    let fields = format.has_fields();
    // An index never mixes field-aware and fieldless segments: refuse a merge
    // whose inputs disagree with the output about the field dimension, and
    // one whose field-aware inputs disagree about the field count.
    for (index, segment) in segments.iter().enumerate() {
        if segment.format().has_fields() != fields {
            return Err(MergeError::InvalidInput {
                index,
                detail: format!("cannot merge {} and {} segments", segment.format(), format),
            });
        }
    }
    let field_count = segments.first().map_or(1, |segment| segment.field_count());
    if fields
        && segments
            .iter()
            .any(|segment| segment.field_count() != field_count)
    {
        return Err(MergeError::InvalidInput {
            index: 0,
            detail: format!(
                "cannot merge segments of differing field counts ({field_count} and others)"
            ),
        });
    }
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
    let mut live_field_rows = HashMap::new();
    let mut doc_builder = PostingsBuilder::default();
    let mut length_bytes = Vec::new();
    let mut total_length = 0u64;
    let mut field_total = vec![0u64; usize::from(field_count)];
    while let Some(Reverse((tid, i))) = heap.pop() {
        checkpoint()?;
        if !inputs[i].dead.contains(&tid) {
            doc_builder.push(tid)?;
            if fields {
                let ordinal = docs[i].ordinal();
                let row = (0..field_count)
                    .map(|field| segments[i].field_length(ordinal, field))
                    .collect::<Result<Vec<u32>>>()?;
                if live_field_rows.insert(tid, (row.clone(), i)).is_some() {
                    return Err(Error::Unordered.into());
                }
                for (field, len) in row.iter().enumerate() {
                    length_bytes.extend_from_slice(&len.to_le_bytes());
                    field_total[field] = field_total[field]
                        .checked_add(u64::from(*len))
                        .ok_or(MergeError::Limit("total document length"))?;
                }
            } else {
                let len = segments[i].length_at(docs[i].ordinal())?;
                if live_lengths.insert(tid, (len, i)).is_some() {
                    return Err(Error::Unordered.into());
                }
                length_bytes.extend_from_slice(&len.to_le_bytes());
                total_length = total_length
                    .checked_add(u64::from(len))
                    .ok_or(MergeError::Limit("total document length"))?;
            }
        }
        docs[i].advance()?;
        if let Some(tid) = docs[i].current() {
            heap.push(Reverse((tid, i)));
        }
    }
    if fields {
        total_length = field_total
            .iter()
            .try_fold(0u64, |sum, len| sum.checked_add(*len))
            .ok_or(MergeError::Limit("total document length"))?;
    }
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
    let mut dictionary = DictionaryBuilder::with_format(format);
    let mut postings_area = Vec::new();
    let mut payload_area = Vec::new();
    let mut positions = Vec::new();
    let mut term_inputs = Vec::new();
    let mut cursors = Vec::new();
    // Field rows aligned with `cursors`, for `LSG4` merges.
    let mut rows: Vec<Option<Vec<u32>>> = Vec::new();
    let mut postings_heap = BinaryHeap::new();
    while let Some(Reverse((term, first))) = terms.pop() {
        checkpoint()?;
        term_inputs.clear();
        term_inputs.push(first);
        while terms.peek().is_some_and(|Reverse((next, _))| next == &term) {
            // The heap was just peeked and no intervening operation can empty it.
            term_inputs.push(terms.pop().expect("peeked term exists").0.1);
        }
        cursors.clear();
        postings_heap.clear();
        rows.clear();
        for &i in &term_inputs {
            let resolved =
                segments[i].resolve(entries[i].take().expect("each queued term owns an entry"))?;
            let mut postings = resolved.cursor()?;
            if resolved.df() == 1
                && postings.current().is_some_and(|tid| {
                    if fields {
                        live_field_rows
                            .get(&tid)
                            .is_none_or(|&(_, owner)| owner != i)
                    } else {
                        live_lengths.get(&tid).is_none_or(|&(_, owner)| owner != i)
                    }
                })
            {
                // This fully validated source term has no surviving posting.
                // No payload cursor will be consumed for it.
                checkpoint()?;
                continue;
            }
            let mut payload = resolved.payload()?.cursor();
            let length = if fields {
                let row = skip_dead_fields(
                    &mut postings,
                    &mut payload,
                    &live_field_rows,
                    i,
                    &mut checkpoint,
                )?;
                rows.push(row);
                None
            } else {
                skip_dead(
                    &mut postings,
                    &mut payload,
                    &live_lengths,
                    i,
                    &mut checkpoint,
                )?
            };
            if let Some(tid) = postings.current() {
                postings_heap.push(Reverse((tid, cursors.len())));
            }
            cursors.push((i, postings, payload, length));
        }
        let mut postings = PostingsBuilder::default();
        let mut payload = PayloadBuilder::default();
        let mut count = 0u32;
        let mut max_bucket = 0;
        while let Some(Reverse((tid, c))) = postings_heap.pop() {
            checkpoint()?;
            let (i, cursor, positions_cursor, length) = &mut cursors[c];
            if fields {
                let field_entry = positions_cursor.next_fields()?;
                let row = rows[c]
                    .as_ref()
                    .ok_or(Error::Corrupt("posting missing document"))?;
                let scores: Vec<(u8, u8)> = field_entry
                    .fields
                    .iter()
                    .map(|hit| (hit.field, hit.tf_bucket))
                    .collect();
                let groups: Vec<(u8, u8, &[u32])> = field_entry
                    .fields
                    .iter()
                    .map(|hit| (hit.field, hit.tf_bucket, hit.positions.as_slice()))
                    .collect();
                postings.push_scored_fields(tid, &scores, row)?;
                payload.push_fields(&groups, field_count)?;
                count = count
                    .checked_add(1)
                    .ok_or(MergeError::Limit("postings count"))?;
                max_bucket = max_bucket.max(
                    field_entry
                        .fields
                        .iter()
                        .map(|hit| hit.tf_bucket)
                        .max()
                        .unwrap_or(0),
                );
            } else {
                positions.clear();
                let bucket = positions_cursor.next_into(&mut positions)?;
                let len = length.ok_or(Error::Corrupt("posting missing document"))?;
                postings.push_scored(tid, bucket, len)?;
                payload.push(bucket, &positions)?;
                count = count
                    .checked_add(1)
                    .ok_or(MergeError::Limit("postings count"))?;
                max_bucket = max_bucket.max(bucket);
            }
            cursor.advance()?;
            if fields {
                rows[c] = skip_dead_fields(
                    cursor,
                    positions_cursor,
                    &live_field_rows,
                    *i,
                    &mut checkpoint,
                )?;
            } else {
                *length = skip_dead(cursor, positions_cursor, &live_lengths, *i, &mut checkpoint)?;
            }
            if let Some(tid) = cursor.current() {
                postings_heap.push(Reverse((tid, c)));
            }
        }
        if count != 0 {
            let posting_bytes = postings.finish_as(format);
            let payload_bytes = payload.finish_as(format);
            check(
                postings_area
                    .len()
                    .checked_add(payload_area.len())
                    .and_then(|n| n.checked_add(posting_bytes.len()))
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
                    postings: Extent {
                        offset: postings_area.len() as u64,
                        len: u32::try_from(posting_bytes.len())
                            .map_err(|_| MergeError::Limit("posting extent"))?,
                    },
                    payload: Extent {
                        offset: payload_area.len() as u64,
                        len: u32::try_from(payload_bytes.len())
                            .map_err(|_| MergeError::Limit("payload extent"))?,
                    },
                },
            )?;
            postings_area.extend_from_slice(&posting_bytes);
            payload_area.extend_from_slice(&payload_bytes);
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
    let documents = doc_builder.finish();
    let live_documents = if fields {
        live_field_rows.len()
    } else {
        live_lengths.len()
    };
    let mut header = Vec::new();
    header.extend_from_slice(format.magic());
    if fields {
        // RFC §5.1: revision, doc_count, total_length, field_count,
        // field_total u64le × field_count, then the area lengths.
        varint::put(&mut header, 1);
    }
    varint::put(&mut header, live_documents as u64);
    varint::put(&mut header, total_length);
    if fields {
        varint::put(&mut header, u64::from(field_count));
        for total in &field_total {
            header.extend_from_slice(&total.to_le_bytes());
        }
    }
    for n in [
        dictionary.len() as u64,
        postings_area.len() as u64,
        payload_area.len() as u64,
        documents.len() as u64,
    ] {
        varint::put(&mut header, n);
    }
    drop(live_lengths);
    drop(live_field_rows);
    let out = assemble(
        [
            header,
            dictionary,
            postings_area,
            payload_area,
            documents,
            length_bytes,
        ],
        limits.max_output_bytes,
    )?;
    checkpoint()?;
    check(out.len(), limits.max_output_bytes, "output bytes")?;
    Ok(out)
}

/// Preserve the byte layout while reusing the largest allocation. Reserving a
/// separate output would retain a second complete copy of the encoded areas.
fn assemble(mut parts: [Vec<u8>; 6], limit: usize) -> std::result::Result<Vec<u8>, MergeError> {
    let mut offsets = [0; 6];
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
mod tests {
    use super::*;
    use crate::direct_merge_poc::{fixture, reference};

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

    #[test]
    fn assembly_preserves_order_and_reuses_every_possible_area() {
        for largest in 0..6 {
            for empty_mask in 0..64 {
                let mut parts: [Vec<u8>; 6] = std::array::from_fn(|i| {
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
        let parts = std::array::from_fn(|i| vec![i as u8; i + 1]);
        assert!(matches!(
            assemble(parts, 20),
            Err(MergeError::Limit("output bytes"))
        ));
        let parts = std::array::from_fn(|i| vec![i as u8; i + 1]);
        assert_eq!(assemble(parts.clone(), 21).unwrap(), parts.concat());
    }

    #[test]
    fn validated_merge_matches_reference_for_all_formats() {
        for interleaved in [false, true] {
            for deletion in [0, 1, 7] {
                let (blobs, dead) = fixture(3, 65, 40, 67, interleaved, deletion);
                for format in [Format::Lsg1, Format::Lsg2, Format::Lsg3] {
                    assert_eq!(
                        merge_as(&inputs(&blobs, &dead), limits(), format, || Ok(())).unwrap(),
                        reference(&blobs, &dead, format).unwrap()
                    );
                }
            }
        }
        let empty = merge(&[], limits(), || Ok(())).unwrap();
        assert_eq!(empty, reference(&[], &[], Format::CURRENT).unwrap());
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
                            Err(MergeError::Cancelled)
                        } else {
                            Ok(())
                        }
                    }),
                    Err(MergeError::Cancelled)
                ));
            }
            assert_eq!(blobs, original);
            assert_eq!(
                merge(&input, limits(), || Ok(())).unwrap(),
                reference(&blobs, &dead, Format::CURRENT).unwrap()
            );
        }
    }

    #[test]
    fn field_merges_preserve_field_lengths_and_refuse_mixed_formats() {
        for interleaved in [false, true] {
            for deletion in [0, 1, 7] {
                let (blobs, dead) = crate::direct_merge_poc::fixture_fields(
                    3,
                    65,
                    40,
                    67,
                    interleaved,
                    deletion,
                    3,
                );
                let merged = merge_fields(&inputs(&blobs, &dead), limits(), || Ok(())).unwrap();
                assert_eq!(
                    merged,
                    crate::direct_merge_poc::reference_fields(&blobs, &dead, 3).unwrap(),
                    "interleaved={interleaved} deletion={deletion}"
                );
                let report = crate::verify::verify_segment(&merged);
                assert!(report.is_clean(), "{report:?}");
                let segment = Segment::parse(&merged).unwrap();
                assert_eq!(segment.format(), Format::Lsg4);
                assert_eq!(segment.field_count(), 3);
                if deletion != 1 {
                    // deletion == 1 marks every document dead; the empty
                    // output keeps the field count but all totals are zero.
                    for field in 0..3 {
                        assert!(segment.field_total(field).unwrap() > 0);
                    }
                }
            }
        }
        // LSG3 × LSG4 is refused in both directions: the field merge refuses
        // fieldless inputs and the fieldless merge refuses field inputs.
        let (lsg4, dead4) = crate::direct_merge_poc::fixture_fields(1, 5, 6, 4, false, 0, 2);
        let (lsg3, dead3) = crate::direct_merge_poc::fixture(1, 5, 6, 4, false, 0);
        let mixed = [
            MergeInput {
                bytes: &lsg4[0],
                dead: &dead4[0],
            },
            MergeInput {
                bytes: &lsg3[0],
                dead: &dead3[0],
            },
        ];
        for result in [
            merge_fields(&mixed, limits(), || Ok(())),
            merge(&mixed, limits(), || Ok(())),
        ] {
            let Err(MergeError::InvalidInput { detail, .. }) = result else {
                panic!("a mixed LSG3/LSG4 merge was accepted");
            };
            assert!(detail.contains("cannot merge"), "{detail}");
        }
        // Field-count disagreement between LSG4 inputs is refused too.
        let (two, dead_two) = crate::direct_merge_poc::fixture_fields(1, 5, 6, 4, false, 0, 2);
        let (three, dead_three) = crate::direct_merge_poc::fixture_fields(1, 5, 6, 4, false, 0, 3);
        let mismatched = [
            MergeInput {
                bytes: &two[0],
                dead: &dead_two[0],
            },
            MergeInput {
                bytes: &three[0],
                dead: &dead_three[0],
            },
        ];
        assert!(matches!(
            merge_fields(&mismatched, limits(), || Ok(())),
            Err(MergeError::InvalidInput { .. })
        ));
        // Merging nothing yields the empty one-field LSG4 blob.
        let empty = merge_fields(&[], limits(), || Ok(())).unwrap();
        assert_eq!(empty, SegmentBuilder::with_field_count(1).finish_fields());
        assert_eq!(Segment::parse(&empty).unwrap().field_count(), 1);
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
            reference(&blobs, &dead, Format::CURRENT).unwrap()
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
                reference(&blobs, &dead, Format::CURRENT).unwrap()
            );
        }
    }

    #[test]
    fn corrupt_lengths_are_rejected_even_for_dead_documents() {
        let (mut blobs, mut dead) = fixture(1, 1, 4, 2, true, 0);
        // Length table is the final u32 for the only document. Its header total
        // and actual positions still say four, so trusting it would corrupt BM25.
        let n = blobs[0].len();
        blobs[0][n - 4..].copy_from_slice(&5u32.to_le_bytes());
        for deleted in [false, true] {
            if deleted {
                dead[0].insert(Tid::new(0, 1).unwrap());
            }
            assert!(matches!(
                merge(&inputs(&blobs, &dead), limits(), || Ok(())),
                Err(MergeError::InvalidInput { index: 0, .. })
            ));
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(32))]
        #[test]
        fn validated_merge_generated_equivalence(parts in 1usize..6,docs in 0usize..40,tokens in 1usize..40,vocab in 1usize..50,deletion in 0usize..8,interleaved in proptest::bool::ANY) {
            let (blobs,dead)=fixture(parts,docs,tokens,vocab,interleaved,deletion);
            let output=merge(&inputs(&blobs,&dead),limits(),|| Ok(())).unwrap();
            proptest::prop_assert_eq!(output,reference(&blobs,&dead,Format::CURRENT).unwrap());
        }
        #[test]
        fn validated_field_merge_generated_equivalence(parts in 1usize..6,docs in 0usize..40,tokens in 1usize..40,vocab in 1usize..50,deletion in 0usize..8,interleaved in proptest::bool::ANY,fields in 1u8..=4) {
            let (blobs,dead)=crate::direct_merge_poc::fixture_fields(parts,docs,tokens,vocab,interleaved,deletion,fields);
            let output=merge_fields(&inputs(&blobs,&dead),limits(),|| Ok(())).unwrap();
            proptest::prop_assert_eq!(output,crate::direct_merge_poc::reference_fields(&blobs,&dead,fields).unwrap());
        }
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(proptest::num::u8::ANY,0..1024)) {
            let dead=BTreeSet::new();
            let _=merge(&[MergeInput { bytes:&bytes,dead:&dead }],limits(),|| Ok(()));
            let _=merge_fields(&[MergeInput { bytes:&bytes,dead:&dead }],limits(),|| Ok(()));
        }
    }

    #[test]
    fn mutated_valid_inputs_never_emit_unverified_output() {
        let (blobs, dead) = fixture(1, 4, 8, 3, true, 0);
        for i in 0..blobs[0].len() {
            let mut changed = blobs[0].clone();
            changed[i] ^= 1;
            if let Ok(output) = merge(
                &[MergeInput {
                    bytes: &changed,
                    dead: &dead[0],
                }],
                limits(),
                || Ok(()),
            ) {
                assert!(
                    crate::verify::verify_segment(&output).is_clean(),
                    "byte {i}"
                );
            }
        }
    }

    #[test]
    #[ignore = "release timing includes full validation; not database throughput"]
    fn hardened_merge_microprobe() {
        use std::time::Instant;
        for (docs, tokens, vocab) in [(32, 40, 4), (512, 400, 4), (512, 400, 400)] {
            let (blobs, dead) = fixture(8, docs, tokens, vocab, true, 7);
            let input = inputs(&blobs, &dead);
            let expected = reference(&blobs, &dead, Format::CURRENT).unwrap();
            let mut times = [Vec::new(), Vec::new()];
            for round in 0..10 {
                for method in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                    let start = Instant::now();
                    let output = if method == 0 {
                        reference(&blobs, &dead, Format::CURRENT).unwrap()
                    } else {
                        merge(&input, limits(), || Ok(())).unwrap()
                    };
                    let elapsed = start.elapsed().as_secs_f64() * 1000.;
                    assert_eq!(output, expected);
                    if round >= 2 {
                        times[method].push(elapsed);
                    }
                }
            }
            for samples in &mut times {
                samples.sort_by(f64::total_cmp);
            }
            println!(
                "8x{docs}, tokens={tokens}, vocab={vocab}: reference {:.3} ms, validated merge {:.3} ms; samples {:?}",
                (times[0][3] + times[0][4]) / 2.,
                (times[1][3] + times[1][4]) / 2.,
                times
            );
        }
    }
}
