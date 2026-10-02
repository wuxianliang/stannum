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
//! * `META` (kind 1, block 0): 0.4.0 tokenizer spec, write-buffer state and
//!   directory. Readers classify it as [`MetaPageClass::PreStn3`].
//! * `BUFFER`: a chained page of the write buffer. Payload is a `next` link
//!   followed by raw stream bytes; the meta page's byte count says how much of
//!   the chain is live.
//! * `RUN`: a chained page of an immutable blob (a segment or a dead list).
//! * `FREE`: a page released by a merge or VACUUM, reusable through the FSM.
//! * `ENVELOPE` (kind 5, block 0): 0.5.0 meta image plus one framed `STNM`
//!   record. Every current index writes this, including single-column.

use std::collections::HashSet;
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
pub const KIND_ENVELOPE: u8 = 5;

/// Meta-page flag: the last writer of this index emits removal-horizon WAL
/// records before freeing pages, so a hot standby with the same resource
/// manager can serve segmented reads from it.
pub const FLAG_REMOVAL_HORIZONS: u16 = 1;
/// 0.4.0/3.2-dev analysis trailer tag. Kind-1 pages may still carry it; it is
/// never Current. 0.5.0 stores the stamp inside the STNM body instead.
#[allow(dead_code)] // 3.2 leftover; kind-1 tails stay PreStn3, not Current.
pub(crate) const ANALYSIS_TAG: u8 = 0x01;
const STNM_MAGIC: &[u8; 4] = b"STNM";
const STNM_VERSION: u8 = 1;
const STNM_STAMP_BYTES: usize = 12;
const MIN_FIELDS: u8 = 1;
const MAX_FIELDS: u8 = 16;
const MIN_NAME_LEN: usize = 1;
const MAX_NAME_LEN: usize = 63;
const FLAGS: usize = PAGE_SIZE - SPECIAL_SIZE + 6;

/// Directory entries the meta page can hold before a merge is forced.
pub const MAX_SEGMENTS: usize = 96;
/// Runs the meta page can hold while they wait for readers to drain: what
/// fits beside a full directory.
pub const MAX_PENDING: usize = 48;

const LOWER: usize = offset_of!(pg_sys::PageHeaderData, pd_lower);
const UPPER: usize = offset_of!(pg_sys::PageHeaderData, pd_upper);
const SPECIAL: usize = offset_of!(pg_sys::PageHeaderData, pd_special);

pub type Result<T> = std::result::Result<T, &'static str>;

/// Why [`Meta::decode`] rejected an envelope payload. Migration of a kind-1
/// page is [`MetaPageClass::PreStn3`], not this error.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaError {
    Invalid(&'static str),
}

/// Pure parser result for page 0. Recovery, WAL redo and page verify call
/// [`classify_meta_page`]; live relation checks belong in `open_index`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetaPageClass {
    Current(Meta),
    PreStn3,
    Corrupt(&'static str),
}

/// One STNM key slot. The stored name is inert on a single-column index.
#[derive(Clone, Debug)]
pub(crate) struct EnvelopeField {
    pub(crate) name: String,
    pub(crate) weight: f32,
}

impl PartialEq for EnvelopeField {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.weight.to_bits() == other.weight.to_bits()
    }
}

impl Eq for EnvelopeField {}

impl EnvelopeField {
    pub(crate) fn single(name: impl Into<String>) -> Vec<Self> {
        vec![Self {
            name: name.into(),
            weight: 1.0,
        }]
    }
}

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
    /// The chain's last page, so joining two retired chains writes one
    /// page instead of walking a run that may be millions of pages, under
    /// the exclusive meta lock.
    pub last: u32,
}

impl Run {
    pub const EMPTY: Self = Self {
        first: NONE,
        blocks: 0,
        bytes: 0,
        last: NONE,
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
    /// Names this dead list, from the index's generation counter: a
    /// replacement can reuse the old list's pages and size, so the run
    /// alone does not tell a reader's cached copy from the current one.
    pub dead_stamp: u32,
    pub docs: u32,
    pub total_length: u64,
    pub generation: u32,
}

const RUN_BYTES: usize = 16;
const ENTRY_BYTES: usize = RUN_BYTES * 3 + 4 + 4 + 8 + 4;
/// 0.4.0 directory sizes: `Run` was 12 bytes (no `last`) and a segment
/// entry had no `dead_stamp`. `52s + 16p` equals `68s + 20p` only at
/// `s = 0, p = 0`, which stays on the current empty-directory path.
const LEGACY_ENTRY_BYTES: usize = 52;
const LEGACY_PENDING_BYTES: usize = 16;

/// A run waiting until every scan that could still read it has finished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pending {
    pub run: Run,
    /// Reclaimable once this transaction id is older than every snapshot.
    pub xid: u32,
}

