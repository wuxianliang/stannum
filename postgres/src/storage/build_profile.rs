// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Optional CREATE INDEX stage counters. Off unless the backend sees
//! `STANNUM_BUILD_PROFILE` as a filesystem path; then one JSON line is
//! appended per `ambuild`. Exclusive stage time: a nested stage pauses its
//! parent. Disabled for acceptance measurements. Does not change answers,
//! output bytes, or the on-disk term representation.

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use segment::build_trace::{Phase, Trace};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Heap,
    Tokenize,
    Mutable,
    Fold,
    Merge,
    Encode,
    Storage,
}

#[derive(Default, Clone, Serialize)]
pub(crate) struct Counters {
    pub schema: &'static str,
    pub field_count: u8,
    pub heap_ns: u64,
    pub heap_rows: u64,
    pub heap_input_bytes: u64,
    pub heap_text_copies: u64,
    pub tokenize_ns: u64,
    pub tokenize_calls: u64,
    pub tokenizer_inits: u64,
    pub tokens_total: u64,
    pub tokens_by_field: [u64; 16],
    pub mutable_ns: u64,
    pub term_lookups: u64,
    pub doc_term_field_records: u64,
    pub alloc_bytes: u64,
    pub fold_ns: u64,
    pub fold_sort_ns: u64,
    pub flush_count: u64,
    pub fold_records: u64,
    pub merge_ns: u64,
    pub merge_count: u64,
    pub merge_input_records: u64,
    pub merge_output_records: u64,
    pub merge_input_bytes: u64,
    pub merge_output_bytes: u64,
    pub encode_ns: u64,
    pub encode_dictionary_ns: u64,
    pub encode_ordinals_ns: u64,
    pub encode_tf_ns: u64,
    pub encode_positions_ns: u64,
    pub encode_fch1_ns: u64,
    pub encode_norms_ns: u64,
    pub encode_assemble_ns: u64,
    pub storage_ns: u64,
    pub bytes_written: u64,
    pub pages_written: u64,
    pub wal_bytes: u64,
    pub flush_sync_waits: u64,
    pub wall_ns: u64,
    pub cpu_ns: u64,
    pub peak_rss_bytes: u64,
    pub peak_rss_delta_bytes: u64,
    pub stage_sum_ns: u64,
    pub unaccounted_ns: i64,
}

fn sink() -> Option<&'static PathBuf> {
    static SINK: OnceLock<Option<PathBuf>> = OnceLock::new();
    SINK.get_or_init(|| {
        let raw = std::env::var("STANNUM_BUILD_PROFILE").ok()?;
        if raw.is_empty() || raw == "0" || raw == "off" {
            return None;
        }
        Some(PathBuf::from(raw))
    })
    .as_ref()
}

#[inline]
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if FORCE.with(Cell::get) {
        return true;
    }
    sink().is_some()
}

#[cfg(test)]
thread_local! {
    static FORCE: Cell<bool> = const { Cell::new(false) };
}

struct Active {
    stage: Stage,
    started: Instant,
    nest: u32,
}

struct Part {
    phase: Phase,
    started: Instant,
}

struct State {
    counters: Counters,
    stack: Vec<Active>,
    part: Option<Part>,
    wall_start: Option<Instant>,
    cpu_start_ns: u64,
    rss_start: u64,
    wal_start: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            counters: Counters {
                schema: "stn4-build-profile-v1",
                ..Counters::default()
            },
            stack: Vec::new(),
            part: None,
            wall_start: None,
            cpu_start_ns: 0,
            rss_start: 0,
            wal_start: 0,
        }
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub(crate) struct Session {
    live: bool,
}

impl Session {
    pub(crate) fn begin() -> Self {
        if !enabled() {
            return Self { live: false };
        }
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            *state = State::default();
            state.wall_start = Some(Instant::now());
            let (cpu, rss) = rusage();
            state.cpu_start_ns = cpu;
            state.rss_start = rss;
            state.wal_start = wal_ptr();
        });
        segment::build_trace::install(Some(on_trace));
        Self { live: true }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            if let Some(part) = state.part.take() {
                credit_part(&mut state.counters, part.phase, part.started.elapsed());
            }
            while let Some(active) = state.stack.pop() {
                credit_stage(&mut state.counters, active.stage, active.started.elapsed());
            }
            if let Some(start) = state.wall_start {
                state.counters.wall_ns = nanos(start.elapsed());
            }
            let (cpu, rss) = rusage();
            state.counters.cpu_ns = cpu.saturating_sub(state.cpu_start_ns);
            state.counters.peak_rss_bytes = rss;
            state.counters.peak_rss_delta_bytes = rss.saturating_sub(state.rss_start);
            let wal = wal_ptr().saturating_sub(state.wal_start);
            if state.counters.wal_bytes == 0 {
                state.counters.wal_bytes = wal;
            }
            reconcile(&mut state.counters);
        });
        segment::build_trace::install(None);
        emit();
    }
}

pub(crate) struct Span {
    stage: Stage,
    live: bool,
}

