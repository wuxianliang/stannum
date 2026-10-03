// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Fused BM25F arithmetic, lifted from 0.4.0 `TermScorer` order.

use crate::bm25::{Bm25Params, bm25_idf};
use crate::tf_bucket::TfBucket;

use super::cursor::FieldHit;

/// Unscoped mask: every field. `field_count == 16` is `u16::MAX` because
/// `1u16 << 16` does not fit.
#[must_use]
pub(crate) fn all_fields_mask(field_count: u8) -> u16 {
    if field_count >= 16 {
        u16::MAX
    } else {
        (1u16 << field_count) - 1
    }
}

/// `tf*` = Σ_{f ∈ mask ∩ present} w_f · representative_count(bucket_f),
/// left-to-right f32. One dequantize per field; never re-quantized; never raw tf.
#[must_use]
pub(crate) fn fused_tf(mask: u16, weights: &[f32], raw_tf: &[Option<u32>]) -> f32 {
    debug_assert_eq!(weights.len(), raw_tf.len());
    debug_assert!(weights.len() <= 16);
    let mut tf_star = 0.0_f32;
    let n = weights.len().min(raw_tf.len()).min(16);
    for field in 0..n {
        if mask & (1u16 << field) == 0 {
            continue;
        }
        let Some(count) = raw_tf[field] else {
            continue;
        };
        let representative = TfBucket::from_count(count).representative_count() as f32;
        tf_star += weights[field] * representative;
    }
    tf_star
}

/// Per-field raw-tf slots from posting position counts.
///
/// Never the stored tf bucket: `fused_tf` re-quantizes through
/// `TfBucket::from_count`, and a bucket id is not a raw count.
#[must_use]
pub(crate) fn raw_tf_from_hits(hits: &[FieldHit], field_count: u8) -> Vec<Option<u32>> {
    let n = usize::from(field_count.min(16));
    let mut slots = vec![None; n];
    for hit in hits {
        let field = usize::from(hit.field);
        if field < n {
            slots[field] = Some(hit.positions.len() as u32);
        }
    }
    slots
}

/// `len*` = Σ_{all index fields} w_f · length_f, exact u32, left-to-right f32.
/// The mask does not apply.
#[must_use]
pub(crate) fn fused_len(weights: &[f32], lengths: &[u32]) -> f32 {
    debug_assert_eq!(weights.len(), lengths.len());
    debug_assert!(weights.len() <= 16);
    let mut len_star = 0.0_f32;
    let n = weights.len().min(lengths.len()).min(16);
    for field in 0..n {
        len_star += weights[field] * (lengths[field] as f32);
    }
    len_star
}

/// `avgdl*` = (Σ w_f · field_total_f as f32) / (N as f32), left-to-right f32.
/// `N = 0` yields `1.0`.
///
/// `total_docs` includes dead ordinals until rewrite. Field totals are the
/// matching corpus token counts (also covering dead rows still stored).
#[must_use]
pub(crate) fn fused_avgdl(weights: &[f32], field_totals: &[u64], total_docs: u64) -> f32 {
    debug_assert_eq!(weights.len(), field_totals.len());
    debug_assert!(weights.len() <= 16);
    if total_docs == 0 {
        return 1.0;
    }
    let mut sum = 0.0_f32;
    let n = weights.len().min(field_totals.len()).min(16);
    for field in 0..n {
        sum += weights[field] * (field_totals[field] as f32);
    }
    sum / (total_docs as f32)
}

/// `bm25_idf` as f64, then cast to f32 — before any boost multiply.
///
/// `total_docs` includes dead ordinals until rewrite; `df_agg` is the matching
/// union cardinality (also including dead until rewrite).
#[must_use]
pub(crate) fn fused_idf(total_docs: u64, df_agg: u64) -> f32 {
    bm25_idf(total_docs, df_agg) as f32
}

/// `score = (idf_f32 * boost * tf* * (k1+1)) / ((tf* + k1*(1-b)) + (k1*b/avgdl*) * len*)`.
/// The product saturates in f32. `idf_f32 * boost` is the multiplier.
#[must_use]
pub(crate) fn saturate(
    tf_star: f32,
    len_star: f32,
    avgdl_star: f32,
    idf_f32: f32,
    boost: f32,
    params: Bm25Params,
) -> f32 {
    let multiplier = idf_f32 * boost;
    let k1_plus_one = params.k1 + 1.0;
    let k1_one_minus_b = params.k1 * (1.0 - params.b);
    let document_length_factor = params.k1 * params.b / avgdl_star;
    let numerator = multiplier * tf_star * k1_plus_one;
    let denominator = tf_star + k1_one_minus_b + document_length_factor * len_star;
    numerator / denominator
}

