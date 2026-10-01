// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Fused BM25F arithmetic, lifted from 0.4.0 `TermScorer` order.

use crate::bm25::{Bm25Params, bm25_idf};
use crate::tf_bucket::TfBucket;

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

/// `len*` = Σ_{all index fields} w_f · length_f, exact u32, left-to-right f32.
/// The mask does not apply.
#[must_use]
pub(crate) fn fused_len(weights: &[f32], lengths: &[u32]) -> f32 {
    let mut len_star = 0.0_f32;
    let n = weights.len().min(lengths.len());
    for field in 0..n {
        len_star += weights[field] * (lengths[field] as f32);
    }
    len_star
}

/// `avgdl*` = (Σ w_f · field_total_f as f32) / (N as f32), left-to-right f32.
/// `N = 0` yields `1.0`.
#[must_use]
pub(crate) fn fused_avgdl(weights: &[f32], field_totals: &[u64], total_docs: u64) -> f32 {
    if total_docs == 0 {
        return 1.0;
    }
    let mut sum = 0.0_f32;
    let n = weights.len().min(field_totals.len());
    for field in 0..n {
        sum += weights[field] * (field_totals[field] as f32);
    }
    sum / (total_docs as f32)
}

/// `bm25_idf` as f64, then cast to f32 — before any boost multiply.
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
    use super::*;
    use crate::bm25::{TermScorer, bm25_idf};

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
}
