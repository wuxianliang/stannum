// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Union cursor over a logical term's field streams. Wraps, never copies.

use segment::ordinals::OrdinalCursor;
use segment::payload::Payload;

use super::types::{FieldTerm, LogicalTerm};

/// One field's payload and positions at the cursor's current ordinal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FieldHit {
    pub(crate) field: u8,
    pub(crate) bucket: u8,
    pub(crate) positions: Vec<u32>,
}

/// One field's stored term-frequency bucket at the cursor's current ordinal.
/// Positions stay on the payload stream until a phrase needs them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FieldTf {
    pub(crate) field: u8,
    pub(crate) bucket: u8,
}

struct FieldStream<'a> {
    field: u8,
    ordinals: OrdinalCursor<'a>,
    payload: Payload<'a>,
}

/// `current_ordinal`, `advance` (every stream to ≥ target; equal ordinals
/// coalesce), `field_hits`, `next_bound_interval` (fused block/chunk/sub
/// truncation for plan 4.4).
pub(crate) struct LogicalPostingCursor<'a> {
    fields: Vec<FieldStream<'a>>,
    current: Option<u32>,
    /// True after a forward walk ran off the end. The next `advance` to a
    /// smaller target rewinds; a sequential `ordinal+1` walk never does.
    exhausted: bool,
}

impl<'a> LogicalPostingCursor<'a> {
    pub(crate) fn open(streams: &[FieldTerm<'a>]) -> segment::Result<Self> {
        let span = super::profile::Span::begin();
        let mut fields = Vec::with_capacity(streams.len());
        for stream in streams {
            fields.push(FieldStream {
                field: stream.field,
                ordinals: stream.term.ordinals()?.cursor()?,
                payload: stream.term.payload()?,
            });
        }
        let mut cursor = Self {
            fields,
            current: None,
            exhausted: false,
        };
        cursor.advance(0)?;
        super::profile::add_cursor_open(std::time::Duration::from_nanos(span.ns()));
        Ok(cursor)
    }

    #[must_use]
    pub(crate) fn current_ordinal(&self) -> Option<u32> {
        self.current
    }

    /// Step every stream to the first ordinal at or after `target`. Equal
    /// ordinals coalesce into one candidate.
    ///
    /// Sequential walks (`target` ≥ the published current) seek forward only.
    /// A backward seek, or a seek after the union is exhausted, rewinds first.
    /// `current` is cleared before any seek and published only after every
    /// seek succeeds, so a mid-loop error cannot leave a stale ordinal
    /// together with already-moved streams.
    pub(crate) fn advance(&mut self, target: u32) -> segment::Result<()> {
        let span = super::profile::Span::begin();
        let result = self.advance_inner(target);
        super::profile::add_advance(std::time::Duration::from_nanos(span.ns()));
        result
    }

    fn advance_inner(&mut self, target: u32) -> segment::Result<()> {
        let previous = self.current;
        self.current = None;
        let going_back = previous.is_some_and(|here| target < here) || self.exhausted;

        let seek_span = super::profile::Span::begin();
        let mut min = None;
        for stream in &mut self.fields {
            if going_back {
                stream.ordinals.rewind()?;
                stream.ordinals.seek(target)?;
            } else if stream.ordinals.current().is_some_and(|at| at < target) {
                stream.ordinals.seek(target)?;
            }
            if let Some(at) = stream.ordinals.current() {
                min = Some(min.map_or(at, |seen: u32| seen.min(at)));
            }
        }
        super::profile::add_rewind_seek(std::time::Duration::from_nanos(seek_span.ns()));
        match min {
            Some(here) => {
                self.exhausted = false;
                self.current = Some(here);
                super::profile::add_advance_span(here.saturating_sub(previous.unwrap_or(0)));
            }
            None => {
                self.exhausted = previous.is_some() || self.exhausted;
            }
        }
        Ok(())
    }

    #[must_use]
    pub(crate) fn is_exhausted(&self) -> bool {
        self.exhausted && self.current.is_none()
    }

    /// Each field that posts at `current_ordinal`, with its payload positions.
    pub(crate) fn field_hits(&self) -> segment::Result<Vec<FieldHit>> {
        let span = super::profile::Span::begin();
        let hits = self.field_hits_inner()?;
        let positions = hits.iter().map(|hit| hit.positions.len() as u64).sum();
        super::profile::add_field_hits(
            std::time::Duration::from_nanos(span.ns()),
            hits.len() as u64,
            positions,
        );
        Ok(hits)
    }

    /// Each field that posts at `current_ordinal`, with the ordinal-stream
    /// term-frequency bucket. Does not open the payload.
    pub(crate) fn field_tfs(&self) -> segment::Result<Vec<FieldTf>> {
        let span = super::profile::Span::begin();
        let tfs = self.field_tfs_inner()?;
        super::profile::add_bucket(std::time::Duration::from_nanos(span.ns()));
        Ok(tfs)
    }

    fn field_hits_inner(&self) -> segment::Result<Vec<FieldHit>> {
        let Some(current) = self.current else {
            return Ok(Vec::new());
        };
        let mut hits = Vec::new();
        for stream in &self.fields {
            if stream.ordinals.current() != Some(current) {
                continue;
            }
            let rank = stream.ordinals.rank();
            let bucket = stream
                .ordinals
                .bucket()
                .ok_or(segment::Error::Corrupt("fielded posting missing tf bucket"))?;
            let payload_span = super::profile::Span::begin();
            let entry = stream.payload.get(rank)?;
            super::profile::add_payload_get(std::time::Duration::from_nanos(payload_span.ns()));
            hits.push(FieldHit {
                field: stream.field,
                bucket,
                positions: entry.positions,
            });
        }
        Ok(hits)
    }

    fn field_tfs_inner(&self) -> segment::Result<Vec<FieldTf>> {
        let Some(current) = self.current else {
            return Ok(Vec::new());
        };
        let mut tfs = Vec::new();
        for stream in &self.fields {
            if stream.ordinals.current() != Some(current) {
                continue;
            }
            let bucket = stream
                .ordinals
                .bucket()
                .ok_or(segment::Error::Corrupt("fielded posting missing tf bucket"))?;
            tfs.push(FieldTf {
                field: stream.field,
                bucket,
            });
        }
        Ok(tfs)
    }

    /// Exclusive end of the fused bound interval covering `current_ordinal`.
    ///
    /// Design §5.1: the earlier of the covering-block exclusive ends and the
    /// next block, chunk, or sub-block start of any mask-internal stream,
    /// including a stream that does not yet cover the pivot. A list's
    /// not-yet-covering start is its first ordinal. `max_tf*` / `min_len*`
    /// over the interval account for every intersecting block.
    #[must_use]
    pub(crate) fn next_bound_interval(&self) -> Option<u32> {
        let pivot = self.current?;
        let streams: Vec<&segment::ordinals::Ordinals<'_>> = self
            .fields
            .iter()
            .map(|stream| stream.ordinals.stream())
            .collect();
        super::bound::next_interval_end(&streams, pivot)
    }
}

impl<'a> LogicalTerm<'a> {
    pub(crate) fn cursor(&self) -> segment::Result<LogicalPostingCursor<'a>> {
        LogicalPostingCursor::open(&self.streams)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use segment::Tid;
    use segment::index::MutableIndex;

