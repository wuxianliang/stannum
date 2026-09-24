// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Byte layouts for LDP2 index pages. No buffer, lock or WAL code lives here,
//! so every codec is testable as plain bytes.
//!
//! Every page is a standard PostgreSQL page with an 8-byte special area:
//!
//! ```text
//! special := magic u32 "LDP2", kind u8, version u8, flags u16
//! payload := bytes [PAGE_HEADER, pd_lower)
//! ```
//!
//! `flags` was reserved (always zero) before removal-horizon logging existed;
//! readers never validate it. Only the meta page uses it: bit 0
//! ([`FLAG_REMOVAL_HORIZONS`]) says the writer logs removal horizons for
//! standbys (see `storage::wal`). A writer without that support clears it.
//!
//! Page kinds:
//!
//! * `META` (block 0): tokenizer spec, write-buffer state, segment directory
//!   and runs awaiting reclamation.
//! * `BUFFER`: a chained page of the write buffer. Payload is a `next` link
//!   followed by raw stream bytes; the meta page's byte count says how much of
//!   the chain is live.
//! * `RUN`: a chained page of an immutable blob (a segment or a dead list).
//! * `FREE`: a page released by a merge or VACUUM, reusable through the FSM.

use std::mem::{offset_of, size_of};

use pgrx::pg_sys;

pub const MAGIC: u32 = 0x4c44_5032;
pub const VERSION: u8 = 2;
pub const SPECIAL_SIZE: usize = 8;
pub const PAGE_SIZE: usize = pg_sys::BLCKSZ as usize;
pub const PAGE_HEADER: usize = size_of::<pg_sys::PageHeaderData>();
/// Payload bytes available on any page.
pub const CAPACITY: usize = PAGE_SIZE - PAGE_HEADER - SPECIAL_SIZE;
/// Payload bytes on a chained page after its `next` link.
pub const CHAIN_CAPACITY: usize = CAPACITY - 4;
pub const NONE: u32 = u32::MAX;

pub const KIND_META: u8 = 1;
pub const KIND_BUFFER: u8 = 2;
pub const KIND_RUN: u8 = 3;
pub const KIND_FREE: u8 = 4;

/// Meta-page flag: the last writer of this index emits removal-horizon WAL
/// records before freeing pages, so a hot standby with the same resource
/// manager can serve segmented reads from it.
pub const FLAG_REMOVAL_HORIZONS: u16 = 1;

/// Extension record tag for the analysis identity trailer. P0-3 knows only
/// this tag; future tags are deliberately rejected until their decoder ships.
pub(crate) const ANALYSIS_TAG: u8 = 0x01;
pub(crate) const FIELDS_TAG: u8 = 0x02;
const FLAGS: usize = PAGE_SIZE - SPECIAL_SIZE + 6;

/// Directory entries the meta page can hold before a merge is forced.
pub const MAX_SEGMENTS: usize = 128;
/// Runs the meta page can hold while they wait for readers to drain.
pub const MAX_PENDING: usize = 64;

const LOWER: usize = offset_of!(pg_sys::PageHeaderData, pd_lower);
const UPPER: usize = offset_of!(pg_sys::PageHeaderData, pd_upper);
const SPECIAL: usize = offset_of!(pg_sys::PageHeaderData, pd_special);

pub type Result<T> = std::result::Result<T, &'static str>;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

pub fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(raw)
}

/// Validates a page's header and special area and returns its kind.
pub fn kind(page: &[u8]) -> Result<u8> {
    if page.len() != PAGE_SIZE {
        return Err("invalid Stannum page size");
    }
    let lower = usize::from(u16_at(page, LOWER));
    let upper = usize::from(u16_at(page, UPPER));
    let special = usize::from(u16_at(page, SPECIAL));
    if special != PAGE_SIZE - SPECIAL_SIZE {
        return Err("not a Stannum LDP2 page");
    }
    if lower < PAGE_HEADER || lower > upper || upper > special {
        return Err("invalid Stannum page bounds");
    }
    if u32_at(page, special) != MAGIC {
        return Err("not a Stannum LDP2 page");
    }
    if page[special + 5] != VERSION {
        return Err("unsupported Stannum page version");
    }
    Ok(page[special + 4])
}

/// The flags of a validated page (zero on pages written before flags existed).
pub fn flags(page: &[u8]) -> u16 {
    u16_at(page, FLAGS)
}

