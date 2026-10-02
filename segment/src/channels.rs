// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Per-field stock streams multiplexed inside one term extent.
//!
//! A multi-column `TermEntry` still has two extents. Each begins with the
//! same directory, then the concatenated stock STN3 streams of the fields
//! that have postings:
//!
//! ```text
//! channel_extent :=
//!     magic "FCH1"               # 4 bytes
//!     n        u8                # present fields, 1..=field_count
//!     record*  n                 # sorted by field ordinal, unique
//!     streams                    # concatenation, same order as records
//! record :=
//!     field    u8                # 0-based, < field_count
//!     len      u32le             # byte length of that field's stock stream
//! ```
//!
//! Directory size is `5 + 5n` (`"FCH1"` + `n` + `n × (1 + 4)`). There is no
//! version byte, and `len` is not a varint. A field with no postings is
//! absent from both directories. `field_count == 1` never writes a directory.

use crate::bound::BlockBound;
use crate::dictionary::{Extent, TermEntry};
use crate::ordinals::Ordinals;
use crate::payload::{Payload, PayloadCursor};
use crate::trailer::{MAX_FIELD_COUNT, MIN_FIELD_COUNT};
use crate::{Error, Result, varint};

/// Field CHannels, version 1. The four bytes `46 43 48 31`.
pub(crate) const MAGIC: &[u8; 4] = b"FCH1";

/// One field's already-encoded stock ordinal and payload streams.
///
/// A.2's flush is the production caller. Until then the tests are the writer,
/// so a non-test build does not name this type.
#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct FieldStreams {
    pub field: u8,
    pub ordinals: Vec<u8>,
    pub payload: Vec<u8>,
}

/// The two extents of one multi-column term, or the bare stock streams when
/// `field_count == 1`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct EncodedChannels {
    pub ordinals: Vec<u8>,
    pub payload: Vec<u8>,
}

/// `5 + 5n`, or corruption when that overflows `usize`.
fn directory_len(n: u128) -> Result<usize> {
    let n = usize::try_from(n).map_err(|_| Error::Corrupt("channel directory"))?;
    5usize
        .checked_mul(n)
        .and_then(|records| 5usize.checked_add(records))
        .ok_or(Error::Corrupt("channel directory"))
}

#[cfg_attr(not(test), allow(dead_code))]
fn stream_count(bytes: &[u8], what: &'static str) -> Result<u32> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let mut at = 0;
    varint::get_u32(bytes, &mut at).map_err(|err| match err {
        Error::Corrupt(msg) => Error::Corrupt(msg),
        _ => Error::Corrupt(what),
    })
}

fn as_corrupt(err: Error, fallback: &'static str) -> Error {
    match err {
        Error::Corrupt(msg) => Error::Corrupt(msg),
        _ => Error::Corrupt(fallback),
    }
}

struct Record {
    field: u8,
    len: u32,
}

/// Parses one extent's directory. `bytes` is the whole extent.
fn parse_directory(bytes: &[u8], field_count: u8) -> Result<(usize, Vec<Record>)> {
    if bytes.len() < 5 {
        return Err(Error::Corrupt("channel header"));
    }
    if bytes.get(..4) != Some(&MAGIC[..]) {
        return Err(Error::Corrupt("channel magic"));
    }
    let n = bytes[4];
    if n == 0 || n > field_count {
        return Err(Error::Corrupt("channel count"));
    }
    let dir = directory_len(u128::from(n))?;
    if dir > bytes.len() {
        return Err(Error::Corrupt("channel directory"));
    }
    let mut records = Vec::with_capacity(usize::from(n));
    let mut previous: Option<u8> = None;
    let mut sum = 0u32;
    for i in 0..usize::from(n) {
        let at = 5 + 5 * i;
        let field = bytes[at];
        let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().expect("4 bytes"));
        if field >= field_count {
            return Err(Error::Corrupt("channel field"));
        }
        if previous.is_some_and(|prev| field <= prev) {
            return Err(Error::Corrupt("channel field"));
        }
        previous = Some(field);
        if len == 0 {
            return Err(Error::Corrupt("channel length"));
        }
        sum = sum
            .checked_add(len)
            .ok_or(Error::Corrupt("channel length"))?;
        records.push(Record { field, len });
    }
    let rest = bytes.len() - dir;
    if u64::from(sum) != rest as u64 {
        return Err(Error::Corrupt("channel length"));
    }
    Ok((dir, records))
}

fn slices<'a>(bytes: &'a [u8], dir: usize, records: &[Record]) -> Result<Vec<&'a [u8]>> {
    let mut at = dir;
    let mut out = Vec::with_capacity(records.len());
    for record in records {
        let len = usize::try_from(record.len).map_err(|_| Error::Corrupt("channel length"))?;
        let end = at
            .checked_add(len)
            .ok_or(Error::Corrupt("channel length"))?;
        if end > bytes.len() {
            return Err(Error::Corrupt("channel length"));
        }
        out.push(&bytes[at..end]);
        at = end;
    }
    if at != bytes.len() {
        return Err(Error::Corrupt("channel length"));
    }
    Ok(out)
}