    use super::*;
    use crate::fields::expand::lookup;
    use crate::fields::score::all_fields_mask;
    use crate::fields::types::Lookup;

    const FIELDS: u8 = 2;
    const MASK: u16 = 0b11;

    fn add_fielded(index: &MutableIndex, id: u32, columns: &[&str]) {
        index
            .begin_fielded_document(Tid::new(id, 1).unwrap())
            .unwrap();
        for (field, text) in columns.iter().enumerate() {
            let mut by_term: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
            let mut len = 0u32;
            for (i, word) in text.split_whitespace().enumerate() {
                len += 1;
                by_term.entry(word).or_default().push(i as u32 + 1);
            }
            if len == 0 {
                continue;
            }
            for (word, positions) in by_term {
                index
                    .add_occurrence(word, field as u8, &positions, len)
                    .unwrap();
            }
        }
    }

    fn fixture() -> MutableIndex {
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        add_fielded(&index, 0, &["foo", "foo"]);
        add_fielded(&index, 1, &["foo", ""]);
        add_fielded(&index, 2, &["", "bar"]);
        index
    }

    fn collect(cursor: &mut LogicalPostingCursor<'_>) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(ordinal) = cursor.current_ordinal() {
            let fields: Vec<u8> = cursor
                .field_hits()
                .unwrap()
                .into_iter()
                .map(|hit| hit.field)
                .collect();
            out.push((ordinal, fields));
            cursor.advance(ordinal.saturating_add(1)).unwrap();
        }
        out
    }