#[must_use]
#[allow(clippy::too_many_arguments)]
pub(crate) fn fused_score(
    mask: u16,
    weights: &[f32],
    raw_tf: &[Option<u32>],
    lengths: &[u32],
    field_totals: &[u64],
    total_docs: u64,
    df_agg: u64,
    boost: f32,
    params: Bm25Params,
) -> f32 {
    debug_assert_eq!(weights.len(), raw_tf.len());
    debug_assert_eq!(weights.len(), lengths.len());
    debug_assert_eq!(weights.len(), field_totals.len());
    debug_assert!(weights.len() <= 16);
    let tf_star = fused_tf(mask, weights, raw_tf);
    let len_star = fused_len(weights, lengths);
    let avgdl_star = fused_avgdl(weights, field_totals, total_docs);
    saturate(
        tf_star,
        len_star,
        avgdl_star,
        fused_idf(total_docs, df_agg),
        boost,
        params,
    )
}

#[cfg(test)]
mod tests {
    use segment::Tid;
    use segment::index::MutableIndex;

    use super::*;
    use crate::bm25::{TermScorer, bm25_idf};
    use crate::fields::df::{union_df_agg, union_df_agg_from_streams};
    use crate::fields::expand::lookup;
    use crate::fields::types::Lookup;

    #[test]
    fn all_fields_mask_uses_u16_max_at_sixteen() {
        assert_eq!(all_fields_mask(0), 0);
        assert_eq!(all_fields_mask(1), 0b1);
        assert_eq!(all_fields_mask(2), 0b11);
        assert_eq!(all_fields_mask(15), (1u16 << 15) - 1);
        assert_eq!(all_fields_mask(16), u16::MAX);
        assert_eq!(all_fields_mask(17), u16::MAX);
    }

    #[test]
    fn tf_star_dequantizes_per_field_and_is_never_requantized() {
        // raw 4 → bucket 2, representative 3. Two fields: 3+3 = 6.
        // Re-quantizing 6 would be bucket 3, representative 5.
        let weights = [1.0_f32, 1.0];
        let raw = [Some(4_u32), Some(4)];
        let tf_star = fused_tf(0b11, &weights, &raw);
        assert_eq!(tf_star.to_bits(), 6.0_f32.to_bits());
        assert_ne!(
            tf_star.to_bits(),
            (TfBucket::from_count(6).representative_count() as f32).to_bits()
        );
        let masked = fused_tf(0b01, &weights, &[Some(4), Some(4)]);
        assert_eq!(masked.to_bits(), 3.0_f32.to_bits());
        let absent = fused_tf(0b11, &weights, &[Some(4), None]);
        assert_eq!(absent.to_bits(), 3.0_f32.to_bits());
        let weighted = fused_tf(0b11, &[2.0, 1.0], &[Some(1), Some(5)]);
        assert_eq!(weighted.to_bits(), (2.0_f32 * 1.0 + 1.0 * 5.0).to_bits());
    }

    #[test]
    fn len_star_sums_every_index_field_left_to_right() {
        let weights = [2.0_f32, 1.0];
        let lengths = [10_u32, 20];
        let len_star = fused_len(&weights, &lengths);
        let expected = 2.0_f32 * 10.0 + 1.0 * 20.0;
        assert_eq!(len_star.to_bits(), expected.to_bits());
        assert_eq!(len_star.to_bits(), 40.0_f32.to_bits());
    }

    #[test]
    fn avgdl_star_is_left_to_right_f32_and_n_zero_is_one() {
        let weights = [2.0_f32, 1.0];
        let totals = [100_u64, 200];
        let n = 50_u64;
        let avgdl = fused_avgdl(&weights, &totals, n);
        let mut sum = 0.0_f32;
        sum += weights[0] * (totals[0] as f32);
        sum += weights[1] * (totals[1] as f32);
        let expected = sum / (n as f32);
        assert_eq!(avgdl.to_bits(), expected.to_bits());
        assert_eq!(avgdl.to_bits(), 8.0_f32.to_bits());
        assert_eq!(
            fused_avgdl(&weights, &totals, 0).to_bits(),
            1.0_f32.to_bits()
        );
    }