fn child_offset(parent: u64, local: usize) -> Result<u64> {
    let local = u64::try_from(local).map_err(|_| Error::Corrupt("channel length"))?;
    parent
        .checked_add(local)
        .ok_or(Error::Corrupt("channel length"))
}

/// Stock ordinals must fill `bytes` exactly. The count varint is the child df.
fn open_ordinals(bytes: &[u8]) -> Result<(u32, u8)> {
    let mut at = 0;
    let count =
        varint::get_u32(bytes, &mut at).map_err(|err| as_corrupt(err, "channel ordinals"))?;
    if count == 0 {
        return Err(Error::Corrupt("channel df"));
    }
    let stream = Ordinals::open(bytes, bytes.len() as u64, true)
        .map_err(|err| as_corrupt(err, "channel ordinals"))?;
    if stream.count() != count {
        return Err(Error::Corrupt("channel ordinals"));
    }
    let max_tf_bucket = {
        let bounds = stream
            .bounds()
            .map_err(|err| as_corrupt(err, "channel ordinals"))?;
        bounds.iter().fold(0u8, |max, bound| {
            max.max(
                BlockBound {
                    min_len: bound.min_len,
                }
                .max_tf_bucket(),
            )
        })
    };
    stream
        .ensure_exact()
        .map_err(|err| as_corrupt(err, "channel ordinals"))?;
    let members = stream
        .to_vec()
        .map_err(|err| as_corrupt(err, "channel ordinals"))?;
    if members.len() as u32 != count {
        return Err(Error::Corrupt("channel ordinals"));
    }
    Ok((count, max_tf_bucket))
}

/// Stock payload must fill `bytes` exactly and name `df` documents.
fn open_payload(bytes: &[u8], df: u32) -> Result<()> {
    let payload = Payload::parse(bytes).map_err(|err| as_corrupt(err, "channel payload"))?;
    if payload.count() != df {
        return Err(Error::Corrupt("channel payload count"));
    }
    let mut cursor = payload.cursor();
    for _ in 0..df {
        cursor
            .next_count()
            .map_err(|err| as_corrupt(err, "channel payload"))?;
    }
    let at = PayloadCursor::whole_position(&cursor).ok_or(Error::Corrupt("channel payload"))?;
    if at != bytes.len() {
        return Err(Error::Corrupt("channel payload"));
    }
    Ok(())
}

/// Unpacks both extents of a multi-column term. `field_count` must be in
/// `2..=16`; the caller (`Term::channels`) checks that first.
pub(crate) fn open_channels(
    field_count: u8,
    ordinals: Extent,
    ordinal_bytes: &[u8],
    payload: Extent,
    payload_bytes: &[u8],
) -> Result<Vec<(u8, TermEntry)>> {
    if !(MIN_FIELD_COUNT..=MAX_FIELD_COUNT).contains(&field_count) {
        return Err(Error::Corrupt("channel field_count"));
    }
    let (ord_dir, ord_records) = parse_directory(ordinal_bytes, field_count)?;
    let (pay_dir, pay_records) = parse_directory(payload_bytes, field_count)?;
    if ord_records.len() != pay_records.len()
        || ord_records
            .iter()
            .zip(&pay_records)
            .any(|(left, right)| left.field != right.field)
    {
        return Err(Error::Corrupt("channel directories"));
    }
    let ord_slices = slices(ordinal_bytes, ord_dir, &ord_records)?;
    let pay_slices = slices(payload_bytes, pay_dir, &pay_records)?;
    let mut children = Vec::with_capacity(ord_records.len());
    let mut ord_at = ord_dir;
    let mut pay_at = pay_dir;
    for (index, record) in ord_records.iter().enumerate() {
        let (df, max_tf_bucket) = open_ordinals(ord_slices[index])?;
        open_payload(pay_slices[index], df)?;
        let ord_off = child_offset(ordinals.offset, ord_at)?;
        let pay_off = child_offset(payload.offset, pay_at)?;
        children.push((
            record.field,
            TermEntry {
                df,
                max_tf_bucket,
                ordinals: Extent {
                    offset: ord_off,
                    len: record.len,
                },
                payload: Extent {
                    offset: pay_off,
                    len: pay_records[index].len,
                },
            },
        ));
        ord_at = ord_at
            .checked_add(usize::try_from(record.len).map_err(|_| Error::Corrupt("channel length"))?)
            .ok_or(Error::Corrupt("channel length"))?;
        pay_at = pay_at
            .checked_add(
                usize::try_from(pay_records[index].len)
                    .map_err(|_| Error::Corrupt("channel length"))?,
            )
            .ok_or(Error::Corrupt("channel length"))?;
    }
    Ok(children)
}

