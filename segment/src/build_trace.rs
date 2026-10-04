// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Optional build-path trace hooks. Off unless the postgres crate arms a
//! listener for `CREATE INDEX` diagnostics. Call sites must not emit, and
//! the armed flag stays false for acceptance builds.

use std::sync::atomic::{AtomicBool, Ordering};

/// Coarse phases inside [`crate::segment::SegmentBuilder`]. Postgres maps
/// these onto exclusive build stages; they are not themselves printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    IngestTokenize,
    IngestInsert,
    Sort,
    Dictionary,
    Ordinals,
    Tf,
    Positions,
    Fch1,
    Norms,
    Assemble,
}

#[derive(Clone, Copy, Debug)]
pub enum Trace {
    Enter(Phase),
    Leave(Phase),
    Lookup,
    Record,
    Alloc(u64),
    Tokens { field: u8, n: u32 },
}

static ARMED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static HOOK: std::cell::Cell<Option<fn(Trace)>> = const { std::cell::Cell::new(None) };
}

/// Installs or clears the process-wide listener. `None` disarms every hook.
pub fn install(hook: Option<fn(Trace)>) {
    HOOK.with(|cell| cell.set(hook));
    ARMED.store(hook.is_some(), Ordering::Relaxed);
}

#[inline]
pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

#[inline]
pub fn note(event: Trace) {
    if !armed() {
        return;
    }
    HOOK.with(|cell| {
        if let Some(hook) = cell.get() {
            hook(event);
        }
    });
}

struct Guard {
    phase: Phase,
}

impl Guard {
    fn enter(phase: Phase) -> Self {
        note(Trace::Enter(phase));
        Self { phase }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        note(Trace::Leave(self.phase));
    }
}

/// Runs `f` under `phase` when armed; otherwise calls `f` only.
#[inline]
pub fn timed<T>(phase: Phase, f: impl FnOnce() -> T) -> T {
    if !armed() {
        return f();
    }
    let _guard = Guard::enter(phase);
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tid;
    use crate::segment::SegmentBuilder;

    fn stock_blob() -> Vec<u8> {
        let mut builder = SegmentBuilder::default();
        builder
            .add_document(
                Tid {
                    block: 1,
                    offset: 1,
                },
                [("hello", 1), ("world", 2)],
            )
            .unwrap();
        builder.finish()
    }

    fn fielded_blob() -> Vec<u8> {
        let mut builder = SegmentBuilder::default();
        builder.set_field_count(2).unwrap();
        builder
            .begin_fielded_document(Tid {
                block: 1,
                offset: 1,
            })
            .unwrap();
        builder.add_occurrence("hello", 0, &[1], 1).unwrap();
        builder.add_occurrence("hello", 1, &[1], 2).unwrap();
        builder.finish()
    }

    #[test]
    fn finish_bytes_unchanged_when_trace_hook_is_installed() {
        let stock = stock_blob();
        let fielded = fielded_blob();
        install(Some(|_| {}));
        let stock_traced = stock_blob();
        let fielded_traced = fielded_blob();
        install(None);
        assert_eq!(stock, stock_traced);
        assert_eq!(fielded, fielded_traced);
        assert!(!armed());
    }
}