    #[test]
    fn single_column_matches_term_scorer_bits() {
        let params = Bm25Params::default();
        let n = 100_u64;
        let df = 10_u64;
        let boost = 0.7_f32;
        let avgdl = 80.0_f32;
        let tf = 1_u32;
        let len = 1_u32;
        let scorer = TermScorer::from_statistics(n, df, boost, params, avgdl).unwrap();
        let fused = fused_score(
            0b1,
            &[1.0],
            &[Some(tf)],
            &[len],
            &[8000],
            n,
            df,
            boost,
            params,
        );
        assert_eq!(fused.to_bits(), scorer.score_count(tf, len).to_bits());
        let reconstructed = saturate(
            TfBucket::from_count(tf).representative_count() as f32,
            len as f32,
            avgdl,
            fused_idf(n, df),
            boost,
            params,
        );
        assert_eq!(
            reconstructed.to_bits(),
            scorer.score_count(tf, len).to_bits()
        );
        assert_eq!(fused.to_bits(), 0x402a_277c);
    }

    #[test]
    fn idf_is_cast_to_f32_before_the_boost_multiplier() {
        let params = Bm25Params::default();
        let n = 100_u64;
        let df = 1_u64;
        let boost = 0.1_f32;
        let idf = bm25_idf(n, df);
        let correct = idf as f32 * boost;
        let wrong = (idf * f64::from(boost)) as f32;
        assert_ne!(
            correct.to_bits(),
            wrong.to_bits(),
            "the f64→f32-then-boost order must be observable"
        );
        assert_eq!((fused_idf(n, df) * boost).to_bits(), correct.to_bits());

        let tf_star = 1.0_f32;
        let len_star = 1.0_f32;
        let avgdl_star = 1.0_f32;
        let score = saturate(
            tf_star,
            len_star,
            avgdl_star,
            fused_idf(n, df),
            boost,
            params,
        );
        let reconstructed = saturate(tf_star, len_star, avgdl_star, idf as f32, boost, params);
        assert_eq!(score.to_bits(), reconstructed.to_bits());
        let wrong_score = saturate(tf_star, len_star, avgdl_star, wrong, 1.0, params);
        assert_ne!(score.to_bits(), wrong_score.to_bits());
        assert_eq!(correct.to_bits(), 0x3ed7_88cb);
        assert_eq!(wrong.to_bits(), 0x3ed7_88cc);
        assert_eq!(score.to_bits(), 0x3ed7_88cb);
        assert_eq!(wrong_score.to_bits(), 0x3ed7_88cc);
    }

    #[test]
    fn weighted_two_field_score_matches_hand_folded_f32() {
        let params = Bm25Params::default();
        let weights = [2.0_f32, 1.0];
        let raw = [Some(1_u32), Some(5)];
        let lengths = [10_u32, 20];
        let totals = [100_u64, 200];
        let n = 50_u64;
        let df = 10_u64;
        let boost = 1.0_f32;
        let mask = 0b11;
        let tf_star = fused_tf(mask, &weights, &raw);
        let len_star = fused_len(&weights, &lengths);
        let avgdl_star = fused_avgdl(&weights, &totals, n);
        assert_eq!(tf_star.to_bits(), 7.0_f32.to_bits());
        assert_eq!(len_star.to_bits(), 40.0_f32.to_bits());
        assert_eq!(avgdl_star.to_bits(), 8.0_f32.to_bits());
        let score = fused_score(
            mask, &weights, &raw, &lengths, &totals, n, df, boost, params,
        );
        let expected = saturate(
            tf_star,
            len_star,
            avgdl_star,
            fused_idf(n, df),
            boost,
            params,
        );
        assert_eq!(score.to_bits(), expected.to_bits());
        assert_eq!(fused_idf(n, df).to_bits(), 0x3fca_4c33);
        assert_eq!(score.to_bits(), 0x4004_01ff);
    }

