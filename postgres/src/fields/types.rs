// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Fielded lookup results. `text` is an owned decoded surface token.

use segment::segment::Term;

/// One field's posting stream for a decoded token.
pub(crate) struct FieldTerm<'a> {
    pub(crate) field: u8,
    pub(crate) term: Term<'a>,
}

/// One scoring key: one decoded token under one scope mask.
pub(crate) struct LogicalTerm<'a> {
    pub(crate) text: String,
    pub(crate) mask: u16,
    pub(crate) df_agg: u64,
    pub(crate) streams: Vec<FieldTerm<'a>>,
}

impl<'a> LogicalTerm<'a> {
    pub(crate) fn absent(text: String, mask: u16) -> Self {
        Self {
            text,
            mask,
            df_agg: 0,
            streams: Vec::new(),
        }
    }

    /// Opens the parent entry's channels (or the stock stream when
    /// `field_count == 1`) and keeps those whose field bit is in `mask`.
    /// `df_agg` is the parent `Term::df()` and does not change with the mask.
    pub(crate) fn from_entry(
        text: String,
        mask: u16,
        field_count: u8,
        term: Term<'a>,
    ) -> segment::Result<Self> {
        let df_agg = u64::from(term.df());
        let streams = if field_count == 1 {
            if mask & 1 != 0 {
                vec![FieldTerm { field: 0, term }]
            } else {
                Vec::new()
            }
        } else {
            term.channels(field_count)?
                .into_iter()
                .filter(|(field, _)| mask & (1u16 << field) != 0)
                .map(|(field, child)| FieldTerm { field, term: child })
                .collect()
        };
        Ok(Self {
            text,
            mask,
            df_agg,
            streams,
        })
    }
}

/// Direct lookup yields [`Lookup::Term`] (empty streams if absent). Expansion
/// yields [`Lookup::Terms`] or [`Lookup::Overflow`].
#[derive(Debug)]
pub(crate) enum Lookup<'a> {
    Term(LogicalTerm<'a>),
    Terms(Vec<LogicalTerm<'a>>),
    Overflow,
}

/// Fields whose bit is set in `mask`, in left-to-right ordinal order.
pub(crate) fn fields_in_mask(mask: u16, field_count: u8) -> impl Iterator<Item = u8> {
    (0..field_count.min(16)).filter(move |field| mask & (1u16 << field) != 0)
}

impl std::fmt::Debug for FieldTerm<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FieldTerm")
            .field("field", &self.field)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for LogicalTerm<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogicalTerm")
            .field("text", &self.text)
            .field("mask", &self.mask)
            .field("df_agg", &self.df_agg)
            .field(
                "streams",
                &self
                    .streams
                    .iter()
                    .map(|stream| stream.field)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}