#[inline]
pub(crate) fn span(stage: Stage) -> Span {
    if !enabled() {
        return Span { stage, live: false };
    }
    push(stage);
    Span { stage, live: true }
}

impl Drop for Span {
    fn drop(&mut self) {
        if self.live {
            pop(self.stage);
        }
    }
}

fn push(stage: Stage) {
    STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let paused = match state.stack.last_mut() {
            Some(top) if top.stage == stage => {
                top.nest = top.nest.saturating_add(1);
                return;
            }
            Some(top) => Some((top.stage, top.started.elapsed())),
            None => None,
        };
        if let Some((parent, elapsed)) = paused {
            credit_stage(&mut state.counters, parent, elapsed);
        }
        state.stack.push(Active {
            stage,
            started: Instant::now(),
            nest: 0,
        });
    });
}

fn pop(stage: Stage) {
    STATE.with(|cell| {
        let mut state = cell.borrow_mut();
        let elapsed = {
            let Some(top) = state.stack.last_mut() else {
                return;
            };
            if top.stage != stage {
                return;
            }
            if top.nest > 0 {
                top.nest -= 1;
                return;
            }
            top.started.elapsed()
        };
        credit_stage(&mut state.counters, stage, elapsed);
        state.stack.pop();
        if let Some(parent) = state.stack.last_mut() {
            parent.started = Instant::now();
        }
    });
}

fn on_trace(event: Trace) {
    match event {
        Trace::Enter(phase) => {
            push(stage_of(phase));
            STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                if let Some(part) = state.part.take() {
                    credit_part(&mut state.counters, part.phase, part.started.elapsed());
                }
                state.part = Some(Part {
                    phase,
                    started: Instant::now(),
                });
            });
        }
        Trace::Leave(phase) => {
            STATE.with(|cell| {
                let mut state = cell.borrow_mut();
                if let Some(part) = state.part.take()
                    && part.phase == phase
                {
                    credit_part(&mut state.counters, part.phase, part.started.elapsed());
                }
            });
            pop(stage_of(phase));
        }
        Trace::Lookup => add_lookups(1),
        Trace::Record => add_records(1),
        Trace::Alloc(bytes) => add_alloc(bytes),
        Trace::Tokens { field, n } => add_tokens(field, u64::from(n)),
    }
}

fn stage_of(phase: Phase) -> Stage {
    match phase {
        Phase::IngestTokenize => Stage::Tokenize,
        Phase::IngestInsert => Stage::Mutable,
        Phase::Sort => Stage::Fold,
        Phase::Dictionary
        | Phase::Ordinals
        | Phase::Tf
        | Phase::Positions
        | Phase::Fch1
        | Phase::Norms
        | Phase::Assemble => Stage::Encode,
    }
}

fn credit_stage(counters: &mut Counters, stage: Stage, elapsed: Duration) {
    let ns = nanos(elapsed);
    let slot = match stage {
        Stage::Heap => &mut counters.heap_ns,
        Stage::Tokenize => &mut counters.tokenize_ns,
        Stage::Mutable => &mut counters.mutable_ns,
        Stage::Fold => &mut counters.fold_ns,
        Stage::Merge => &mut counters.merge_ns,
        Stage::Encode => &mut counters.encode_ns,
        Stage::Storage => &mut counters.storage_ns,
    };
    *slot = slot.saturating_add(ns);
}

fn credit_part(counters: &mut Counters, phase: Phase, elapsed: Duration) {
    let ns = nanos(elapsed);
    let slot = match phase {
        Phase::Sort => &mut counters.fold_sort_ns,
        Phase::Dictionary => &mut counters.encode_dictionary_ns,
        Phase::Ordinals => &mut counters.encode_ordinals_ns,
        Phase::Tf => &mut counters.encode_tf_ns,
        Phase::Positions => &mut counters.encode_positions_ns,
        Phase::Fch1 => &mut counters.encode_fch1_ns,
        Phase::Norms => &mut counters.encode_norms_ns,
        Phase::Assemble => &mut counters.encode_assemble_ns,
        Phase::IngestTokenize | Phase::IngestInsert => return,
    };
    *slot = slot.saturating_add(ns);
}

fn reconcile(counters: &mut Counters) {
    counters.stage_sum_ns = counters
        .heap_ns
        .saturating_add(counters.tokenize_ns)
        .saturating_add(counters.mutable_ns)
        .saturating_add(counters.fold_ns)
        .saturating_add(counters.merge_ns)
        .saturating_add(counters.encode_ns)
        .saturating_add(counters.storage_ns);
    counters.unaccounted_ns = i64::try_from(counters.wall_ns).unwrap_or(i64::MAX)
        - i64::try_from(counters.stage_sum_ns).unwrap_or(i64::MAX);
}

pub(crate) fn set_field_count(field_count: u8) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| cell.borrow_mut().counters.field_count = field_count);
}

pub(crate) fn add_heap_row(input_bytes: u64, text_copies: u64) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.heap_rows = counters.heap_rows.saturating_add(1);
        counters.heap_input_bytes = counters.heap_input_bytes.saturating_add(input_bytes);
        counters.heap_text_copies = counters.heap_text_copies.saturating_add(text_copies);
    });
}