    #[test]
    fn fused_tf_reads_position_count_not_the_stored_bucket() {
        let positions = vec![10_u32, 20, 30, 40];
        assert_eq!(positions.len(), 4);
        let hit = FieldHit {
            field: 0,
            bucket: TfBucket::from_count(1).value(),
            positions,
        };
        assert_ne!(
            u32::from(hit.bucket),
            hit.positions.len() as u32,
            "the stored bucket must disagree with using it as raw tf"
        );
        let slots = raw_tf_from_hits(std::slice::from_ref(&hit), 2);
        assert_eq!(slots, vec![Some(4), None]);
        assert_ne!(slots[0], Some(u32::from(hit.bucket)));
        let from_positions = fused_tf(0b1, &[1.0, 1.0], &slots);
        let from_bucket_as_raw = fused_tf(0b1, &[1.0, 1.0], &[Some(u32::from(hit.bucket)), None]);
        assert_eq!(from_positions.to_bits(), 3.0_f32.to_bits());
        assert_ne!(from_positions.to_bits(), from_bucket_as_raw.to_bits());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic]
    fn fused_score_rejects_unequal_slice_lengths() {
        let _ = fused_score(
            0b1,
            &[1.0, 1.0],
            &[Some(1)],
            &[1, 1],
            &[1, 1],
            1,
            1,
            1.0,
            Bm25Params::default(),
        );
    }

    #[test]
    fn single_column_matches_term_scorer_bits_at_tf_four_and_len_not_one() {
        let params = Bm25Params::default();
        let n = 100_u64;
        let df = 10_u64;
        let boost = 0.7_f32;
        let avgdl = 80.0_f32;
        let tf = 4_u32;
        let len = 17_u32;
        let scorer = TermScorer::from_statistics(n, df, boost, params, avgdl).unwrap();
        let fused = fused_score(
            0b1,
            &[1.0],
            &[Some(tf)],
            &[len],
            &[8000],
            n,
            df,
            boost,
            params,
        );
        assert_eq!(fused.to_bits(), scorer.score_count(tf, len).to_bits());
        assert_ne!(
            fused.to_bits(),
            0x402a_277c,
            "tf=4,len=17 must diverge from the tf=1,len=1 R-BIT pin"
        );
        assert_eq!(
            fused.to_bits(),
            scorer.score_count(tf, len).to_bits(),
            "computed {:08x} vs TermScorer {:08x}",
            fused.to_bits(),
            scorer.score_count(tf, len).to_bits()
        );
        assert_eq!(
            fused.to_bits(),
            0x403f_b873,
            "computed {:08x} vs recorded {:08x}",
            fused.to_bits(),
            0x403f_b873u32
        );
    }

    fn add_columns(index: &MutableIndex, id: u32, columns: &[&str], _field_count: u8) {
        index
            .begin_fielded_document(Tid::new(id, 1).unwrap())
            .unwrap();
        for (field, text) in columns.iter().enumerate() {
            let mut by_term: std::collections::BTreeMap<&str, Vec<u32>> =
                std::collections::BTreeMap::new();
            let mut len = 0u32;
            for (i, token) in text.split_whitespace().enumerate() {
                len += 1;
                by_term.entry(token).or_default().push(i as u32 + 1);
            }
            if len == 0 {
                continue;
            }
            for (token, positions) in by_term {
                index
                    .add_occurrence(token, field as u8, &positions, len)
                    .unwrap();
            }
        }
    }

    fn stream_ordinals(term: &crate::fields::types::LogicalTerm<'_>) -> Vec<(u8, Vec<u32>)> {
        term.streams
            .iter()
            .map(|stream| {
                (
                    stream.field,
                    stream.term.ordinals().unwrap().to_vec().unwrap(),
                )
            })
            .collect()
    }

    fn rank_fused(
        term: &crate::fields::types::LogicalTerm<'_>,
        weights: &[f32],
        lengths_by_ordinal: &[Vec<u32>],
        field_totals: &[u64],
        total_docs: u64,
        ids_by_ordinal: &[u32],
    ) -> Vec<(u32, u32)> {
        let field_count = weights.len() as u8;
        let mut cursor = term.cursor().unwrap();
        let mut ranked = Vec::new();
        while let Some(ordinal) = cursor.current_ordinal() {
            let hits = cursor.field_hits().unwrap();
            let raw = raw_tf_from_hits(&hits, field_count);
            let score = fused_score(
                term.mask,
                weights,
                &raw,
                &lengths_by_ordinal[ordinal as usize],
                field_totals,
                total_docs,
                term.df_agg,
                1.0,
                Bm25Params::default(),
            );
            ranked.push((ids_by_ordinal[ordinal as usize], score.to_bits(), score));
            cursor.advance(ordinal.saturating_add(1)).unwrap();
        }
        ranked.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
        ranked.into_iter().map(|(id, bits, _)| (id, bits)).collect()
    }

