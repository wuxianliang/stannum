// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! One read interface over immutable segments and the mutable index.
//!
//! The planner and scorer only need to resolve terms, expand term windows,
//! walk the document table and look up lengths. [`Index`] expresses exactly
//! that, so an immutable [`Reader`] and an in-memory [`MutableIndex`] that
//! grows by one record at a time are interchangeable at query time.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use rustc_hash::FxHashMap;

use crate::dictionary::{Extent, TermEntry};
use crate::docs::{self, DocCursor, DocTable, PageTable};
use crate::forward::{ForwardRecord, ForwardTerm};
use crate::payload::PayloadBuilder;
use crate::segment::{AreaFetch, Lengths, Reader, Term};
use crate::source::Source;
use crate::tf_bucket::TfBucket;
use crate::{Error, Result, Tid};

/// A lexicographic window of the term space.
#[derive(Clone, Copy, Debug)]
pub enum Window<'q> {
    Prefix(&'q str),
    /// Inclusive bounds; `None` is open.
    Range(Option<&'q str>, Option<&'q str>),
    All,
}

/// Terms found in a window, or a signal that more than `limit` matched.
pub enum Expanded<'a> {
    Terms(Vec<(String, Term<'a>)>),
    Overflow,
}

pub trait Index {
    fn document_count(&self) -> u32;
    fn total_length(&self) -> u64;
    fn term(&self, term: &str) -> Result<Option<Term<'_>>>;
    /// Terms in `window` accepted by `filter`, in order, up to `limit`.
    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>>;
    /// Every document in the index, in TID order.
    fn documents(&self) -> Result<DocCursor<'_>>;
    /// The document table: ordinals to locations and back.
    fn doc_table(&self) -> Result<DocTable<'_>>;
    /// The heap blocks the documents span.
    fn page_table(&self) -> Result<PageTable<'_>>;
    fn lengths(&self) -> Lengths<'_>;
    /// The document's length class (see [`crate::length_class`]), a lower
    /// bound on its length for bounding scores without reading the length.
    fn length_class(&self, ordinal: u32) -> Result<u8>;
    /// Opens or closes a span within which length and class lookups may
    /// keep the pages they read pinned (see [`Source::hold`]).
    fn hold(&self, open: bool) {
        let _ = open;
    }
}

impl<S: Source> Index for Reader<S> {
    fn document_count(&self) -> u32 {
        Reader::document_count(self)
    }

    fn page_table(&self) -> Result<PageTable<'_>> {
        Reader::page_table(self)
    }

    fn doc_table(&self) -> Result<DocTable<'_>> {
        Reader::doc_table(self)
    }

    fn length_class(&self, ordinal: u32) -> Result<u8> {
        Reader::length_class(self, ordinal)
    }

    fn hold(&self, open: bool) {
        Reader::hold(self, open);
    }

    fn total_length(&self) -> u64 {
        Reader::total_length(self)
    }

    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        Reader::term(self, term)
    }

    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        let dictionary = self.dictionary()?;
        let items: Box<dyn Iterator<Item = Result<(String, TermEntry)>>> = match window {
            Window::Prefix(prefix) => Box::new(dictionary.prefix(prefix)),
            Window::Range(lower, upper) => Box::new(dictionary.range(lower, upper)),
            Window::All => Box::new(dictionary.iter()),
        };
        let mut found = Vec::new();
        for (scanned, item) in items.enumerate() {
            if (scanned + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                crate::check_interrupts("expand:scan");
            }
            let (term, entry) = item?;
            if !filter(&term) {
                continue;
            }
            if found.len() >= limit {
                return Ok(Expanded::Overflow);
            }
            found.push((term, self.resolve(entry)?));
        }
        Ok(Expanded::Terms(found))
    }

    fn documents(&self) -> Result<DocCursor<'_>> {
        Reader::documents(self)
    }

    fn lengths(&self) -> Lengths<'_> {
        Reader::lengths(self)
    }
}

impl<I: Index + ?Sized> Index for &I {
    fn document_count(&self) -> u32 {
        (**self).document_count()
    }
    fn total_length(&self) -> u64 {
        (**self).total_length()
    }
    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        (**self).term(term)
    }
    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        (**self).expand(window, filter, limit)
    }
    fn documents(&self) -> Result<DocCursor<'_>> {
        (**self).documents()
    }
    fn doc_table(&self) -> Result<DocTable<'_>> {
        (**self).doc_table()
    }
    fn page_table(&self) -> Result<PageTable<'_>> {
        (**self).page_table()
    }
    fn lengths(&self) -> Lengths<'_> {
        (**self).lengths()
    }
    fn length_class(&self, ordinal: u32) -> Result<u8> {
        (**self).length_class(ordinal)
    }

    fn hold(&self, open: bool) {
        (**self).hold(open);
    }
}

impl<I: Index + ?Sized> Index for Box<I> {
    fn document_count(&self) -> u32 {
        (**self).document_count()
    }
    fn total_length(&self) -> u64 {
        (**self).total_length()
    }
    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        (**self).term(term)
    }
    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        (**self).expand(window, filter, limit)
    }
    fn documents(&self) -> Result<DocCursor<'_>> {
        (**self).documents()
    }
    fn doc_table(&self) -> Result<DocTable<'_>> {
        (**self).doc_table()
    }
    fn page_table(&self) -> Result<PageTable<'_>> {
        (**self).page_table()
    }
    fn lengths(&self) -> Lengths<'_> {
        (**self).lengths()
    }
    fn length_class(&self, ordinal: u32) -> Result<u8> {
        (**self).length_class(ordinal)
    }

    fn hold(&self, open: bool) {
        (**self).hold(open);
    }
}

impl<I: Index + ?Sized> Index for std::rc::Rc<I> {
    fn document_count(&self) -> u32 {
        (**self).document_count()
    }
    fn total_length(&self) -> u64 {
        (**self).total_length()
    }
    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        (**self).term(term)
    }
    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        (**self).expand(window, filter, limit)
    }
    fn documents(&self) -> Result<DocCursor<'_>> {
        (**self).documents()
    }
    fn doc_table(&self) -> Result<DocTable<'_>> {
        (**self).doc_table()
    }
    fn page_table(&self) -> Result<PageTable<'_>> {
        (**self).page_table()
    }
    fn lengths(&self) -> Lengths<'_> {
        (**self).lengths()
    }
    fn length_class(&self, ordinal: u32) -> Result<u8> {
        (**self).length_class(ordinal)
    }

    fn hold(&self, open: bool) {
        (**self).hold(open);
    }
}

