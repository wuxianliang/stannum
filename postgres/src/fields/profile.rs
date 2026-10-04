// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Optional per-query fielded-path counters. Off unless the backend sees
//! `STANNUM_FIELDED_PROFILE` as a filesystem path; then one JSON line is
//! appended per ranked `search()` (multi or single arm). Does not change
//! answers, the BM25F formula, or on-disk layout. Removable: delete this
//! module and its call sites.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Default, Clone, Serialize)]
pub(crate) struct Counters {
    pub schema: &'static str,
    pub arm: String,
    pub query: String,
    pub want_scores: bool,
    pub cursor_open_ns: u64,
    pub cursor_open_count: u64,
    pub advance_ns: u64,
    pub advance_count: u64,
    pub rewind_seek_ns: u64,
    pub peek_successor_ns: u64,
    pub peek_successor_count: u64,
    pub field_hits_ns: u64,
    pub field_hits_count: u64,
    pub payload_get_ns: u64,
    pub payload_get_count: u64,
    pub payload_positions: u64,
    pub channel_open_ns: u64,
    pub channel_open_count: u64,
    pub lookup_ns: u64,
    pub lookup_count: u64,
    pub fused_score_ns: u64,
    pub scoring_passes: u64,
    pub eval_fielded_ns: u64,
    pub ranked_ns: u64,
    pub candidates: u64,
    pub hashset_inserts: u64,
    pub and_advance_count: u64,
    pub and_ordinal_span: u64,
    pub and_hits: u64,
    pub advance_ordinal_span: u64,
    pub field_hit_allocs: u64,
    pub bucket_ns: u64,
    pub bucket_count: u64,
    pub norms_ns: u64,
    pub norms_count: u64,
    pub walk_blocks: u64,
    pub blocks_skipped: u64,
    pub cpu_ns: u64,
    pub total_ns: u64,
    pub exclusive_ns: u64,
    pub unaccounted_ns: i64,
}

fn sink() -> Option<&'static PathBuf> {
    static SINK: OnceLock<Option<PathBuf>> = OnceLock::new();
    SINK.get_or_init(|| {
        let raw = std::env::var("STANNUM_FIELDED_PROFILE").ok()?;
        if raw.is_empty() || raw == "0" || raw == "off" {
            return None;
        }
        Some(PathBuf::from(raw))
    })
    .as_ref()
}

#[inline]
pub(crate) fn enabled() -> bool {
    sink().is_some()
}

thread_local! {
    static COUNTERS: RefCell<Counters> = RefCell::new(Counters {
        schema: "stn4-query-profile-v2",
        ..Counters::default()
    });
}

pub(crate) struct Span {
    start: Option<Instant>,
}

impl Span {
    #[inline]
    pub(crate) fn begin() -> Self {
        Self {
            start: enabled().then(Instant::now),
        }
    }

    #[inline]
    pub(crate) fn ns(self) -> u64 {
        self.start
            .map(|start| u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// One ranked `search()` sample. Drop flushes one JSON line.
pub(crate) struct Session {
    live: bool,
    cpu_start_ns: u64,
    wall: Option<Instant>,
}

impl Session {
    pub(crate) fn begin(query: &str, want_scores: bool, arm: &str) -> Self {
        if !enabled() {
            return Self {
                live: false,
                cpu_start_ns: 0,
                wall: None,
            };
        }
        reset(query, want_scores, arm);
        Self {
            live: true,
            cpu_start_ns: cpu_ns(),
            wall: Some(Instant::now()),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        if let Some(wall) = self.wall {
            set_total(wall.elapsed());
        }
        add_cpu(cpu_ns().saturating_sub(self.cpu_start_ns));
        reconcile();
        flush();
    }
}

#[inline]
fn add(ns: u64, write: impl FnOnce(&mut Counters, u64)) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| write(&mut cell.borrow_mut(), ns));
}

pub(crate) fn reset(query: &str, want_scores: bool, arm: &str) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        *cell.borrow_mut() = Counters {
            schema: "stn4-query-profile-v2",
            arm: arm.to_owned(),
            query: query.to_owned(),
            want_scores,
            ..Counters::default()
        };
    });
}

pub(crate) fn add_cursor_open(d: Duration) {
    add(nanos(d), |c, ns| {
        c.cursor_open_ns = c.cursor_open_ns.saturating_add(ns);
        c.cursor_open_count = c.cursor_open_count.saturating_add(1);
    });
}

pub(crate) fn add_advance(d: Duration) {
    add(nanos(d), |c, ns| {
        c.advance_ns = c.advance_ns.saturating_add(ns);
        c.advance_count = c.advance_count.saturating_add(1);
    });
}

pub(crate) fn add_rewind_seek(d: Duration) {
    add(nanos(d), |c, ns| {
        c.rewind_seek_ns = c.rewind_seek_ns.saturating_add(ns);
    });
}

pub(crate) fn add_peek_successor(d: Duration) {
    add(nanos(d), |c, ns| {
        c.peek_successor_ns = c.peek_successor_ns.saturating_add(ns);
        c.peek_successor_count = c.peek_successor_count.saturating_add(1);
    });
}

pub(crate) fn add_field_hits(d: Duration, hits: u64, positions: u64) {
    add(nanos(d), |c, ns| {
        c.field_hits_ns = c.field_hits_ns.saturating_add(ns);
        c.field_hits_count = c.field_hits_count.saturating_add(1);
        c.field_hit_allocs = c.field_hit_allocs.saturating_add(hits);
        c.payload_positions = c.payload_positions.saturating_add(positions);
    });
}

