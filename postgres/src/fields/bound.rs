// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Fused WAND bound: §5.1 derivative of `saturate`, not a scaled stock bound.
//!
//! ```text
//! max_tf*  = Σ_{f ∈ mask ∩ present} w_f · representative_count(max_tf_bucket_f)
//! min_len* = MIN over mask ∩ present of w_f · shortest_f
//! bound    = saturate(max_tf*, min_len*)
//! ```
//!
//! Bound-time dequantize is `TfBucket::new(max_tf_bucket()).representative_count()`.
//! Exact `fused_tf` still uses `from_count` on raw position counts.

use segment::bound::BlockBound;
use segment::ordinals::{CHUNK, ChunkBound, Ordinals, SUB, SUBS};
use segment::tf_bucket::TfBucket;

use super::score::saturate;
use super::types::LogicalTerm;
use crate::bm25::Bm25Params;

/// One field's envelope over blocks that intersect a fused interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FieldEnvelope {
    pub(crate) max_tf_bucket: u8,
    pub(crate) shortest: u32,
}

impl FieldEnvelope {
    fn merge(self, other: Self) -> Self {
        Self {
            max_tf_bucket: self.max_tf_bucket.max(other.max_tf_bucket),
            shortest: self.shortest.min(other.shortest),
        }
    }
}

fn consider(cut: &mut Option<u32>, pivot: u32, value: u32) {
    if value > pivot {
        *cut = Some(cut.map_or(value, |seen| seen.min(value)));
    }
}

fn next_aligned_end(pivot: u32, align: u32) -> Option<u32> {
    let start = (pivot / align).saturating_mul(align);
    start.checked_add(align)
}

fn block_from(bound: &ChunkBound) -> BlockBound {
    BlockBound {
        min_len: bound.min_len,
    }
}

