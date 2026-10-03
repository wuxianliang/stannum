// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The §6.3 relation classification: mutually exclusive predicates, one
//! class per relation.
//!
//! The classification is pure. [`SegmentLabel`] reads one immutable blob
//! (a full parse — the error fences and VACUUM paths, which are about to
//! read everything anyway); [`RelationClass`] folds the per-segment labels
//! and the §6.3.1 buffer label through the generation predicates:
//!
//! ```text
//! G_v2 = (S contains ValidV2) ∨ (B = BufferCurrent)
//! G_v1 = (S contains ValidV1) ∨ (B = BufferStale)
//! M    = (S contains Malformed) ∨ (B = BufferMalformed) ∨ LSG-under-kind-5
//! ```
//!
//! `BufferEmpty` and `BufferNoTerms` contribute to neither generation.
//! Single-column blobs are never `ValidV1`/`ValidV2`: they are `Stock`
//! (out of `S`) or `Malformed`. A blob is never both v1 and v2;
//! `Malformed` is not a version.
//!
//! `PreStn3` is **not** a class here: kind-first precedence (parent §8)
//! means a recognized kind-1 meta page is the migration error before any
//! segment byte is read, and an LSG1–LSG4 segment under a kind-5 envelope
//! is the existing mixed-format corruption — both fire in
//! [`read_meta`](super::read_meta)/[`live_check_meta`](super::live_check_meta)
//! upstream of this module, so segment magic can never reclassify them.
//!
//! A successful v1 decode is evidence for the `StaleFielded`/`MixedFielded`
//! rebuild error only — never eligibility for normal operations (design
//! §6.3): the guarded callbacks fence every non-`Current` class before any
//! page is dirtied.

use crate::storage::buffer_label::BufferLabel;
use segment::segment::Segment;
use segment::trailer;

/// The §6.3 label of one immutable segment blob, judged against the
/// envelope's field count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SegmentLabel {
    /// A well-formed STNF v2 blob: the open-time trailer walk (FCH1
    /// framing, df unions, norms agreement) succeeded.
    ValidV2,
    /// A well-formed STNF v1 blob: decodes, dims agree, and the v2
    /// semantic walk is skipped — readable for classification only.
    ValidV1,
    /// A single-column stock blob the stock decoder accepts. Not a member
    /// of `S`: it contributes to neither generation.
    Stock,
    /// Anything else: parse failure, a multi-column blob without a
    /// trailer, a single-column blob with one, dimensions that disagree
    /// with the envelope, or an LSG/STN2 blob under a kind-5 envelope.
    Malformed(String),
}

/// Labels one immutable segment blob against the envelope's `field_count`.
pub(crate) fn segment_label(field_count: usize, blob: &[u8]) -> SegmentLabel {
    let reader = match Segment::parse(blob) {
        Ok(reader) => reader,
        // A parse failure is corruption whatever its shape — including
        // the withdrawn-codec failures (v1 df_len 0, v2 with a df section,
        // unknown version, bad CRC) and an LSG magic under kind-5.
        Err(error) => return SegmentLabel::Malformed(error.to_string()),
    };
    match reader.trailer() {
        None => {
            if field_count >= 2 {
                SegmentLabel::Malformed(
                    "multi-column segment is missing the STNF field-norms trailer".to_owned(),
                )
            } else {
                SegmentLabel::Stock
            }
        }
        Some(parsed) => {
            if field_count == 1 {
                SegmentLabel::Malformed("single-column segment carries an STNF trailer".to_owned())
            } else if usize::from(parsed.field_count) != field_count {
                SegmentLabel::Malformed(format!(
                    "STNF field_count {} does not match the envelope's {field_count}",
                    parsed.field_count
                ))
            } else if parsed.version == trailer::VERSION {
                SegmentLabel::ValidV2
            } else {
                // decode rejects every version but 1 and 2.
                SegmentLabel::ValidV1
            }
        }
    }
}

/// The §6.3 relation class. Exactly one; kind-first `PreStn3` and the
/// meta-page corruption classes fire upstream in `read_meta`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RelationClass {
    /// `¬M ∧ ¬G_v1`: proceed. Requires every immutable multi-column
    /// segment `ValidV2` (or none) and a current or generation-neutral
    /// buffer.
    Current,
    /// `¬M ∧ G_v1 ∧ ¬G_v2`: the 4.3–4.6 fielded-terms generation only.
    StaleFielded,
    /// `¬M ∧ G_v1 ∧ G_v2`: both generations, every part well-formed.
    MixedFielded,
    /// `M`: any malformed segment or buffer. Not a migration class.
    Corrupt(String),
}

impl RelationClass {}