const PENDING_BYTES: usize = RUN_BYTES + 4;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Distinguishes this index's contents from a reused relation number.
    pub identity: u64,
    pub spec: [u8; crate::options::SPEC_BYTES],
    pub buffer: BufferState,
    pub next_generation: u32,
    pub segments: Vec<SegmentEntry>,
    pub pending: Vec<Pending>,
    /// STNM key slots. Single-column indexes store one inert name at weight 1.0.
    pub(crate) fields: Vec<EnvelopeField>,
    /// Analysis identity from the STNM flag/stamp. `None` is flag = 0.
    pub(crate) analysis: Option<AnalysisStamp>,
}

const META_HEADER: usize = 8 + crate::options::SPEC_BYTES + 28 + 4 + 4 + 4;

fn put_run(out: &mut Vec<u8>, run: Run) {
    out.extend_from_slice(&run.first.to_le_bytes());
    out.extend_from_slice(&run.blocks.to_le_bytes());
    out.extend_from_slice(&run.bytes.to_le_bytes());
    out.extend_from_slice(&run.last.to_le_bytes());
}

fn get_run(bytes: &[u8], at: usize) -> Run {
    Run {
        first: u32_at(bytes, at),
        blocks: u32_at(bytes, at + 4),
        bytes: u32_at(bytes, at + 8),
        last: u32_at(bytes, at + 12),
    }
}

struct MetaHeader {
    identity: u64,
    spec: [u8; crate::options::SPEC_BYTES],
    buffer: BufferState,
    next_generation: u32,
    segment_count: usize,
    pending_count: usize,
    dir_at: usize,
}

fn parse_meta_header(bytes: &[u8]) -> std::result::Result<MetaHeader, &'static str> {
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
    if segment_count > MAX_SEGMENTS || pending_count > MAX_PENDING {
        return Err("invalid Stannum meta page");
    }
    Ok(MetaHeader {
        identity,
        spec,
        buffer,
        next_generation,
        segment_count,
        pending_count,
        dir_at: at,
    })
}

fn current_image_len(header: &MetaHeader) -> usize {
    header.dir_at + header.segment_count * ENTRY_BYTES + header.pending_count * PENDING_BYTES
}

fn legacy_image_len(header: &MetaHeader) -> usize {
    header.dir_at
        + header.segment_count * LEGACY_ENTRY_BYTES
        + header.pending_count * LEGACY_PENDING_BYTES
}

/// Kind-1 0.4.0 layout, including empty, buffer-only, current-stride and
/// leftover ANALYSIS_TAG tails. Length of the recognizable prefix is enough;
/// extra bytes after it do not make the page Current.
fn recognizable_040_layout(bytes: &[u8]) -> bool {
    let Ok(header) = parse_meta_header(bytes) else {
        return false;
    };
    let current_len = current_image_len(&header);
    let legacy_len = legacy_image_len(&header);
    bytes.len() >= current_len || bytes.len() == legacy_len
}

fn parse_current_directory(
    bytes: &[u8],
    header: &MetaHeader,
    validate_entries: bool,
) -> std::result::Result<(Vec<SegmentEntry>, Vec<Pending>), &'static str> {
    let current_len = current_image_len(header);
    if bytes.len() < current_len {
        return Err("invalid Stannum meta page");
    }
    let mut at = header.dir_at;
    let mut segments = Vec::with_capacity(header.segment_count);
    for _ in 0..header.segment_count {
        let entry = SegmentEntry {
            run: get_run(bytes, at),
            map: get_run(bytes, at + RUN_BYTES),
            dead: get_run(bytes, at + 2 * RUN_BYTES),
            dead_stamp: u32_at(bytes, at + 3 * RUN_BYTES),
            docs: u32_at(bytes, at + 3 * RUN_BYTES + 4),
            total_length: u64_at(bytes, at + 3 * RUN_BYTES + 8),
            generation: u32_at(bytes, at + 3 * RUN_BYTES + 16),
        };
        if validate_entries && (entry.run.is_empty() || entry.run.blocks == 0) {
            return Err("invalid Stannum segment entry");
        }
        segments.push(entry);
        at += ENTRY_BYTES;
    }
    let mut pending = Vec::with_capacity(header.pending_count);
    for _ in 0..header.pending_count {
        pending.push(Pending {
            run: get_run(bytes, at),
            xid: u32_at(bytes, at + RUN_BYTES),
        });
        at += PENDING_BYTES;
    }
    Ok((segments, pending))
}

