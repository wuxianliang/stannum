// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! One document as a single record for a mutable write buffer.
//!
//! The legacy codec has no field id and remains byte-for-byte compatible.  The
//! field codec is selected by the fields trailer and adds one field-id varint
//! before each term group.

use crate::payload::{decode_positions, encode_positions, validate_positions};
use crate::reader::Reader;
use crate::{Error, Result, Tid, varint};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardTerm {
    pub field: u8,
    pub term: String,
    pub positions: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordHeader {
    pub tid: Tid,
    pub doc_len: u32,
    pub term_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardRecord {
    pub tid: Tid,
    pub doc_len: u32,
    /// Empty for the legacy fieldless codec; otherwise one entry per field.
    pub field_lengths: Vec<u32>,
    /// Sorted by `(term bytes, field)` and unique.
    pub terms: Vec<ForwardTerm>,
}

impl ForwardRecord {
    pub fn from_tokens<'t>(
        tid: Tid,
        tokens: impl IntoIterator<Item = (&'t str, u32)>,
    ) -> Result<Self> {
        let mut by_term = std::collections::BTreeMap::<&str, Vec<u32>>::new();
        let mut doc_len = 0u32;
        let mut last_position = None;
        for (term, position) in tokens {
            if term.is_empty() {
                return Err(Error::EmptyTerm);
            }
            if last_position.is_some_and(|last| last >= position) {
                return Err(Error::InvalidPositions);
            }
            last_position = Some(position);
            doc_len = doc_len
                .checked_add(1)
                .ok_or(Error::Corrupt("forward length"))?;
            by_term.entry(term).or_default().push(position);
        }
        Ok(Self {
            tid,
            doc_len,
            field_lengths: Vec::new(),
            terms: by_term
                .into_iter()
                .map(|(term, positions)| ForwardTerm {
                    field: 0,
                    term: term.to_owned(),
                    positions,
                })
                .collect(),
        })
    }

    /// Builds a field-aware record. Positions are independent and must be
    /// strictly increasing within each field; field groups are sorted on
    /// encode so callers may provide tokens in any field order.
    pub fn from_tokens_fields<'t>(
        tid: Tid,
        field_count: u8,
        tokens: impl IntoIterator<Item = (u8, &'t str, u32)>,
    ) -> Result<Self> {
        if !(1..=16).contains(&field_count) {
            return Err(Error::Corrupt("forward field count"));
        }
        let mut by_term = std::collections::BTreeMap::<(&str, u8), Vec<u32>>::new();
        let mut lengths = vec![0u32; usize::from(field_count)];
        let mut last = vec![None; usize::from(field_count)];
        for (field, term, position) in tokens {
            if usize::from(field) >= lengths.len() {
                return Err(Error::Corrupt("forward field id"));
            }
            if term.is_empty() {
                return Err(Error::EmptyTerm);
            }
            if last[usize::from(field)].is_some_and(|p| p >= position) {
                return Err(Error::InvalidPositions);
            }
            last[usize::from(field)] = Some(position);
            lengths[usize::from(field)] = lengths[usize::from(field)]
                .checked_add(1)
                .ok_or(Error::Corrupt("forward field length"))?;
            by_term.entry((term, field)).or_default().push(position);
        }
        let doc_len = lengths
            .iter()
            .try_fold(0u32, |sum, len| sum.checked_add(*len))
            .ok_or(Error::Corrupt("forward length"))?;
        Ok(Self {
            tid,
            doc_len,
            field_lengths: lengths,
            terms: by_term
                .into_iter()
                .map(|((term, field), positions)| ForwardTerm {
                    field,
                    term: term.to_owned(),
                    positions,
                })
                .collect(),
        })
    }

    fn validate(&self) -> Result<()> {
        Tid::new(self.tid.block, self.tid.offset)?;
        if self.field_lengths.len() > 16 {
            return Err(Error::Corrupt("forward field count"));
        }
        let fields = !self.field_lengths.is_empty();
        let mut previous: Option<(&[u8], u8)> = None;
        let mut counted = 0u32;
        for term in &self.terms {
            if term.term.is_empty() {
                return Err(Error::EmptyTerm);
            }
            if fields {
                if usize::from(term.field) >= self.field_lengths.len() {
                    return Err(Error::Corrupt("forward field id"));
                }
            } else if term.field != 0 {
                return Err(Error::Corrupt("forward field id"));
            }
            if previous.is_some_and(|(p, f)| {
                p > term.term.as_bytes() || (p == term.term.as_bytes() && f >= term.field)
            }) {
                return Err(Error::Unordered);
            }
            validate_positions(&term.positions)?;
            counted = counted
                .checked_add(term.positions.len() as u32)
                .ok_or(Error::Corrupt("forward length"))?;
            previous = Some((term.term.as_bytes(), term.field));
        }
        if counted != self.doc_len {
            return Err(Error::Corrupt("forward document length"));
        }
        if fields {
            let mut lengths = vec![0u32; self.field_lengths.len()];
            for term in &self.terms {
                lengths[usize::from(term.field)] += term.positions.len() as u32;
            }
            if lengths != self.field_lengths {
                return Err(Error::Corrupt("forward field lengths"));
            }
        }
        Ok(())
    }

    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        self.validate()?;
        let mut body = Vec::new();
        varint::put(&mut body, u64::from(self.tid.block));
        varint::put(&mut body, u64::from(self.tid.offset));
        varint::put(&mut body, u64::from(self.doc_len));
        varint::put(&mut body, self.terms.len() as u64);
        let fields = !self.field_lengths.is_empty();
        let mut previous: &[u8] = &[];
        for term in &self.terms {
            if fields {
                varint::put(&mut body, u64::from(term.field));
            }
            let bytes = term.term.as_bytes();
            let shared = previous
                .iter()
                .zip(bytes)
                .take_while(|(a, b)| a == b)
                .count();
            varint::put(&mut body, shared as u64);
            varint::put(&mut body, (bytes.len() - shared) as u64);
            body.extend_from_slice(&bytes[shared..]);
            encode_positions(&mut body, &term.positions);
            previous = bytes;
        }
        varint::put(out, body.len() as u64);
        out.extend_from_slice(&body);
        Ok(())
    }

    pub fn encoded_len(bytes: &[u8]) -> Result<usize> {
        let mut reader = Reader::new(bytes);
        let len = reader.varint()? as usize;
        reader
            .position()
            .checked_add(len)
            .filter(|n| *n <= bytes.len())
            .ok_or(Error::Truncated)
    }

    pub fn decode(bytes: &[u8]) -> Result<(Self, usize)> {
        Self::decode_mode(bytes, false, 0)
    }

    pub fn decode_fields(bytes: &[u8], field_count: u8) -> Result<(Self, usize)> {
        if field_count == 0 || field_count > 16 {
            return Err(Error::Corrupt("segment field count"));
        }
        Self::decode_mode(bytes, true, field_count)
    }

    fn decode_mode(bytes: &[u8], fields: bool, field_count: u8) -> Result<(Self, usize)> {
        let mut terms = Vec::new();
        let (header, total) = Self::decode_with_mode(bytes, fields, |field, term, positions| {
            terms.push(ForwardTerm {
                field,
                term: term.to_owned(),
                positions: positions.to_vec(),
            });
            Ok(())
        })?;
        let field_lengths = if fields {
            let count = usize::from(field_count.max(1));
            let mut lengths = vec![0u32; count];
            for term in &terms {
                let slot = lengths
                    .get_mut(usize::from(term.field))
                    .ok_or(Error::Corrupt("forward field id"))?;
                *slot = slot
                    .checked_add(term.positions.len() as u32)
                    .ok_or(Error::Corrupt("forward field length"))?;
            }
            lengths
        } else {
            Vec::new()
        };
        let record = Self {
            tid: header.tid,
            doc_len: header.doc_len,
            field_lengths,
            terms,
        };
        Ok((record, total))
    }

    pub fn peek(bytes: &[u8]) -> Result<RecordHeader> {
        let mut reader = Reader::new(bytes);
        let len = reader.varint()? as usize;
        let mut body = Reader::new(reader.take(len)?);
        let block = body.varint_u32()?;
        let offset = u16::try_from(body.varint_u32()?).map_err(|_| Error::InvalidTid)?;
        Ok(RecordHeader {
            tid: Tid::new(block, offset)?,
            doc_len: body.varint_u32()?,
            term_count: body.varint_u32()?,
        })
    }

    pub fn decode_with(
        bytes: &[u8],
        mut visit: impl FnMut(&str, &[u32]) -> Result<()>,
    ) -> Result<(RecordHeader, usize)> {
        Self::decode_with_mode(bytes, false, |_, term, positions| visit(term, positions))
    }

    pub fn decode_with_fields(
        bytes: &[u8],
        visit: impl FnMut(u8, &str, &[u32]) -> Result<()>,
    ) -> Result<(RecordHeader, usize)> {
        Self::decode_with_mode(bytes, true, visit)
    }

    fn decode_with_mode(
        bytes: &[u8],
        fields: bool,
        mut visit: impl FnMut(u8, &str, &[u32]) -> Result<()>,
    ) -> Result<(RecordHeader, usize)> {
        let total = Self::encoded_len(bytes)?;
        let mut reader = Reader::new(bytes);
        let len = reader.varint()? as usize;
        let mut body = Reader::new(reader.take(len)?);
        let tid = Tid::new(
            body.varint_u32()?,
            u16::try_from(body.varint_u32()?).map_err(|_| Error::InvalidTid)?,
        )?;
        let header = RecordHeader {
            tid,
            doc_len: body.varint_u32()?,
            term_count: body.varint_u32()?,
        };
        let mut term = Vec::new();
        let mut positions = Vec::new();
        let mut previous: Option<(Vec<u8>, u8)> = None;
        for _ in 0..header.term_count {
            let field = if fields {
                u8::try_from(body.varint_u32()?).map_err(|_| Error::Corrupt("forward field id"))?
            } else {
                0
            };
            let shared = body.varint_u32()? as usize;
            if shared > term.len() {
                return Err(Error::Corrupt("forward term prefix"));
            }
            let suffix_len = body.varint_u32()? as usize;
            let suffix = body.take(suffix_len)?.to_vec();
            term.truncate(shared);
            term.extend_from_slice(&suffix);
            if term.is_empty() {
                return Err(Error::Corrupt("forward term order"));
            }
            if previous.as_ref().is_some_and(|(p, f)| {
                p.as_slice() > term.as_slice() || (p.as_slice() == term.as_slice() && *f >= field)
            }) {
                return Err(Error::Corrupt("forward term order"));
            }
            positions.clear();
            decode_positions(&mut body, &mut positions)?;
            let text =
                std::str::from_utf8(&term).map_err(|_| Error::Corrupt("forward term UTF-8"))?;
            visit(field, text, &positions)?;
            previous = Some((term.clone(), field));
        }
        if body.remaining() != 0 {
            return Err(Error::Corrupt("forward record length"));
        }
        Ok((header, total))
    }

    pub fn tokens(&self) -> Vec<(&str, u32)> {
        let mut out: Vec<(&str, u32)> = self
            .terms
            .iter()
            .flat_map(|term| term.positions.iter().map(move |p| (term.term.as_str(), *p)))
            .collect();
        out.sort_unstable_by_key(|(_, position)| *position);
        out
    }
}

pub fn records(mut bytes: &[u8]) -> impl Iterator<Item = Result<ForwardRecord>> + '_ {
    std::iter::from_fn(move || {
        if bytes.is_empty() {
            return None;
        }
        match ForwardRecord::decode(bytes) {
            Ok((record, consumed)) => {
                bytes = &bytes[consumed..];
                Some(Ok(record))
            }
            Err(error) => {
                bytes = &[];
                Some(Err(error))
            }
        }
    })
}

