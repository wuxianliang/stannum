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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use segment::ordinals::{LIST_MAX, encode_scored};
    use segment::tf_bucket::TfBucket;

    use super::*;
    use crate::fields::score::{fused_avgdl, fused_idf, fused_len, fused_score, fused_tf};

    const ITERATIONS: u32 = 512;
    const TOP_K: usize = 5;

    fn open(bytes: &[u8]) -> Ordinals<'_> {
        Ordinals::open(bytes, bytes.len() as u64, true).expect("scored stream")
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
        let mut out = Vec::new();
        let mut i = 0;
        while i < candidates.len() {
            let start = candidates[i];
            let end = next_interval_end(streams, start)
                .filter(|end| *end > start)
                .unwrap_or_else(|| start.saturating_add(1));
            out.push((start, end));
            while i < candidates.len() && candidates[i] < end {
                i += 1;
            }
        }
        out
    }

    struct TermFixture {
        field_count: u8,
        mask: u16,
        weights: Vec<f32>,
        bytes: Vec<Vec<u8>>,
        fields: Vec<u8>,
        raw: BTreeMap<(u8, u32), u32>,
        lengths: BTreeMap<u32, Vec<u32>>,
        field_totals: Vec<u64>,
        total_docs: u64,
        df_agg: u64,
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
            let opened: Vec<(u8, Ordinals<'_>)> = self
                .fields
                .iter()
                .zip(&self.bytes)
                .map(|(&field, bytes)| (field, open(bytes)))
                .collect();
            f(&opened)
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
        let mut fields = Vec::new();
        for (field, mut rows) in by_field {
            rows.sort_by_key(|(ordinal, _, _)| *ordinal);
            rows.dedup_by_key(|(ordinal, _, _)| *ordinal);
            let ordinals: Vec<u32> = rows.iter().map(|(o, _, _)| *o).collect();
            let tfs: Vec<u32> = rows.iter().map(|(_, tf, _)| *tf).collect();
            let lens: Vec<u32> = rows.iter().map(|(_, _, len)| *len).collect();
            bytes.push(scored(&ordinals, &tfs, &lens));
            fields.push(field);
        }
        let df_agg = docs.len() as u64;
        TermFixture {
            field_count,
            mask,
            weights,
            bytes,
            fields,
            raw,
            lengths,
            field_totals,
            total_docs: df_agg.max(1),
            df_agg,
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
}