/// Exclusive end of the fused bound interval covering `pivot`.
///
/// The end is the minimum of covering-block exclusive ends and the next
/// block, chunk, or sub-block start of any mask-internal stream — including
/// a list stream that does not yet cover the pivot, whose next start is its
/// first ordinal (the 0..1000 / 500 witness; 500 need not be a `SUB` multiple).
#[must_use]
pub(crate) fn next_interval_end(streams: &[&Ordinals<'_>], pivot: u32) -> Option<u32> {
    let mut end = None;
    for stream in streams {
        stream_cuts(stream, pivot, &mut end);
    }
    end
}

fn stream_cuts(stream: &Ordinals<'_>, pivot: u32, cut: &mut Option<u32>) {
    if let Some(list) = stream.list() {
        list_cuts(list, pivot, cut);
        return;
    }
    chunked_cuts(stream, pivot, cut);
}

fn list_cuts(list: &[u32], pivot: u32, cut: &mut Option<u32>) {
    let Some((&first, rest)) = list.split_first() else {
        return;
    };
    let last = rest.last().copied().unwrap_or(first);
    if first > pivot {
        // Not yet covering: the list is one block starting at the first
        // ordinal, not at an aligned SUB.
        consider(cut, pivot, first);
        return;
    }
    if pivot > last {
        return;
    }
    consider(cut, pivot, last.saturating_add(1));
    if let Some(sub_end) = next_aligned_end(pivot, SUB) {
        consider(cut, pivot, sub_end);
    }
    if let Some(chunk_end) = next_aligned_end(pivot, CHUNK) {
        consider(cut, pivot, chunk_end);
    }
    for &ordinal in list {
        let sub_start = (ordinal / SUB).saturating_mul(SUB);
        consider(cut, pivot, sub_start);
    }
}

fn chunked_cuts(stream: &Ordinals<'_>, pivot: u32, cut: &mut Option<u32>) {
    for i in 0..stream.chunk_count() {
        let key = stream.chunk_key(i);
        let chunk_start = u32::from(key) << 16;
        let chunk_end = chunk_start.saturating_add(CHUNK);
        if chunk_end <= pivot {
            continue;
        }
        let bound = stream.chunk_bound(i).ok().flatten();
        if chunk_start > pivot {
            consider(cut, pivot, chunk_start);
            if let Some(bound) = bound {
                for (sub, &slot) in bound.subs.iter().enumerate() {
                    if slot == 0 {
                        continue;
                    }
                    consider(
                        cut,
                        pivot,
                        chunk_start.saturating_add((sub as u32).saturating_mul(SUB)),
                    );
                    break;
                }
            }
            return;
        }
        consider(cut, pivot, chunk_end);
        if let Some(sub_end) = next_aligned_end(pivot, SUB) {
            consider(cut, pivot, sub_end.min(chunk_end));
        }
        let Some(bound) = bound else {
            return;
        };
        let local = pivot - chunk_start;
        let current_sub = (local / SUB) as usize;
        for sub in current_sub..SUBS {
            if bound.subs[sub] == 0 {
                continue;
            }
            let sub_start = chunk_start.saturating_add((sub as u32).saturating_mul(SUB));
            if sub_start > pivot {
                consider(cut, pivot, sub_start);
            }
            let sub_end = sub_start.saturating_add(SUB).min(chunk_end);
            consider(cut, pivot, sub_end);
            if sub > current_sub {
                break;
            }
        }
        return;
    }
}

/// Per-field envelopes for every block that intersects `[start, end)`.
///
/// `Err` means a stored bound could not be read. The interval must not prune:
/// skipping that block can drop `fused_bound` below `exact_score`. `Ok(None)`
/// on a field is an empty / non-intersecting block and stays a skip.
pub(crate) fn interval_envelopes(
    streams: &[(u8, &Ordinals<'_>)],
    start: u32,
    end: u32,
    field_count: u8,
) -> segment::Result<Vec<Option<FieldEnvelope>>> {
    let n = usize::from(field_count.min(16));
    let mut present: Vec<Option<FieldEnvelope>> = vec![None; n];
    for &(field, stream) in streams {
        let index = usize::from(field);
        if index >= n {
            continue;
        }
        let Some(env) = field_envelope(stream, start, end)? else {
            continue;
        };
        present[index] = Some(match present[index] {
            Some(old) => old.merge(env),
            None => env,
        });
    }
    Ok(present)
}

fn field_envelope(
    stream: &Ordinals<'_>,
    start: u32,
    end: u32,
) -> segment::Result<Option<FieldEnvelope>> {
    if end <= start {
        return Ok(None);
    }
    match stream.list() {
        Some(list) => list_envelope(stream, list, start, end),
        None => chunked_envelope(stream, start, end),
    }
}

fn list_envelope(
    stream: &Ordinals<'_>,
    list: &[u32],
    start: u32,
    end: u32,
) -> segment::Result<Option<FieldEnvelope>> {
    let Some((&first, rest)) = list.split_first() else {
        return Ok(None);
    };
    let last = rest.last().copied().unwrap_or(first);
    let list_end = last.saturating_add(1);
    if list_end <= start || first >= end {
        return Ok(None);
    }
    match stream.chunk_bound(0)? {
        Some(bound) => Ok(envelope_from_bound(
            bound,
            first.max(start),
            list_end.min(end),
        )),
        None => Ok(None),
    }
}

fn chunked_envelope(
    stream: &Ordinals<'_>,
    start: u32,
    end: u32,
) -> segment::Result<Option<FieldEnvelope>> {
    let mut acc: Option<FieldEnvelope> = None;
    for i in 0..stream.chunk_count() {
        let key = stream.chunk_key(i);
        let chunk_start = u32::from(key) << 16;
        let chunk_end = chunk_start.saturating_add(CHUNK);
        if chunk_end <= start || chunk_start >= end {
            continue;
        }
        let Some(bound) = stream.chunk_bound(i)? else {
            continue;
        };
        let Some(env) = envelope_from_bound(bound, chunk_start.max(start), chunk_end.min(end))
        else {
            continue;
        };
        acc = Some(match acc {
            Some(old) => old.merge(env),
            None => env,
        });
    }
    Ok(acc)
}

fn envelope_from_bound(
    bound: &ChunkBound,
    span_start: u32,
    span_end: u32,
) -> Option<FieldEnvelope> {
    if span_end <= span_start {
        return None;
    }
    let shortest = block_from(bound).shortest();
    if shortest == u32::MAX {
        return None;
    }
    let mut max_bucket: Option<u8> = None;
    let mut at = span_start;
    while at < span_end {
        let chunk_start = (at >> 16) << 16;
        let local = at - chunk_start;
        let sub = (local / SUB) as usize;
        if sub < SUBS && bound.subs[sub] > 0 {
            let bucket = bound.subs[sub] - 1;
            max_bucket = Some(max_bucket.map_or(bucket, |seen| seen.max(bucket)));
        }
        let next = chunk_start
            .saturating_add((sub as u32 + 1).saturating_mul(SUB))
            .min(span_end);
        if next <= at {
            break;
        }
        at = next;
    }
    let max_tf_bucket = max_bucket.unwrap_or_else(|| block_from(bound).max_tf_bucket());
    Some(FieldEnvelope {
        max_tf_bucket,
        shortest,
    })
}

/// `saturate(max_tf*, min_len*)` with the same avgdl*/idf/boost/params as exact.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub(crate) fn fused_bound(
    mask: u16,
    weights: &[f32],
    present: &[Option<FieldEnvelope>],
    avgdl_star: f32,
    idf_f32: f32,
    boost: f32,
    params: Bm25Params,
) -> f32 {
    debug_assert_eq!(weights.len(), present.len());
    let n = weights.len().min(present.len()).min(16);
    let mut max_tf_star = 0.0_f32;
    let mut min_len_star = None;
    for field in 0..n {
        if mask & (1u16 << field) == 0 {
            continue;
        }
        let Some(env) = present[field] else {
            continue;
        };
        let representative = match TfBucket::new(env.max_tf_bucket) {
            Some(bucket) => bucket.representative_count() as f32,
            None => continue,
        };
        max_tf_star += weights[field] * representative;
        let weighted = weights[field] * (env.shortest as f32);
        min_len_star = Some(min_len_star.map_or(weighted, |seen: f32| seen.min(weighted)));
    }
    let Some(min_len_star) = min_len_star else {
        return 0.0;
    };
    saturate(
        max_tf_star,
        min_len_star,
        avgdl_star,
        idf_f32,
        boost,
        params,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fused_interval_bound(
    streams: &[(u8, &Ordinals<'_>)],
    mask: u16,
    weights: &[f32],
    start: u32,
    end: u32,
    field_count: u8,
    avgdl_star: f32,
    idf_f32: f32,
    boost: f32,
    params: Bm25Params,
) -> f32 {
    match interval_envelopes(streams, start, end, field_count) {
        Ok(present) => fused_bound(mask, weights, &present, avgdl_star, idf_f32, boost, params),
        // A bound-read failure must not look like an empty block: WAND would
        // prune real hits if the remaining envelope fell below exact_score.
        Err(_) => f32::INFINITY,
    }
}

/// Multi-column bound from a parent `Term`. Unpacks `channels()`; never reads
/// the parent nibble / stock `Term::ordinals()`. Unpack or bound-read failure
/// is unprunable `INFINITY` (design §3).
#[allow(clippy::too_many_arguments)]
pub(crate) fn fused_interval_bound_from_term(
    term: &segment::segment::Term<'_>,
    field_count: u8,
    mask: u16,
    weights: &[f32],
    start: u32,
    end: u32,
    avgdl_star: f32,
    idf_f32: f32,
    boost: f32,
    params: Bm25Params,
) -> f32 {
    if !(2..=16).contains(&field_count) {
        return f32::INFINITY;
    }
    let logical = match LogicalTerm::from_entry(String::new(), mask, field_count, *term) {
        Ok(logical) => logical,
        Err(_) => return f32::INFINITY,
    };
    let mut opened = Vec::with_capacity(logical.streams.len());
    for stream in &logical.streams {
        match stream.term.ordinals() {
            Ok(ordinals) => opened.push((stream.field, ordinals)),
            Err(_) => return f32::INFINITY,
        }
    }
    let pairs: Vec<(u8, &Ordinals<'_>)> = opened
        .iter()
        .map(|(field, stream)| (*field, stream))
        .collect();
    fused_interval_bound(
        &pairs,
        mask,
        weights,
        start,
        end,
        field_count,
        avgdl_star,
        idf_f32,
        boost,
        params,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use segment::ordinals::{LIST_MAX, encode_scored};
    use segment::tf_bucket::TfBucket;

    use super::*;
    use crate::fields::intersect::{Front, Intersect, next_atleast, next_conjunction};
    use crate::fields::score::{fused_avgdl, fused_idf, fused_len, fused_score, fused_tf};
    use crate::fields::types::LogicalTerm;
    use segment::index::Index;

    const ITERATIONS: u32 = 512;
    const TOP_K: usize = 5;

    fn open(bytes: &[u8]) -> Ordinals<'_> {
        Ordinals::open(bytes, bytes.len() as u64, true).expect("scored stream")
    }

    struct ChannelMem {
        ordinals: Vec<u8>,
        payload: Vec<u8>,
    }

    impl segment::segment::AreaFetch for ChannelMem {
        fn ordinals_bytes(&self, offset: u64, len: usize) -> segment::Result<&[u8]> {
            let at = usize::try_from(offset).map_err(|_| segment::Error::Truncated)?;
            self.ordinals
                .get(at..at.checked_add(len).ok_or(segment::Error::Truncated)?)
                .ok_or(segment::Error::Truncated)
        }

        fn payload_bytes(&self, extent: segment::dictionary::Extent) -> segment::Result<&[u8]> {
            let at = usize::try_from(extent.offset).map_err(|_| segment::Error::Truncated)?;
            let len = extent.len as usize;
            self.payload
                .get(at..at.checked_add(len).ok_or(segment::Error::Truncated)?)
                .ok_or(segment::Error::Truncated)
        }

        fn doc_table(&self) -> segment::Result<segment::docs::DocTable<'_>> {
            Err(segment::Error::Corrupt("channel documents"))
        }

        fn length(&self, _ordinal: u32) -> segment::Result<u32> {
            Ok(1)
        }

        fn length_class(&self, ordinal: u32) -> segment::Result<u8> {
            Ok(segment::length_class::class_of(self.length(ordinal)?))
        }
    }

    fn payload_for(tfs: &[u32]) -> Vec<u8> {
        let mut builder = segment::payload::PayloadBuilder::default();
        for &tf in tfs {
            let n = tf.max(1);
            let positions: Vec<u32> = (1..=n).collect();
            builder.push(&positions).expect("positions");
        }
        builder.finish()
    }

    fn unpack_channel_streams<R>(
        term: &TermFixture,
        f: impl FnOnce(&[(u8, Ordinals<'_>)]) -> R,
    ) -> R {
        let records_ord: Vec<(u8, u32, &[u8])> = term
            .fields
            .iter()
            .zip(&term.bytes)
            .map(|(&field, bytes)| {
                (
                    field,
                    u32::try_from(bytes.len()).expect("stream fits u32"),
                    bytes.as_slice(),
                )
            })
            .collect();
        let records_pay: Vec<(u8, u32, &[u8])> = term
            .fields
            .iter()
            .zip(&term.payloads)
            .map(|(&field, bytes)| {
                (
                    field,
                    u32::try_from(bytes.len()).expect("stream fits u32"),
                    bytes.as_slice(),
                )
            })
            .collect();
        let mem = ChannelMem {
            ordinals: segment::channels::frame(&records_ord),
            payload: segment::channels::frame(&records_pay),
        };
        assert_eq!(&mem.ordinals[..4], b"FCH1", "multi-column fixture is FCH1");
        let entry = segment::dictionary::TermEntry {
            df: u32::try_from(term.df_agg).unwrap_or(u32::MAX),
            max_tf_bucket: term.parent_max_tf_bucket,
            ordinals: segment::dictionary::Extent {
                offset: 0,
                len: u32::try_from(mem.ordinals.len()).expect("extent fits u32"),
            },
            payload: segment::dictionary::Extent {
                offset: 0,
                len: u32::try_from(mem.payload.len()).expect("extent fits u32"),
            },
        };
        let parent = segment::segment::Term::new(entry, &mem);
        let logical = LogicalTerm::from_entry("bound".into(), term.mask, term.field_count, parent)
            .expect("Term::channels unpack");
        let opened: Vec<(u8, Ordinals<'_>)> = logical
            .streams
            .iter()
            .map(|stream| {
                (
                    stream.field,
                    stream.term.ordinals().expect("channel ordinals"),
                )
            })
            .collect();
        f(&opened)
    }

    fn bucket_of(raw: u32) -> u8 {
        TfBucket::from_count(raw).value()
    }

    fn scored(ordinals: &[u32], raw_tf: &[u32], lengths: &[u32]) -> Vec<u8> {
        assert_eq!(ordinals.len(), raw_tf.len());
        assert_eq!(ordinals.len(), lengths.len());
        let scores: Vec<(u8, u32)> = raw_tf
            .iter()
            .zip(lengths)
            .map(|(&tf, &len)| (bucket_of(tf), len))
            .collect();
        encode_scored(ordinals, &scores)
    }

    fn members(stream: &Ordinals<'_>) -> Vec<u32> {
        stream.to_vec().expect("members")
    }

    fn walk_intervals(streams: &[&Ordinals<'_>], candidates: &[u32]) -> Vec<(u32, u32)> {
        walk_intervals_with(candidates, |pivot| next_interval_end(streams, pivot))
    }

    fn walk_intervals_with(
        candidates: &[u32],
        interval_end: impl Fn(u32) -> Option<u32>,
    ) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < candidates.len() {
            let start = candidates[i];
            let end = interval_end(start)
                .filter(|end| *end > start)
                .unwrap_or_else(|| start.saturating_add(1));
            out.push((start, end));
            while i < candidates.len() && candidates[i] < end {
                i += 1;
            }
        }
        out
    }

    #[derive(Clone)]
    struct TermFixture {
        field_count: u8,
        mask: u16,
        weights: Vec<f32>,
        bytes: Vec<Vec<u8>>,
        payloads: Vec<Vec<u8>>,
        fields: Vec<u8>,
        raw: BTreeMap<(u8, u32), u32>,
        lengths: BTreeMap<u32, Vec<u32>>,
        field_totals: Vec<u64>,
        total_docs: u64,
        df_agg: u64,
        parent_max_tf_bucket: u8,
        boost: f32,
        params: Bm25Params,
    }

    impl TermFixture {
        fn avgdl(&self) -> f32 {
            fused_avgdl(&self.weights, &self.field_totals, self.total_docs)
        }

        fn idf(&self) -> f32 {
            fused_idf(self.total_docs, self.df_agg)
        }

        fn exact_at(&self, ordinal: u32) -> f32 {
            let n = usize::from(self.field_count);
            let mut raw_tf = vec![None; n];
            for field in 0..self.field_count {
                if self.mask & (1u16 << field) == 0 {
                    continue;
                }
                raw_tf[usize::from(field)] = self.raw.get(&(field, ordinal)).copied();
            }
            let lengths = self
                .lengths
                .get(&ordinal)
                .cloned()
                .unwrap_or_else(|| vec![0; n]);
            fused_score(
                self.mask,
                &self.weights,
                &raw_tf,
                &lengths,
                &self.field_totals,
                self.total_docs,
                self.df_agg,
                self.boost,
                self.params,
            )
        }

        fn with_streams<R>(&self, f: impl FnOnce(&[(u8, Ordinals<'_>)]) -> R) -> R {
            if self.field_count == 1 {
                let opened: Vec<(u8, Ordinals<'_>)> = self
                    .fields
                    .iter()
                    .zip(&self.bytes)
                    .map(|(&field, bytes)| (field, open(bytes)))
                    .collect();
                return f(&opened);
            }
            unpack_channel_streams(self, f)
        }
    }

    fn sidecar_and_totals(
        field_count: u8,
        postings: &[(u8, u32, u32, u32)],
        docs: &[u32],
    ) -> (BTreeMap<u32, Vec<u32>>, Vec<u64>) {
        let n = usize::from(field_count);
        let mut lengths = BTreeMap::new();
        for &ordinal in docs {
            lengths.insert(ordinal, vec![1u32; n]);
        }
        for &(field, ordinal, _, len) in postings {
            let row = lengths.entry(ordinal).or_insert_with(|| vec![1u32; n]);
            row[usize::from(field)] = len;
        }
        let mut totals = vec![0u64; n];
        for row in lengths.values() {
            for (total, &len) in totals.iter_mut().zip(row) {
                *total += u64::from(len);
            }
        }
        (lengths, totals)
    }

    fn fixture_from_postings(
        field_count: u8,
        mask: u16,
        weights: Vec<f32>,
        postings: &[(u8, u32, u32, u32)],
        boost: f32,
    ) -> TermFixture {
        let mut by_field: BTreeMap<u8, Vec<(u32, u32, u32)>> = BTreeMap::new();
        let mut docs = BTreeSet::new();
        let mut raw = BTreeMap::new();
        for &(field, ordinal, tf, len) in postings {
            if mask & (1u16 << field) == 0 {
                continue;
            }
            by_field.entry(field).or_default().push((ordinal, tf, len));
            docs.insert(ordinal);
            raw.insert((field, ordinal), tf);
        }
        let docs: Vec<u32> = docs.into_iter().collect();
        let (lengths, field_totals) = sidecar_and_totals(field_count, postings, &docs);
        let mut bytes = Vec::new();
        let mut payloads = Vec::new();
        let mut fields = Vec::new();
        let mut parent_max_tf_bucket = 0u8;
        for (field, mut rows) in by_field {
            rows.sort_by_key(|(ordinal, _, _)| *ordinal);
            rows.dedup_by_key(|(ordinal, _, _)| *ordinal);
            let ordinals: Vec<u32> = rows.iter().map(|(o, _, _)| *o).collect();
            let tfs: Vec<u32> = rows.iter().map(|(_, tf, _)| *tf).collect();
            let lens: Vec<u32> = rows.iter().map(|(_, _, len)| *len).collect();
            for &tf in &tfs {
                parent_max_tf_bucket = parent_max_tf_bucket.max(bucket_of(tf));
            }
            bytes.push(scored(&ordinals, &tfs, &lens));
            payloads.push(payload_for(&tfs));
            fields.push(field);
        }
        let df_agg = docs.len() as u64;
        TermFixture {
            field_count,
            mask,
            weights,
            bytes,
            payloads,
            fields,
            raw,
            lengths,
            field_totals,
            total_docs: df_agg.max(1),
            df_agg,
            parent_max_tf_bucket,
            boost,
            params: Bm25Params::default(),
        }
    }

    fn check_invariant(term: &TermFixture) -> (Vec<(u32, f32, f32)>, usize, usize) {
        term.with_streams(|opened| {
            let pairs: Vec<(u8, &Ordinals<'_>)> = opened.iter().map(|(f, s)| (*f, s)).collect();
            let only: Vec<&Ordinals<'_>> = pairs.iter().map(|(_, s)| *s).collect();
            let mut seen = BTreeSet::new();
            for (_, stream) in opened {
                seen.extend(members(stream));
            }
            let candidates: Vec<u32> = seen.into_iter().collect();
            let intervals = walk_intervals(&only, &candidates);
            let mut rows = Vec::new();
            let mut compared = 0usize;
            for &(start, end) in &intervals {
                let bound = fused_interval_bound(
                    &pairs,
                    term.mask,
                    &term.weights,
                    start,
                    end,
                    term.field_count,
                    term.avgdl(),
                    term.idf(),
                    term.boost,
                    term.params,
                );
                for &ordinal in candidates.iter().filter(|o| (start..end).contains(o)) {
                    let exact = term.exact_at(ordinal);
                    assert!(
                        exact.to_bits() == bound.to_bits() || exact <= bound,
                        "exact {exact} > bound {bound} at ordinal {ordinal} in [{start}, {end}) mask={:#b}",
                        term.mask
                    );
                    rows.push((ordinal, exact, bound));
                    compared += 1;
                }
            }
            let scored: Vec<(u32, f32)> =
                candidates.iter().map(|&o| (o, term.exact_at(o))).collect();
            let exhaustive = top_k(&scored, TOP_K);
            let (pruned, skipped) = pruned_top_k(&intervals, &pairs, term, &scored);
            assert_eq!(
                pruned, exhaustive,
                "pruned vs exhaustive top-{TOP_K} rows+order"
            );
            (rows, compared, skipped)
        })
    }

    fn top_k(scored: &[(u32, f32)], k: usize) -> Vec<(u32, f32)> {
        let mut rows = scored.to_vec();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        rows.truncate(k);
        rows
    }

    fn insert_top_k(heap: &mut Vec<(u32, f32)>, ordinal: u32, score: f32, k: usize) {
        if heap.iter().any(|(o, _)| *o == ordinal) {
            return;
        }
        heap.push((ordinal, score));
        heap.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        heap.truncate(k);
    }

    fn pruned_top_k(
        intervals: &[(u32, u32)],
        pairs: &[(u8, &Ordinals<'_>)],
        term: &TermFixture,
        scored: &[(u32, f32)],
    ) -> (Vec<(u32, f32)>, usize) {
        let mut heap = Vec::new();
        let mut skipped = 0usize;
        let mut i = 0usize;
        for &(start, end) in intervals {
            let bound = fused_interval_bound(
                pairs,
                term.mask,
                &term.weights,
                start,
                end,
                term.field_count,
                term.avgdl(),
                term.idf(),
                term.boost,
                term.params,
            );
            let mut inside = Vec::new();
            while i < scored.len() && scored[i].0 < end {
                if scored[i].0 >= start {
                    inside.push(scored[i]);
                }
                i += 1;
            }
            let threshold = if heap.len() == TOP_K {
                heap.last().map(|(_, s)| *s).unwrap_or(f32::NEG_INFINITY)
            } else {
                f32::NEG_INFINITY
            };
            if heap.len() == TOP_K && bound <= threshold {
                skipped += inside.len();
                continue;
            }
            for (ordinal, score) in inside {
                insert_top_k(&mut heap, ordinal, score, TOP_K);
            }
        }
        (heap, skipped)
    }

    #[derive(Clone, Copy, Debug)]
    enum BooleanOp {
        And,
        AtLeast(u32),
    }

    struct OrdFront<'a> {
        items: &'a [u32],
        pos: usize,
        dead: &'a [u32],
    }

    impl<'a> OrdFront<'a> {
        fn new(items: &'a [u32], dead: &'a [u32]) -> Self {
            Self {
                items,
                pos: 0,
                dead,
            }
        }

        fn skip_dead(&mut self, ix: &mut Intersect<'_>) {
            while let Some(at) = self.current() {
                if !self.dead.contains(&at) {
                    break;
                }
                let Some(next) = at.checked_add(1) else {
                    self.pos = self.items.len();
                    break;
                };
                ix.tick();
                self.pos += self.items[self.pos..].partition_point(|&o| o < next);
            }
        }
    }

    impl Front for OrdFront<'_> {
        fn current(&self) -> Option<u32> {
            self.items.get(self.pos).copied()
        }

        fn advance(&mut self, target: u32, ix: &mut Intersect<'_>) {
            ix.tick();
            if self.current().is_some_and(|at| at >= target) {
                self.skip_dead(ix);
                return;
            }
            self.pos += self.items[self.pos..].partition_point(|&o| o < target);
            self.skip_dead(ix);
        }

        fn hint(&self) -> u64 {
            (self.items.len() - self.pos) as u64
        }
    }

    fn next_boolean(
        children: &mut [OrdFront<'_>],
        op: BooleanOp,
        target: u32,
        ix: &mut Intersect<'_>,
    ) -> Option<u32> {
        match op {
            BooleanOp::And => next_conjunction(children, target, ix),
            BooleanOp::AtLeast(min) => next_atleast(children, min, target, ix),
        }
    }

    fn drain_boolean(children: &mut [OrdFront<'_>], op: BooleanOp) -> Vec<u32> {
        let mut step = |_n: usize| {};
        let mut ix = Intersect::new(&mut step);
        let mut out = Vec::new();
        let mut target = 0u32;
        while let Some(hit) = next_boolean(children, op, target, &mut ix) {
            out.push(hit);
            if hit == u32::MAX {
                break;
            }
            target = hit + 1;
        }
        out
    }

    /// Independent boolean candidate set: per-child dedup, then AND / AtLeast,
    /// excluding `dead`. Must not be derived from `drain_boolean`.
    fn independent_boolean_set(lists: &[Vec<u32>], op: BooleanOp, dead: &[u32]) -> Vec<u32> {
        let held: Vec<BTreeSet<u32>> = lists
            .iter()
            .map(|list| list.iter().copied().collect())
            .collect();
        let mut universe = BTreeSet::new();
        for child in &held {
            universe.extend(child.iter().copied());
        }
        universe
            .into_iter()
            .filter(|ordinal| {
                if dead.contains(ordinal) {
                    return false;
                }
                let count = held.iter().filter(|child| child.contains(ordinal)).count();
                match op {
                    BooleanOp::And => count == held.len(),
                    BooleanOp::AtLeast(min) => count as u32 >= min,
                }
            })
            .collect()
    }

    /// Exact AND / `AtLeast` score at `ordinal`.
    ///
    /// The score is the sum over children that hold the ordinal of that child's
    /// exact score; a child that does not hold the ordinal contributes `0.0`.
    /// Ordinal membership is tested against the child's posting list.
    fn boolean_exact(children: &[TermFixture], lists: &[Vec<u32>], ordinal: u32) -> f32 {
        children
            .iter()
            .zip(lists)
            .map(|(child, list)| {
                if list.binary_search(&ordinal).is_ok() {
                    child.exact_at(ordinal)
                } else {
                    0.0
                }
            })
            .sum()
    }

    fn child_bound_at(
        streams: &[(u8, Ordinals<'_>)],
        term: &TermFixture,
        start: u32,
        end: u32,
    ) -> f32 {
        let pairs: Vec<(u8, &Ordinals<'_>)> = streams
            .iter()
            .map(|(field, stream)| (*field, stream))
            .collect();
        fused_interval_bound(
            &pairs,
            term.mask,
            &term.weights,
            start,
            end,
            term.field_count,
            term.avgdl(),
            term.idf(),
            term.boost,
            term.params,
        )
    }

    fn union_list(streams: &[(u8, Ordinals<'_>)], duplicate: bool) -> Vec<u32> {
        let mut set = BTreeSet::new();
        for (_, stream) in streams {
            set.extend(members(stream));
        }
        let mut items: Vec<u32> = set.into_iter().collect();
        if duplicate && items.len() >= 2 {
            let mid = items[items.len() / 2];
            items.insert(items.len() / 2, mid);
        }
        items
    }

    struct IntersectWandReport {
        expected: Vec<u32>,
        pruned: usize,
        skips: Vec<(u32, u32, usize)>,
        top: Vec<(u32, f32)>,
    }

    /// Compare intersect-WAND pruning against an independent exhaustive top-k.
    ///
    /// `walk_intervals_with` must partition `expected`: intervals are sorted,
    /// each satisfies `start < end`, consecutive intervals do not overlap
    /// (`next.start >= prev.end`), and their union covers `expected` exactly
    /// (first start equals `expected.first()`, last end is strictly greater
    /// than `expected.last()`, every expected ordinal falls in exactly one
    /// interval).
    ///
    /// `bound_parts(start, end)` is the envelope for the whole `[start, end)`
    /// interval: its value is used for every pivot in the interval, not a
    /// per-pivot bound. Every expected pivot inside the interval must satisfy
    /// `exact(pivot) <= bound(start, end)`. The per-candidate check enforces
    /// that for every inside pivot; a counter requires it was actually checked
    /// for at least one pivot per interval.
    ///
    /// Summing the per-child interval bounds is a valid upper bound because
    /// (a) each child's `fused_interval_bound(start, end)` is an upper bound
    /// on that child's contribution over `[start, end)`, (b) a child that does
    /// not hold the pivot contributes `0.0`, so summing **all** children is at
    /// least the true score, and (c) looseness of the sum (worst for `AtLeast`)
    /// costs pruning efficiency only, never correctness. An unsafe aggregation
    /// such as `max()` when several children contribute positively would be
    /// caught by the existing per-candidate `exact <= bound` assertion.
    ///
    /// Nonnegativity is a precondition of that `sum()`: boost factors are
    /// parse-constrained to `0.0..=BoostFactor::MAX` in
    /// `tinql/src/parser/descent.rs` (`boost_suffix`, the
    /// `if !(0.0..=BoostFactor::MAX).contains(&factor)` check), and index
    /// `field_weights` must be positive finite
    /// (`postgres/src/storage/mod.rs::apply_field_weights` rejects
    /// `!weight.is_finite() || weight <= 0.0`). Because both are nonnegative,
    /// a plain `sum()` cannot underestimate a nonnegative per-child score.
    /// Negative boosts are not legal input; a future change that admits them
    /// must revisit this aggregation.
    fn check_intersect_wand_lists(
        lists: &[Vec<u32>],
        children: &[TermFixture],
        op: BooleanOp,
        dead: &[u32],
        k: usize,
        interval_end: impl Fn(u32) -> Option<u32>,
        bound_parts: impl Fn(u32, u32) -> Vec<f32>,
    ) -> IntersectWandReport {
        assert!(k > 0, "top-k must be positive");
        let expected = independent_boolean_set(lists, op, dead);
        let mut fronts: Vec<OrdFront<'_>> = lists
            .iter()
            .map(|items| OrdFront::new(items, dead))
            .collect();
        let candidates = drain_boolean(&mut fronts, op);
        assert_eq!(
            candidates, expected,
            "drain_boolean must match the independent boolean set element-for-element before scoring op={op:?} dead={dead:?}"
        );

        let scored: Vec<(u32, f32)> = expected
            .iter()
            .map(|&ordinal| (ordinal, boolean_exact(children, lists, ordinal)))
            .collect();
        let exhaustive = top_k(&scored, k);

        let intervals = walk_intervals_with(&expected, interval_end);
        let mut covered = Vec::with_capacity(expected.len());
        for (i, &(start, end)) in intervals.iter().enumerate() {
            assert!(
                start < end,
                "interval {i} [{start}, {end}) must satisfy start < end",
            );
            if i > 0 {
                let (prev_start, prev_end) = intervals[i - 1];
                assert!(
                    start >= prev_start,
                    "interval {i} [{start}, {end}) is not sorted after [{prev_start}, {prev_end})",
                );
                assert!(
                    start >= prev_end,
                    "interval {i} [{start}, {end}) overlaps previous [{prev_start}, {prev_end})",
                );
            }
            let inside: Vec<u32> = expected
                .iter()
                .copied()
                .filter(|ordinal| (start..end).contains(ordinal))
                .collect();
            let from = covered.len();
            covered.extend_from_slice(&inside);
            let expected_slice = expected.get(from..covered.len());
            assert_eq!(
                Some(inside.as_slice()),
                expected_slice,
                "interval {i} [{start}, {end}) is not the next disjoint slice of expected inside={inside:?} expected_slice={expected_slice:?} op={op:?}",
            );
        }
        if let Some(&first) = expected.first() {
            let &(start, first_end) = intervals
                .first()
                .expect("nonempty expected must yield intervals");
            assert_eq!(
                start, first,
                "interval 0 [{start}, {first_end}) start must equal expected.first()={first}",
            );
        }
        if let Some(&last) = expected.last() {
            let (start, end) = *intervals
                .last()
                .expect("nonempty expected must yield intervals");
            assert!(
                end > last,
                "last interval [{start}, {end}) must cover expected.last()={last}",
            );
        }
        assert_eq!(
            covered,
            expected,
            "interval walk must partition expected (disjoint + total + ordered); last interval {:?} covered={covered:?} expected={expected:?} op={op:?}",
            intervals.last(),
        );
        let mut fronts: Vec<OrdFront<'_>> = lists
            .iter()
            .map(|items| OrdFront::new(items, dead))
            .collect();
        let mut step = |_n: usize| {};
        let mut ix = Intersect::new(&mut step);
        let mut heap = Vec::new();
        let mut skipped = 0usize;
        let mut skips = Vec::new();
        let mut target = 0u32;

        for &(start, end) in &intervals {
            let parts = bound_parts(start, end);
            // Valid envelope: per-child `fused_interval_bound` summed. See fn docs.
            let bound: f32 = parts.iter().copied().sum();
            let inside: Vec<u32> = expected
                .iter()
                .copied()
                .filter(|ordinal| (start..end).contains(ordinal))
                .collect();
            // Envelope: exact(pivot) <= bound(start, end) for every inside pivot.
            let mut envelope_checked = 0usize;
            for &ordinal in &inside {
                let exact = boolean_exact(children, lists, ordinal);
                if !(exact.to_bits() == bound.to_bits() || exact <= bound) {
                    let child_exact: Vec<f32> = children
                        .iter()
                        .enumerate()
                        .map(|(i, child)| {
                            if lists
                                .get(i)
                                .is_some_and(|list| list.binary_search(&ordinal).is_ok())
                            {
                                child.exact_at(ordinal)
                            } else {
                                0.0
                            }
                        })
                        .collect();
                    panic!(
                        "exact {exact} > bound {bound} at {ordinal} in [{start}, {end}) op={op:?} child_exact={child_exact:?} child_bound={parts:?}"
                    );
                }
                envelope_checked += 1;
            }
            assert!(
                envelope_checked >= 1,
                "interval [{start}, {end}) must check exact <= bound(start, end) for at least one expected pivot (checked {envelope_checked}) op={op:?}",
            );
            let threshold = if heap.len() == k {
                heap.last().map(|(_, s)| *s).unwrap_or(f32::NEG_INFINITY)
            } else {
                f32::NEG_INFINITY
            };
            if heap.len() == k && bound <= threshold {
                skipped += inside.len();
                skips.push((start, end, inside.len()));
                for front in &mut fronts {
                    front.advance(end, &mut ix);
                }
                target = end;
                continue;
            }
            let mut drained = Vec::new();
            while let Some(hit) = next_boolean(&mut fronts, op, target, &mut ix) {
                if hit >= end {
                    target = hit;
                    break;
                }
                drained.push(hit);
                insert_top_k(&mut heap, hit, boolean_exact(children, lists, hit), k);
                if hit == u32::MAX {
                    target = u32::MAX;
                    break;
                }
                target = hit + 1;
            }
            assert_eq!(
                drained, inside,
                "interval [{start}, {end}) drain must match independent set members op={op:?}"
            );
        }
        assert_eq!(
            heap, exhaustive,
            "intersect WAND pruned vs exhaustive top-{k} ids+scores+order op={op:?} dead={dead:?} skipped={skipped}"
        );
        IntersectWandReport {
            expected,
            pruned: skipped,
            skips,
            top: heap,
        }
    }

    fn opened_interval_end(opened: &[(u8, Ordinals<'_>)], pivot: u32) -> Option<u32> {
        let only: Vec<&Ordinals<'_>> = opened.iter().map(|(_, stream)| stream).collect();
        next_interval_end(&only, pivot)
    }

    fn run_intersect_wand(
        children: &[TermFixture],
        op: BooleanOp,
        dead: &[u32],
        k: usize,
        duplicate: bool,
    ) -> IntersectWandReport {
        match children {
            [a, b] => a.with_streams(|oa| {
                b.with_streams(|ob| {
                    let lists = [union_list(oa, duplicate), union_list(ob, duplicate)];
                    check_intersect_wand_lists(
                        &lists,
                        children,
                        op,
                        dead,
                        k,
                        |pivot| {
                            opened_interval_end(oa, pivot)
                                .into_iter()
                                .chain(opened_interval_end(ob, pivot))
                                .min()
                        },
                        |start, end| {
                            vec![
                                child_bound_at(oa, a, start, end),
                                child_bound_at(ob, b, start, end),
                            ]
                        },
                    )
                })
            }),
            [a, b, c] => a.with_streams(|oa| {
                b.with_streams(|ob| {
                    c.with_streams(|oc| {
                        let lists = [
                            union_list(oa, duplicate),
                            union_list(ob, duplicate),
                            union_list(oc, duplicate),
                        ];
                        check_intersect_wand_lists(
                            &lists,
                            children,
                            op,
                            dead,
                            k,
                            |pivot| {
                                opened_interval_end(oa, pivot)
                                    .into_iter()
                                    .chain(opened_interval_end(ob, pivot))
                                    .chain(opened_interval_end(oc, pivot))
                                    .min()
                            },
                            |start, end| {
                                vec![
                                    child_bound_at(oa, a, start, end),
                                    child_bound_at(ob, b, start, end),
                                    child_bound_at(oc, c, start, end),
                                ]
                            },
                        )
                    })
                })
            }),
            _ => panic!("intersect WAND fixture wants 2 or 3 children"),
        }
    }

    #[test]
    fn bound_time_uses_new_not_from_count() {
        let stored = 4u8;
        assert_eq!(TfBucket::new(stored).unwrap().representative_count(), 10);
        assert_ne!(
            TfBucket::from_count(u32::from(stored)).representative_count(),
            10
        );
        let bytes = scored(&[0], &[10], &[7]);
        let stream = open(&bytes);
        let env = field_envelope(&stream, 0, 1).unwrap().unwrap();
        assert_eq!(env.max_tf_bucket, stored);
        let bound = fused_bound(
            0b1,
            &[1.0],
            &[Some(env)],
            7.0,
            fused_idf(1, 1),
            1.0,
            Bm25Params::default(),
        );
        let from_rep = saturate(10.0, 7.0, 7.0, fused_idf(1, 1), 1.0, Bm25Params::default());
        let from_count_wrong = saturate(
            TfBucket::from_count(u32::from(stored)).representative_count() as f32,
            7.0,
            7.0,
            fused_idf(1, 1),
            1.0,
            Bm25Params::default(),
        );
        assert_eq!(bound.to_bits(), from_rep.to_bits());
        assert_ne!(bound.to_bits(), from_count_wrong.to_bits());
    }

    #[test]
    fn witness_title_0_1000_body_at_500() {
        let term = fixture_from_postings(
            2,
            0b11,
            vec![1.0, 2.0],
            &[
                (0, 0, 1, 10),
                (0, 999, 1, 10),
                (1, 500, 100, 8),
                (1, 1000, 1, 8),
            ],
            1.0,
        );
        term.with_streams(|opened| {
            let pairs: Vec<(u8, &Ordinals<'_>)> =
                opened.iter().map(|(f, s)| (*f, s)).collect();
            let only: Vec<&Ordinals<'_>> = pairs.iter().map(|(_, s)| *s).collect();
            let end0 = next_interval_end(&only, 0).unwrap();
            assert_eq!(end0, 500, "title 0..1000 must truncate at body start 500");

            let env0 = interval_envelopes(&pairs, 0, end0, 2).expect("readable bounds");
            assert!(env0[0].is_some(), "title covers [0, 500)");
            assert!(
                env0[1].is_none(),
                "body block [500, …) must not intersect [0, 500)"
            );

            let end500 = next_interval_end(&only, 500).unwrap();
            assert_eq!(end500, 1000);
            let env500 = interval_envelopes(&pairs, 500, end500, 2).expect("readable bounds");
            assert!(env500[0].is_some(), "title 0..1000 still intersects [500, 1000)");
            assert!(env500[1].is_some(), "body raises tf* at 500");

            let end1000 = next_interval_end(&only, 1000).unwrap();
            let env1000 =
                interval_envelopes(&pairs, 1000, end1000, 2).expect("readable bounds");
            assert!(
                env1000[0].is_none(),
                "title exclusive end 1000 does not intersect [1000, {end1000})"
            );
            assert!(env1000[1].is_some());

            let b0 = fused_bound(
                term.mask,
                &term.weights,
                &env0,
                term.avgdl(),
                term.idf(),
                term.boost,
                term.params,
            );
            let b500 = fused_bound(
                term.mask,
                &term.weights,
                &env500,
                term.avgdl(),
                term.idf(),
                term.boost,
                term.params,
            );
            let b1000 = fused_bound(
                term.mask,
                &term.weights,
                &env1000,
                term.avgdl(),
                term.idf(),
                term.boost,
                term.params,
            );
            let title_only = fused_bound(
                term.mask,
                &term.weights,
                &env0,
                term.avgdl(),
                term.idf(),
                term.boost,
                term.params,
            );
            let e0 = term.exact_at(0);
            let e500 = term.exact_at(500);
            let e1000 = term.exact_at(1000);
            eprintln!(
                "witness exact vs bound: 0 exact={e0} bound={b0}; 500 exact={e500} bound={b500}; 1000 exact={e1000} bound={b1000}"
            );
            eprintln!(
                "witness intervals: 0→{end0} 500→{end500} 1000→{end1000}; title-only bound held across 500 would be {title_only}"
            );
            assert!(e0 <= b0, "exact {e0} > bound {b0} at 0");
            assert!(e500 <= b500, "exact {e500} > bound {b500} at 500");
            assert!(e1000 <= b1000, "exact {e1000} > bound {b1000} at 1000");
            assert!(
                e500 > title_only,
                "holding the title-only bound across 500 breaks exact ≤ bound: exact {e500} title-only {title_only}"
            );
            let title_rep = TfBucket::new(env0[0].unwrap().max_tf_bucket)
                .unwrap()
                .representative_count() as f32;
            let body_rep = TfBucket::new(env500[1].unwrap().max_tf_bucket)
                .unwrap()
                .representative_count() as f32;
            assert!(body_rep > 0.0);
            let tf_star_0 = fused_tf(0b11, &term.weights, &[Some(1), None]);
            assert_eq!(tf_star_0.to_bits(), (term.weights[0] * title_rep).to_bits());
            assert!(
                b500 > b0,
                "fused bound must rise at 500: before {b0} after {b500}"
            );
            assert!(
                env500[1].unwrap().max_tf_bucket > env0[0].unwrap().max_tf_bucket,
                "body envelope bucket must exceed the title-only bucket at 500"
            );
        });
        let (_rows, compared, skipped) = check_invariant(&term);
        assert!(compared >= 3);
        eprintln!("witness pruned skipped={skipped} compared={compared}");
    }

    #[test]
    fn intra_block_sub_step_at_1024() {
        let mut postings = Vec::new();
        for i in 0..40u32 {
            postings.push((0, i * 20, 2, 40));
        }
        for i in 0..30u32 {
            postings.push((0, SUB + i * 20, 85, 40));
        }
        assert!(postings.len() > LIST_MAX);
        let term = fixture_from_postings(1, 0b1, vec![1.0], &postings, 1.0);
        term.with_streams(|opened| {
            let pairs: Vec<(u8, &Ordinals<'_>)> = opened.iter().map(|(f, s)| (*f, s)).collect();
            let only: Vec<&Ordinals<'_>> = pairs.iter().map(|(_, s)| *s).collect();
            assert!(opened[0].1.list().is_none(), "fixture is chunked");
            let end0 = next_interval_end(&only, 0).unwrap();
            assert_eq!(end0, SUB);
            let env0 = interval_envelopes(&pairs, 0, end0, 1).expect("readable bounds");
            let env1 = interval_envelopes(&pairs, SUB, SUB * 2, 1).expect("readable bounds");
            let b0 = env0[0].unwrap().max_tf_bucket;
            let b1 = env1[0].unwrap().max_tf_bucket;
            assert!(
                b1 > b0,
                "subs must change at 1024: sub0 bucket {b0} sub1 bucket {b1}"
            );
            let exact0 = term.exact_at(0);
            let bound0 = fused_bound(
                0b1,
                &[1.0],
                &env0,
                term.avgdl(),
                term.idf(),
                term.boost,
                term.params,
            );
            let exact1 = term.exact_at(SUB);
            let bound1 = fused_bound(
                0b1,
                &[1.0],
                &env1,
                term.avgdl(),
                term.idf(),
                term.boost,
                term.params,
            );
            assert!(exact0 <= bound0);
            assert!(exact1 <= bound1);
        });
        check_invariant(&term);
    }

    #[test]
    fn conjunction_uses_sum_of_per_term_bounds_not_min() {
        let a = fixture_from_postings(
            2,
            0b11,
            vec![2.0, 1.0],
            &[(0, 0, 20, 12), (0, 10, 20, 12)],
            1.0,
        );
        let b = fixture_from_postings(
            2,
            0b11,
            vec![2.0, 1.0],
            &[(1, 0, 41, 9), (1, 10, 41, 9)],
            1.0,
        );
        a.with_streams(|opened_a| {
            b.with_streams(|opened_b| {
                let pairs_a: Vec<(u8, &Ordinals<'_>)> =
                    opened_a.iter().map(|(f, s)| (*f, s)).collect();
                let pairs_b: Vec<(u8, &Ordinals<'_>)> =
                    opened_b.iter().map(|(f, s)| (*f, s)).collect();
                let only_a: Vec<&Ordinals<'_>> = pairs_a.iter().map(|(_, s)| *s).collect();
                let only_b: Vec<&Ordinals<'_>> = pairs_b.iter().map(|(_, s)| *s).collect();
                let cand_a: BTreeSet<u32> = members(&opened_a[0].1).into_iter().collect();
                let cand_b: BTreeSet<u32> = members(&opened_b[0].1).into_iter().collect();
                let and: Vec<u32> = cand_a.intersection(&cand_b).copied().collect();
                assert_eq!(and, vec![0, 10]);
                let list_a: Vec<u32> = cand_a.iter().copied().collect();
                let list_b: Vec<u32> = cand_b.iter().copied().collect();
                let mut fronts = [
                    OrdFront::new(&list_a, &[]),
                    OrdFront::new(&list_b, &[]),
                ];
                assert_eq!(
                    drain_boolean(&mut fronts, BooleanOp::And),
                    and,
                    "AND candidates come from next_conjunction"
                );
                let mut saw_min_too_tight = false;
                for &ordinal in &and {
                    let end = next_interval_end(&only_a, ordinal)
                        .into_iter()
                        .chain(next_interval_end(&only_b, ordinal))
                        .min()
                        .unwrap();
                    let bound_a = fused_interval_bound(
                        &pairs_a,
                        a.mask,
                        &a.weights,
                        ordinal,
                        end,
                        a.field_count,
                        a.avgdl(),
                        a.idf(),
                        a.boost,
                        a.params,
                    );
                    let bound_b = fused_interval_bound(
                        &pairs_b,
                        b.mask,
                        &b.weights,
                        ordinal,
                        end,
                        b.field_count,
                        b.avgdl(),
                        b.idf(),
                        b.boost,
                        b.params,
                    );
                    let conjunction_bound = bound_a + bound_b;
                    let exact = a.exact_at(ordinal) + b.exact_at(ordinal);
                    assert!(
                        exact <= conjunction_bound,
                        "AND exact {exact} > sum of fused bounds {conjunction_bound} at {ordinal}"
                    );
                    let min_bound = bound_a.min(bound_b);
                    if exact > min_bound {
                        saw_min_too_tight = true;
                    }
                    eprintln!(
                        "conjunction o={ordinal} exact={exact} bound_sum={conjunction_bound} min={min_bound} a={bound_a} b={bound_b}"
                    );
                }
                assert!(
                    saw_min_too_tight,
                    "conjunction fixture must show min(per-term bounds) is not an upper bound"
                );

                let mut intervals = Vec::new();
                let mut walk = 0usize;
                while walk < and.len() {
                    let start = and[walk];
                    let end = next_interval_end(&only_a, start)
                        .into_iter()
                        .chain(next_interval_end(&only_b, start))
                        .min()
                        .filter(|end| *end > start)
                        .unwrap_or_else(|| start.saturating_add(1));
                    intervals.push((start, end));
                    while walk < and.len() && and[walk] < end {
                        walk += 1;
                    }
                }
                let scored: Vec<(u32, f32)> = and
                    .iter()
                    .map(|&ordinal| (ordinal, a.exact_at(ordinal) + b.exact_at(ordinal)))
                    .collect();
                let exhaustive = top_k(&scored, TOP_K);
                let mut heap = Vec::new();
                let mut i = 0usize;
                for &(start, end) in &intervals {
                    let bound = fused_interval_bound(
                        &pairs_a,
                        a.mask,
                        &a.weights,
                        start,
                        end,
                        a.field_count,
                        a.avgdl(),
                        a.idf(),
                        a.boost,
                        a.params,
                    ) + fused_interval_bound(
                        &pairs_b,
                        b.mask,
                        &b.weights,
                        start,
                        end,
                        b.field_count,
                        b.avgdl(),
                        b.idf(),
                        b.boost,
                        b.params,
                    );
                    let mut inside = Vec::new();
                    while i < scored.len() && scored[i].0 < end {
                        if scored[i].0 >= start {
                            inside.push(scored[i]);
                        }
                        i += 1;
                    }
                    let threshold = if heap.len() == TOP_K {
                        heap.last().map(|(_, s)| *s).unwrap_or(f32::NEG_INFINITY)
                    } else {
                        f32::NEG_INFINITY
                    };
                    if heap.len() == TOP_K && bound <= threshold {
                        continue;
                    }
                    for (ordinal, score) in inside {
                        insert_top_k(&mut heap, ordinal, score, TOP_K);
                    }
                }
                assert_eq!(
                    heap, exhaustive,
                    "conjunction pruned vs exhaustive top-{TOP_K} rows+order"
                );
            });
        });
    }

    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn under(&mut self, n: u32) -> u32 {
            debug_assert!(n > 0);
            (self.next_u64() % u64::from(n.max(1))) as u32
        }

        fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
            xs[self.under(xs.len() as u32) as usize]
        }
    }

    fn gen_ordinals(rng: &mut SplitMix64, count: usize) -> Vec<u32> {
        let mut set = BTreeSet::new();
        let base = rng.pick(&[0, 1, 500, SUB, SUB * 2, CHUNK, CHUNK + 500]);
        set.insert(base);
        let mut guard = 0u32;
        while set.len() < count && guard < 10_000 {
            guard += 1;
            let last = *set.iter().next_back().unwrap();
            let next = match rng.under(7) {
                0 => last.saturating_add(1),
                1 => last.saturating_add(SUB),
                2 => last.saturating_add(CHUNK),
                3 => last.saturating_add(500),
                4 => (last / SUB + 1).saturating_mul(SUB),
                5 => (last / CHUNK + 1).saturating_mul(CHUNK),
                _ => last.saturating_add(1 + rng.under(40)),
            };
            if next > last {
                set.insert(next);
            } else if last < u32::MAX {
                set.insert(last + 1);
            } else {
                break;
            }
        }
        set.into_iter().take(count).collect()
    }

    fn generate(rng: &mut SplitMix64) -> TermFixture {
        let field_count = 2 + rng.under(3) as u8;
        let mut mask = 0u16;
        while mask == 0 {
            mask = rng.under(1u32 << field_count) as u16;
        }
        let weights: Vec<f32> = (0..field_count)
            .map(|_| rng.pick(&[0.5_f32, 1.0, 1.5, 2.0, 3.0]))
            .collect();
        let tfs = [1u32, 2, 4, 10, 20, 41, 85, 100];
        let mut postings = Vec::new();
        for field in 0..field_count {
            if mask & (1u16 << field) == 0 || rng.under(8) == 0 {
                continue;
            }
            let chunked = rng.under(3) == 0;
            let count = if chunked {
                LIST_MAX + 1 + rng.under(40) as usize
            } else {
                1 + rng.under(LIST_MAX as u32) as usize
            };
            let ordinals = gen_ordinals(rng, count);
            for ordinal in ordinals {
                let tf = rng.pick(&tfs);
                let len = tf.max(1) + rng.under(80);
                postings.push((field, ordinal, tf, len));
            }
        }
        if postings.is_empty() {
            postings.push((0, 0, 1, 4));
            mask |= 1;
        }
        let boost = rng.pick(&[0.5_f32, 1.0, 2.0]);
        fixture_from_postings(field_count, mask, weights, &postings, boost)
    }

    fn skip_boundary_children() -> Vec<TermFixture> {
        let mut postings = vec![
            (0, 0, 1, 80),
            (1, 0, 40, 12),
            (0, SUB, 1, 80),
            (2, SUB, 100, 8),
        ];
        let mut weak = 10u32;
        while weak < SUB {
            postings.push((0, weak, 1, 80));
            weak = weak.saturating_add(64);
        }
        let term = fixture_from_postings(3, 0b111, vec![1.0, 2.0, 4.0], &postings, 1.0);
        vec![term.clone(), term]
    }

    fn generate_boolean(
        rng: &mut SplitMix64,
    ) -> (Vec<TermFixture>, BooleanOp, Vec<u32>, usize, bool) {
        if rng.under(5) == 0 {
            return (
                skip_boundary_children(),
                BooleanOp::And,
                Vec::new(),
                1,
                false,
            );
        }
        let n = 2 + rng.under(2) as usize;
        let base = generate(rng);
        let mut children = vec![base.clone(); n];
        if rng.under(2) == 0 {
            children[n - 1] = generate(rng);
        } else if n == 3 && rng.under(2) == 0 {
            children[1] = generate(rng);
        }
        let op = if n == 2 {
            if rng.under(2) == 0 {
                BooleanOp::And
            } else {
                BooleanOp::AtLeast(2)
            }
        } else if rng.under(3) == 0 {
            BooleanOp::And
        } else if rng.under(2) == 0 {
            BooleanOp::AtLeast(2)
        } else {
            BooleanOp::AtLeast(3)
        };
        let mut dead = Vec::new();
        if rng.under(2) == 0 {
            let some = children
                .iter()
                .flat_map(|child| child.raw.keys().map(|(_, ordinal)| *ordinal))
                .collect::<BTreeSet<_>>();
            let some: Vec<u32> = some.into_iter().collect();
            if some.len() > 3 {
                dead.push(some[some.len() / 2]);
                dead.push(some[1]);
            }
        }
        let k = 1 + rng.under(8) as usize;
        let duplicate = rng.under(3) == 0;
        (children, op, dead, k, duplicate)
    }

    #[test]
    fn randomized_exact_le_bound_and_pruned_matches_exhaustive() {
        let mut rng = SplitMix64(0x5354_4E33_3434);
        let mut compared = 0u64;
        let mut skipped = 0u64;
        let mut identical = 0u32;
        for i in 0..ITERATIONS {
            let term = generate(&mut rng);
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check_invariant(&term)));
            let (rows, n, skip) = match result {
                Ok(v) => v,
                Err(_) => panic!("invariant failed on iteration {i}"),
            };
            compared += n as u64;
            skipped += skip as u64;
            identical += 1;
            let _ = rows;
        }
        eprintln!(
            "randomized {ITERATIONS} iterations, compared={compared} pruned_skipped={skipped} identical_topk={identical}"
        );
        assert!(compared > 0);
        assert_eq!(identical, ITERATIONS);
    }

    #[test]
    fn intersect_wand_matches_exhaustive_on_named_shapes() {
        let a = fixture_from_postings(
            2,
            0b11,
            vec![2.0, 1.0],
            &[
                (0, 0, 20, 12),
                (0, SUB - 1, 100, 8),
                (0, SUB, 4, 8),
                (0, CHUNK, 41, 9),
                (1, 10, 10, 12),
            ],
            1.0,
        );
        let b = fixture_from_postings(
            2,
            0b11,
            vec![1.0, 2.0],
            &[
                (1, 0, 41, 9),
                (1, SUB - 1, 100, 8),
                (1, SUB, 4, 8),
                (1, CHUNK, 20, 9),
                (0, 10, 10, 12),
            ],
            1.0,
        );
        let c = fixture_from_postings(
            2,
            0b11,
            vec![1.0, 1.0],
            &[(0, 0, 2, 10), (0, SUB - 1, 85, 8), (1, CHUNK, 1, 8)],
            1.0,
        );
        // Conjunction, including a max-score document on a block boundary.
        run_intersect_wand(&[a.clone(), b.clone()], BooleanOp::And, &[], 3, false);
        // Duplicate ordinals inside a child and a tightening top-1 heap.
        run_intersect_wand(&[a.clone(), b.clone()], BooleanOp::And, &[], 1, true);
        // Disjunction min>1 / AtLeast, tied pivots (0 and SUB-1 in every child).
        run_intersect_wand(
            &[a.clone(), b.clone(), c.clone()],
            BooleanOp::AtLeast(2),
            &[],
            5,
            false,
        );
        run_intersect_wand(
            &[a.clone(), b.clone(), c.clone()],
            BooleanOp::AtLeast(3),
            &[],
            5,
            false,
        );
        // Dead ordinals around the SUB pivot.
        run_intersect_wand(
            &[a.clone(), b.clone()],
            BooleanOp::And,
            &[SUB - 1, SUB],
            3,
            false,
        );
        // Exhausted / non-overlapping child: AND yields no candidates.
        let miss = fixture_from_postings(2, 0b11, vec![1.0, 1.0], &[(0, 99, 1, 4)], 1.0);
        a.with_streams(|oa| {
            miss.with_streams(|om| {
                let lists = [union_list(oa, false), union_list(om, false)];
                let expected = independent_boolean_set(&lists, BooleanOp::And, &[]);
                assert!(
                    expected.is_empty(),
                    "AND with miss must yield an independently empty candidate set, got {expected:?}"
                );
            });
        });
        run_intersect_wand(&[a.clone(), miss], BooleanOp::And, &[], 5, false);
        // Threshold tightens as the heap fills.
        run_intersect_wand(&[a, b, c], BooleanOp::AtLeast(2), &[], 2, true);
    }

    /// Strong hit at 0 on a stream that does not cover later ordinals; weak AND
    /// hits occupy `[SUB, 2*SUB)`; optional best document sits at `2*SUB`.
    fn prunable_and_children(best_after_skip: bool) -> (Vec<TermFixture>, u32, usize) {
        let mut weak = Vec::new();
        for i in 0..40u32 {
            weak.push(SUB + i * 20);
        }
        let best = SUB * 2;
        let mut postings = vec![(0u8, 0u32, 1u32, 80u32), (1u8, 0u32, 100u32, 8u32)];
        for &ordinal in &weak {
            postings.push((0, ordinal, 1, 80));
        }
        if best_after_skip {
            postings.push((0, best, 1, 80));
            postings.push((2, best, 177, 8));
        }
        let term = fixture_from_postings(3, 0b111, vec![1.0, 2.0, 4.0], &postings, 1.0);
        (vec![term.clone(), term], best, weak.len())
    }

    #[test]
    fn intersect_wand_prunes_weak_later_interval() {
        let (children, _, weak_count) = prunable_and_children(false);
        let report = run_intersect_wand(&children, BooleanOp::And, &[], 1, false);
        eprintln!(
            "prunable pruned={} skips={:?} weak={weak_count}",
            report.pruned, report.skips
        );
        assert!(
            report.pruned > 0,
            "expected interval prune of weak later docs, pruned={}",
            report.pruned
        );
        assert_eq!(report.pruned, weak_count);
        assert!(
            report
                .skips
                .iter()
                .any(|&(start, end, n)| { start == SUB && end > SUB && n == weak_count }),
            "must skip [SUB, …) in one jump so start+1 interval ends fail, skips={:?}",
            report.skips
        );
    }

    #[test]
    fn intersect_wand_best_after_pruned_interval_boundary() {
        let (children, best, weak_count) = prunable_and_children(true);
        let score_0 = children[0].exact_at(0) + children[1].exact_at(0);
        let score_best = children[0].exact_at(best) + children[1].exact_at(best);
        assert!(
            score_best > score_0,
            "fixture best {best} score {score_best} must beat ordinal 0 score {score_0}"
        );
        let report = run_intersect_wand(&children, BooleanOp::And, &[], 1, false);
        eprintln!(
            "skip-boundary pruned={} skips={:?} weak={weak_count} best={best} top={:?}",
            report.pruned, report.skips, report.top
        );
        assert!(
            report.pruned > 0,
            "weak interval before best={best} must prune, pruned={}",
            report.pruned
        );
        assert_eq!(report.pruned, weak_count);
        assert!(
            report
                .skips
                .iter()
                .any(|&(start, end, n)| { start == SUB && end == best && n == weak_count }),
            "must skip [SUB, {best}) in one jump so a wrong skip target is caught, skips={:?}",
            report.skips
        );
        assert_eq!(
            report.top.first().map(|(ordinal, _)| *ordinal),
            Some(best),
            "best document must sit after the pruned interval boundary, top={:?}",
            report.top
        );
    }

    #[test]
    fn randomized_intersect_wand_matches_exhaustive() {
        let mut rng = SplitMix64(0x5354_4E34_4333);
        let mut pruned = 0u64;
        for i in 0..ITERATIONS {
            let (children, op, dead, k, duplicate) = generate_boolean(&mut rng);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_intersect_wand(&children, op, &dead, k, duplicate)
            }));
            let report = match result {
                Ok(report) => report,
                Err(_) => panic!(
                    "intersect WAND invariant failed on iteration {i} op={op:?} k={k} dead={dead:?} dups={duplicate}"
                ),
            };
            pruned += report.pruned as u64;
        }
        eprintln!("randomized intersect WAND {ITERATIONS} iterations, pruned={pruned}");
        assert!(
            pruned > 0,
            "randomized generator must produce prunable shapes"
        );
    }

    #[test]
    fn min_len_star_is_min_not_sum() {
        let env0 = FieldEnvelope {
            max_tf_bucket: 0,
            shortest: 10,
        };
        let env1 = FieldEnvelope {
            max_tf_bucket: 0,
            shortest: 100,
        };
        let params = Bm25Params::default();
        let bound = fused_bound(
            0b11,
            &[1.0, 1.0],
            &[Some(env0), Some(env1)],
            10.0,
            fused_idf(10, 2),
            1.0,
            params,
        );
        let as_min = saturate(2.0, 10.0, 10.0, fused_idf(10, 2), 1.0, params);
        let as_sum = saturate(2.0, 110.0, 10.0, fused_idf(10, 2), 1.0, params);
        assert_eq!(bound.to_bits(), as_min.to_bits());
        assert_ne!(bound.to_bits(), as_sum.to_bits());
        let _ = fused_len(&[1.0, 1.0], &[10, 100]);
    }

    fn take_uleb128(bytes: &[u8], at: &mut usize) -> u32 {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = bytes[*at];
            *at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value as u32;
            }
            shift += 7;
        }
    }

    /// Zero the lazy bounds section of a chunked scored stream. `open` still
    /// succeeds; the first `chunk_bound` parse fails.
    fn zero_chunked_bounds(bytes: &mut [u8]) {
        let mut at = 0;
        let count = take_uleb128(bytes, &mut at);
        assert!(
            count as usize > LIST_MAX,
            "fixture must be chunked so bounds stay lazy"
        );
        let chunks = take_uleb128(bytes, &mut at);
        let bounds_len = take_uleb128(bytes, &mut at) as usize;
        const ENTRY: usize = 8;
        let bounds_at = at + chunks as usize * ENTRY;
        bytes[bounds_at..bounds_at + bounds_len].fill(0);
    }

    #[test]
    fn unreadable_chunk_bound_is_unprunable() {
        let count = LIST_MAX as u32 + 8;
        let ordinals: Vec<u32> = (0..count).collect();
        let tfs = vec![20u32; count as usize];
        let lens = vec![12u32; count as usize];
        let mut bytes = scored(&ordinals, &tfs, &lens);
        zero_chunked_bounds(&mut bytes);
        let stream = open(&bytes);
        assert!(
            stream.chunk_bound(0).is_err(),
            "fixture must fail the bound read, not look empty"
        );
        assert!(
            stream.list().is_none(),
            "unreadable fixture is the chunked path"
        );
        let pairs = [(0u8, &stream)];
        let bound = fused_interval_bound(
            &pairs,
            0b1,
            &[1.0],
            0,
            count,
            1,
            12.0,
            fused_idf(1, 1),
            1.0,
            Bm25Params::default(),
        );
        assert!(
            bound.is_infinite(),
            "unread bound must not prune: got {bound}"
        );
        let exact = saturate(
            TfBucket::from_count(20).representative_count() as f32,
            12.0,
            12.0,
            fused_idf(1, 1),
            1.0,
            Bm25Params::default(),
        );
        assert!(exact <= bound, "exact {exact} > unprunable bound {bound}");
    }

    #[test]
    fn two_unit_weight_channels_fuse_tf_star_two_parent_nibble_is_not_a_bound() {
        let term =
            fixture_from_postings(2, 0b11, vec![1.0, 1.0], &[(0, 0, 1, 8), (1, 0, 1, 8)], 1.0);
        assert_eq!(
            term.parent_max_tf_bucket, 0,
            "parent nibble is max(1,1) → bucket 0"
        );
        term.with_streams(|opened| {
            assert_eq!(opened.len(), 2, "two channel streams from one FCH1 term");
            let pairs: Vec<(u8, &Ordinals<'_>)> = opened.iter().map(|(f, s)| (*f, s)).collect();
            let env = interval_envelopes(&pairs, 0, 1, 2).expect("readable bounds");
            let fused = fused_bound(
                term.mask,
                &term.weights,
                &env,
                term.avgdl(),
                term.idf(),
                term.boost,
                term.params,
            );
            let parent_nibble = fused_bound(
                0b1,
                &[1.0],
                &[Some(FieldEnvelope {
                    max_tf_bucket: term.parent_max_tf_bucket,
                    shortest: 8,
                })],
                term.avgdl(),
                term.idf(),
                term.boost,
                term.params,
            );
            let tf_star = fused_tf(0b11, &term.weights, &[Some(1), Some(1)]);
            assert_eq!(tf_star.to_bits(), 2.0_f32.to_bits());
            let exact = term.exact_at(0);
            assert!(exact <= fused, "exact {exact} > fused {fused}");
            assert!(
                fused > parent_nibble,
                "parent nibble bound {parent_nibble} underestimates fused {fused} (tf*=2)"
            );
            assert!(
                exact > parent_nibble,
                "consulting parent max=1 would prune a document whose fused score {exact} exceeds it"
            );
        });
    }

    #[test]
    fn missing_channels_on_multi_column_bound_is_infinity() {
        let index = segment::index::MutableIndex::with_field_count(2).unwrap();
        index
            .begin_fielded_document(segment::Tid::new(1, 1).unwrap())
            .unwrap();
        index.add_occurrence("foo", 0, &[1], 1).unwrap();
        let empty = segment::channels::frame(&[]);
        index
            .install_term_extents("foo", empty.clone(), empty, 1, 0)
            .unwrap();
        let parent = index.term("foo").unwrap().unwrap();
        let bound = fused_interval_bound_from_term(
            &parent,
            2,
            0b11,
            &[1.0, 1.0],
            0,
            1,
            1.0,
            fused_idf(1, 1),
            1.0,
            Bm25Params::default(),
        );
        assert!(
            bound.is_infinite(),
            "bound without channels() must not prune: got {bound}"
        );
    }

    #[test]
    fn missing_channels_on_multi_column_score_is_error() {
        let index = segment::index::MutableIndex::with_field_count(2).unwrap();
        index
            .begin_fielded_document(segment::Tid::new(1, 1).unwrap())
            .unwrap();
        index.add_occurrence("foo", 0, &[1], 1).unwrap();
        let empty = segment::channels::frame(&[]);
        index
            .install_term_extents("foo", empty.clone(), empty, 1, 0)
            .unwrap();
        let err = crate::fields::score::fused_score_from_term(
            &index,
            "foo",
            0b11,
            2,
            &[1.0, 1.0],
            0,
            &[1, 1],
            &[1, 1],
            1,
            1,
            1.0,
            Bm25Params::default(),
        )
        .expect_err("score without channels() is an error");
        assert!(
            matches!(err, crate::fields::error::AdapterError::Index(_)),
            "expected Index error, got {err:?}"
        );
    }
}
