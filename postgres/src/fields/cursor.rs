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

struct FieldStream<'a> {
    field: u8,
    ordinals: OrdinalCursor<'a>,
    payload: Payload<'a>,
    /// Next ordinal that truncates the fused bound interval: a covering
    /// stream's successor, or a not-yet-covering stream's current.
    successor: Option<u32>,
}

/// `current_ordinal`, `advance` (every stream to ≥ target; equal ordinals
/// coalesce), `field_hits`, `next_bound_interval` (skeleton hook for 4.4).
pub(crate) struct LogicalPostingCursor<'a> {
    fields: Vec<FieldStream<'a>>,
    current: Option<u32>,
}

impl<'a> LogicalPostingCursor<'a> {
    pub(crate) fn open(streams: &[FieldTerm<'a>]) -> segment::Result<Self> {
        let mut fields = Vec::with_capacity(streams.len());
        for stream in streams {
            fields.push(FieldStream {
                field: stream.field,
                ordinals: stream.term.ordinals()?.cursor()?,
                payload: stream.term.payload()?,
                successor: None,
            });
        }
        let mut cursor = Self {
            fields,
            current: None,
        };
        cursor.advance(0)?;
        Ok(cursor)
    }

    #[must_use]
    pub(crate) fn current_ordinal(&self) -> Option<u32> {
        self.current
    }

    /// Step every stream to the first ordinal at or after `target`. Equal
    /// ordinals coalesce into one candidate.
    ///
    /// Published `current` and every stream `successor` are cleared before
    /// any seek. `current` is published only after every seek (and successor
    /// peek) succeeds, so a mid-loop error cannot leave a stale ordinal
    /// together with already-moved streams.
    pub(crate) fn advance(&mut self, target: u32) -> segment::Result<()> {
        self.current = None;
        for stream in &mut self.fields {
            stream.successor = None;
        }

        let mut min = None;
        for stream in &mut self.fields {
            stream.ordinals.rewind()?;
            stream.ordinals.seek(target)?;
            if let Some(at) = stream.ordinals.current() {
                min = Some(min.map_or(at, |seen: u32| seen.min(at)));
            }
        }
        let Some(here) = min else {
            return Ok(());
        };

        let mut successors = Vec::with_capacity(self.fields.len());
        for stream in &mut self.fields {
            successors.push(match stream.ordinals.current() {
                Some(at) if at == here => peek_successor(&mut stream.ordinals)?,
                Some(at) => Some(at),
                None => None,
            });
        }
        for (stream, successor) in self.fields.iter_mut().zip(successors) {
            stream.successor = successor;
        }
        self.current = Some(here);
        Ok(())
    }

    /// Each field that posts at `current_ordinal`, with its payload positions.
    pub(crate) fn field_hits(&self) -> segment::Result<Vec<FieldHit>> {
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
            let entry = stream.payload.get(rank)?;
            hits.push(FieldHit {
                field: stream.field,
                bucket,
                positions: entry.positions,
            });
        }
        Ok(hits)
    }

    /// Exclusive end of the fused bound interval covering `current_ordinal`.
    ///
    /// Design §5.1 intersecting-blocks truncation: the fused interval ends at
    /// the earlier of (1) the end of the blocks that currently cover the
    /// pivot and (2) the start of the next block, chunk, or sub-block of any
    /// mask-internal stream, including a stream that does not yet cover the
    /// pivot. Witness: a title block covering 0..1000 with a body block that
    /// starts at 500 must truncate at 500 — a body posting there raises
    /// `tf*`, and holding the title-only bound across that start breaks
    /// `exact_score ≤ fused_bound`. Plan 4.4 implements that rule; this hook
    /// currently returns the earliest stream successor.
    #[must_use]
    pub(crate) fn next_bound_interval(&self) -> Option<u32> {
        self.fields
            .iter()
            .filter_map(|stream| stream.successor)
            .min()
    }
}

impl<'a> LogicalTerm<'a> {
    pub(crate) fn cursor(&self) -> segment::Result<LogicalPostingCursor<'a>> {
        LogicalPostingCursor::open(&self.streams)
    }
}

fn peek_successor(ordinals: &mut OrdinalCursor<'_>) -> segment::Result<Option<u32>> {
    let Some(here) = ordinals.current() else {
        return Ok(None);
    };
    ordinals.advance()?;
    let next = ordinals.current();
    ordinals.rewind()?;
    ordinals.seek(here)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use segment::Tid;
    use segment::forward::ForwardRecord;
    use segment::index::MutableIndex;

    use super::*;
    use crate::fields::expand::lookup;
    use crate::fields::score::all_fields_mask;
    use crate::fields::types::Lookup;
    use crate::fields::{FieldTerm, fielded_key};

    const FIELDS: u8 = 2;
    const MASK: u16 = 0b11;

    fn add(index: &MutableIndex, id: u32, tokens: &[(&str, u32)]) {
        index
            .add_record(
                ForwardRecord::from_tokens(Tid::new(id, 1).unwrap(), tokens.iter().copied())
                    .unwrap(),
            )
            .unwrap();
    }

    fn fixture() -> MutableIndex {
        let index = MutableIndex::default();
        let both0 = fielded_key(0, "foo", FIELDS).unwrap();
        let both1 = fielded_key(1, "foo", FIELDS).unwrap();
        add(&index, 0, &[(both0.as_str(), 1), (both1.as_str(), 2)]);
        let one = fielded_key(0, "foo", FIELDS).unwrap();
        add(&index, 1, &[(one.as_str(), 1)]);
        let other = fielded_key(1, "bar", FIELDS).unwrap();
        add(&index, 2, &[(other.as_str(), 1)]);
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
        assert_eq!(hits0[1].positions, vec![2]);
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
    fn unscoped_mask_opens_every_field_and_empty_streams_are_not_an_error() {
        assert_eq!(all_fields_mask(FIELDS), MASK);
        let index = MutableIndex::default();
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