/// One document's occurrence of a term: the score inputs and where its
/// positions sit in the term's flat position store.
#[derive(Clone, Copy)]
struct Occurrence {
    bucket: u8,
    doc_len: u32,
    start: u32,
    len: u32,
}

/// A term's postings under construction. Positions of every occurrence share
/// one vector in arrival order, so adding a record costs one append per term
/// rather than one allocation per term; `occurrences` follows `tids` in TID
/// order and points back into it.
struct TermData {
    /// Strictly increasing.
    tids: Vec<Tid>,
    /// Aligned with `tids`.
    occurrences: Vec<Occurrence>,
    positions: Vec<u32>,
}

impl TermData {
    fn positions_of(&self, occurrence: &Occurrence) -> &[u32] {
        let start = occurrence.start as usize;
        &self.positions[start..start + occurrence.len as usize]
    }
}

/// One field's postings under construction for a multi-column (STN4) term:
/// the same shape as [`TermData`], with `doc_len` per occurrence being that
/// field's raw token count (design §1.3).
struct FieldChannel {
    /// Strictly increasing.
    tids: Vec<Tid>,
    /// Aligned with `tids`.
    occurrences: Vec<Occurrence>,
    positions: Vec<u32>,
}

/// A surface token's per-field channels. `channels[f]` is `None` while the
/// token has no posting in field `f`.
struct FieldedTermData {
    channels: Vec<Option<FieldChannel>>,
}
/// Encoded streams handed to cursors. Append-only: a slot is never freed or
/// moved while the index lives, so borrowed views survive later appends.
#[derive(Default)]
struct Encoded {
    slots: Vec<Box<[u8]>>,
    /// Slots and entry per term, dropped from the map when the term changes
    /// or when a document is inserted before others, which renumbers them.
    terms: FxHashMap<String, TermEntry>,
    /// The documents in TID order, as the document table numbers them.
    doc_list: Option<Rc<Vec<Tid>>>,
    /// Slots of the offsets, page table and lengths, in that order.
    documents: Option<(usize, usize)>,
    lengths: Option<usize>,
}

/// An extent naming slot `slot` whole; a stream offset within it goes in
/// the low bits, so a term's stream is fetched by ranges like a segment's.
fn slot_extent(slot: usize, len: usize) -> Extent {
    Extent {
        offset: (slot as u64) << 32,
        len: len as u32,
    }
}

impl Encoded {
    fn push(&mut self, bytes: Vec<u8>) -> Extent {
        self.slots.push(bytes.into_boxed_slice());
        let slot = self.slots.len() - 1;
        slot_extent(slot, self.slots[slot].len())
    }

    /// The lifetime is the caller's: slots are boxed, never removed and never
    /// reallocated in place, so a slice stays valid for as long as the owning
    /// index lives, which is the only lifetime callers ask for.
    fn slot<'s>(&self, slot: usize) -> Result<&'s [u8]> {
        let bytes = self
            .slots
            .get(slot)
            .ok_or(Error::Corrupt("mutable index slot"))?;
        // SAFETY: see above; the box's heap allocation outlives every borrow.
        Ok(unsafe { &*std::ptr::from_ref::<[u8]>(bytes) })
    }

    /// `len` bytes at `at`, an offset in the slot-and-offset form.
    fn range<'s>(&self, at: u64, len: usize) -> Result<&'s [u8]> {
        let bytes = self.slot((at >> 32) as usize)?;
        let within = (at & 0xffff_ffff) as usize;
        bytes
            .get(within..within.checked_add(len).ok_or(Error::Truncated)?)
            .ok_or(Error::Truncated)
    }
}

/// An in-memory inverted index that grows one record at a time. Adding a
/// record costs work proportional to that record; encoded streams are built
/// lazily per term on first use and rebuilt only after that term changes.
pub struct MutableIndex {
    documents: RefCell<BTreeMap<Tid, u32>>,
    total_length: RefCell<u64>,
    terms: RefCell<FxHashMap<String, TermData>>,
    /// Term names in order, built on the first expansion and dropped when a
    /// new term appears.
    sorted: RefCell<Option<Vec<String>>>,
    encoded: RefCell<Encoded>,
    field_count: u8,
    /// Whether this instance ingests STN4 fielded records rather than stock
    /// (or legacy fielded-key) records. A property of the stream, set by the
    /// first `begin_fielded_document`.
    fielded: Cell<bool>,
    fielded_documents: RefCell<BTreeMap<Tid, Vec<u32>>>,
    fielded_terms: RefCell<FxHashMap<String, FieldedTermData>>,
    pending_fielded: Cell<Option<Tid>>,
}

impl Default for MutableIndex {
    fn default() -> Self {
        Self {
            documents: RefCell::new(BTreeMap::new()),
            total_length: RefCell::new(0),
            terms: RefCell::new(FxHashMap::default()),
            sorted: RefCell::new(None),
            encoded: RefCell::new(Encoded::default()),
            field_count: 1,
            fielded: Cell::new(false),
            fielded_documents: RefCell::new(BTreeMap::new()),
            fielded_terms: RefCell::new(FxHashMap::default()),
            pending_fielded: Cell::new(None),
        }
    }
}

impl MutableIndex {
    /// `1` (default) holds no sidecar; `2..=16` keep the same norms/`df_agg`
    /// tables a flush would write.
    pub fn with_field_count(field_count: u8) -> Result<Self> {
        crate::trailer::check_writer_field_count(field_count)?;
        Ok(Self {
            field_count,
            ..Self::default()
        })
    }

    pub fn field_count(&self) -> u8 {
        self.field_count
    }

    /// Whether this instance ingests STN4 fielded records. An untagged
    /// legacy buffer keeps the stock record grammar, so the mode is a
    /// property of the stream, not of `field_count`.
    pub fn is_fielded(&self) -> bool {
        self.fielded.get()
    }

