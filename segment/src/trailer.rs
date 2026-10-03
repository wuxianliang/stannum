// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! STNF sidecar after a segment's page table: per-field norms.
//!
//! ```text
//! v2 := magic "STNF", version u8 = 2, norms_len u32le, norms
//! v1 := magic "STNF", version u8 = 1, norms_len u32le, df_len u32le,
//!       norms, df_agg
//! norms    := field_count u8 (2..=16), field_total u64le × field_count,
//!             rows u32le × field_count × document_count, crc32 u32le
//! df_agg   := count u32le, entry*   (v1 only, withdrawn)
//! entry    := token_len u32le, token utf-8, df u64le
//! ```
//!
//! This version writes **v2** (norms only): union df lives on each
//! `TermEntry`, not in a sidecar. v1 stays decodable so 4.3–4.6 segments can
//! be classified (`StaleFielded`, design §6.2); no writer emits it.
//! Present only on a multi-column blob (`total > pages_end`). A single-column
//! segment omits the section. `STNF` is not `STN3` and not `LDP2`. CRC-32
//! is ISO-HDLC (zlib `crc32`) over the row bytes only, `u32le`, after the
//! rows and inside `norms_len`. Row `o`, field `f` is at
//! `(o * field_count + f) * 4` from the start of the rows.

use crate::reader::Reader;
use crate::{Error, Result};

/// Sidecar magic. Distinct from segment `STN3` and meta `LDP2`.
pub const MAGIC: &[u8; 4] = b"STNF";

/// The version this codec writes (design §6.2: norms only).
pub const VERSION: u8 = 2;
/// The withdrawn fielded-terms version, decodable for classification only.
pub const VERSION_V1: u8 = 1;

/// Inclusive field-count range a trailer may name. `1` is omitted: a
/// single-column segment has no trailer.
pub const MIN_FIELD_COUNT: u8 = 2;
pub const MAX_FIELD_COUNT: u8 = 16;

/// v1: magic + version + norms_len + df_len.
pub(crate) const PREFIX_LEN_V1: usize = 13;
/// v2: magic + version + norms_len.
pub(crate) const PREFIX_LEN_V2: usize = 9;

/// One decoded-token union-df row. `token` is analyzed text, not a fielded key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DfEntry {
    pub token: String,
    pub df: u64,
}

/// Parsed STNF sidecar. Dimensions are well-formed; CRC-32 over the rows
/// has been checked. Semantic agreement with postings is a later pass.
/// `df_agg` is nonempty only on v1 (withdrawn); v2 carries norms alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trailer {
    /// 1 (withdrawn fielded-terms) or 2 (this codec).
    pub version: u8,
    pub field_count: u8,
    pub field_totals: Vec<u64>,
    pub rows: Vec<u32>,
    pub df_agg: Vec<DfEntry>,
}
impl Trailer {
    /// Length at document ordinal `o`, field `f`: index
    /// `(o * field_count + f)` from the start of `rows`.
    pub fn row(&self, ordinal: u32, field: u8) -> Option<u32> {
        let index = (ordinal as usize)
            .checked_mul(usize::from(self.field_count))?
            .checked_add(usize::from(field))?;
        self.rows.get(index).copied()
    }
}

/// Encodes a **v2** trailer whose dimensions satisfy the wire grammar.
///
/// `rows` is document-major: length `field_totals.len() * document_count`.
/// Union df is not written: it lives on each `TermEntry` (design §6.2).
pub fn encode(field_totals: &[u64], rows: &[u32]) -> Result<Vec<u8>> {
    let field_count = u8::try_from(field_totals.len()).map_err(|_| field_count_error())?;
    check_field_count(field_count)?;
    if !rows.len().is_multiple_of(usize::from(field_count)) {
        return Err(Error::Corrupt("STNF norms"));
    }

    let mut norms = Vec::new();
    norms.push(field_count);
    for total in field_totals {
        norms.extend_from_slice(&total.to_le_bytes());
    }
    let rows_at = norms.len();
    for length in rows {
        norms.extend_from_slice(&length.to_le_bytes());
    }
    let crc = crc32fast::hash(&norms[rows_at..]);
    norms.extend_from_slice(&crc.to_le_bytes());

    let norms_len = u32::try_from(norms.len()).map_err(|_| Error::Corrupt("STNF norms"))?;
    let mut out = Vec::with_capacity(PREFIX_LEN_V2 + norms.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&norms_len.to_le_bytes());
    out.extend_from_slice(&norms);
    Ok(out)
}