fn encode_stnm(fields: &[EnvelopeField], analysis: Option<AnalysisStamp>) -> Result<Vec<u8>> {
    let count = fields.len();
    if count < usize::from(MIN_FIELDS) || count > usize::from(MAX_FIELDS) {
        return Err("invalid STNM field_count");
    }
    let mut names = HashSet::with_capacity(count);
    let mut body = Vec::new();
    body.push(count as u8);
    for field in fields {
        let name = field.name.as_bytes();
        if name.len() < MIN_NAME_LEN || name.len() > MAX_NAME_LEN || name.contains(&0) {
            return Err("invalid STNM name");
        }
        if std::str::from_utf8(name).is_err() {
            return Err("STNM name is not UTF-8");
        }
        if !field.weight.is_finite() || field.weight <= 0.0 {
            return Err("invalid STNM weight");
        }
        if !names.insert(field.name.as_str()) {
            return Err("duplicate STNM field name");
        }
        body.extend_from_slice(&(name.len() as u16).to_le_bytes());
        body.extend_from_slice(name);
        body.extend_from_slice(&field.weight.to_le_bytes());
    }
    match analysis {
        None => body.push(0),
        Some(stamp) => {
            if stamp.dict_fingerprint == 0 {
                return Err("STNM dict_fingerprint is 0");
            }
            body.push(1);
            body.extend_from_slice(&stamp.jieba_rs_version.to_le_bytes());
            body.extend_from_slice(&stamp.dict_fingerprint.to_le_bytes());
        }
    }
    let mut record = Vec::with_capacity(9 + body.len());
    record.extend_from_slice(STNM_MAGIC);
    record.push(STNM_VERSION);
    record.extend_from_slice(&(body.len() as u32).to_le_bytes());
    record.extend_from_slice(&body);
    Ok(record)
}

fn decode_stnm(
    bytes: &[u8],
) -> std::result::Result<(Vec<EnvelopeField>, Option<AnalysisStamp>), &'static str> {
    if bytes.len() < 9 {
        return Err("truncated STNM record");
    }
    if &bytes[..4] != STNM_MAGIC {
        return Err("invalid STNM magic");
    }
    if bytes[4] != STNM_VERSION {
        return Err("unsupported STNM version");
    }
    let body_len = u32_at(bytes, 5) as usize;
    let record_len = 9usize
        .checked_add(body_len)
        .ok_or("STNM body_len does not match body")?;
    if bytes.len() < record_len {
        return Err("truncated STNM record");
    }
    if bytes.len() > record_len {
        return Err("bytes after STNM record");
    }
    let body = &bytes[9..record_len];
    if body.is_empty() {
        return Err("truncated STNM record");
    }
    let field_count = body[0];
    if !(MIN_FIELDS..=MAX_FIELDS).contains(&field_count) {
        return Err("invalid STNM field_count");
    }
    let mut at = 1usize;
    let mut names = HashSet::new();
    let mut fields = Vec::with_capacity(usize::from(field_count));
    for _ in 0..field_count {
        if at + 2 > body.len() {
            return Err("truncated STNM record");
        }
        let name_len = u16_at(body, at) as usize;
        at += 2;
        if !(MIN_NAME_LEN..=MAX_NAME_LEN).contains(&name_len) {
            return Err("invalid STNM name length");
        }
        if at + name_len + 4 > body.len() {
            return Err("truncated STNM record");
        }
        let name_bytes = &body[at..at + name_len];
        if name_bytes.contains(&0) {
            return Err("STNM name contains interior NUL");
        }
        let name = std::str::from_utf8(name_bytes).map_err(|_| "STNM name is not UTF-8")?;
        at += name_len;
        let weight = f32::from_le_bytes(body[at..at + 4].try_into().unwrap());
        at += 4;
        if !weight.is_finite() || weight <= 0.0 {
            return Err("invalid STNM weight");
        }
        if !names.insert(name.to_owned()) {
            return Err("duplicate STNM field name");
        }
        fields.push(EnvelopeField {
            name: name.to_owned(),
            weight,
        });
    }
    if at >= body.len() {
        return Err("truncated STNM record");
    }
    let flag = body[at];
    at += 1;
    let analysis = match flag {
        0 => {
            if at != body.len() {
                return Err("bytes after STNM analysis flag");
            }
            None
        }
        1 => {
            if at + STNM_STAMP_BYTES > body.len() {
                return Err("truncated STNM record");
            }
            if at + STNM_STAMP_BYTES != body.len() {
                return Err("STNM body_len does not match body");
            }
            let jieba_rs_version = u32_at(body, at);
            let dict_fingerprint = u64_at(body, at + 4);
            if dict_fingerprint == 0 {
                return Err("STNM dict_fingerprint is 0");
            }
            Some(AnalysisStamp {
                jieba_rs_version,
                dict_fingerprint,
            })
        }
        _ => return Err("invalid STNM analysis flag"),
    };
    Ok((fields, analysis))
}