    fn enter_fielded_mode(&self) -> Result<()> {
        if self.field_count < crate::trailer::MIN_FIELD_COUNT {
            return Err(Error::Corrupt("fielded record on a single-column index"));
        }
        if !self.terms.borrow().is_empty() || !self.documents.borrow().is_empty() {
            return Err(Error::Corrupt("mixed fielded and stock records"));
        }
        self.fielded.set(true);
        Ok(())
    }

    /// Begins one multi-column document for STN4 ingestion (see
    /// [`crate::segment::SegmentBuilder::begin_fielded_document`]).
    pub fn begin_fielded_document(&self, tid: Tid) -> Result<()> {
        self.enter_fielded_mode()?;
        Tid::new(tid.block, tid.offset)?;
        let documents = self.fielded_documents.borrow();
        if documents.contains_key(&tid)
            || self
                .pending_fielded
                .get()
                .is_some_and(|pending| pending == tid)
        {
            return Err(Error::Unordered);
        }
        self.pending_fielded.set(Some(tid));
        Ok(())
    }

    /// One posting of a multi-column document (design §1.3): the field it
    /// belongs to, that field's raw token count, and the posting's positions
    /// within that field.
    pub fn add_occurrence(
        &self,
        token: &str,
        field: u8,
        positions: &[u32],
        field_length: u32,
    ) -> Result<()> {
        if !self.fielded.get() {
            return Err(Error::Corrupt(
                "fielded occurrence without a fielded document",
            ));
        }
        let tid = self
            .pending_fielded
            .get()
            .ok_or(Error::Corrupt("fielded occurrence without a document"))?;
        if token.is_empty() {
            return Err(Error::EmptyTerm);
        }
        if field >= self.field_count {
            return Err(Error::Corrupt("fielded occurrence field"));
        }
        if positions.is_empty() {
            return Err(Error::InvalidPositions);
        }
        crate::payload::validate_positions(positions)?;
        if field_length < *positions.last().expect("nonempty") {
            return Err(Error::InvalidPositions);
        }
        // Length agreement is validated before any mutation; the posting is
        // installed first and the document's field length committed after, so
        // a rejected occurrence leaves the index exactly as it was.
        if let Some(lengths) = self.fielded_documents.borrow().get(&tid)
            && let Some(&cell) = lengths.get(usize::from(field))
            && cell != 0
            && cell != field_length
        {
            return Err(Error::Corrupt("fielded length disagreement"));
        }
        let appended = {
            let documents = self.fielded_documents.borrow();
            documents.keys().next_back().is_none_or(|last| *last < tid)
        };
        let mut encoded = self.encoded.borrow_mut();
        encoded.documents = None;
        encoded.lengths = None;
        encoded.doc_list = None;
        if !appended {
            // Reused heap space before existing documents renumbers every
            // ordinal after it: every encoded stream is stale.
            encoded.terms.clear();
        }
        let bucket = crate::tf_bucket::TfBucket::from_count(positions.len() as u32).value();
        let mut terms = self.fielded_terms.borrow_mut();
        let mut sorted = self.sorted.borrow_mut();
        let vacant = !terms.contains_key(token);
        let data = terms
            .entry(token.to_owned())
            .or_insert_with(|| FieldedTermData {
                channels: (0..usize::from(self.field_count)).map(|_| None).collect(),
            });
        if vacant {
            *sorted = None;
        }
        if !encoded.terms.is_empty() {
            encoded.terms.remove(token);
        }
        let channel = data
            .channels
            .get_mut(usize::from(field))
            .ok_or(Error::Corrupt("fielded occurrence field"))?
            .get_or_insert_with(|| FieldChannel {
                tids: Vec::new(),
                occurrences: Vec::new(),
                positions: Vec::new(),
            });
        let at = match channel.tids.last() {
            Some(last) if *last < tid => channel.tids.len(),
            _ => channel.tids.partition_point(|existing| *existing < tid),
        };
        let start = u32::try_from(channel.positions.len())
            .ok()
            .filter(|start| start.checked_add(positions.len() as u32).is_some())
            .ok_or(Error::Corrupt("mutable index positions"))?;
        channel.positions.extend_from_slice(positions);
        channel.tids.insert(at, tid);
        channel.occurrences.insert(
            at,
            Occurrence {
                bucket,
                doc_len: field_length,
                start,
                len: positions.len() as u32,
            },
        );
        let mut documents = self.fielded_documents.borrow_mut();
        let lengths = documents
            .entry(tid)
            .or_insert_with(|| vec![0u32; usize::from(self.field_count)]);
        let cell = lengths
            .get_mut(usize::from(field))
            .ok_or(Error::Corrupt("fielded occurrence field"))?;
        if *cell == 0 {
            *cell = field_length;
            *self.total_length.borrow_mut() += u64::from(field_length);
        }
        Ok(())
    }
    #[allow(clippy::type_complexity)]
    pub fn field_norms(&self) -> Result<Option<(u8, Vec<u64>, Vec<u32>)>> {
        match self.sidecar_tables()? {
            Some(tables) => Ok(Some(tables.norms()?)),
            None => Ok(None),
        }
    }

    fn sidecar_tables(&self) -> Result<Option<crate::trailer::Tables>> {
        if self.field_count < crate::trailer::MIN_FIELD_COUNT {
            return Ok(None);
        }
        if self.fielded.get() {
            let documents: Vec<Tid> = self.fielded_documents.borrow().keys().copied().collect();
            let mut tables = crate::trailer::Tables::new(self.field_count, documents.len() as u32)?;
            let terms = self.fielded_terms.borrow();
            for (term, data) in terms.iter() {
                for (field, channel) in data.channels.iter().enumerate() {
                    let Some(channel) = channel else { continue };
                    for (tid, occurrence) in channel.tids.iter().zip(&channel.occurrences) {
                        let ordinal = documents
                            .binary_search(tid)
                            .expect("every occurrence belongs to a recorded document")
                            as u32;
                        tables.add(
                            field as u8,
                            term,
                            ordinal,
                            self.positions_of(channel, occurrence).len() as u32,
                        )?;
                    }
                }
            }
            return Ok(Some(tables));
        }
        let documents: Vec<Tid> = self.documents.borrow().keys().copied().collect();
        let mut tables = crate::trailer::Tables::new(self.field_count, documents.len() as u32)?;
        let terms = self.terms.borrow();
        for (term, data) in terms.iter() {
            let Ok(Some((field, token))) = crate::trailer::inspect_stored_term(term) else {
                continue;
            };
            for (tid, occurrence) in data.tids.iter().zip(&data.occurrences) {
                let ordinal = documents
                    .binary_search(tid)
                    .expect("every occurrence belongs to a recorded document")
                    as u32;
                tables.add(field, &token, ordinal, occurrence.len)?;
            }
        }
        Ok(Some(tables))
    }