/// Encodes a withdrawn **v1** trailer (norms plus the df sidecar). No
/// production writer emits this; classification fixtures need the bytes.
#[cfg(any(test, feature = "pg_test"))]
pub fn encode_v1(field_totals: &[u64], rows: &[u32], df_agg: &[(&str, u64)]) -> Result<Vec<u8>> {
    let field_count = u8::try_from(field_totals.len()).map_err(|_| field_count_error())?;
    check_field_count(field_count)?;
    if !rows.len().is_multiple_of(usize::from(field_count)) {
        return Err(Error::Corrupt("STNF norms"));
    }
    check_df_agg(df_agg)?;

    let mut norms = Vec::new();
    norms.push(field_count);
    for total in field_totals {
        norms.extend_from_slice(&total.to_le_bytes());
    }
    let rows_at = norms.len();
    for length in rows {
        norms.extend_from_slice(&length.to_le_bytes());
    }
    let crc = crc32fast::hash(&norms[rows_at..]);
    norms.extend_from_slice(&crc.to_le_bytes());

    let mut df = Vec::new();
    df.extend_from_slice(&(df_agg.len() as u32).to_le_bytes());
    for &(token, df_value) in df_agg {
        let bytes = token.as_bytes();
        let token_len = u32::try_from(bytes.len()).map_err(|_| Error::Corrupt("STNF df_agg"))?;
        df.extend_from_slice(&token_len.to_le_bytes());
        df.extend_from_slice(bytes);
        df.extend_from_slice(&df_value.to_le_bytes());
    }

    let norms_len = u32::try_from(norms.len()).map_err(|_| Error::Corrupt("STNF norms"))?;
    let df_len = u32::try_from(df.len()).map_err(|_| Error::Corrupt("STNF df_agg"))?;
    let mut out = Vec::with_capacity(PREFIX_LEN_V1 + norms.len() + df.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION_V1);
    out.extend_from_slice(&norms_len.to_le_bytes());
    out.extend_from_slice(&df_len.to_le_bytes());
    out.extend_from_slice(&norms);
    out.extend_from_slice(&df);
    Ok(out)
}

/// Decodes a complete STNF blob. `bytes` is exactly `pages_end..total`.
/// Short, long, bad magic, or a version other than 1 or 2 is corruption.
pub fn decode(bytes: &[u8], document_count: u32) -> Result<Trailer> {
    match decode_inner(bytes, document_count) {
        Err(Error::Truncated) => Err(Error::Corrupt("STNF trailer")),
        other => other,
    }
}

fn decode_inner(bytes: &[u8], document_count: u32) -> Result<Trailer> {
    if bytes.len() < 4 {
        return Err(Error::Corrupt("STNF trailer"));
    }
    if &bytes[..4] != MAGIC {
        return Err(Error::Corrupt("STNF magic"));
    }
    if bytes.len() < 5 {
        return Err(Error::Corrupt("STNF trailer"));
    }
    if bytes[4] != VERSION_V1 && bytes[4] != VERSION {
        return Err(Error::Corrupt("STNF version"));
    }
    let v1 = bytes[4] == VERSION_V1;
    let prefix_len = if v1 { PREFIX_LEN_V1 } else { PREFIX_LEN_V2 };
    if bytes.len() < prefix_len {
        return Err(Error::Corrupt("STNF trailer"));
    }
    let norms_len = u32::from_le_bytes(bytes[5..9].try_into().unwrap()) as usize;
    let df_len = if v1 {
        u32::from_le_bytes(bytes[9..13].try_into().unwrap()) as usize
    } else {
        0
    };
    let claimed = prefix_len
        .checked_add(norms_len)
        .and_then(|n| n.checked_add(df_len))
        .ok_or(Error::Corrupt("STNF trailer"))?;
    if bytes.len() != claimed {
        return Err(Error::Corrupt("STNF trailer"));
    }

    let norms = &bytes[prefix_len..prefix_len + norms_len];
    let df = &bytes[prefix_len + norms_len..];
    let (field_count, field_totals, rows) = decode_norms(norms, document_count)?;
    let df_agg = if v1 { decode_df_agg(df)? } else { Vec::new() };
    Ok(Trailer {
        version: bytes[4],
        field_count,
        field_totals,
        rows,
        df_agg,
    })
}