#[cfg_attr(not(test), allow(dead_code))]
fn write_directory(out: &mut Vec<u8>, records: &[(u8, u32)]) {
    out.extend_from_slice(MAGIC);
    out.push(u8::try_from(records.len()).expect("at most 16 channels"));
    for (field, len) in records {
        out.push(*field);
        out.extend_from_slice(&len.to_le_bytes());
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn stream_len(bytes: &[u8]) -> Result<u32> {
    u32::try_from(bytes.len()).map_err(|_| Error::Corrupt("channel length"))
}

/// Writes channel extents.
///
/// `field_count == 1` returns the single nonempty stock stream with no
/// directory. `2..=16` writes `FCH1` and omits streams whose count varint is
/// 0. A multi-column token present in only one field still gets a directory.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn encode(field_count: u8, fields: &[FieldStreams]) -> Result<EncodedChannels> {
    if !(1..=MAX_FIELD_COUNT).contains(&field_count) {
        return Err(Error::Corrupt("channel field_count"));
    }
    let mut present: Vec<&FieldStreams> = Vec::new();
    for field in fields {
        if field.field >= field_count {
            return Err(Error::Corrupt("channel field"));
        }
        let ordinals = stream_count(&field.ordinals, "channel ordinals")?;
        let payload = stream_count(&field.payload, "channel payload")?;
        if ordinals == 0 && payload == 0 {
            continue;
        }
        if ordinals == 0 || ordinals != payload {
            return Err(Error::Corrupt("channel payload count"));
        }
        present.push(field);
    }
    present.sort_by_key(|field| field.field);
    if present
        .windows(2)
        .any(|pair| pair[0].field == pair[1].field)
    {
        return Err(Error::Corrupt("channel field"));
    }
    if field_count < MIN_FIELD_COUNT {
        let Some(field) = present.first() else {
            return Ok(EncodedChannels {
                ordinals: Vec::new(),
                payload: Vec::new(),
            });
        };
        if present.len() != 1 {
            return Err(Error::Corrupt("channel field"));
        }
        return Ok(EncodedChannels {
            ordinals: field.ordinals.clone(),
            payload: field.payload.clone(),
        });
    }
    if present.is_empty() {
        return Ok(EncodedChannels {
            ordinals: Vec::new(),
            payload: Vec::new(),
        });
    }
    let records: Vec<(u8, u32, u32)> = present
        .iter()
        .map(|field| {
            Ok((
                field.field,
                stream_len(&field.ordinals)?,
                stream_len(&field.payload)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut ordinals = Vec::new();
    let mut payload = Vec::new();
    write_directory(
        &mut ordinals,
        &records
            .iter()
            .map(|(field, len, _)| (*field, *len))
            .collect::<Vec<_>>(),
    );
    write_directory(
        &mut payload,
        &records
            .iter()
            .map(|(field, _, len)| (*field, *len))
            .collect::<Vec<_>>(),
    );
    for field in &present {
        ordinals.extend_from_slice(&field.ordinals);
        payload.extend_from_slice(&field.payload);
    }
    Ok(EncodedChannels { ordinals, payload })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::{DictionaryBuilder, OwnedDictionary, TermEntry};
    use crate::docs::DocTable;
    use crate::ordinals::{self, Ordinals};
    use crate::payload::PayloadBuilder;
    use crate::segment::{AreaFetch, Term};
    use crate::tf_bucket::TfBucket;
    use proptest::prelude::*;

    struct Mem {
        ordinals: Vec<u8>,
        payload: Vec<u8>,
    }

    impl AreaFetch for Mem {
        fn ordinals_bytes(&self, offset: u64, len: usize) -> Result<&[u8]> {
            let at = usize::try_from(offset).map_err(|_| Error::Truncated)?;
            self.ordinals
                .get(at..at.checked_add(len).ok_or(Error::Truncated)?)
                .ok_or(Error::Truncated)
        }

        fn payload_bytes(&self, extent: Extent) -> Result<&[u8]> {
            let at = usize::try_from(extent.offset).map_err(|_| Error::Truncated)?;
            let len = extent.len as usize;
            self.payload
                .get(at..at.checked_add(len).ok_or(Error::Truncated)?)
                .ok_or(Error::Truncated)
        }

        fn doc_table(&self) -> Result<DocTable<'_>> {
            Err(Error::Corrupt("channel documents"))
        }

        fn length(&self, _ordinal: u32) -> Result<u32> {
            Ok(1)
        }

        fn length_class(&self, ordinal: u32) -> Result<u8> {
            Ok(crate::length_class::class_of(self.length(ordinal)?))
        }
    }

    fn stock(docs: &[(u32, &[u32], u32)]) -> (Vec<u8>, Vec<u8>, u32, u8) {
        let ordinals: Vec<u32> = docs.iter().map(|doc| doc.0).collect();
        let scores: Vec<(u8, u32)> = docs
            .iter()
            .map(|doc| (TfBucket::from_count(doc.1.len() as u32).value(), doc.2))
            .collect();
        let ordinal_bytes = ordinals::encode_scored(&ordinals, &scores);
        let mut payload = PayloadBuilder::default();
        for doc in docs {
            payload.push(doc.1).unwrap();
        }
        let max_bucket = scores.iter().map(|score| score.0).max().unwrap_or(0);
        (
            ordinal_bytes,
            payload.finish(),
            docs.len() as u32,
            max_bucket,
        )
    }

    fn field(
        field: u8,
        docs: &[(u32, &[u32], u32)],
    ) -> (FieldStreams, u32, u8, Vec<u32>, Vec<Vec<u32>>) {
        let (ordinals, payload, df, max_bucket) = stock(docs);
        let positions = docs.iter().map(|doc| doc.1.to_vec()).collect();
        let ordinal_list = docs.iter().map(|doc| doc.0).collect();
        (
            FieldStreams {
                field,
                ordinals,
                payload,
            },
            df,
            max_bucket,
            ordinal_list,
            positions,
        )
    }

    fn open(field_count: u8, ordinals: &[u8], payload: &[u8]) -> Result<Vec<(u8, TermEntry)>> {
        let mem = Mem {
            ordinals: ordinals.to_vec(),
            payload: payload.to_vec(),
        };
        let term = Term::new(
            TermEntry {
                df: 99,
                max_tf_bucket: 0,
                ordinals: Extent {
                    offset: 0,
                    len: u32::try_from(ordinals.len()).unwrap(),
                },
                payload: Extent {
                    offset: 0,
                    len: u32::try_from(payload.len()).unwrap(),
                },
            },
            &mem,
        );
        term.channels(field_count).map(|channels| {
            channels
                .into_iter()
                .map(|(field, child)| (field, child.entry))
                .collect()
        })
    }

    fn expect(field_count: u8, ordinals: &[u8], payload: &[u8], message: &'static str) {
        let err = match open(field_count, ordinals, payload) {
            Ok(children) => panic!("expected {message}, opened {} channels", children.len()),
            Err(err) => err,
        };
        assert_eq!(
            err,
            Error::Corrupt(message),
            "ordinals {ordinals:?} payload {payload:?}"
        );
    }

    /// Directory written in the given order, including illegal records.
    fn frame(records: &[(u8, u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(u8::try_from(records.len()).unwrap());
        for (field, len, _) in records {
            out.push(*field);
            out.extend_from_slice(&len.to_le_bytes());
        }
        for (_, _, bytes) in records {
            out.extend_from_slice(bytes);
        }
        out
    }

    fn child_postings(mem: &Mem, child: TermEntry) -> (Vec<u32>, Vec<u8>, Vec<Vec<u32>>) {
        let term = Term::new(child, mem);
        let stream = term.ordinals().unwrap();
        let mut cursor = stream.cursor().unwrap();
        let mut ordinals = Vec::new();
        let mut buckets = Vec::new();
        while let Some(ordinal) = cursor.current() {
            ordinals.push(ordinal);
            buckets.push(cursor.bucket().unwrap());
            cursor.advance().unwrap();
        }
        let payload = term.payload().unwrap();
        let mut positions = Vec::new();
        let mut walk = payload.cursor();
        for _ in 0..payload.count() {
            positions.push(walk.next_entry().unwrap().positions);
        }
        (ordinals, buckets, positions)
    }

    #[test]
    fn channels_rejects_field_count_outside_2_to_16() {
        let mem = Mem {
            ordinals: Vec::new(),
            payload: Vec::new(),
        };
        let term = Term::new(TermEntry::default(), &mem);
        for field_count in [0, 1, 17, 255] {
            let err = match term.channels(field_count) {
                Ok(_) => panic!("channels({field_count}) opened"),
                Err(err) => err,
            };
            assert_eq!(
                err,
                Error::Corrupt("channel field_count"),
                "channels({field_count})"
            );
        }
    }

    #[test]
    fn round_trip_two_fields_preserves_stock_streams() {
        let (left, left_df, left_max, left_ords, left_pos) =
            field(0, &[(0, &[1, 2], 4), (3, &[1], 4)]);
        let (right, right_df, right_max, right_ords, right_pos) =
            field(2, &[(1, &[1, 2, 3, 4, 5], 8)]);
        let encoded = encode(4, &[right.clone(), left.clone()]).unwrap();
        assert!(encoded.ordinals.starts_with(MAGIC));
        assert!(encoded.payload.starts_with(MAGIC));
        assert_eq!(encoded.ordinals[4], 2, "n");
        assert_eq!(
            encoded.ordinals.len(),
            5 + 5 * 2 + left.ordinals.len() + right.ordinals.len()
        );
        let mem = Mem {
            ordinals: encoded.ordinals.clone(),
            payload: encoded.payload.clone(),
        };
        let term = Term::new(
            TermEntry {
                df: 99,
                max_tf_bucket: 0,
                ordinals: Extent {
                    offset: 0,
                    len: encoded.ordinals.len() as u32,
                },
                payload: Extent {
                    offset: 0,
                    len: encoded.payload.len() as u32,
                },
            },
            &mem,
        );
        let channels = term.channels(4).unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(term.df(), 99, "parent df stays the caller's union");
        let (field0, child0) = &channels[0];
        let (field2, child2) = &channels[1];
        assert_eq!(*field0, 0);
        assert_eq!(child0.df(), left_df);
        assert_ne!(child0.df(), child0.entry.ordinals.len);
        assert_eq!(child0.entry.max_tf_bucket, left_max);
        assert_eq!(child0.entry.ordinals.offset, (5 + 10) as u64);
        let (ords, buckets, positions) = child_postings(&mem, child0.entry);
        assert_eq!(ords, left_ords);
        assert_eq!(
            buckets,
            left_ords
                .iter()
                .zip(&left_pos)
                .map(|(_, pos)| TfBucket::from_count(pos.len() as u32).value())
                .collect::<Vec<_>>()
        );
        assert_eq!(positions, left_pos);
        assert_eq!(*field2, 2);
        assert_eq!(child2.df(), right_df);
        assert_eq!(child2.entry.max_tf_bucket, right_max);
        let (ords, _, positions) = child_postings(&mem, child2.entry);
        assert_eq!(ords, right_ords);
        assert_eq!(positions, right_pos);
    }

    #[test]
    fn child_df_is_the_ordinals_count_not_the_byte_length() {
        let (streams, df, _, _, _) = field(0, &[(4, &[1, 2, 3], 3)]);
        let encoded = encode(2, std::slice::from_ref(&streams)).unwrap();
        let children = open(2, &encoded.ordinals, &encoded.payload).unwrap();
        assert_eq!(children[0].1.df, df);
        assert_ne!(u32::try_from(streams.ordinals.len()).unwrap(), df);
        assert_ne!(children[0].1.ordinals.len, df);
    }

    #[test]
    fn single_present_field_still_writes_a_directory() {
        let (streams, _, _, _, _) = field(1, &[(0, &[1], 1)]);
        let encoded = encode(3, &[streams]).unwrap();
        assert_eq!(&encoded.ordinals[..5], b"FCH1\x01");
        assert_eq!(encoded.ordinals[5], 1, "the present field, not zero");
        assert_eq!(&encoded.payload[..5], b"FCH1\x01");
        assert_eq!(encoded.payload[5], 1);
        let children = open(3, &encoded.ordinals, &encoded.payload).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].0, 1);
        assert_eq!(children[0].1.df, 1);
    }

    #[test]
    fn single_column_writer_emits_no_directory() {
        let (streams, _, _, _, _) = field(0, &[(0, &[1], 1)]);
        let encoded = encode(1, std::slice::from_ref(&streams)).unwrap();
        assert_eq!(encoded.ordinals, streams.ordinals);
        assert_eq!(encoded.payload, streams.payload);
        assert_ne!(encoded.ordinals.get(..4), Some(&MAGIC[..]));
        let empty = encode(
            1,
            &[FieldStreams {
                field: 0,
                ordinals: ordinals::encode_scored(&[], &[]),
                payload: PayloadBuilder::default().finish(),
            }],
        )
        .unwrap();
        assert!(empty.ordinals.is_empty());
        assert!(empty.payload.is_empty());
    }

    #[test]
    fn zero_posting_fields_are_omitted_from_both_directories() {
        let (kept, _, _, _, _) = field(2, &[(1, &[1], 2)]);
        let omitted = FieldStreams {
            field: 0,
            ordinals: ordinals::encode_scored(&[], &[]),
            payload: PayloadBuilder::default().finish(),
        };
        let encoded = encode(4, &[omitted, kept]).unwrap();
        assert_eq!(encoded.ordinals[4], 1);
        assert_eq!(encoded.payload[4], 1);
        assert_eq!(encoded.ordinals[5], 2);
        assert_eq!(encoded.payload[5], 2);
        let ord_fields = open(4, &encoded.ordinals, &encoded.payload)
            .unwrap()
            .into_iter()
            .map(|(field, _)| field)
            .collect::<Vec<_>>();
        assert_eq!(ord_fields, vec![2]);
    }

    #[test]
    fn every_field_ordinal_through_15_is_legal() {
        let mut fields = Vec::new();
        for field_id in 0..16u8 {
            let (streams, _, _, _, _) = field(field_id, &[(u32::from(field_id), &[1], 1)]);
            fields.push(streams);
        }
        let encoded = encode(16, &fields).unwrap();
        assert_eq!(directory_len(16).unwrap(), 5 + 5 * 16);
        assert_eq!(
            encoded.ordinals.len(),
            5 + 5 * 16 + fields.iter().map(|f| f.ordinals.len()).sum::<usize>()
        );
        let children = open(16, &encoded.ordinals, &encoded.payload).unwrap();
        assert_eq!(children.len(), 16);
        for (index, (field_id, child)) in children.iter().enumerate() {
            assert_eq!(*field_id, index as u8);
            assert_eq!(child.df, 1);
            assert!(*field_id < 16);
        }
        let docs: Vec<(u32, Vec<u32>, u32)> =
            (0..=15).map(|ordinal| (ordinal, vec![1u32], 1)).collect();
        let borrowed: Vec<(u32, &[u32], u32)> = docs
            .iter()
            .map(|(ordinal, positions, len)| (*ordinal, positions.as_slice(), *len))
            .collect();
        let (streams, df, _, _, _) = field(0, &borrowed);
        let encoded = encode(2, &[streams]).unwrap();
        let children = open(2, &encoded.ordinals, &encoded.payload).unwrap();
        assert_eq!(df, 16);
        assert_eq!(children[0].1.df, 16);
        let mem = Mem {
            ordinals: encoded.ordinals,
            payload: encoded.payload,
        };
        let (got, _, _) = child_postings(&mem, children[0].1);
        assert_eq!(got, (0..=15).collect::<Vec<_>>());
    }

    #[test]
    fn surface_token_is_not_a_field_nibble() {
        let token = "a~b";
        let (streams, _, max_bucket, _, _) = field(0, &[(1, &[1], 1)]);
        let encoded = encode(2, &[streams]).unwrap();
        assert!(
            !encoded
                .ordinals
                .windows(token.len())
                .any(|window| window == token.as_bytes()),
            "the directory does not store the token"
        );
        assert!(!encoded.ordinals.windows(3).any(|window| window == b"~0~"));
        let mut dictionary = DictionaryBuilder::default();
        dictionary
            .push(
                token,
                TermEntry {
                    df: 1,
                    max_tf_bucket: max_bucket,
                    ordinals: Extent {
                        offset: 0,
                        len: encoded.ordinals.len() as u32,
                    },
                    payload: Extent {
                        offset: 0,
                        len: encoded.payload.len() as u32,
                    },
                },
            )
            .unwrap();
        let bytes = dictionary.finish();
        let owned = OwnedDictionary::parse(&bytes).unwrap();
        let view = owned.view();
        assert!(view.get(token).unwrap().is_some());
        assert!(view.get("~0~a~b").unwrap().is_none());
        let terms: Vec<String> = view.iter().map(|item| item.unwrap().0).collect();
        assert_eq!(terms, vec![token.to_string()]);
        let mem = Mem {
            ordinals: encoded.ordinals,
            payload: encoded.payload,
        };
        let entry = view.get(token).unwrap().unwrap();
        let child = Term::new(entry, &mem).channels(2).unwrap();
        assert_eq!(child.len(), 1);
        assert_eq!(child[0].1.ordinals().unwrap().count(), 1);
        assert_eq!(child[0].1.payload().unwrap().count(), 1);
    }

    #[test]
    fn empty_token_is_rejected() {
        let mut dictionary = DictionaryBuilder::default();
        assert_eq!(
            dictionary.push("", TermEntry::default()).unwrap_err(),
            Error::EmptyTerm
        );
        assert!(dictionary.is_empty());
    }

    #[test]
    fn listed_zero_count_is_corrupt() {
        let empty = ordinals::encode_scored(&[], &[]);
        assert_eq!(stream_count(&empty, "channel ordinals").unwrap(), 0);
        let ordinals = frame(&[(0, empty.len() as u32, &empty)]);
        let payload = frame(&[(0, 1, &[0])]);
        expect(2, &ordinals, &payload, "channel df");
    }

    #[test]
    fn bad_magic_is_corrupt() {
        let mut bytes = b"FCH0\x01".to_vec();
        bytes.extend_from_slice(&[0, 1, 0, 0, 0, 0x01]);
        expect(2, &bytes, &bytes, "channel magic");
    }

    #[test]
    fn missing_magic_on_multi_column_is_corrupt() {
        let (streams, _, _, _, _) = field(0, &[(0, &[1], 1)]);
        expect(2, &streams.ordinals, &streams.payload, "channel magic");
        expect(16, &streams.ordinals, &streams.payload, "channel magic");
    }

    #[test]
    fn zero_channel_count_is_corrupt() {
        let bytes = b"FCH1\x00".to_vec();
        expect(2, &bytes, &bytes, "channel count");
    }

    #[test]
    fn channel_count_above_field_count_is_corrupt() {
        let mut bytes = b"FCH1\x03".to_vec();
        for field in 0..3u8 {
            bytes.push(field);
            bytes.extend_from_slice(&1u32.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0, 0]);
        expect(2, &bytes, &bytes, "channel count");
    }

    #[test]
    fn field_at_or_above_field_count_is_corrupt() {
        let (streams, _, _, _, _) = field(0, &[(0, &[1], 1)]);
        let bytes = frame(&[(2, streams.ordinals.len() as u32, &streams.ordinals)]);
        let payload = frame(&[(2, streams.payload.len() as u32, &streams.payload)]);
        expect(2, &bytes, &payload, "channel field");
    }

    #[test]
    fn unsorted_or_duplicate_fields_are_corrupt() {
        let (streams, _, _, _, _) = field(0, &[(0, &[1], 1)]);
        let len = streams.ordinals.len() as u32;
        let pay_len = streams.payload.len() as u32;
        let ordinals = frame(&[(1, len, &streams.ordinals), (0, len, &streams.ordinals)]);
        let payload = frame(&[
            (1, pay_len, &streams.payload),
            (0, pay_len, &streams.payload),
        ]);
        expect(2, &ordinals, &payload, "channel field");
        let ordinals = frame(&[(0, len, &streams.ordinals), (0, len, &streams.ordinals)]);
        let payload = frame(&[
            (0, pay_len, &streams.payload),
            (0, pay_len, &streams.payload),
        ]);
        expect(2, &ordinals, &payload, "channel field");
    }

    #[test]
    fn zero_record_length_is_corrupt() {
        let bytes = frame(&[(0, 0, &[])]);
        expect(2, &bytes, &bytes, "channel length");
    }

    #[test]
    fn length_sum_shortfall_and_remainder_are_corrupt() {
        let (streams, _, _, _, _) = field(0, &[(0, &[1], 1)]);
        let short = frame(&[(0, streams.ordinals.len() as u32 + 8, &streams.ordinals)]);
        expect(2, &short, &short, "channel length");
        let mut extra = frame(&[(0, streams.ordinals.len() as u32, &streams.ordinals)]);
        extra.push(0x00);
        expect(2, &extra, &extra, "channel length");
    }

    #[test]
    fn length_sum_overflow_does_not_wrap() {
        // Two u32::MAX lengths wrap to 0 in u32 and would match a
        // directory-only extent. Checked addition must reject that.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.push(2);
        bytes.push(0);
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(bytes.len(), 5 + 5 * 2);
        expect(2, &bytes, &bytes, "channel length");
    }

    #[test]
    fn directory_len_overflow_is_corrupt() {
        assert_eq!(directory_len(0).unwrap(), 5);
        assert_eq!(directory_len(1).unwrap(), 10);
        assert_eq!(directory_len(16).unwrap(), 5 + 5 * 16);
        let n = (usize::MAX / 5) as u128 + 1;
        assert_eq!(
            directory_len(n).unwrap_err(),
            Error::Corrupt("channel directory")
        );
        assert_eq!(
            directory_len(u128::from(u64::MAX)).unwrap_err(),
            Error::Corrupt("channel directory")
        );
    }

    #[test]
    fn directory_that_does_not_fit_is_corrupt() {
        let bytes = b"FCH1\x02".to_vec();
        expect(16, &bytes, &bytes, "channel directory");
    }

    #[test]
    fn header_shorter_than_five_bytes_is_corrupt() {
        expect(2, b"FCH1", b"FCH1", "channel header");
        expect(2, b"FCH", b"FCH", "channel header");
    }

    #[test]
    fn trailing_bytes_in_a_child_slice_are_corrupt() {
        let (streams, _, _, _, _) = field(0, &[(0, &[1], 1), (2, &[1], 1)]);
        let mut ordinals = streams.ordinals.clone();
        ordinals.push(0x00);
        let framed_ord = frame(&[(0, ordinals.len() as u32, &ordinals)]);
        let framed_pay = frame(&[(0, streams.payload.len() as u32, &streams.payload)]);
        expect(2, &framed_ord, &framed_pay, "ordinal list length");

        let chunked_docs: Vec<(u32, Vec<u32>, u32)> =
            (0..70).map(|ordinal| (ordinal, vec![1u32], 1)).collect();
        let borrowed: Vec<(u32, &[u32], u32)> = chunked_docs
            .iter()
            .map(|(ordinal, positions, len)| (*ordinal, positions.as_slice(), *len))
            .collect();
        let (chunked, _, _, _, _) = field(0, &borrowed);
        assert!(
            Ordinals::open(&chunked.ordinals[..], chunked.ordinals.len() as u64, true)
                .unwrap()
                .list()
                .is_none()
        );
        let mut ordinals = chunked.ordinals.clone();
        ordinals.push(0x00);
        let framed_ord = frame(&[(0, ordinals.len() as u32, &ordinals)]);
        let framed_pay = frame(&[(0, chunked.payload.len() as u32, &chunked.payload)]);
        expect(2, &framed_ord, &framed_pay, "ordinal stream length");

        let mut payload = streams.payload.clone();
        payload.push(0x00);
        let framed_ord = frame(&[(0, streams.ordinals.len() as u32, &streams.ordinals)]);
        let framed_pay = frame(&[(0, payload.len() as u32, &payload)]);
        expect(2, &framed_ord, &framed_pay, "channel payload");
    }

    #[test]
    fn ordinal_payload_directory_disagreement_is_corrupt() {
        let (left, _, _, _, _) = field(0, &[(0, &[1], 1)]);
        let (right, _, _, _, _) = field(1, &[(1, &[1], 1)]);
        let ordinals = encode(2, std::slice::from_ref(&left)).unwrap().ordinals;
        let payload = encode(2, &[right]).unwrap().payload;
        expect(2, &ordinals, &payload, "channel directories");
        let both = encode(
            2,
            &[left.clone(), {
                let (other, _, _, _, _) = field(1, &[(2, &[1], 1)]);
                other
            }],
        )
        .unwrap();
        let only = encode(2, &[left]).unwrap();
        expect(2, &both.ordinals, &only.payload, "channel directories");
    }

    #[test]
    fn payload_count_must_equal_child_df() {
        let (streams, _, _, _, _) = field(0, &[(0, &[1], 1), (4, &[1], 1)]);
        let ordinals = frame(&[(0, streams.ordinals.len() as u32, &streams.ordinals)]);
        let payload = frame(&[(0, 1, &[0])]);
        expect(2, &ordinals, &payload, "channel payload count");
    }

    #[test]
    fn encode_rejects_a_half_empty_field_and_a_bad_field_count() {
        let (streams, _, _, _, _) = field(0, &[(0, &[1], 1)]);
        let err = encode(
            2,
            &[FieldStreams {
                field: 0,
                ordinals: streams.ordinals.clone(),
                payload: vec![0],
            }],
        )
        .unwrap_err();
        assert_eq!(err, Error::Corrupt("channel payload count"));
        assert_eq!(
            encode(0, &[]).unwrap_err(),
            Error::Corrupt("channel field_count")
        );
        assert_eq!(
            encode(17, &[]).unwrap_err(),
            Error::Corrupt("channel field_count")
        );
        assert_eq!(
            encode(
                2,
                &[FieldStreams {
                    field: 2,
                    ordinals: streams.ordinals,
                    payload: streams.payload,
                }],
            )
            .unwrap_err(),
            Error::Corrupt("channel field")
        );
    }

    fn arb_postings(field: u8, docs: usize, npos: u8) -> FieldStreams {
        let mut ordinals = Vec::new();
        let mut scores = Vec::new();
        let mut payload = PayloadBuilder::default();
        for index in 0..docs {
            let ordinal = u32::from(field) * 32 + index as u32;
            let positions: Vec<u32> = (0..npos).map(|k| u32::from(k) + 1 + index as u32).collect();
            let bucket = TfBucket::from_count(positions.len() as u32).value();
            ordinals.push(ordinal);
            scores.push((bucket, *positions.last().unwrap()));
            payload.push(&positions).unwrap();
        }
        FieldStreams {
            field,
            ordinals: ordinals::encode_scored(&ordinals, &scores),
            payload: payload.finish(),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        #[test]
        fn round_trip_preserves_stock_channels(
            field_count in 2u8..=16,
            mask in 0u16..=u16::MAX,
            docs in 1usize..=3,
            npos in 1u8..=3,
        ) {
            let mut fields = Vec::new();
            for field in 0..field_count {
                if mask & (1u16 << field) == 0 {
                    continue;
                }
                fields.push(arb_postings(field, docs, npos));
            }
            if fields.is_empty() {
                fields.push(arb_postings(0, docs, npos));
            }
            let present: Vec<u8> = fields.iter().map(|field| field.field).collect();
            let encoded = encode(field_count, &fields).unwrap();
            prop_assert!(encoded.ordinals.starts_with(MAGIC));
            prop_assert!(encoded.payload.starts_with(MAGIC));
            prop_assert_eq!(encoded.ordinals[4], present.len() as u8);
            prop_assert_eq!(encoded.payload[4], present.len() as u8);
            let dir = 5 + 5 * present.len();
            prop_assert_eq!(
                encoded.ordinals.len(),
                dir + fields.iter().map(|field| field.ordinals.len()).sum::<usize>()
            );
            let children = open(field_count, &encoded.ordinals, &encoded.payload).unwrap();
            prop_assert_eq!(children.len(), present.len());
            for ((field_id, child), source) in children.iter().zip(&fields) {
                prop_assert_eq!(*field_id, source.field);
                prop_assert!(child.df > 0);
                prop_assert_ne!(child.df, child.ordinals.len);
                let mem = Mem {
                    ordinals: encoded.ordinals.clone(),
                    payload: encoded.payload.clone(),
                };
                let term = Term::new(*child, &mem);
                let decoded = term.ordinals().unwrap().to_vec().unwrap();
                let original = Ordinals::open(&source.ordinals[..], source.ordinals.len() as u64, true)
                    .unwrap()
                    .to_vec()
                    .unwrap();
                prop_assert_eq!(decoded, original);
                prop_assert_eq!(term.payload().unwrap().count(), child.df);
                prop_assert_eq!(term.entry.max_tf_bucket, child.max_tf_bucket);
            }
        }
    }
}