    /// CRC-32/ISO-HDLC over the in-memory row bytes, before any immutable
    /// section exists.
    pub fn check_sidecar(&self) -> Result<()> {
        if let Some(tables) = self.sidecar_tables()? {
            tables.crc_check()?;
        }
        Ok(())
    }

    /// Emits an immutable blob. Checks the sidecar CRC, then writes the
    /// trailer through [`crate::segment::SegmentBuilder::finish`].
    pub fn flush(&self) -> Result<Vec<u8>> {
        self.check_sidecar()?;
        let mut builder = crate::segment::SegmentBuilder::default();
        builder.set_field_count(self.field_count)?;
        if self.fielded.get() {
            let documents = self.fielded_documents.borrow();
            let terms = self.fielded_terms.borrow();
            for (tid, lengths) in documents.iter() {
                builder.begin_fielded_document(*tid)?;
                for (term, data) in terms.iter() {
                    for (field, channel) in data.channels.iter().enumerate() {
                        let Some(channel) = channel else { continue };
                        if let Ok(i) = channel.tids.binary_search(tid) {
                            builder.add_occurrence(
                                term,
                                field as u8,
                                self.positions_of(channel, &channel.occurrences[i]),
                                lengths
                                    .get(field)
                                    .copied()
                                    .unwrap_or(channel.occurrences[i].doc_len),
                            )?;
                        }
                    }
                }
            }
            return Ok(builder.finish());
        }
        let documents = self.documents.borrow();
        let terms = self.terms.borrow();
        for (tid, doc_len) in documents.iter() {
            let mut rec_terms = Vec::new();
            for (name, data) in terms.iter() {
                if let Ok(i) = data.tids.binary_search(tid) {
                    rec_terms.push(ForwardTerm {
                        term: name.clone(),
                        positions: data.positions_of(&data.occurrences[i]).to_vec(),
                    });
                }
            }
            rec_terms.sort_by(|a, b| a.term.cmp(&b.term));
            builder.add_record(&ForwardRecord {
                tid: *tid,
                doc_len: *doc_len,
                terms: rec_terms,
            })?;
        }
        Ok(builder.finish())
    }

    /// Adds a document. A token-less record is not recorded, matching the
    /// segment builder. A TID already present is rejected.
    pub fn add_record(&self, record: ForwardRecord) -> Result<()> {
        let mut bytes = Vec::new();
        record.encode(&mut bytes)?;
        self.add_encoded(&bytes).map(drop)
    }

    /// Adds the encoded record at the start of `bytes` straight from its
    /// bytes, returning the bytes consumed. Only a new term allocates its
    /// name; positions are copied once.
    pub fn add_encoded(&self, bytes: &[u8]) -> Result<usize> {
        let header = ForwardRecord::peek(bytes)?;
        if header.doc_len == 0 {
            return ForwardRecord::encoded_len(bytes);
        }
        let mut documents = self.documents.borrow_mut();
        if documents.contains_key(&header.tid) {
            return Err(Error::Unordered);
        }
        let appended = documents
            .keys()
            .next_back()
            .is_none_or(|last| *last < header.tid);
        documents.insert(header.tid, header.doc_len);
        *self.total_length.borrow_mut() += u64::from(header.doc_len);
        let mut terms = self.terms.borrow_mut();
        let mut sorted = self.sorted.borrow_mut();
        let mut encoded = self.encoded.borrow_mut();
        encoded.documents = None;
        encoded.lengths = None;
        encoded.doc_list = None;
        if !appended {
            // Reused heap space before existing documents renumbers every
            // ordinal after it: every encoded stream is stale.
            encoded.terms.clear();
        }
        let tid = header.tid;
        let doc_len = header.doc_len;
        let (_, consumed) = ForwardRecord::decode_with(bytes, |term, positions| {
            crate::payload::validate_positions(positions)?;
            let bucket = TfBucket::from_count(positions.len() as u32).value();
            if !encoded.terms.is_empty() {
                encoded.terms.remove(term);
            }
            let data = match terms.get_mut(term) {
                Some(data) => data,
                None => {
                    *sorted = None;
                    terms.entry(term.to_owned()).or_insert_with(|| TermData {
                        tids: Vec::new(),
                        occurrences: Vec::new(),
                        positions: Vec::new(),
                    })
                }
            };
            // Records arrive in TID order almost always; insertion elsewhere
            // is the rare out-of-order case.
            let at = match data.tids.last() {
                Some(last) if *last < tid => data.tids.len(),
                _ => data.tids.partition_point(|existing| *existing < tid),
            };
            let start = u32::try_from(data.positions.len())
                .ok()
                .filter(|start| start.checked_add(positions.len() as u32).is_some())
                .ok_or(Error::Corrupt("mutable index positions"))?;
            data.positions.extend_from_slice(positions);
            data.tids.insert(at, tid);
            data.occurrences.insert(
                at,
                Occurrence {
                    bucket,
                    doc_len,
                    start,
                    len: positions.len() as u32,
                },
            );
            Ok(())
        })?;
        Ok(consumed)
    }

    pub fn is_empty(&self) -> bool {
        if self.fielded.get() {
            return self.fielded_documents.borrow().is_empty();
        }
        self.documents.borrow().is_empty()
    }