fn decode_norms(bytes: &[u8], document_count: u32) -> Result<(u8, Vec<u64>, Vec<u32>)> {
    let mut reader = Reader::new(bytes);
    let field_count = reader.take(1)?[0];
    check_field_count(field_count)?;
    let expected = norms_len(field_count, document_count)?;
    if bytes.len() != expected {
        return Err(Error::Corrupt("STNF norms"));
    }
    let mut field_totals = Vec::with_capacity(usize::from(field_count));
    for _ in 0..field_count {
        field_totals.push(take_u64le(&mut reader)?);
    }
    let row_count = usize::from(field_count)
        .checked_mul(document_count as usize)
        .ok_or(Error::Corrupt("STNF norms"))?;
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        rows.push(take_u32le(&mut reader)?);
    }
    let crc_got = take_u32le(&mut reader)?;
    if reader.remaining() != 0 {
        return Err(Error::Corrupt("STNF norms"));
    }
    let row_bytes = row_count
        .checked_mul(4)
        .ok_or(Error::Corrupt("STNF norms"))?;
    let crc_at = bytes.len() - 4;
    let crc_start = crc_at
        .checked_sub(row_bytes)
        .ok_or(Error::Corrupt("STNF norms"))?;
    if crc32fast::hash(&bytes[crc_start..crc_at]) != crc_got {
        return Err(Error::Corrupt("STNF crc32"));
    }
    Ok((field_count, field_totals, rows))
}

fn decode_df_agg(bytes: &[u8]) -> Result<Vec<DfEntry>> {
    let mut reader = Reader::new(bytes);
    let count = take_u32le(&mut reader)?;
    // token_len u32le + nonempty token + df u64le. Cap before allocating so a
    // huge count with a tiny remaining section is Corrupt, not an abort.
    const MIN_ENTRY: usize = 13;
    let max_count = reader.remaining() / MIN_ENTRY;
    if count as usize > max_count {
        return Err(Error::Corrupt("STNF df_agg"));
    }
    let mut entries = Vec::with_capacity(count as usize);
    let mut prev: Option<&[u8]> = None;
    for _ in 0..count {
        let token_len = take_u32le(&mut reader)? as usize;
        let token_bytes = reader.take(token_len)?;
        let token = std::str::from_utf8(token_bytes).map_err(|_| Error::Corrupt("STNF df_agg"))?;
        let df = take_u64le(&mut reader)?;
        if token.is_empty() || df == 0 {
            return Err(Error::Corrupt("STNF df_agg"));
        }
        if prev.is_some_and(|earlier| token_bytes <= earlier) {
            return Err(Error::Corrupt("STNF df_agg"));
        }
        prev = Some(token_bytes);
        entries.push(DfEntry {
            token: token.to_owned(),
            df,
        });
    }
    if reader.remaining() != 0 {
        return Err(Error::Corrupt("STNF df_agg"));
    }
    Ok(entries)
}