pub fn records_fields(
    mut bytes: &[u8],
    field_count: u8,
) -> impl Iterator<Item = Result<ForwardRecord>> + '_ {
    std::iter::from_fn(move || {
        if bytes.is_empty() {
            return None;
        }
        match ForwardRecord::decode_fields(bytes, field_count) {
            Ok((record, consumed)) => {
                bytes = &bytes[consumed..];
                Some(Ok(record))
            }
            Err(error) => {
                bytes = &[];
                Some(Err(error))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trips_tokens_and_packs_records() {
        let tid = Tid::new(12, 3).unwrap();
        let tokens = [("craft", 1), ("beer", 2), ("craft", 4), ("ale", 9)];
        let record = ForwardRecord::from_tokens(tid, tokens).unwrap();
        assert_eq!(record.doc_len, 4);
        assert_eq!(record.terms[2].positions, [1, 4]);
        assert_eq!(record.tokens(), tokens);
        let mut bytes = Vec::new();
        record.encode(&mut bytes).unwrap();
        let (decoded, consumed) = ForwardRecord::decode(&bytes).unwrap();
        assert_eq!(decoded, record);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn fields_round_trip_with_independent_positions() {
        let record = ForwardRecord::from_tokens_fields(
            Tid::new(1, 1).unwrap(),
            2,
            [(0, "x", 0), (1, "x", 0), (1, "y", 1)],
        )
        .unwrap();
        let mut bytes = Vec::new();
        record.encode(&mut bytes).unwrap();
        assert_eq!(ForwardRecord::decode_fields(&bytes, 2).unwrap().0, record);
        assert!(ForwardRecord::decode(&bytes).is_err());
    }
}