    fn bits_table(rows: &[(u32, u32)]) -> String {
        rows.iter()
            .map(|(id, bits)| format!("({id}, {bits:08x})"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn print_bit_pairs(case: &str, computed: &[(u32, u32)], recorded: &[(u32, u32)]) {
        eprintln!("{case} computed vs recorded:");
        for (row, recorded_row) in computed.iter().zip(recorded) {
            let (id, bits) = row;
            let (rid, rbits) = recorded_row;
            eprintln!(
                "  id {id}: computed {bits:08x}  recorded {rbits:08x}  match={}",
                id == rid && bits == rbits
            );
        }
    }

    #[test]
    fn arithmetic_row1_unweighted_tie_bits_match_0_4_0() {
        const FIELDS: u8 = 2;
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        add_columns(&index, 1, &["needle", "pad"], FIELDS);
        add_columns(&index, 2, &["pad", "needle"], FIELDS);
        add_columns(&index, 3, &["needle needle", "pad"], FIELDS);

        let mask = all_fields_mask(FIELDS);
        let Lookup::Term(term) = lookup(&index, "needle", mask, FIELDS).unwrap() else {
            panic!("needle");
        };
        let by_field = stream_ordinals(&term);
        let title = by_field
            .iter()
            .find(|(field, _)| *field == 0)
            .map(|(_, o)| o.as_slice())
            .unwrap_or(&[]);
        let body = by_field
            .iter()
            .find(|(field, _)| *field == 1)
            .map(|(_, o)| o.as_slice())
            .unwrap_or(&[]);
        let union = union_df_agg([title, body]);
        assert_eq!(title, [0_u32, 2], "title needle ordinals (ids 1, 3)");
        assert_eq!(body, [1_u32], "body needle ordinals (id 2)");
        assert_eq!(union, 3, "title ∪ body ordinals");
        assert_eq!(term.df_agg, union);
        assert_eq!(
            term.df_agg,
            union_df_agg_from_streams(&term.streams).unwrap()
        );
        assert_ne!(term.df_agg, 0);

        let lengths = [vec![1_u32, 1], vec![1, 1], vec![2, 1]];
        let totals = [4_u64, 3];
        let ids = [1_u32, 2, 3];
        let computed = rank_fused(&term, &[1.0, 1.0], &lengths, &totals, 3, &ids);
        let recorded = [(3_u32, 0x3e2e_071f), (1, 0x3e11_3925), (2, 0x3e11_3925)];
        print_bit_pairs("arithmetic.row1_unweighted_tie", &computed, &recorded);
        eprintln!(
            "  df_agg={} (union title{title:?} ∪ body{body:?})",
            term.df_agg
        );
        assert_eq!(
            computed,
            recorded,
            "computed [{}] vs recorded [{}]; df_agg={} title={title:?} body={body:?}",
            bits_table(&computed),
            bits_table(&recorded),
            term.df_agg
        );
        assert_eq!(
            computed[1].1, computed[2].1,
            "tie identity is identical bits"
        );
    }

    #[test]
    fn arithmetic_row2_field_weights_bits_match_0_4_0() {
        const FIELDS: u8 = 2;
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        add_columns(&index, 1, &["needle", "pad"], FIELDS);
        add_columns(&index, 2, &["pad", "needle needle needle"], FIELDS);

        let mask = all_fields_mask(FIELDS);
        let Lookup::Term(term) = lookup(&index, "needle", mask, FIELDS).unwrap() else {
            panic!("needle");
        };
        let union = union_df_agg_from_streams(&term.streams).unwrap();
        assert_eq!(term.df_agg, union);
        assert_eq!(term.df_agg, 2);

        let lengths = [vec![1_u32, 1], vec![1, 3]];
        let totals = [2_u64, 4];
        let ids = [1_u32, 2];
        let computed = rank_fused(&term, &[3.0, 1.0], &lengths, &totals, 2, &ids);
        let recorded = [(1_u32, 0x3e99_424c), (2, 0x3e8c_a98f)];
        print_bit_pairs("arithmetic.row2_field_weights", &computed, &recorded);
        assert_eq!(
            computed,
            recorded,
            "computed [{}] vs recorded [{}]",
            bits_table(&computed),
            bits_table(&recorded)
        );
    }
}