    /// Adds one encoded STN4 record (see [`crate::forward::FieldedRecord`])
    /// at the start of `bytes`, returning the bytes consumed. A record with
    /// no groups registers nothing, matching the stock token-less rule.
    /// The record is decoded and checked against `field_count` completely
    /// before any state is installed: a malformed tagged record never
    /// leaves a partially ingested document behind (design §6.3.1).
    pub fn add_fielded_encoded(&self, bytes: &[u8]) -> Result<usize> {
        self.enter_fielded_mode()?;
        let (record, consumed) = crate::forward::FieldedRecord::decode(bytes)?;
        for group in &record.groups {
            if group.field >= self.field_count {
                return Err(Error::Corrupt("fielded occurrence field"));
            }
        }
        if record.groups.is_empty() {
            return Ok(consumed);
        }
        self.begin_fielded_document(record.tid)?;
        for group in &record.groups {
            for term in &group.terms {
                self.add_occurrence(&term.term, group.field, &term.positions, group.field_length)?;
            }
        }
        Ok(consumed)
    }

    /// The documents in TID order, shared until the next insertion.
    fn doc_list(&self) -> Rc<Vec<Tid>> {
        if let Some(list) = &self.encoded.borrow().doc_list {
            return list.clone();
        }
        let list = if self.fielded.get() {
            self.fielded_documents
                .borrow()
                .keys()
                .copied()
                .collect::<Vec<_>>()
        } else {
            self.documents.borrow().keys().copied().collect::<Vec<_>>()
        };
        let list = Rc::new(list);
        self.encoded.borrow_mut().doc_list = Some(list.clone());
        list
    }

    fn encode_term(&self, term: &str, data: &TermData) -> TermEntry {
        let documents = self.doc_list();
        let mut payload = PayloadBuilder::default();
        let mut max_tf_bucket = 0;
        let mut ordinals = Vec::with_capacity(data.tids.len());
        let mut scores = Vec::with_capacity(data.tids.len());
        for (tid, occurrence) in data.tids.iter().zip(&data.occurrences) {
            ordinals.push(
                documents
                    .binary_search(tid)
                    .expect("every occurrence belongs to a recorded document")
                    as u32,
            );
            scores.push((occurrence.bucket, occurrence.doc_len));
            payload
                .push(data.positions_of(occurrence))
                .expect("positions validated on insertion");
            max_tf_bucket = max_tf_bucket.max(occurrence.bucket);
        }
        let mut encoded = self.encoded.borrow_mut();
        let entry = TermEntry {
            df: data.tids.len() as u32,
            max_tf_bucket,
            ordinals: encoded.push(crate::ordinals::encode_scored(&ordinals, &scores)),
            payload: encoded.push(payload.finish()),
        };
        encoded.terms.insert(term.to_owned(), entry);
        entry
    }

    fn entry(&self, term: &str) -> Option<TermEntry> {
        if let Some(entry) = self.encoded.borrow().terms.get(term) {
            return Some(*entry);
        }
        if self.fielded.get() {
            let terms = self.fielded_terms.borrow();
            let data = terms.get(term)?;
            return Some(self.encode_fielded_term(term, data));
        }
        let terms = self.terms.borrow();
        let data = terms.get(term)?;
        Some(self.encode_term(term, data))
    }

    /// Encodes one multi-column term: per-field stock streams wrapped in an
    /// FCH1 directory (design §1.2/§1.3). Parent `df` is the union across
    /// channels; `max_tf_bucket` the max of the channel maxima; a field with
    /// no postings is omitted from both directories.
    fn encode_fielded_term(&self, term: &str, data: &FieldedTermData) -> TermEntry {
        let documents = self.doc_list();
        let mut union = Vec::new();
        let mut max_tf_bucket = 0u8;
        let mut streams: Vec<crate::channels::FieldStreams> = Vec::new();
        for (field, channel) in data.channels.iter().enumerate() {
            let Some(channel) = channel else { continue };
            let mut payload = PayloadBuilder::default();
            let mut ordinals = Vec::with_capacity(channel.tids.len());
            let mut scores = Vec::with_capacity(channel.tids.len());
            for (tid, occurrence) in channel.tids.iter().zip(&channel.occurrences) {
                ordinals.push(
                    documents
                        .binary_search(tid)
                        .expect("every occurrence belongs to a recorded document")
                        as u32,
                );
                scores.push((occurrence.bucket, occurrence.doc_len));
                payload
                    .push(self.positions_of(channel, occurrence))
                    .expect("positions validated on insertion");
                max_tf_bucket = max_tf_bucket.max(occurrence.bucket);
                union.push(*tid);
            }
            streams.push(crate::channels::FieldStreams {
                field: field as u8,
                ordinals: crate::ordinals::encode_scored(&ordinals, &scores),
                payload: payload.finish(),
            });
        }
        union.sort_unstable();
        union.dedup();
        let encoded = crate::channels::encode(self.field_count, &streams)
            .expect("writer streams are well formed");
        let mut encoded_slots = self.encoded.borrow_mut();
        let entry = TermEntry {
            df: union.len() as u32,
            max_tf_bucket,
            ordinals: encoded_slots.push(encoded.ordinals),
            payload: encoded_slots.push(encoded.payload),
        };
        encoded_slots.terms.insert(term.to_owned(), entry);
        entry
    }

    fn positions_of<'a>(&self, channel: &'a FieldChannel, occurrence: &Occurrence) -> &'a [u32] {
        let start = occurrence.start as usize;
        &channel.positions[start..start + occurrence.len as usize]
    }

    /// Slots of the offsets and page table, encoded on first use.
    fn document_slots(&self) -> (usize, usize) {
        if let Some(slots) = self.encoded.borrow().documents {
            return slots;
        }
        let documents = self.doc_list();
        let offsets = docs::offsets(documents.iter().copied());
        let pages = docs::page_table(documents.iter().copied());
        let mut encoded = self.encoded.borrow_mut();
        let offsets = (encoded.push(offsets).offset >> 32) as usize;
        let pages = (encoded.push(pages).offset >> 32) as usize;
        encoded.documents = Some((offsets, pages));
        (offsets, pages)
    }

    fn lengths_slot(&self) -> usize {
        if let Some(slot) = self.encoded.borrow().lengths {
            return slot;
        }
        let mut bytes = Vec::with_capacity(self.documents.borrow().len() * 4);
        if self.fielded.get() {
            for lengths in self.fielded_documents.borrow().values() {
                let doc_len = lengths
                    .iter()
                    .copied()
                    .try_fold(0u32, |sum, len| sum.checked_add(len))
                    .unwrap_or(u32::MAX);
                bytes.extend_from_slice(&doc_len.to_le_bytes());
            }
        } else {
            for len in self.documents.borrow().values() {
                bytes.extend_from_slice(&len.to_le_bytes());
            }
        }
        let mut encoded = self.encoded.borrow_mut();
        let slot = (encoded.push(bytes).offset >> 32) as usize;
        encoded.lengths = Some(slot);
        slot
    }

    fn term_view(&self, entry: TermEntry) -> Term<'_> {
        Term::new(entry, self)
    }
}