/// Folds the immutable labels and the buffer label into the one relation
/// class. `segments` holds one label per immutable segment; single-column
/// relations pass `Stock`/`Malformed` labels only.
pub(crate) fn relation_class(segments: &[SegmentLabel], buffer: BufferLabel) -> RelationClass {
    // M dominates: a malformed part is corruption even when both
    // generations are otherwise present (v1 + v2 + malformed is Corrupt,
    // not mixed).
    for label in segments {
        if let SegmentLabel::Malformed(reason) = label {
            return RelationClass::Corrupt(reason.clone());
        }
    }
    if let BufferLabel::Malformed(reason) = buffer {
        return RelationClass::Corrupt(format!("write buffer: {reason}"));
    }
    let g_v1 = buffer == BufferLabel::Stale || segments.contains(&SegmentLabel::ValidV1);
    let g_v2 = buffer == BufferLabel::Current || segments.contains(&SegmentLabel::ValidV2);
    match (g_v1, g_v2) {
        (true, true) => RelationClass::MixedFielded,
        (true, false) => RelationClass::StaleFielded,
        (false, _) => RelationClass::Current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1() -> SegmentLabel {
        SegmentLabel::ValidV1
    }
    fn v2() -> SegmentLabel {
        SegmentLabel::ValidV2
    }
    fn stock() -> SegmentLabel {
        SegmentLabel::Stock
    }

    /// The §6.3 fixture rows, at label level: every combination of
    /// immutable labels and buffer label lands on exactly one class.
    #[test]
    fn relation_matrix_matches_the_design_table() {
        let cases: &[(&[SegmentLabel], BufferLabel, RelationClass)] = &[
            // All ValidV2 beside every generation-neutral buffer.
            (&[v2(), v2()], BufferLabel::Empty, RelationClass::Current),
            (&[v2()], BufferLabel::Current, RelationClass::Current),
            (&[v2()], BufferLabel::NoTerms, RelationClass::Current),
            // Empty directory, live or neutral buffer.
            (&[], BufferLabel::Empty, RelationClass::Current),
            (&[], BufferLabel::Current, RelationClass::Current),
            (&[], BufferLabel::NoTerms, RelationClass::Current),
            // Stock blobs are outside S: single-column shapes stay Current.
            (
                &[stock(), stock()],
                BufferLabel::Current,
                RelationClass::Current,
            ),
            (&[stock()], BufferLabel::Empty, RelationClass::Current),
            // All ValidV1, any buffer that is not the live generation.
            (
                &[v1(), v1()],
                BufferLabel::Empty,
                RelationClass::StaleFielded,
            ),
            (&[v1()], BufferLabel::Stale, RelationClass::StaleFielded),
            // Tagged zero-term is generation-neutral: v1-only stays stale.
            (&[v1()], BufferLabel::NoTerms, RelationClass::StaleFielded),
            (&[], BufferLabel::Stale, RelationClass::StaleFielded),
            // Both generations, everything well-formed.
            (
                &[v1(), v2()],
                BufferLabel::Empty,
                RelationClass::MixedFielded,
            ),
            (
                &[v1(), v2()],
                BufferLabel::NoTerms,
                RelationClass::MixedFielded,
            ),
            (&[v2()], BufferLabel::Stale, RelationClass::MixedFielded),
            (&[v1()], BufferLabel::Current, RelationClass::MixedFielded),
            // Malformed segments or buffer dominate every mix.
            (
                &[v1(), v2(), SegmentLabel::Malformed("trailer".into())],
                BufferLabel::Empty,
                RelationClass::Corrupt("trailer".into()),
            ),
            (
                &[v1(), v2(), SegmentLabel::Malformed("no trailer".into())],
                BufferLabel::Empty,
                RelationClass::Corrupt("no trailer".into()),
            ),
            (
                &[v2()],
                BufferLabel::Malformed("counts"),
                RelationClass::Corrupt("write buffer: counts".into()),
            ),
            // A malformed blob beside a stale buffer is Corrupt, not a
            // rebuild class (the retired any_v2_segment proxy answered
            // MixedFielded here).
            (
                &[v1(), SegmentLabel::Malformed("truncated".into())],
                BufferLabel::Stale,
                RelationClass::Corrupt("truncated".into()),
            ),
        ];
        for (segments, buffer, class) in cases {
            assert_eq!(
                relation_class(segments, *buffer),
                *class,
                "segments {segments:?} + buffer {buffer:?}"
            );
        }
    }

    #[test]
    fn stock_labels_contribute_to_neither_generation() {
        // A stock blob beside a v1 blob is still StaleFielded: Stock is
        // outside S, so it cannot lift the relation to mixed.
        assert_eq!(
            relation_class(&[stock(), v1()], BufferLabel::Empty),
            RelationClass::StaleFielded
        );
        assert_eq!(
            relation_class(&[stock(), v2()], BufferLabel::Stale),
            RelationClass::MixedFielded
        );
    }
}