#[cfg(any(test, feature = "pg_test"))]
fn check_df_agg(df_agg: &[(&str, u64)]) -> Result<()> {
    let mut prev: Option<&str> = None;
    for &(token, df) in df_agg {
        if token.is_empty() || df == 0 {
            return Err(Error::Corrupt("STNF df_agg"));
        }
        if prev.is_some_and(|earlier| token.as_bytes() <= earlier.as_bytes()) {
            return Err(Error::Corrupt("STNF df_agg"));
        }
        prev = Some(token);
    }
    Ok(())
}

pub(crate) fn check_field_count(field_count: u8) -> Result<()> {
    if (MIN_FIELD_COUNT..=MAX_FIELD_COUNT).contains(&field_count) {
        Ok(())
    } else {
        Err(field_count_error())
    }
}

/// Writer field counts: `1` omits the trailer; `2..=16` emit it.
pub(crate) fn check_writer_field_count(field_count: u8) -> Result<()> {
    if (1..=MAX_FIELD_COUNT).contains(&field_count) {
        Ok(())
    } else {
        Err(field_count_error())
    }
}

/// Inspect a stored dictionary term against the fields/codec grammar
/// (`~` + lowercase hex nibble + `~` + doubled-tilde payload).
///
/// `Ok(None)` is not a fielded header. `Ok(Some)` is a decoded surface token.
/// An unpaired `~` after a matching header is corruption. This is a decoder
/// only: the encoder stays in `postgres/src/fields/codec.rs`.
pub(crate) fn inspect_stored_term(key: &str) -> Result<Option<(u8, String)>> {
    let mut chars = key.chars();
    if chars.next() != Some('~') {
        return Ok(None);
    }
    let Some(ordinal) = chars.next().and_then(nibble) else {
        return Ok(None);
    };
    if chars.next() != Some('~') {
        return Ok(None);
    }
    let token = unescape_payload(chars.as_str())?;
    if token.is_empty() {
        return Err(Error::Corrupt("STNF unpaired tilde"));
    }
    Ok(Some((ordinal, token)))
}

fn nibble(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        'a'..='f' => Some(c as u8 - b'a' + 10),
        _ => None,
    }
}