fn decode_envelope(bytes: &[u8]) -> std::result::Result<Meta, &'static str> {
    let header = parse_meta_header(bytes)?;
    let image_len = current_image_len(&header);
    if bytes.len() < image_len {
        return Err("invalid Stannum meta page");
    }
    let (segments, pending) = parse_current_directory(bytes, &header, true)?;
    let (fields, analysis) = decode_stnm(&bytes[image_len..])?;
    Ok(Meta {
        identity: header.identity,
        spec: header.spec,
        buffer: header.buffer,
        next_generation: header.next_generation,
        segments,
        pending,
        fields,
        analysis,
    })
}

/// Classifies a full LDP2 page image as the current envelope, a 0.4.0 meta
/// page that needs REINDEX, or corruption. Grammar only: no relation, STNF,
/// or segment-magic checks.
pub fn classify_meta_page(page: &[u8]) -> MetaPageClass {
    match kind(page) {
        Err(message) => MetaPageClass::Corrupt(message),
        Ok(page_kind) => classify_meta_kind(page_kind, payload(page)),
    }
}

pub fn classify_meta_kind(page_kind: u8, payload: &[u8]) -> MetaPageClass {
    match page_kind {
        KIND_ENVELOPE => match decode_envelope(payload) {
            Ok(meta) => MetaPageClass::Current(meta),
            Err(message) => MetaPageClass::Corrupt(message),
        },
        KIND_META => {
            if recognizable_040_layout(payload) {
                MetaPageClass::PreStn3
            } else if payload.len() < META_HEADER {
                MetaPageClass::Corrupt("truncated Stannum meta page")
            } else {
                MetaPageClass::Corrupt("invalid Stannum meta page")
            }
        }
        _ => MetaPageClass::Corrupt("meta page kind is neither envelope nor meta"),
    }
}