pub(crate) fn add_payload_get(d: Duration) {
    add(nanos(d), |c, ns| {
        c.payload_get_ns = c.payload_get_ns.saturating_add(ns);
        c.payload_get_count = c.payload_get_count.saturating_add(1);
    });
}

pub(crate) fn add_channel_open(d: Duration) {
    add(nanos(d), |c, ns| {
        c.channel_open_ns = c.channel_open_ns.saturating_add(ns);
        c.channel_open_count = c.channel_open_count.saturating_add(1);
    });
}

pub(crate) fn add_lookup(d: Duration) {
    add(nanos(d), |c, ns| {
        c.lookup_ns = c.lookup_ns.saturating_add(ns);
        c.lookup_count = c.lookup_count.saturating_add(1);
    });
}

pub(crate) fn add_fused_score(d: Duration) {
    add(nanos(d), |c, ns| {
        c.fused_score_ns = c.fused_score_ns.saturating_add(ns);
        c.scoring_passes = c.scoring_passes.saturating_add(1);
    });
}

pub(crate) fn add_eval_fielded(d: Duration) {
    add(nanos(d), |c, ns| {
        c.eval_fielded_ns = c.eval_fielded_ns.saturating_add(ns);
    });
}

pub(crate) fn add_ranked(d: Duration) {
    add(nanos(d), |c, ns| {
        c.ranked_ns = c.ranked_ns.saturating_add(ns);
    });
}

pub(crate) fn add_bucket(d: Duration) {
    add(nanos(d), |c, ns| {
        c.bucket_ns = c.bucket_ns.saturating_add(ns);
        c.bucket_count = c.bucket_count.saturating_add(1);
    });
}

pub(crate) fn add_norms(d: Duration) {
    add(nanos(d), |c, ns| {
        c.norms_ns = c.norms_ns.saturating_add(ns);
        c.norms_count = c.norms_count.saturating_add(1);
    });
}

pub(crate) fn add_blocks_skipped(n: u64) {
    if n == 0 || !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        let mut c = cell.borrow_mut();
        c.blocks_skipped = c.blocks_skipped.saturating_add(n);
    });
}

pub(crate) fn add_candidate() {
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        let mut c = cell.borrow_mut();
        c.candidates = c.candidates.saturating_add(1);
    });
}

pub(crate) fn add_hashset_insert() {
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        let mut c = cell.borrow_mut();
        c.hashset_inserts = c.hashset_inserts.saturating_add(1);
    });
}

pub(crate) fn add_and_advance() {
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        let mut c = cell.borrow_mut();
        c.and_advance_count = c.and_advance_count.saturating_add(1);
    });
}

pub(crate) fn add_and_span(distance: u32) {
    if distance == 0 || !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        let mut c = cell.borrow_mut();
        c.and_ordinal_span = c.and_ordinal_span.saturating_add(u64::from(distance));
    });
}

pub(crate) fn add_and_hit() {
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        let mut c = cell.borrow_mut();
        c.and_hits = c.and_hits.saturating_add(1);
    });
}

pub(crate) fn add_advance_span(distance: u32) {
    if distance == 0 || !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        let mut c = cell.borrow_mut();
        c.advance_ordinal_span = c.advance_ordinal_span.saturating_add(u64::from(distance));
    });
}

/// Single-column comparable probes: phrase position reads and WAND walk pages.
pub(crate) fn absorb_single_walk(position_reads: u64, walk_blocks: u64) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        let mut c = cell.borrow_mut();
        c.payload_positions = c.payload_positions.saturating_add(position_reads);
        c.walk_blocks = walk_blocks;
    });
}

pub(crate) fn set_total(d: Duration) {
    add(nanos(d), |c, ns| {
        c.total_ns = ns;
    });
}

fn add_cpu(ns: u64) {
    add(ns, |c, ns| {
        c.cpu_ns = ns;
    });
}

fn reconcile() {
    COUNTERS.with(|cell| {
        let mut c = cell.borrow_mut();
        // Innermost exclusive work: nested parents (advance/field_hits/cursor_open/
        // eval_fielded) are omitted so the sum is not double-counted.
        let exclusive = c
            .rewind_seek_ns
            .saturating_add(c.peek_successor_ns)
            .saturating_add(c.payload_get_ns)
            .saturating_add(c.fused_score_ns)
            .saturating_add(c.lookup_ns)
            .saturating_add(c.channel_open_ns)
            .saturating_add(c.ranked_ns)
            .saturating_add(c.bucket_ns)
            .saturating_add(c.norms_ns);
        c.exclusive_ns = exclusive;
        c.unaccounted_ns = c.total_ns as i64 - exclusive as i64;
    });
}

fn flush() {
    let Some(path) = sink() else {
        return;
    };
    let snapshot = COUNTERS.with(|cell| cell.borrow().clone());
    let Ok(line) = serde_json::to_string(&snapshot) else {
        return;
    };
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write;
            writeln!(file, "{line}")
        });
}

#[inline]
fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn cpu_ns() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let ok = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if ok != 0 {
        return 0;
    }
    let usage = unsafe { usage.assume_init() };
    timeval_ns(usage.ru_utime).saturating_add(timeval_ns(usage.ru_stime))
}

fn timeval_ns(tv: libc::timeval) -> u64 {
    (tv.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add((tv.tv_usec as u64).saturating_mul(1_000))
}
