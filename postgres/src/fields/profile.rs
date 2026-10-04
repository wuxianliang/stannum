// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Optional per-query fielded-path counters. Off unless the backend sees
//! `STANNUM_FIELDED_PROFILE` as a filesystem path; then one JSON line is
//! appended per `fielded_eval`. Does not change answers, the BM25F formula,
//! or on-disk layout. Removable: delete this module and its call sites.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Default, Clone, Serialize)]
pub(crate) struct Counters {
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
    pub field_hit_allocs: u64,
    pub total_ns: u64,
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
    static COUNTERS: RefCell<Counters> = const { RefCell::new(Counters {
        query: String::new(),
        want_scores: false,
        cursor_open_ns: 0,
        cursor_open_count: 0,
        advance_ns: 0,
        advance_count: 0,
        rewind_seek_ns: 0,
        peek_successor_ns: 0,
        peek_successor_count: 0,
        field_hits_ns: 0,
        field_hits_count: 0,
        payload_get_ns: 0,
        payload_get_count: 0,
        payload_positions: 0,
        channel_open_ns: 0,
        channel_open_count: 0,
        lookup_ns: 0,
        lookup_count: 0,
        fused_score_ns: 0,
        scoring_passes: 0,
        eval_fielded_ns: 0,
        ranked_ns: 0,
        candidates: 0,
        hashset_inserts: 0,
        field_hit_allocs: 0,
        total_ns: 0,
    }) };
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

#[inline]
fn add(ns: u64, write: impl FnOnce(&mut Counters, u64)) {
    if ns == 0 && !enabled() {
        return;
    }
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| write(&mut cell.borrow_mut(), ns));
}

pub(crate) fn reset(query: &str, want_scores: bool) {
    if !enabled() {
        return;
    }
    COUNTERS.with(|cell| {
        *cell.borrow_mut() = Counters {
            query: query.to_owned(),
            want_scores,
            ..Counters {
                query: String::new(),
                want_scores: false,
                cursor_open_ns: 0,
                cursor_open_count: 0,
                advance_ns: 0,
                advance_count: 0,
                rewind_seek_ns: 0,
                peek_successor_ns: 0,
                peek_successor_count: 0,
                field_hits_ns: 0,
                field_hits_count: 0,
                payload_get_ns: 0,
                payload_get_count: 0,
                payload_positions: 0,
                channel_open_ns: 0,
                channel_open_count: 0,
                lookup_ns: 0,
                lookup_count: 0,
                fused_score_ns: 0,
                scoring_passes: 0,
                eval_fielded_ns: 0,
                ranked_ns: 0,
                candidates: 0,
                hashset_inserts: 0,
                field_hit_allocs: 0,
                total_ns: 0,
            }
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

pub(crate) fn set_total(d: Duration) {
    add(nanos(d), |c, ns| {
        c.total_ns = ns;
    });
}

pub(crate) fn flush() {
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