impl Meta {
    pub(crate) fn encode_image(&self) -> Result<Vec<u8>> {
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
            out.extend_from_slice(&entry.dead_stamp.to_le_bytes());
            out.extend_from_slice(&entry.docs.to_le_bytes());
            out.extend_from_slice(&entry.total_length.to_le_bytes());
            out.extend_from_slice(&entry.generation.to_le_bytes());
        }
        for pending in &self.pending {
            put_run(&mut out, pending.run);
            out.extend_from_slice(&pending.xid.to_le_bytes());
        }
        Ok(out)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = self.encode_image()?;
        out.extend_from_slice(&encode_stnm(&self.fields, self.analysis)?);
        if out.len() > CAPACITY {
            return Err("Stannum meta page overflow");
        }
        Ok(out)
    }

    #[allow(dead_code)]
    pub fn decode(bytes: &[u8]) -> std::result::Result<Self, MetaError> {
        decode_envelope(bytes).map_err(MetaError::Invalid)
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
                        last: 6,
                    },
                    map: Run {
                        first: 12,
                        blocks: 1,
                        bytes: 20,
                        last: 12,
                    },
                    dead: Run::EMPTY,
                    dead_stamp: 0,
                    docs: 100,
                    total_length: 12_345,
                    generation: 1,
                },
                SegmentEntry {
                    run: Run {
                        first: 9,
                        blocks: 1,
                        bytes: 12,
                        last: 9,
                    },
                    map: Run::EMPTY,
                    dead: Run {
                        first: 10,
                        blocks: 1,
                        bytes: 5,
                        last: 10,
                    },
                    dead_stamp: 7,
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
                    last: 12,
                },
                xid: 77,
            }],
            fields: EnvelopeField::single("expr"),
            analysis: None,
        }
    }

    fn empty_meta() -> Meta {
        Meta {
            identity: 1,
            spec: [0; crate::options::SPEC_BYTES],
            buffer: BufferState {
                version: 0,
                epoch: 0,
                head: 1,
                tail: 1,
                tail_used: 0,
                bytes: 0,
                docs: 0,
            },
            next_generation: 1,
            segments: Vec::new(),
            pending: Vec::new(),
            fields: EnvelopeField::single("expr"),
            analysis: None,
        }
    }

    fn buffer_only_meta() -> Meta {
        let mut meta = empty_meta();
        meta.buffer = BufferState {
            version: 3,
            epoch: 1,
            head: 1,
            tail: 2,
            tail_used: 40,
            bytes: 8_192,
            docs: 4,
        };
        meta
    }

    fn page_with(page_kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut page = vec![0u8; PAGE_SIZE];
        write(&mut page, page_kind, payload).unwrap();
        page
    }

    fn envelope_page(meta: &Meta) -> Vec<u8> {
        page_with(KIND_ENVELOPE, &meta.encode().unwrap())
    }

    fn kind1_page(payload: &[u8]) -> Vec<u8> {
        page_with(KIND_META, payload)
    }

    fn header_with_counts(segment_count: u32, pending_count: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; META_HEADER];
        bytes[META_HEADER - 8..META_HEADER - 4].copy_from_slice(&segment_count.to_le_bytes());
        bytes[META_HEADER - 4..META_HEADER].copy_from_slice(&pending_count.to_le_bytes());
        bytes
    }

    fn analysis_tag_trailer(stamp: AnalysisStamp) -> Vec<u8> {
        let mut out = vec![ANALYSIS_TAG];
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&stamp.jieba_rs_version.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&stamp.dict_fingerprint.to_le_bytes());
        out
    }

    fn frame_stnm(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::from(*STNM_MAGIC);
        out.push(STNM_VERSION);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    fn assert_current(page: &[u8]) -> Meta {
        match classify_meta_page(page) {
            MetaPageClass::Current(meta) => meta,
            other => panic!("expected Current, got {other:?}"),
        }
    }

    fn assert_pre_stn3(page: &[u8]) {
        assert_eq!(classify_meta_page(page), MetaPageClass::PreStn3);
    }

    fn assert_corrupt(page: &[u8]) {
        match classify_meta_page(page) {
            MetaPageClass::Corrupt(_) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn meta_round_trips_and_rejects_malformed_input() {
        let meta = sample_meta();
        let bytes = meta.encode().unwrap();
        assert_eq!(&bytes[meta.encode_image().unwrap().len()..][..4], b"STNM");
        assert_eq!(Meta::decode(&bytes).unwrap(), meta);
        assert!(Meta::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(Meta::decode(&[]).is_err());
        let mut stamped = meta.clone();
        stamped.analysis = Some(AnalysisStamp {
            jieba_rs_version: 0x0000_0704,
            dict_fingerprint: 0x0123_4567_89ab_cdef,
        });
        assert_eq!(Meta::decode(&stamped.encode().unwrap()).unwrap(), stamped);
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
        assert!(encode_stnm(&[], None).is_err());
        assert!(
            encode_stnm(
                &EnvelopeField::single("expr"),
                Some(AnalysisStamp {
                    jieba_rs_version: 1,
                    dict_fingerprint: 0,
                })
            )
            .is_err()
        );
    }

    #[test]
    fn classify_kind5_valid_v1_is_current_never_legacy() {
        let meta = sample_meta();
        let page = envelope_page(&meta);
        let got = assert_current(&page);
        assert_eq!(got, meta);
        assert_ne!(classify_meta_page(&page), MetaPageClass::PreStn3);
        assert_eq!(kind(&page).unwrap(), KIND_ENVELOPE);
        assert_eq!(VERSION, 2);
    }

    #[test]
    fn classify_kind5_stamped_analysis_fills_meta_analysis() {
        let mut meta = sample_meta();
        meta.spec = [2, 1, 1, 2, 0, 1, 1, 1];
        meta.analysis = Some(AnalysisStamp {
            jieba_rs_version: 0x0000_0704,
            dict_fingerprint: 0x6855_a073_6155_f3dd,
        });
        let got = assert_current(&envelope_page(&meta));
        assert_eq!(got.analysis, meta.analysis);
        assert_eq!(got.fields, EnvelopeField::single("expr"));
        assert_eq!(got.fields[0].weight, 1.0);
    }

    #[test]
    fn classify_kind5_round_trips_empty_buffer_only_immutable_and_replay() {
        for meta in [empty_meta(), buffer_only_meta(), sample_meta()] {
            let page = envelope_page(&meta);
            let got = assert_current(&page);
            assert_eq!(got.spec, meta.spec);
            assert_eq!(got.buffer, meta.buffer);
            assert_eq!(got.segments, meta.segments);
            assert_eq!(got.pending, meta.pending);
            assert_eq!(got.fields, meta.fields);
            // Generic WAL full-page image replay is the stored page bytes.
            let replayed = page.clone();
            let recovered = assert_current(&replayed);
            assert_eq!(recovered.spec, meta.spec);
            assert_eq!(recovered.segments, meta.segments);
            assert_eq!(recovered.pending, meta.pending);
            assert_eq!(recovered.buffer, meta.buffer);
        }
    }

    #[test]
    fn classify_every_040_kind1_shape_is_pre_stn3() {
        assert_pre_stn3(&kind1_page(&header_with_counts(0, 0)));
        assert_pre_stn3(&kind1_page(&buffer_only_meta().encode_image().unwrap()));

        let mut legacy = header_with_counts(1, 0);
        legacy.extend(vec![0u8; LEGACY_ENTRY_BYTES]);
        assert_pre_stn3(&kind1_page(&legacy));

        let current = sample_meta().encode_image().unwrap();
        assert_pre_stn3(&kind1_page(&current));

        let mut tagged = current;
        tagged.extend_from_slice(&analysis_tag_trailer(AnalysisStamp {
            jieba_rs_version: 1,
            dict_fingerprint: 2,
        }));
        assert_pre_stn3(&kind1_page(&tagged));
        assert_ne!(
            classify_meta_page(&kind1_page(&tagged)),
            classify_meta_page(&envelope_page(&sample_meta()))
        );
    }

    #[test]
    fn classify_kind1_garbage_length_is_corrupt() {
        let mut garbage = header_with_counts(1, 0);
        garbage.extend(vec![0u8; 40]);
        assert_corrupt(&kind1_page(&garbage));
        assert_eq!(
            classify_meta_page(&kind1_page(&garbage)),
            MetaPageClass::Corrupt("invalid Stannum meta page")
        );
    }

    #[test]
    fn classify_other_kinds_are_corrupt() {
        for page_kind in [KIND_BUFFER, KIND_RUN, KIND_FREE, 0, 6] {
            let page = page_with(page_kind, &sample_meta().encode().unwrap());
            assert_eq!(
                classify_meta_page(&page),
                MetaPageClass::Corrupt("meta page kind is neither envelope nor meta")
            );
        }
    }

    fn envelope_with_stnm(image: &[u8], stnm: &[u8]) -> Vec<u8> {
        let mut payload = image.to_vec();
        payload.extend_from_slice(stnm);
        page_with(KIND_ENVELOPE, &payload)
    }

    fn valid_flag0_body() -> Vec<u8> {
        let mut body = vec![1u8];
        body.extend_from_slice(&4u16.to_le_bytes());
        body.extend_from_slice(b"expr");
        body.extend_from_slice(&1.0f32.to_le_bytes());
        body.push(0);
        body
    }

    #[test]
    fn classify_kind5_grammar_corruption_classes() {
        let image = empty_meta().encode_image().unwrap();
        let valid = frame_stnm(&valid_flag0_body());
        assert_current(&envelope_with_stnm(&image, &valid));

        let mut bad_magic = valid.clone();
        bad_magic[0] = b'X';
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &bad_magic)),
            MetaPageClass::Corrupt("invalid STNM magic")
        );

        let mut bad_version = valid.clone();
        bad_version[4] = 2;
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &bad_version)),
            MetaPageClass::Corrupt("unsupported STNM version")
        );

        let mut short_len = valid.clone();
        short_len[5..9].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &short_len)),
            MetaPageClass::Corrupt("bytes after STNM record")
        );

        let mut long_len = valid.clone();
        long_len[5..9].copy_from_slice(&10_000u32.to_le_bytes());
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &long_len)),
            MetaPageClass::Corrupt("truncated STNM record")
        );

        let mut extra_after_record = valid.clone();
        extra_after_record.push(0xFF);
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &extra_after_record)),
            MetaPageClass::Corrupt("bytes after STNM record")
        );

        let mut extra_after_flag = valid_flag0_body();
        extra_after_flag.extend_from_slice(&[0; STNM_STAMP_BYTES]);
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&extra_after_flag))),
            MetaPageClass::Corrupt("bytes after STNM analysis flag")
        );

        let mut zero_fp = valid_flag0_body();
        *zero_fp.last_mut().unwrap() = 1;
        zero_fp.extend_from_slice(&1u32.to_le_bytes());
        zero_fp.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&zero_fp))),
            MetaPageClass::Corrupt("STNM dict_fingerprint is 0")
        );

        for count in [0u8, 17] {
            let body = vec![count, 0];
            assert_eq!(
                classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&body))),
                MetaPageClass::Corrupt("invalid STNM field_count")
            );
        }

        let mut name_len_zero = valid_flag0_body();
        name_len_zero[1..3].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&name_len_zero))),
            MetaPageClass::Corrupt("invalid STNM name length")
        );

        let mut name_len_64 = vec![1u8];
        name_len_64.extend_from_slice(&64u16.to_le_bytes());
        name_len_64.extend_from_slice(&[b'a'; 64]);
        name_len_64.extend_from_slice(&1.0f32.to_le_bytes());
        name_len_64.push(0);
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&name_len_64))),
            MetaPageClass::Corrupt("invalid STNM name length")
        );

        let mut nul_name = vec![1u8];
        nul_name.extend_from_slice(&4u16.to_le_bytes());
        nul_name.extend_from_slice(b"ex\0r");
        nul_name.extend_from_slice(&1.0f32.to_le_bytes());
        nul_name.push(0);
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&nul_name))),
            MetaPageClass::Corrupt("STNM name contains interior NUL")
        );

        let mut bad_utf8 = vec![1u8];
        bad_utf8.extend_from_slice(&2u16.to_le_bytes());
        bad_utf8.extend_from_slice(&[0xff, 0xfe]);
        bad_utf8.extend_from_slice(&1.0f32.to_le_bytes());
        bad_utf8.push(0);
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&bad_utf8))),
            MetaPageClass::Corrupt("STNM name is not UTF-8")
        );

        let mut dup = vec![2u8];
        for _ in 0..2 {
            dup.extend_from_slice(&1u16.to_le_bytes());
            dup.push(b'a');
            dup.extend_from_slice(&1.0f32.to_le_bytes());
        }
        dup.push(0);
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&dup))),
            MetaPageClass::Corrupt("duplicate STNM field name")
        );

        for weight in [0.0f32, -1.0, f32::NAN, f32::INFINITY] {
            let mut body = vec![1u8];
            body.extend_from_slice(&4u16.to_le_bytes());
            body.extend_from_slice(b"expr");
            body.extend_from_slice(&weight.to_le_bytes());
            body.push(0);
            assert_eq!(
                classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&body))),
                MetaPageClass::Corrupt("invalid STNM weight"),
                "weight={weight}"
            );
        }

        let mut bad_flag = valid_flag0_body();
        *bad_flag.last_mut().unwrap() = 2;
        assert_eq!(
            classify_meta_page(&envelope_with_stnm(&image, &frame_stnm(&bad_flag))),
            MetaPageClass::Corrupt("invalid STNM analysis flag")
        );
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
        write(&mut page, KIND_ENVELOPE, b"meta").unwrap();
        assert_eq!(flags(&page), 0);
        set_flags(&mut page, FLAG_REMOVAL_HORIZONS);
        assert_eq!(flags(&page), FLAG_REMOVAL_HORIZONS);
        assert_eq!(kind(&page).unwrap(), KIND_ENVELOPE);
        assert_eq!(super::payload(&page), b"meta");
        write(&mut page, KIND_ENVELOPE, b"meta").unwrap();
        assert_eq!(flags(&page), 0);
        write(&mut page, KIND_META, b"meta").unwrap();
        assert_eq!(kind(&page).unwrap(), KIND_META);
    }
}