/// Sets the flags of a page already written by [`write`].
pub fn set_flags(page: &mut [u8], flags: u16) {
    page[FLAGS..FLAGS + 2].copy_from_slice(&flags.to_le_bytes());
}

/// The live payload of a validated page.
pub fn payload(page: &[u8]) -> &[u8] {
    let lower = usize::from(u16_at(page, LOWER));
    &page[PAGE_HEADER..lower]
}

/// Writes a payload into a page initialized by `PageInit` with
/// `SPECIAL_SIZE`, stamping the special area.
pub fn write(page: &mut [u8], kind: u8, payload: &[u8]) -> Result<()> {
    if page.len() != PAGE_SIZE || payload.len() > CAPACITY {
        return Err("Stannum page payload too large");
    }
    let special = PAGE_SIZE - SPECIAL_SIZE;
    page[PAGE_HEADER..PAGE_HEADER + payload.len()].copy_from_slice(payload);
    let lower = (PAGE_HEADER + payload.len()) as u16;
    page[LOWER..LOWER + 2].copy_from_slice(&lower.to_le_bytes());
    page[UPPER..UPPER + 2].copy_from_slice(&(special as u16).to_le_bytes());
    page[SPECIAL..SPECIAL + 2].copy_from_slice(&(special as u16).to_le_bytes());
    page[special..special + 4].copy_from_slice(&MAGIC.to_le_bytes());
    page[special + 4] = kind;
    page[special + 5] = VERSION;
    page[special + 6] = 0;
    page[special + 7] = 0;
    Ok(())
}

/// A chained page's link and data.
pub fn chain(page: &[u8]) -> Result<(u32, &[u8])> {
    let payload = payload(page);
    if payload.len() < 4 {
        return Err("truncated Stannum chain page");
    }
    Ok((u32_at(payload, 0), &payload[4..]))
}

pub fn chain_payload(next: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// A chain of pages holding one immutable blob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub first: u32,
    pub blocks: u32,
    pub bytes: u32,
}

impl Run {
    pub const EMPTY: Self = Self {
        first: NONE,
        blocks: 0,
        bytes: 0,
    };