pub(crate) fn add_tokenize_call() {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.tokenize_calls = counters.tokenize_calls.saturating_add(1);
    });
}

pub(crate) fn add_tokenizer_init() {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.tokenizer_inits = counters.tokenizer_inits.saturating_add(1);
    });
}

pub(crate) fn add_tokens(field: u8, n: u64) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.tokens_total = counters.tokens_total.saturating_add(n);
        if let Some(slot) = counters.tokens_by_field.get_mut(usize::from(field)) {
            *slot = slot.saturating_add(n);
        }
    });
}

pub(crate) fn add_lookups(n: u64) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.term_lookups = counters.term_lookups.saturating_add(n);
    });
}

pub(crate) fn add_records(n: u64) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.doc_term_field_records = counters.doc_term_field_records.saturating_add(n);
    });
}

pub(crate) fn add_alloc(bytes: u64) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.alloc_bytes = counters.alloc_bytes.saturating_add(bytes);
    });
}

pub(crate) fn add_flush(records: u64) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.flush_count = counters.flush_count.saturating_add(1);
        counters.fold_records = counters.fold_records.saturating_add(records);
    });
}

pub(crate) fn add_merge_input(records: u64, bytes: u64) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.merge_count = counters.merge_count.saturating_add(1);
        counters.merge_input_records = counters.merge_input_records.saturating_add(records);
        counters.merge_input_bytes = counters.merge_input_bytes.saturating_add(bytes);
    });
}

pub(crate) fn add_merge_output(records: u64, bytes: u64) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.merge_output_records = counters.merge_output_records.saturating_add(records);
        counters.merge_output_bytes = counters.merge_output_bytes.saturating_add(bytes);
    });
}

pub(crate) fn add_write(bytes: u64, pages: u64) {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.bytes_written = counters.bytes_written.saturating_add(bytes);
        counters.pages_written = counters.pages_written.saturating_add(pages);
    });
}

pub(crate) fn add_sync_wait() {
    if !enabled() {
        return;
    }
    STATE.with(|cell| {
        let counters = &mut cell.borrow_mut().counters;
        counters.flush_sync_waits = counters.flush_sync_waits.saturating_add(1);
    });
}

fn emit() {
    let Some(path) = sink() else {
        return;
    };
    let snapshot = STATE.with(|cell| cell.borrow().counters.clone());
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
fn nanos(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

fn rusage() -> (u64, u64) {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let ok = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if ok != 0 {
        return (0, 0);
    }
    let usage = unsafe { usage.assume_init() };
    let cpu = timeval_ns(usage.ru_utime).saturating_add(timeval_ns(usage.ru_stime));
    let rss = {
        let raw = usage.ru_maxrss as u64;
        if cfg!(target_os = "macos") {
            raw
        } else {
            raw.saturating_mul(1024)
        }
    };
    (cpu, rss)
}

fn timeval_ns(tv: libc::timeval) -> u64 {
    (tv.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add((tv.tv_usec as u64).saturating_mul(1_000))
}

fn wal_ptr() -> u64 {
    #[cfg(not(test))]
    {
        pg_sys_wal_ptr()
    }
    #[cfg(test)]
    {
        0
    }
}

#[cfg(not(test))]
fn pg_sys_wal_ptr() -> u64 {
    unsafe { pgrx::pg_sys::GetXLogInsertRecPtr() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn force(on: bool) {
        FORCE.with(|cell| cell.set(on));
    }

    fn snapshot() -> Counters {
        STATE.with(|cell| cell.borrow().counters.clone())
    }

    fn reset_state() {
        STATE.with(|cell| *cell.borrow_mut() = State::default());
    }

    #[test]
    fn disabled_path_does_not_count() {
        force(false);
        reset_state();
        {
            let _heap = span(Stage::Heap);
            add_tokens(0, 9);
            add_heap_row(4, 1);
        }
        let counters = snapshot();
        assert_eq!(counters.heap_ns, 0);
        assert_eq!(counters.tokens_total, 0);
        assert_eq!(counters.heap_rows, 0);
    }

    #[test]
    fn nested_stages_are_exclusive() {
        force(true);
        reset_state();
        {
            let _heap = span(Stage::Heap);
            std::thread::sleep(Duration::from_millis(4));
            {
                let _tok = span(Stage::Tokenize);
                std::thread::sleep(Duration::from_millis(4));
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        let counters = snapshot();
        force(false);
        reset_state();
        assert!(
            counters.tokenize_ns > 0,
            "tokenize {}",
            counters.tokenize_ns
        );
        assert!(counters.heap_ns > 0, "heap {}", counters.heap_ns);
        let ratio = counters.heap_ns as f64 / counters.tokenize_ns as f64;
        assert!(
            (0.4..=3.5).contains(&ratio),
            "heap {} tokenize {} ratio {ratio}",
            counters.heap_ns,
            counters.tokenize_ns
        );
    }
}
