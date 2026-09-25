// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

/// One matched query part over token-position coordinates.
///
/// `start` / `end` are inclusive token positions in the indexed token stream.
/// Point matches use `start == end`. `field` is the field id the match
/// belongs to (0 on a single-column index or a fieldless call); a
/// multi-column renderer picks each field's positions by it, so marks never
/// leak into another field's text (RFC §5.11 highlights).
#[derive(Debug, PartialEq)]
pub struct MatchPosition {
    pub part: String,
    pub start: i32,
    pub end: i32,
    pub field: u16,
}

impl MatchPosition {
    pub(crate) fn point(part: impl Into<String>, pos: u32) -> Self {
        Self::span(part, pos, pos, 0)
    }

    pub(crate) fn span(part: impl Into<String>, start: u32, end: u32, field: u16) -> Self {
        Self {
            part: part.into(),
            start: start as i32,
            end: end as i32,
            field,
        }
    }
}