fn unescape_payload(payload: &str) -> Result<String> {
    let mut out = String::with_capacity(payload.len());
    let mut chars = payload.chars();
    while let Some(c) = chars.next() {
        if c == '~' {
            match chars.next() {
                Some('~') => out.push('~'),
                _ => return Err(Error::Corrupt("STNF unpaired tilde")),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// Per-field norms accumulated from fielded postings. Union df is not
/// tracked: v2 keeps it on the `TermEntry`, and the postings themselves are
/// the recount source (design §6.2).
pub(crate) struct Tables {
    field_count: u8,
    doc_count: u32,
    rows: Vec<u32>,
}

impl Tables {
    pub(crate) fn new(field_count: u8, doc_count: u32) -> Result<Self> {
        check_field_count(field_count)?;
        let n = usize::from(field_count)
            .checked_mul(doc_count as usize)
            .ok_or(Error::Corrupt("STNF norms"))?;
        Ok(Self {
            field_count,
            doc_count,
            rows: vec![0u32; n],
        })
    }

    pub(crate) fn add(&mut self, field: u8, token: &str, ordinal: u32, pos_len: u32) -> Result<()> {
        let _ = token;
        if field >= self.field_count || ordinal >= self.doc_count {
            return Err(Error::Corrupt("STNF field_count"));
        }
        let index = (ordinal as usize)
            .checked_mul(usize::from(self.field_count))
            .and_then(|i| i.checked_add(usize::from(field)))
            .ok_or(Error::Corrupt("STNF norms"))?;
        let cell = self
            .rows
            .get_mut(index)
            .ok_or(Error::Corrupt("STNF norms"))?;
        *cell = cell
            .checked_add(pos_len)
            .ok_or(Error::Corrupt("STNF norms"))?;
        Ok(())
    }

    fn field_totals(&self) -> Result<Vec<u64>> {
        let fields = usize::from(self.field_count);
        let mut totals = vec![0u64; fields];
        for row in self.rows.chunks_exact(fields) {
            for (total, &cell) in totals.iter_mut().zip(row) {
                *total = total
                    .checked_add(u64::from(cell))
                    .ok_or(Error::Corrupt("STNF field_total"))?;
            }
        }
        Ok(totals)
    }

    pub(crate) fn norms(&self) -> Result<(u8, Vec<u64>, Vec<u32>)> {
        Ok((self.field_count, self.field_totals()?, self.rows.clone()))
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let totals = self.field_totals()?;
        encode(&totals, &self.rows)
    }

    /// Encode then decode so CRC-32/ISO-HDLC over the row bytes is checked
    /// before a flush writes the sidecar.
    pub(crate) fn crc_check(&self) -> Result<()> {
        let bytes = self.encode()?;
        decode(&bytes, self.doc_count).map(drop)
    }
}

fn field_count_error() -> Error {
    Error::Corrupt("STNF field_count")
}

fn norms_len(field_count: u8, document_count: u32) -> Result<usize> {
    let fields = usize::from(field_count);
    let docs = document_count as usize;
    let totals = fields.checked_mul(8).ok_or(Error::Corrupt("STNF norms"))?;
    let rows = fields
        .checked_mul(docs)
        .and_then(|n| n.checked_mul(4))
        .ok_or(Error::Corrupt("STNF norms"))?;
    1usize
        .checked_add(totals)
        .and_then(|n| n.checked_add(rows))
        .and_then(|n| n.checked_add(4))
        .ok_or(Error::Corrupt("STNF norms"))
}

fn take_u32le(reader: &mut Reader<'_>) -> Result<u32> {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(reader.take(4)?);
    Ok(u32::from_le_bytes(buf))
}

fn take_u64le(reader: &mut Reader<'_>) -> Result<u64> {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(reader.take(8)?);
    Ok(u64::from_le_bytes(buf))
}

/// Test-only fielded key. Production encoding stays in postgres fields/codec.
#[cfg(test)]
pub(crate) fn test_fielded_key(ordinal: u8, token: &str) -> String {
    debug_assert!(ordinal <= 15);
    let extra = token.chars().filter(|&c| c == '~').count();
    let mut payload = String::with_capacity(token.len() + extra);
    for c in token.chars() {
        if c == '~' {
            payload.push_str("~~");
        } else {
            payload.push(c);
        }
    }
    format!("~{ordinal:x}~{payload}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Vec<u64>, Vec<u32>) {
        (vec![3, 5], vec![1, 2, 2, 3])
    }

    #[test]
    fn crc32_is_iso_hdlc() {
        assert_eq!(crc32fast::hash(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32fast::hash(&[]), 0);
    }

    #[test]
    fn roundtrip_doc_major_rows_v2() {
        let (totals, rows) = sample();
        let bytes = encode(&totals, &rows).unwrap();
        assert_eq!(&bytes[..4], MAGIC);
        assert_eq!(bytes[4], VERSION);
        assert_eq!(bytes[4], 2);
        // v2 prefix is magic + version + norms_len only: no df_len word.
        assert_eq!(
            bytes.len(),
            PREFIX_LEN_V2 + u32::from_le_bytes(bytes[5..9].try_into().unwrap()) as usize
        );
        let trailer = decode(&bytes, 2).unwrap();
        assert_eq!(trailer.version, 2);
        assert_eq!(trailer.field_count, 2);
        assert_eq!(trailer.field_totals, totals);
        assert_eq!(trailer.rows, rows);
        assert_eq!(trailer.row(1, 1), Some(3));
        assert!(trailer.df_agg.is_empty(), "v2 carries no df section");
    }

    #[test]
    fn v1_round_trips_for_classification() {
        let (totals, rows) = sample();
        let bytes = encode_v1(&totals, &rows, &[("beer", 2), ("wine", 1)]).unwrap();
        assert_eq!(bytes[4], VERSION_V1);
        let trailer = decode(&bytes, 2).unwrap();
        assert_eq!(trailer.version, 1);
        assert_eq!(trailer.rows, rows);
        assert_eq!(
            trailer.df_agg,
            vec![
                DfEntry {
                    token: "beer".into(),
                    df: 2
                },
                DfEntry {
                    token: "wine".into(),
                    df: 1
                },
            ]
        );
    }

    #[test]
    fn empty_docs_and_empty_df_roundtrip() {
        let bytes = encode(&[0, 0], &[]).unwrap();
        let trailer = decode(&bytes, 0).unwrap();
        assert_eq!(trailer.field_count, 2);
        assert!(trailer.rows.is_empty());
        assert!(trailer.df_agg.is_empty());
        let v1 = encode_v1(&[0, 0], &[], &[]).unwrap();
        assert_eq!(decode(&v1, 0).unwrap().df_agg, Vec::new());
    }

    #[test]
    fn roundtrip_overlap_keeps_per_field_cells() {
        // One document posted in both fields: the cells stay per-field
        // (2 and 1), never a union count, and each total sums its own column.
        let totals = vec![2, 1];
        let rows = vec![2, 1];
        let bytes = encode(&totals, &rows).unwrap();
        let trailer = decode(&bytes, 1).unwrap();
        assert_eq!(trailer.row(0, 0), Some(2));
        assert_eq!(trailer.row(0, 1), Some(1));
        assert_ne!(trailer.row(0, 0), trailer.row(0, 1));
        assert_eq!(trailer.field_totals, totals);
        assert_eq!(trailer.rows, rows);
    }

    #[test]
    fn roundtrip_all_dead_rows_survive() {
        // Dead ordinals keep their rows until rewrite: three documents, only
        // ordinal 0 ever posted, ordinals 1 and 2 still carry zero cells so
        // doc-major addressing stays stable.
        let rows = vec![4, 0, 0, 0, 0, 0];
        let bytes = encode(&[4, 0], &rows).unwrap();
        let trailer = decode(&bytes, 3).unwrap();
        assert_eq!(trailer.rows, rows);
        assert_eq!(trailer.row(2, 0), Some(0));
        assert_eq!(trailer.row(2, 1), Some(0));
        assert_eq!(trailer.field_totals, [4, 0]);
    }

    #[test]
    fn roundtrip_merged_recount_shape() {
        // Merge-product shape: several documents with mixed field presence
        // and totals large enough that only the recounted u64 sums express
        // them. Row addressing is doc-major: (o * field_count + f).
        let totals = vec![7, 4];
        let rows = vec![3, 2, 4, 2, 0, 0];
        let bytes = encode(&totals, &rows).unwrap();
        let trailer = decode(&bytes, 3).unwrap();
        assert_eq!(trailer.field_count, 2);
        assert_eq!(trailer.field_totals, totals);
        assert_eq!(trailer.rows, rows);
        assert_eq!(trailer.row(1, 0), Some(4));
        assert_eq!(trailer.row(1, 1), Some(2));
        assert_eq!(trailer.row(2, 1), Some(0));
    }

    #[test]
    fn norms_dims_disagreeing_with_document_count_is_corrupt() {
        // The rows block must hold exactly field_count * document_count
        // cells: decoding a two-document trailer against any other document
        // count is corruption, in either direction.
        let (totals, rows) = sample();
        let bytes = encode(&totals, &rows).unwrap();
        assert_eq!(decode(&bytes, 2).unwrap().rows, rows);
        assert_eq!(decode(&bytes, 1).err(), Some(Error::Corrupt("STNF norms")));
        assert_eq!(decode(&bytes, 3).err(), Some(Error::Corrupt("STNF norms")));
    }

    #[test]
    fn field_count_one_is_corruption() {
        assert_eq!(encode(&[1], &[1]).err(), Some(field_count_error()));
        let mut bytes = encode(&[0, 0], &[]).unwrap();
        bytes[PREFIX_LEN_V2] = 1;
        assert_eq!(decode(&bytes, 0).err(), Some(field_count_error()));
    }

    #[test]
    fn v2_layout_with_a_df_section_is_corrupt() {
        // A v2 trailer followed by df bytes fails the exact-length check:
        // the v2 grammar has no df section to consume them.
        let (totals, rows) = sample();
        let mut bytes = encode(&totals, &rows).unwrap();
        bytes.extend_from_slice(&4u32.to_le_bytes());
        assert_eq!(
            decode(&bytes, 2).err(),
            Some(Error::Corrupt("STNF trailer"))
        );
    }

    #[test]
    fn structural_rejects() {
        let (totals, rows) = sample();
        let good = encode(&totals, &rows).unwrap();

        assert_eq!(
            decode(&good[..3], 2).err(),
            Some(Error::Corrupt("STNF trailer"))
        );

        let mut short = good.clone();
        short.pop();
        assert_eq!(
            decode(&short, 2).err(),
            Some(Error::Corrupt("STNF trailer"))
        );

        let mut long = good.clone();
        long.push(0);
        assert_eq!(decode(&long, 2).err(), Some(Error::Corrupt("STNF trailer")));

        let mut magic = good.clone();
        magic[..4].copy_from_slice(b"STN3");
        assert_eq!(decode(&magic, 2).err(), Some(Error::Corrupt("STNF magic")));

        let mut version = good.clone();
        version[4] = 3;
        assert_eq!(
            decode(&version, 2).err(),
            Some(Error::Corrupt("STNF version"))
        );
        assert_eq!(
            decode(b"STNF\x03", 2).err(),
            Some(Error::Corrupt("STNF version"))
        );

        let mut crc = good.clone();
        let norms_len = u32::from_le_bytes(crc[5..9].try_into().unwrap()) as usize;
        crc[PREFIX_LEN_V2 + norms_len - 1] ^= 1;
        assert_eq!(decode(&crc, 2).err(), Some(Error::Corrupt("STNF crc32")));
    }

    #[test]
    fn huge_df_agg_count_with_tiny_section_is_corrupt() {
        let mut bytes = encode_v1(&[0, 0], &[], &[]).unwrap();
        let norms_len = u32::from_le_bytes(bytes[5..9].try_into().unwrap()) as usize;
        let df_at = PREFIX_LEN_V1 + norms_len;
        assert_eq!(
            bytes.len() - df_at,
            4,
            "empty df_agg is the count word only"
        );
        bytes[df_at..df_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&bytes, 0).err(), Some(Error::Corrupt("STNF df_agg")));
    }

    #[test]
    fn v1_encoder_rejects_unsorted_empty_or_zero_df() {
        assert_eq!(
            encode_v1(&[0, 0], &[], &[("b", 1), ("a", 1)]).err(),
            Some(Error::Corrupt("STNF df_agg"))
        );
        assert_eq!(
            encode_v1(&[0, 0], &[], &[("", 1)]).err(),
            Some(Error::Corrupt("STNF df_agg"))
        );
        assert_eq!(
            encode_v1(&[0, 0], &[], &[("a", 0)]).err(),
            Some(Error::Corrupt("STNF df_agg"))
        );
    }

    #[test]
    fn inspect_agrees_with_fields_codec_grammar() {
        assert_eq!(
            inspect_stored_term("~0~~~0~~foo").unwrap(),
            Some((0, "~0~foo".into()))
        );
        assert_eq!(
            inspect_stored_term("~0~~0~foo").err(),
            Some(Error::Corrupt("STNF unpaired tilde"))
        );
        assert_eq!(inspect_stored_term("beer").unwrap(), None);
        assert_eq!(inspect_stored_term("~hello").unwrap(), None);
        assert_eq!(
            inspect_stored_term("~0~foo~").err(),
            Some(Error::Corrupt("STNF unpaired tilde"))
        );
        assert_eq!(
            inspect_stored_term(&test_fielded_key(10, "foo")).unwrap(),
            Some((10, "foo".into()))
        );
        assert_eq!(
            inspect_stored_term(&test_fielded_key(0, "~")).unwrap(),
            Some((0, "~".into()))
        );
    }
}