impl AreaFetch for MutableIndex {
    fn ordinals_bytes(&self, offset: u64, len: usize) -> Result<&[u8]> {
        self.encoded.borrow().range(offset, len)
    }

    fn payload_bytes(&self, extent: Extent) -> Result<&[u8]> {
        self.encoded
            .borrow()
            .range(extent.offset, extent.len as usize)
    }

    fn doc_table(&self) -> Result<DocTable<'_>> {
        let (offsets, pages) = self.document_slots();
        let encoded = self.encoded.borrow();
        let offsets: &[u8] = encoded.slot(offsets)?;
        let pages: &[u8] = encoded.slot(pages)?;
        DocTable::parse(pages, offsets, (offsets.len() / 2) as u32)
    }

    fn length(&self, ordinal: u32) -> Result<u32> {
        let slot = self.lengths_slot();
        let bytes = self.encoded.borrow().slot(slot)?;
        let at = ordinal as usize * 4;
        let bytes = bytes
            .get(at..at + 4)
            .ok_or(Error::Corrupt("document ordinal out of range"))?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn length_class(&self, ordinal: u32) -> Result<u8> {
        AreaFetch::length(self, ordinal).map(crate::length_class::class_of)
    }
}

impl Index for MutableIndex {
    fn document_count(&self) -> u32 {
        if self.fielded.get() {
            return self.fielded_documents.borrow().len() as u32;
        }
        self.documents.borrow().len() as u32
    }

    fn total_length(&self) -> u64 {
        *self.total_length.borrow()
    }