    pub const fn is_empty(&self) -> bool {
        self.first == NONE
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentEntry {
    pub run: Run,
    /// The run's block numbers in order, so byte offsets map to pages.
    pub map: Run,
    pub dead: Run,
    pub docs: u32,
    pub total_length: u64,
    pub generation: u32,
}

const ENTRY_BYTES: usize = 12 + 12 + 12 + 4 + 8 + 4;

/// A run waiting until every scan that could still read it has finished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    pub run: Run,
    /// Reclaimable once this transaction id is older than every snapshot.
    pub xid: u32,
}

const PENDING_BYTES: usize = 12 + 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferState {
    /// Incremented on every change, so backends can cache the buffer's contents.
    pub version: u32,
    /// Incremented when the buffer is rewritten from its head (fold, VACUUM),
    /// so a cached prefix knows appends since it was built are still valid.
    pub epoch: u32,
    pub head: u32,
    /// Page holding the byte after the last written one.
    pub tail: u32,
    /// Bytes used on the tail page.
    pub tail_used: u32,
    pub bytes: u32,
    pub docs: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnalysisStamp {
    pub(crate) jieba_rs_version: u32,
    pub(crate) dict_fingerprint: u64,
}

/// The `FIELDS_TAG` extension record of `fields`, exactly as the meta page
/// carries it: `tag u8, payload_len u32le, payload` (RFC §5.7). `None` when
/// the plan cannot be encoded, which the decoder's own rules rule out for a
/// decoded plan.
///
/// The insert path compares these bytes next to `(identity, spec)`, so a
/// concurrent REINDEX that changes the field count cannot publish a forward
/// record the rebuilt index does not match.
#[must_use]
pub fn fields_tag_bytes(fields: &FieldMeta) -> Option<Vec<u8>> {
    if fields.names.len() != fields.weights.len() || !(2..=16).contains(&fields.names.len()) {
        return None;
    }
    let mut payload = Vec::new();
    payload.push(1);
    payload.push(fields.names.len() as u8);
    payload.extend_from_slice(&0u16.to_le_bytes());
    for (name, weight) in fields.names.iter().zip(&fields.weights) {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.len() > u16::MAX as usize
            || !weight.is_finite()
            || *weight <= 0.0
        {
            return None;
        }
        payload.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        payload.extend_from_slice(bytes);
        payload.extend_from_slice(&weight.to_bits().to_le_bytes());
    }
    let mut record = Vec::with_capacity(5 + payload.len());
    record.push(FIELDS_TAG);
    record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    record.extend_from_slice(&payload);
    Some(record)
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldMeta {
    pub names: Vec<String>,
    pub weights: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    /// Distinguishes this index's contents from a reused relation number.
    pub identity: u64,
    pub spec: [u8; crate::options::SPEC_BYTES],
    pub buffer: BufferState,
    pub next_generation: u32,
    pub segments: Vec<SegmentEntry>,
    pub pending: Vec<Pending>,
    /// Optional tail-appended analysis identity. `None` is the legacy format.
    pub analysis: Option<AnalysisStamp>,
    pub fields: Option<FieldMeta>,
}

const META_HEADER: usize = 8 + crate::options::SPEC_BYTES + 28 + 4 + 4 + 4;

fn put_run(out: &mut Vec<u8>, run: Run) {
    out.extend_from_slice(&run.first.to_le_bytes());
    out.extend_from_slice(&run.blocks.to_le_bytes());
    out.extend_from_slice(&run.bytes.to_le_bytes());
}

fn get_run(bytes: &[u8], at: usize) -> Run {
    Run {
        first: u32_at(bytes, at),
        blocks: u32_at(bytes, at + 4),
        bytes: u32_at(bytes, at + 8),
    }
}

impl Meta {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.segments.len() > MAX_SEGMENTS || self.pending.len() > MAX_PENDING {
            return Err("Stannum meta page overflow");
        }
        let mut out = Vec::with_capacity(
            META_HEADER + self.segments.len() * ENTRY_BYTES + self.pending.len() * PENDING_BYTES,
        );
        out.extend_from_slice(&self.identity.to_le_bytes());
        out.extend_from_slice(&self.spec);
        out.extend_from_slice(&self.buffer.version.to_le_bytes());
        out.extend_from_slice(&self.buffer.epoch.to_le_bytes());
        out.extend_from_slice(&self.buffer.head.to_le_bytes());
        out.extend_from_slice(&self.buffer.tail.to_le_bytes());
        out.extend_from_slice(&self.buffer.tail_used.to_le_bytes());
        out.extend_from_slice(&self.buffer.bytes.to_le_bytes());
        out.extend_from_slice(&self.buffer.docs.to_le_bytes());
        out.extend_from_slice(&self.next_generation.to_le_bytes());
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.pending.len() as u32).to_le_bytes());
        for entry in &self.segments {
            put_run(&mut out, entry.run);
            put_run(&mut out, entry.map);
            put_run(&mut out, entry.dead);
            out.extend_from_slice(&entry.docs.to_le_bytes());
            out.extend_from_slice(&entry.total_length.to_le_bytes());
            out.extend_from_slice(&entry.generation.to_le_bytes());
        }
        for pending in &self.pending {
            put_run(&mut out, pending.run);
            out.extend_from_slice(&pending.xid.to_le_bytes());
        }
        if let Some(fields) = &self.fields {
            let record = fields_tag_bytes(fields).ok_or("invalid Stannum fields metadata")?;
            out.extend_from_slice(&record);
        }
        if let Some(analysis) = self.analysis {
            out.push(ANALYSIS_TAG);
            out.extend_from_slice(&16u32.to_le_bytes());
            out.extend_from_slice(&analysis.jieba_rs_version.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&analysis.dict_fingerprint.to_le_bytes());
        }
        if out.len() > CAPACITY {
            return Err("Stannum meta page overflow");
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < META_HEADER {
            return Err("truncated Stannum meta page");
        }
        let identity = u64_at(bytes, 0);
        let mut spec = [0u8; crate::options::SPEC_BYTES];
        spec.copy_from_slice(&bytes[8..8 + crate::options::SPEC_BYTES]);
        let mut at = 8 + crate::options::SPEC_BYTES;
        let buffer = BufferState {
            version: u32_at(bytes, at),
            epoch: u32_at(bytes, at + 4),
            head: u32_at(bytes, at + 8),
            tail: u32_at(bytes, at + 12),
            tail_used: u32_at(bytes, at + 16),
            bytes: u32_at(bytes, at + 20),
            docs: u32_at(bytes, at + 24),
        };
        at += 28;
        let next_generation = u32_at(bytes, at);
        let segment_count = u32_at(bytes, at + 4) as usize;
        let pending_count = u32_at(bytes, at + 8) as usize;
        at += 12;
        let records_at = at
            .checked_add(
                segment_count
                    .checked_mul(ENTRY_BYTES)
                    .ok_or("invalid Stannum meta page")?,
            )
            .and_then(|at| at.checked_add(pending_count.checked_mul(PENDING_BYTES)?))
            .ok_or("invalid Stannum meta page")?;
        if segment_count > MAX_SEGMENTS || pending_count > MAX_PENDING || bytes.len() < records_at {
            return Err("invalid Stannum meta page");
        }
        let mut segments = Vec::with_capacity(segment_count);
        for _ in 0..segment_count {
            let entry = SegmentEntry {
                run: get_run(bytes, at),
                map: get_run(bytes, at + 12),
                dead: get_run(bytes, at + 24),
                docs: u32_at(bytes, at + 36),
                total_length: u64_at(bytes, at + 40),
                generation: u32_at(bytes, at + 48),
            };
            if entry.run.is_empty() || entry.run.blocks == 0 {
                return Err("invalid Stannum segment entry");
            }
            segments.push(entry);
            at += ENTRY_BYTES;
        }
        let mut pending = Vec::with_capacity(pending_count);
        for _ in 0..pending_count {
            pending.push(Pending {
                run: get_run(bytes, at),
                xid: u32_at(bytes, at + 12),
            });
            at += PENDING_BYTES;
        }
        let mut analysis = None;
        let mut fields = None;
        while at < bytes.len() {
            if bytes.len() - at < 5 {
                return Err("truncated Stannum meta extension record");
            }
            let tag = bytes[at];
            let payload_len = u32_at(bytes, at + 1) as usize;
            at += 5;
            let end = at
                .checked_add(payload_len)
                .ok_or("invalid Stannum meta extension record")?;
            if end > bytes.len() {
                return Err("truncated Stannum meta extension record");
            }
            match tag {
                FIELDS_TAG => {
                    if fields.is_some() {
                        return Err("duplicate Stannum meta fields record");
                    }
                    let payload = &bytes[at..end];
                    if payload.len() < 4
                        || payload[0] != 1
                        || !(2..=16).contains(&(payload[1] as usize))
                        || u16::from_le_bytes([payload[2], payload[3]]) != 0
                    {
                        return Err("invalid Stannum meta fields record");
                    }
                    let count = payload[1] as usize;
                    let mut p = 4;
                    let mut names = Vec::with_capacity(count);
                    let mut weights = Vec::with_capacity(count);
                    for _ in 0..count {
                        if p + 2 > payload.len() {
                            return Err("invalid Stannum meta fields record");
                        }
                        let len = u16::from_le_bytes([payload[p], payload[p + 1]]) as usize;
                        p += 2;
                        if len == 0 || p + len + 4 > payload.len() {
                            return Err("invalid Stannum meta fields record");
                        }
                        let name = std::str::from_utf8(&payload[p..p + len])
                            .map_err(|_| "invalid Stannum meta fields record")?
                            .to_owned();
                        p += len;
                        let weight = f32::from_bits(u32::from_le_bytes(
                            payload[p..p + 4].try_into().unwrap(),
                        ));
                        p += 4;
                        if !weight.is_finite() || weight <= 0.0 || names.iter().any(|n| n == &name)
                        {
                            return Err("invalid Stannum meta fields record");
                        }
                        names.push(name);
                        weights.push(weight);
                    }
                    if p != payload.len() {
                        return Err("invalid Stannum meta fields record");
                    }
                    fields = Some(FieldMeta { names, weights });
                }
                ANALYSIS_TAG => {
                    if analysis.is_some() {
                        return Err("duplicate Stannum meta analysis record");
                    }
                    if payload_len != 16 {
                        return Err("invalid Stannum meta analysis record");
                    }
                    let jieba_rs_version = u32_at(bytes, at);
                    let reserved = u32_at(bytes, at + 4);
                    if reserved != 0 {
                        return Err("invalid Stannum meta analysis record");
                    }
                    analysis = Some(AnalysisStamp {
                        jieba_rs_version,
                        dict_fingerprint: u64_at(bytes, at + 8),
                    });
                }
                _ => return Err("unknown Stannum meta extension record"),
            }
            at = end;
        }
        Ok(Self {
            identity,
            spec,
            buffer,
            next_generation,
            segments,
            pending,
            analysis,
            fields,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta() -> Meta {
        Meta {
            identity: 0x1234_5678_9abc_def0,
            spec: [0, 1, 1, 2, 0, 1, 1, 1],
            buffer: BufferState {
                version: 5,
                epoch: 2,
                head: 1,
                tail: 7,
                tail_used: 300,
                bytes: 40_000,
                docs: 12,
            },
            next_generation: 3,
            segments: vec![
                SegmentEntry {
                    run: Run {
                        first: 2,
                        blocks: 5,
                        bytes: 40_123,
                    },
                    map: Run {
                        first: 12,
                        blocks: 1,
                        bytes: 20,
                    },
                    dead: Run::EMPTY,
                    docs: 100,
                    total_length: 12_345,
                    generation: 1,
                },
                SegmentEntry {
                    run: Run {
                        first: 9,
                        blocks: 1,
                        bytes: 12,
                    },
                    map: Run::EMPTY,
                    dead: Run {
                        first: 10,
                        blocks: 1,
                        bytes: 5,
                    },
                    docs: 1,
                    total_length: 2,
                    generation: 2,
                },
            ],
            pending: vec![Pending {
                run: Run {
                    first: 11,
                    blocks: 2,
                    bytes: 9_000,
                },
                xid: 77,
            }],
            analysis: None,
            fields: None,
        }
    }

    #[test]
    fn meta_round_trips_and_rejects_malformed_input() {
        let meta = sample_meta();
        let bytes = meta.encode().unwrap();
        assert_eq!(Meta::decode(&bytes).unwrap(), meta);
        assert!(Meta::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(Meta::decode(&[]).is_err());

        let mut stamped = meta.clone();
        stamped.analysis = Some(AnalysisStamp {
            jieba_rs_version: 0x0000_0704,
            dict_fingerprint: 0x0123_4567_89ab_cdef,
        });
        let stamped_bytes = stamped.encode().unwrap();
        assert_eq!(Meta::decode(&stamped_bytes).unwrap(), stamped);
        assert_eq!(Meta::decode(&bytes).unwrap().analysis, None);
        let mut too_many = meta.clone();
        too_many.segments = vec![meta.segments[0]; MAX_SEGMENTS + 1];
        assert!(too_many.encode().is_err());
        let mut full = meta.clone();
        full.segments = vec![meta.segments[0]; MAX_SEGMENTS];
        full.pending = vec![meta.pending[0]; MAX_PENDING];
        let bytes = full.encode().unwrap();
        assert!(bytes.len() <= CAPACITY);
        assert_eq!(Meta::decode(&bytes).unwrap(), full);

        let mut full_stamped = full;
        full_stamped.analysis = Some(AnalysisStamp {
            jieba_rs_version: 1,
            dict_fingerprint: 2,
        });
        let bytes = full_stamped.encode().unwrap();
        assert!(bytes.len() <= CAPACITY);
        assert_eq!(Meta::decode(&bytes).unwrap(), full_stamped);
    }

    #[test]
    fn analysis_trailer_rejects_duplicate_unknown_truncated_and_reserved_records() {
        let meta = sample_meta();
        let mut bytes = meta.encode().unwrap();
        bytes.push(ANALYSIS_TAG);
        assert!(Meta::decode(&bytes).is_err());

        let mut bytes = meta.encode().unwrap();
        bytes.extend_from_slice(&[ANALYSIS_TAG, 16, 0, 0, 0]);
        assert!(Meta::decode(&bytes).is_err());

        let mut bytes = meta.encode().unwrap();
        bytes.extend_from_slice(&[ANALYSIS_TAG, 17, 0, 0, 0]);
        bytes.extend_from_slice(&[0; 16]);
        assert!(Meta::decode(&bytes).is_err());

        let mut bytes = meta.encode().unwrap();
        bytes.extend_from_slice(&[ANALYSIS_TAG, 16, 0, 0, 0]);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&2u64.to_le_bytes());
        assert!(Meta::decode(&bytes).is_err());

        let stamped = AnalysisStamp {
            jieba_rs_version: 1,
            dict_fingerprint: 2,
        };
        let mut bytes = meta.clone();
        bytes.analysis = Some(stamped);
        let mut bytes = bytes.encode().unwrap();
        bytes.extend_from_slice(&[ANALYSIS_TAG, 16, 0, 0, 0]);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&2u64.to_le_bytes());
        assert!(Meta::decode(&bytes).is_err());

        let mut bytes = meta.encode().unwrap();
        bytes.extend_from_slice(&[0x02, 0, 0, 0, 0]);
        assert!(Meta::decode(&bytes).is_err());
    }

    #[test]
    fn stamped_trailer_all_single_byte_mutations_and_truncations_are_checked() {
        let mut meta = sample_meta();
        let legacy = meta.encode().unwrap();
        meta.analysis = Some(AnalysisStamp {
            jieba_rs_version: 0x0000_0704,
            dict_fingerprint: 0x0123_4567_89ab_cdef,
        });
        let bytes = meta.encode().unwrap();
        let start = legacy.len();
        assert_eq!(bytes.len() - start, 21);
        for at in start..bytes.len() {
            for replacement in 0..=u8::MAX {
                if replacement == bytes[at] {
                    continue;
                }
                let mut mutated = bytes.clone();
                mutated[at] = replacement;
                // No catch_unwind: any decoder panic fails the test. Version
                // and fingerprint bytes are opaque; framing/reserved bytes aren't.
                let result = Meta::decode(&mutated);
                if (start + 5..start + 9).contains(&at) || at >= start + 13 {
                    let decoded = result.unwrap_or_else(|error| {
                        panic!("opaque identity mutation at {at}: {replacement}: {error}")
                    });
                    assert_eq!(decoded.encode().unwrap(), mutated);
                } else {
                    assert!(result.is_err(), "accepted mutation at {at}: {replacement}");
                }
            }
        }
        assert_eq!(Meta::decode(&bytes[..start]).unwrap().analysis, None);
        for end in start + 1..bytes.len() {
            assert!(
                Meta::decode(&bytes[..end]).is_err(),
                "accepted prefix {end}"
            );
        }
        // No single garbage byte appended to a legacy page is a valid trailer.
        for garbage in 0..=u8::MAX {
            let mut trailing = legacy.clone();
            trailing.push(garbage);
            assert!(Meta::decode(&trailing).is_err());
        }
    }

    #[test]
    fn stamped_meta_rejects_one_entry_beyond_the_pending_count_limit() {
        let mut meta = sample_meta();
        meta.segments = vec![meta.segments[0]; MAX_SEGMENTS];
        meta.pending = vec![meta.pending[0]; MAX_PENDING];
        meta.analysis = Some(AnalysisStamp {
            jieba_rs_version: 1,
            dict_fingerprint: 2,
        });
        let bytes = meta.encode().unwrap();
        assert!(bytes.len() <= CAPACITY);
        assert_eq!(Meta::decode(&bytes).unwrap(), meta);
        // The configured count limit leaves byte slack; this specifically
        // exercises that guard, not the later serialized-size guard.
        meta.pending.push(meta.pending[0]);
        assert_eq!(meta.encode().unwrap_err(), "Stannum meta page overflow");
    }

    #[test]
    fn pages_stamp_and_validate_special_area() {
        let mut page = vec![0u8; PAGE_SIZE];
        let payload = chain_payload(42, b"hello");
        write(&mut page, KIND_BUFFER, &payload).unwrap();
        assert_eq!(kind(&page).unwrap(), KIND_BUFFER);
        assert_eq!(chain(&page).unwrap(), (42, &b"hello"[..]));
        let mut bad = page.clone();
        bad[PAGE_SIZE - SPECIAL_SIZE] ^= 1;
        assert!(kind(&bad).is_err());
        let mut legacy = vec![0u8; PAGE_SIZE];
        legacy[SPECIAL..SPECIAL + 2].copy_from_slice(&(PAGE_SIZE as u16).to_le_bytes());
        assert!(kind(&legacy).is_err());
        assert!(write(&mut page, KIND_RUN, &vec![0; CAPACITY + 1]).is_err());
        assert!(write(&mut page, KIND_RUN, &vec![7; CAPACITY]).is_ok());
        assert_eq!(super::payload(&page).len(), CAPACITY);
    }

    #[test]
    fn flags_live_in_the_special_area_and_are_not_validated() {
        let mut page = vec![0u8; PAGE_SIZE];
        write(&mut page, KIND_META, b"meta").unwrap();
        assert_eq!(flags(&page), 0);
        set_flags(&mut page, FLAG_REMOVAL_HORIZONS);
        assert_eq!(flags(&page), FLAG_REMOVAL_HORIZONS);
        assert_eq!(kind(&page).unwrap(), KIND_META);
        assert_eq!(super::payload(&page), b"meta");
        // A page written by a build without flags reads as zero, and a
        // rewrite clears whatever was there.
        write(&mut page, KIND_META, b"meta").unwrap();
        assert_eq!(flags(&page), 0);
    }
}
