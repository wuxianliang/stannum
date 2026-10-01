// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! `df_agg` union cardinality over term-stream ordinals.
//!
//! Index-time: distinct document ordinals that contain the token in any
//! field, inside one segment, including dead ordinals. Query-time
//! `total_df` is the sum of per-segment `df_agg`. Merge recounts from the
//! merged streams; addition is not a substitute for that recount.

use std::borrow::Borrow;
use std::collections::BTreeSet;

/// Distinct document ordinals across field streams of one token, one segment.
#[must_use]
pub(crate) fn union_df_agg<I, S>(field_ordinals: I) -> u64
where
    I: IntoIterator<Item = S>,
    S: IntoIterator,
    S::Item: Borrow<u32>,
{
    let mut seen = BTreeSet::new();
    for stream in field_ordinals {
        for ordinal in stream {
            seen.insert(*ordinal.borrow());
        }
    }
    seen.len() as u64
}

/// Query-time `total_df` = Σ per-segment `df_agg`. Ordinals are disjoint
/// across segments; this sum is not a merge of overlapping field streams.
#[must_use]
pub(crate) fn query_total_df(per_segment: &[u64]) -> u64 {
    per_segment.iter().copied().sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_posted_in_both_fields_counts_once() {
        let title = [0_u32];
        let body = [0_u32];
        assert_eq!(union_df_agg([&title[..], &body[..]]), 1);
        assert_ne!(u64::try_from(title.len() + body.len()).unwrap(), 1);

        let title = [0_u32, 1];
        let body = [0_u32, 2];
        assert_eq!(union_df_agg([&title[..], &body[..]]), 3);
        assert_eq!(u64::try_from(title.len() + body.len()).unwrap(), 4);
    }

    #[test]
    fn query_time_sums_per_segment_values() {
        assert_eq!(query_total_df(&[1, 1]), 2);
        assert_eq!(query_total_df(&[3, 5, 0]), 8);
        assert_eq!(query_total_df(&[]), 0);
    }

    #[test]
    fn merge_must_recount_union_not_add_field_local_dfs() {
        let title = [0_u32, 1];
        let body = [0_u32, 1];
        let union = union_df_agg([&title[..], &body[..]]);
        let field_sum = u64::from(title.len() as u32) + u64::from(body.len() as u32);
        assert_eq!(union, 2);
        assert_eq!(field_sum, 4);
        assert_ne!(union, field_sum);
    }
}