    #[test]
    fn token_in_both_fields_one_field_and_absent() {
        let index = fixture();
        let Lookup::Term(both) = lookup(&index, "foo", MASK, FIELDS).unwrap() else {
            panic!("direct lookup returns Lookup::Term");
        };
        assert_eq!(both.text, "foo");
        assert_eq!(both.mask, MASK);
        assert_eq!(
            both.streams.iter().map(|s| s.field).collect::<Vec<_>>(),
            vec![0, 1]
        );
        let cursor = both.cursor().unwrap();
        let hits0 = cursor.field_hits().unwrap();
        assert_eq!(cursor.current_ordinal(), Some(0));
        assert_eq!(hits0.len(), 2);
        assert_eq!(hits0[0].field, 0);
        assert_eq!(hits0[0].positions, vec![1]);
        assert_eq!(hits0[1].field, 1);
        assert_eq!(hits0[1].positions, vec![1]);
        assert_eq!(cursor.next_bound_interval(), Some(1));

        let walked = collect(&mut both.cursor().unwrap());
        assert_eq!(walked, vec![(0, vec![0, 1]), (1, vec![0])]);

        let Lookup::Term(one) = lookup(&index, "bar", MASK, FIELDS).unwrap() else {
            panic!("direct lookup returns Lookup::Term");
        };
        assert_eq!(
            one.mask, MASK,
            "collapsed mask is the query scope, not the body-only hit-set"
        );
        assert_eq!(
            one.streams.iter().map(|s| s.field).collect::<Vec<_>>(),
            vec![1]
        );
        let walked = collect(&mut one.cursor().unwrap());
        assert_eq!(walked, vec![(2, vec![1])]);

        let Lookup::Term(absent) = lookup(&index, "zzz", MASK, FIELDS).unwrap() else {
            panic!("absent is Lookup::Term, not an error");
        };
        assert!(absent.streams.is_empty());
        let mut cursor = absent.cursor().unwrap();
        assert_eq!(cursor.current_ordinal(), None);
        assert!(cursor.field_hits().unwrap().is_empty());
        assert_eq!(cursor.next_bound_interval(), None);
        assert_eq!(collect(&mut cursor), Vec::<(u32, Vec<u8>)>::new());
    }

    #[test]
    fn next_bound_interval_is_list_block_end_not_posting_successor() {
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        add_fielded(&index, 0, &["foo", ""]);
        add_fielded(&index, 1, &["foo", ""]);
        let Lookup::Term(term) = lookup(&index, "foo", MASK, FIELDS).unwrap() else {
            panic!("foo");
        };
        assert_eq!(
            term.streams.iter().map(|s| s.field).collect::<Vec<_>>(),
            vec![0]
        );
        let cursor = term.cursor().unwrap();
        assert_eq!(cursor.current_ordinal(), Some(0));
        assert_eq!(
            cursor.next_bound_interval(),
            Some(2),
            "list [0, 1] is one block whose exclusive end is last+1, not the next posting"
        );
    }

    #[test]
    fn unscoped_mask_opens_every_field_and_empty_streams_are_not_an_error() {
        assert_eq!(all_fields_mask(FIELDS), MASK);
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        let Lookup::Term(missing) = lookup(&index, "foo", all_fields_mask(FIELDS), FIELDS).unwrap()
        else {
            panic!("empty index lookup is Term with empty streams");
        };
        assert!(missing.streams.is_empty());
        let empty: &[FieldTerm<'_>] = &[];
        let cursor = LogicalPostingCursor::open(empty).unwrap();
        assert_eq!(cursor.current_ordinal(), None);
    }

    #[test]
    fn body_only_cursor_mask_is_query_scope_not_the_hit_set() {
        let index = fixture();
        let Lookup::Term(bar) = lookup(&index, "bar", MASK, FIELDS).unwrap() else {
            panic!("body-only lookup");
        };
        assert_eq!(bar.mask, MASK);
        assert_ne!(bar.mask, 1u16 << 1);
        assert_eq!(
            bar.streams.iter().map(|s| s.field).collect::<Vec<_>>(),
            vec![1]
        );
        let walked = collect(&mut bar.cursor().unwrap());
        assert_eq!(walked, vec![(2, vec![1])]);
    }

    #[test]
    fn advance_clears_current_and_successors_before_seek_and_publishes_after() {
        let index = fixture();
        let Lookup::Term(both) = lookup(&index, "foo", MASK, FIELDS).unwrap() else {
            panic!("foo");
        };
        let mut cursor = both.cursor().unwrap();
        assert_eq!(cursor.current_ordinal(), Some(0));
        assert_eq!(cursor.next_bound_interval(), Some(1));

        cursor.advance(u32::MAX).unwrap();
        assert_eq!(cursor.current_ordinal(), None);
        assert_eq!(cursor.next_bound_interval(), None);

        cursor.advance(0).unwrap();
        assert_eq!(cursor.current_ordinal(), Some(0));
        assert_eq!(cursor.next_bound_interval(), Some(1));
        assert_eq!(
            cursor
                .field_hits()
                .unwrap()
                .into_iter()
                .map(|hit| hit.field)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }
}