    fn term(&self, term: &str) -> Result<Option<Term<'_>>> {
        Ok(self.entry(term).map(|entry| self.term_view(entry)))
    }

    fn expand(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
        limit: usize,
    ) -> Result<Expanded<'_>> {
        let names: Vec<String> = {
            let mut sorted = self.sorted.borrow_mut();
            let sorted = sorted.get_or_insert_with(|| {
                let source = if self.fielded.get() {
                    self.fielded_terms
                        .borrow()
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                } else {
                    self.terms.borrow().keys().cloned().collect::<Vec<_>>()
                };
                let mut names = source;
                names.sort_unstable();
                names
            });
            let keys: &[String] = match window {
                Window::Prefix(prefix) => {
                    let start = sorted.partition_point(|k| k.as_str() < prefix);
                    let end = start + sorted[start..].partition_point(|k| k.starts_with(prefix));
                    &sorted[start..end]
                }
                Window::Range(lower, upper) => {
                    let start =
                        lower.map_or(0, |lower| sorted.partition_point(|k| k.as_str() < lower));
                    let end = upper.map_or(sorted.len(), |upper| {
                        sorted.partition_point(|k| k.as_str() <= upper)
                    });
                    &sorted[start..end.max(start)]
                }
                Window::All => sorted,
            };
            keys.iter()
                .enumerate()
                .filter(|(scanned, k)| {
                    if (scanned + 1).is_multiple_of(crate::INTERRUPT_INTERVAL) {
                        crate::check_interrupts("expand:scan");
                    }
                    filter(k)
                })
                .map(|(_, k)| k.clone())
                .collect()
        };
        if names.len() > limit {
            return Ok(Expanded::Overflow);
        }
        let mut found = Vec::with_capacity(names.len());
        for name in names {
            let entry = self.entry(&name).expect("term exists");
            found.push((name, self.term_view(entry)));
        }
        Ok(Expanded::Terms(found))
    }

    fn documents(&self) -> Result<DocCursor<'_>> {
        AreaFetch::doc_table(self)?.into_cursor()
    }

    fn doc_table(&self) -> Result<DocTable<'_>> {
        AreaFetch::doc_table(self)
    }

    fn page_table(&self) -> Result<PageTable<'_>> {
        let (_, pages) = self.document_slots();
        let encoded = self.encoded.borrow();
        PageTable::parse(encoded.slot(pages)?, self.document_count())
    }

    fn length_class(&self, ordinal: u32) -> Result<u8> {
        AreaFetch::length_class(self, ordinal)
    }

    fn lengths(&self) -> Lengths<'_> {
        // A document cursor retains its encoded TID order. Keep the matching
        // length order too: appending a record in reused heap space may insert
        // before those TIDs. A lazy lookup into the current map would then
        // score retained documents using another document's length.
        let slot = self.lengths_slot();
        Lengths::Bytes(
            self.encoded
                .borrow()
                .slot(slot)
                .expect("length extent was just encoded"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegmentBuilder;
    use crate::set::collect;

    fn record(id: u32, text: &str) -> ForwardRecord {
        let tokens: Vec<(&str, u32)> = text
            .split_whitespace()
            .enumerate()
            .map(|(i, w)| (w, i as u32 + 1))
            .collect();
        ForwardRecord::from_tokens(Tid::new(id / 50, (id % 50 + 1) as u16).unwrap(), tokens)
            .unwrap()
    }

    fn same<'a>(a: &'a dyn Index, b: &'a dyn Index, terms: &[&str]) {
        assert_eq!(a.document_count(), b.document_count());
        assert_eq!(a.total_length(), b.total_length());
        assert_eq!(
            collect(a.documents().unwrap()).unwrap(),
            collect(b.documents().unwrap()).unwrap()
        );
        for ordinal in 0..a.document_count() {
            assert_eq!(
                a.lengths().get(ordinal).unwrap(),
                b.lengths().get(ordinal).unwrap()
            );
        }
        for term in terms {
            let (x, y) = (a.term(term).unwrap(), b.term(term).unwrap());
            assert_eq!(x.is_some(), y.is_some(), "{term}");
            let (Some(x), Some(y)) = (x, y) else { continue };
            assert_eq!(x.entry.df, y.entry.df);
            assert_eq!(x.entry.max_tf_bucket, y.entry.max_tf_bucket);
            let tids = collect(x.cursor().unwrap()).unwrap();
            assert_eq!(tids, collect(y.cursor().unwrap()).unwrap());
            // Both carry the same chunk bounds, computed from the same documents.
            let (ox, oy) = (x.ordinals().unwrap(), y.ordinals().unwrap());
            assert_eq!(ox.to_vec().unwrap(), oy.to_vec().unwrap(), "{term}");
            assert!(!ox.bounds().unwrap().is_empty(), "{term}");
            assert_eq!(ox.bounds().unwrap(), oy.bounds().unwrap(), "{term}");
            let (px, py) = (x.payload().unwrap(), y.payload().unwrap());
            for ordinal in 0..tids.len() as u32 {
                assert_eq!(px.get(ordinal).unwrap(), py.get(ordinal).unwrap());
            }
        }
        for window in [
            Window::Prefix("b"),
            Window::Range(Some("a"), Some("c")),
            Window::All,
        ] {
            let names = |e: Expanded<'_>| match e {
                Expanded::Terms(t) => t.into_iter().map(|(n, _)| n).collect::<Vec<_>>(),
                Expanded::Overflow => vec!["<overflow>".into()],
            };
            let x = names(a.expand(window, &|_| true, 1000).unwrap());
            let y = names(b.expand(window, &|_| true, 1000).unwrap());
            assert_eq!(x, y);
        }
    }

    #[test]
    fn retained_document_ordinals_keep_their_lengths_after_buffer_growth() {
        let mutable = MutableIndex::default();
        let original = record(50, "needle filler filler filler");
        let tid = original.tid;
        mutable.add_record(original).unwrap();
        let mut documents = mutable.documents().unwrap();
        let lengths = mutable.lengths();
        // Reused heap space inserts before the retained document cursor.
        mutable.add_record(record(1, "needle")).unwrap();
        let ordinal = documents.rank(tid).unwrap().unwrap();
        assert_eq!(lengths.get(ordinal).unwrap(), 4);
        assert_eq!(mutable.lengths().get(0).unwrap(), 1);
        assert_eq!(mutable.lengths().get(1).unwrap(), 4);
    }

    #[test]
    fn out_of_order_records_keep_each_occurrence_with_its_own_positions() {
        // Positions are stored flat per term in arrival order while postings
        // stay in TID order; an occurrence inserted before earlier ones must
        // still resolve to its own positions and term frequency.
        let mutable = MutableIndex::default();
        mutable
            .add_record(record(300, "beer beer beer ale"))
            .unwrap();
        mutable.add_record(record(200, "ale beer")).unwrap();
        mutable.add_record(record(100, "beer")).unwrap();
        let beer = mutable.term("beer").unwrap().unwrap();
        assert_eq!(beer.df(), 3);
        let tids = collect(beer.cursor().unwrap()).unwrap();
        assert_eq!(
            tids,
            [
                record(100, "").tid,
                record(200, "").tid,
                record(300, "").tid
            ]
        );
        let payload = beer.payload().unwrap();
        assert_eq!(payload.get(0).unwrap().positions, [1]);
        assert_eq!(payload.get(1).unwrap().positions, [2]);
        assert_eq!(payload.get(2).unwrap().positions, [1, 2, 3]);
        let ale = mutable.term("ale").unwrap().unwrap();
        let payload = ale.payload().unwrap();
        assert_eq!(payload.get(0).unwrap().positions, [1]);
        assert_eq!(payload.get(1).unwrap().positions, [4]);
        // The same records through the segment builder agree in every detail.
        let mut builder = SegmentBuilder::default();
        for (id, text) in [
            (300, "beer beer beer ale"),
            (200, "ale beer"),
            (100, "beer"),
        ] {
            builder.add_record(&record(id, text)).unwrap();
        }
        let bytes = builder.finish();
        same(&mutable, &Reader::parse(&bytes).unwrap(), &["beer", "ale"]);
    }

    #[test]
    fn mutable_index_matches_a_segment_built_from_the_same_records() {
        let texts = [
            (7, "beer beer wine"),
            (3, "craft beer"),
            (9, ""),
            (120, "wine cellar beer"),
            (1, "aardvark bee"),
            (55, "craft craft craft cider"),
        ];
        let mutable = MutableIndex::default();
        let mut builder = SegmentBuilder::default();
        let mut expected_docs = 0;
        for (i, (id, text)) in texts.iter().enumerate() {
            let record = record(*id, text);
            builder.add_record(&record).unwrap();
            if i == 3 {
                // Use the index before it is complete; later appends must not
                // invalidate what was handed out.
                let beer = mutable.term("beer").unwrap().unwrap();
                let early = collect(beer.cursor().unwrap()).unwrap();
                mutable.add_record(record.clone()).unwrap();
                assert_eq!(early.len(), 2, "earlier view stays readable");
            } else {
                mutable.add_record(record.clone()).unwrap();
            }
            if !text.is_empty() {
                expected_docs += 1;
            }
        }
        assert_eq!(mutable.document_count(), expected_docs);
        assert_eq!(
            mutable.add_record(record(7, "duplicate")),
            Err(Error::Unordered)
        );
        let bytes = builder.finish();
        let segment = Reader::parse(&bytes).unwrap();
        same(
            &mutable,
            &segment,
            &[
                "beer", "wine", "craft", "cider", "bee", "absent", "aardvark",
            ],
        );
        let overflow = mutable.expand(Window::All, &|_| true, 2).unwrap();
        assert!(matches!(overflow, Expanded::Overflow));
        let overflow = segment.expand(Window::All, &|_| true, 2).unwrap();
        assert!(matches!(overflow, Expanded::Overflow));
    }

    fn fielded_record(id: u32, columns: &[&str]) -> ForwardRecord {
        let mut tokens = Vec::new();
        let mut position = 0u32;
        for (field, text) in columns.iter().enumerate() {
            for word in text.split_whitespace() {
                position += 1;
                tokens.push((
                    crate::trailer::test_fielded_key(field as u8, word),
                    position,
                ));
            }
        }
        let refs: Vec<(&str, u32)> = tokens.iter().map(|(t, p)| (t.as_str(), *p)).collect();
        ForwardRecord::from_tokens(Tid::new(id / 50, (id % 50 + 1) as u16).unwrap(), refs).unwrap()
    }

    /// Feeds `columns` through the STN4 posting API: per-field grouping with
    /// raw per-field positions and field lengths.
    fn add_fielded(index: &MutableIndex, id: u32, columns: &[&str]) -> crate::Result<()> {
        index.begin_fielded_document(Tid::new(id / 50, (id % 50 + 1) as u16).unwrap())?;
        for (field, text) in columns.iter().enumerate() {
            let mut by_term: std::collections::BTreeMap<&str, Vec<u32>> =
                std::collections::BTreeMap::new();
            let mut len = 0u32;
            for (i, word) in text.split_whitespace().enumerate() {
                len += 1;
                by_term.entry(word).or_default().push(i as u32 + 1);
            }
            if len == 0 {
                continue;
            }
            for (word, positions) in by_term {
                index.add_occurrence(word, field as u8, &positions, len)?;
            }
        }
        crate::Result::Ok(())
    }

    #[test]
    fn fielded_mutable_index_round_trips_through_the_buffer_grammar() {
        // insert → buffer → fold: the STN4 record grammar decodes back into
        // the same mutable state, and flush matches a segment built by hand.
        let tid = Tid::new(0, 1).unwrap();
        let mut record = crate::forward::FieldedRecord {
            tid,
            groups: Vec::new(),
        };
        for (field, text, len) in [(0u8, "beer wine", 2u32), (1, "beer", 1)] {
            let mut terms = Vec::new();
            for (i, word) in text.split_whitespace().enumerate() {
                if word == "beer" {
                    terms.push(crate::forward::FieldedTerm {
                        term: word.into(),
                        positions: vec![i as u32 + 1],
                    });
                }
            }
            if field == 0 {
                terms.push(crate::forward::FieldedTerm {
                    term: "wine".into(),
                    positions: vec![2],
                });
            }
            record.groups.push(crate::forward::FieldedGroup {
                field,
                field_length: len,
                terms,
            });
        }
        let mut stream = Vec::new();
        stream.extend_from_slice(&crate::forward::STN4_BUFFER_TAG);
        record.encode(&mut stream).unwrap();
        let mutable = MutableIndex::with_field_count(2).unwrap();
        let mut at = 2;
        while at < stream.len() {
            at += mutable.add_fielded_encoded(&stream[at..]).unwrap();
        }
        assert_eq!(mutable.document_count(), 1);
        let beer = mutable.term("beer").unwrap().unwrap();
        assert_eq!(beer.df(), 1, "union df across the two channels");
        let channels = beer.channels(2).unwrap();
        assert_eq!(channels.len(), 2);
        mutable.check_sidecar().unwrap();
        let bytes = mutable.flush().unwrap();
        let segment = Reader::parse(&bytes).unwrap();
        let trailer = segment.trailer().unwrap();
        assert_eq!(trailer.version, 2);
        assert_eq!(trailer.row(0, 0), Some(2));
        assert_eq!(trailer.row(0, 1), Some(1));
        let segment_beer = segment.term("beer").unwrap().unwrap();
        assert_eq!(segment_beer.df(), 1);
        assert!(crate::verify::verify_segment(&bytes).is_clean());
        // The same postings through the direct API produce the same blob.
        let mut builder = SegmentBuilder::default();
        builder.set_field_count(2).unwrap();
        builder.begin_fielded_document(tid).unwrap();
        builder.add_occurrence("beer", 0, &[1], 2).unwrap();
        builder.add_occurrence("wine", 0, &[2], 2).unwrap();
        builder.add_occurrence("beer", 1, &[1], 1).unwrap();
        assert_eq!(bytes, builder.finish());
    }

    #[test]
    fn fielded_mutable_index_df_is_the_union_not_the_sum() {
        let mutable = MutableIndex::with_field_count(2).unwrap();
        add_fielded(&mutable, 1, &["needle", "needle"]).unwrap();
        add_fielded(&mutable, 2, &["needle", "pad"]).unwrap();
        add_fielded(&mutable, 3, &["needle", "needle needle"]).unwrap();
        let needle = mutable.term("needle").unwrap().unwrap();
        assert_eq!(needle.df(), 3, "union df == 3, not 4");
        let channels = needle.channels(2).unwrap();
        let per_field: Vec<(u8, u32)> = channels
            .iter()
            .map(|(field, child)| (*field, child.df()))
            .collect();
        assert_eq!(per_field, vec![(0, 3), (1, 2)]);
        assert_eq!(
            per_field.iter().map(|(_, df)| df).sum::<u32>(),
            5,
            "the sum the entry df must not be"
        );
        assert_eq!(mutable.total_length(), 2 + 2 + 3);
        let bytes = mutable.flush().unwrap();
        let segment = Reader::parse(&bytes).unwrap();
        assert_eq!(segment.term("needle").unwrap().unwrap().df(), 3);
        assert!(crate::verify::verify_segment(&bytes).is_clean());
    }

    #[test]
    fn mutable_index_crc_checks_sidecar_before_flush() {
        let mutable = MutableIndex::with_field_count(2).unwrap();
        mutable
            .add_record(fielded_record(1, &["beer wine", "beer"]))
            .unwrap();
        mutable.check_sidecar().unwrap();
        // Legacy fielded-key content must not reach a flush: this version
        // writes one TermEntry per surface token, never a `~{h}~` key.
        assert_eq!(
            mutable.flush().unwrap_err(),
            Error::Corrupt("fielded build expects add_occurrence")
        );
    }
}
