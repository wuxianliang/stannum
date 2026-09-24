// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! `pg_stat_progress_create_index` reporting for index builds. No GUC.
//!
//! Core owns the progress command's lifecycle: it starts the command, keeps
//! the phase at `building index` while `ambuild` runs, and — because
//! `ambuild` passes `progress = true` to `table_index_build_scan` — already
//! advances `tuples_done`/`tuples_total` and the block columns for the heap
//! scan. Stannum therefore writes only the AM subphase slot and, for its own
//! run writing, the block columns. A second writer of the tuple columns
//! would double-count, and Stannum's phase numbers below are subphases of
//! core's `building index` phase, never mapped onto core's own
//! lock-wait/validation phase numbers.
//!
//! The subphase slot is projected by PG18's view (`building index: <name>`
//! through `pg_indexam_progress_phasename`), so `ambuildphasename` supplies
//! the names. While subphase 1 (heap scan) runs, `blocks_done`/`blocks_total`
//! count heap blocks; from subphase 2 on they count blob pages in the same
//! columns — the mid-command denominator switch; the subphase is the signal
//! (documented in `docs/architecture/segmented-storage.md`).
//!
//! A thread-local guard set only by `ambuild` gates every update, so insert
//! folds and VACUUM merges — which share the run writer with builds — never
//! publish create-index progress (a VACUUM command would otherwise be
//! corrupted: the slot meanings differ per command).

use std::cell::Cell;

use pgrx::pg_guard;
use pgrx::pg_sys;

/// Scanning the heap; core advances the tuple and block counters.
pub(crate) const SUBPHASE_HEAP_SCAN: i64 = 1;
/// Writing an accumulated segment out as run pages.
pub(crate) const SUBPHASE_SEGMENT_FLUSH: i64 = 2;
/// The final flush and any merges it triggers.
pub(crate) const SUBPHASE_MERGE_FINISH: i64 = 3;

thread_local! {
    static IN_INDEX_BUILD: Cell<bool> = const { Cell::new(false) };
}

/// Claims create-index progress reporting until dropped, including through
/// an error unwind. Only `ambuild` creates one.
pub(crate) struct BuildGuard {
    _private: (),
}

pub(crate) fn enter_build() -> BuildGuard {
    IN_INDEX_BUILD.set(true);
    BuildGuard { _private: () }
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        IN_INDEX_BUILD.set(false);
    }
}

/// Updates one progress slot, and only while this backend is inside
/// `ambuild` with its progress command active.
fn update(index: u32, value: i64) {
    if !IN_INDEX_BUILD.get() {
        return;
    }
    unsafe { pg_sys::pgstat_progress_update_param(index as std::ffi::c_int, value) };
}

/// Names this access method's build subphases for the progress view.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn ambuildphasename(phasenum: i64) -> *mut std::ffi::c_char {
    let name = match phasenum {
        SUBPHASE_HEAP_SCAN => c"heap scan",
        SUBPHASE_SEGMENT_FLUSH => c"segment flush",
        SUBPHASE_MERGE_FINISH => c"final merge-finish",
        _ => return std::ptr::null_mut(),
    };
    name.as_ptr().cast_mut()
}

/// Reports the build's current subphase.
pub(crate) fn update_subphase(subphase: i64) {
    update(pg_sys::PROGRESS_CREATEIDX_SUBPHASE, subphase);
}

/// The pages of the run about to be written become the block denominator.
/// The heap scan left heap blocks there; run writing switches the unit.
pub(crate) fn set_blocks_total(pages: usize) {
    update(pg_sys::PROGRESS_SCAN_BLOCKS_TOTAL, pages as i64);
}

/// One more run page written. Counts within the run whose total
/// [`set_blocks_total`] announced.
pub(crate) fn set_blocks_done(pages: usize) {
    update(pg_sys::PROGRESS_SCAN_BLOCKS_DONE, pages as i64);
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod hardening_progress_tests {
    use super::*;
    use pgrx::prelude::*;

    #[pg_test(schema = "tests")]
    fn progress_updates_preserve_core_slots_and_ignore_insert_folds() {
        Spi::run(
            "CREATE TABLE hardening_progress(body text);
             CREATE INDEX hardening_progress_idx ON hardening_progress USING stannum(body);
             SET LOCAL stannum.write_buffer_docs = 2;",
        )
        .unwrap();
        // Own this backend's progress command so the assertion is deterministic,
        // rather than hoping a polling connection catches a short build phase.
        struct EndProgress;
        impl Drop for EndProgress {
            fn drop(&mut self) {
                unsafe { pg_sys::pgstat_progress_end_command() };
            }
        }
        unsafe {
            assert!(!pg_sys::MyBEEntry.is_null());
            pg_sys::pgstat_progress_start_command(
                pg_sys::ProgressCommandType::PROGRESS_COMMAND_CREATE_INDEX,
                pg_sys::InvalidOid,
            );
            let _end = EndProgress;
            for (slot, value) in [
                (pg_sys::PROGRESS_CREATEIDX_TUPLES_DONE, 17),
                (pg_sys::PROGRESS_CREATEIDX_TUPLES_TOTAL, 31),
                (
                    pg_sys::PROGRESS_CREATEIDX_PHASE,
                    i64::from(pg_sys::PROGRESS_CREATEIDX_PHASE_BUILD),
                ),
            ] {
                pg_sys::pgstat_progress_update_param(slot as i32, value);
            }
            let before = (*pg_sys::MyBEEntry).st_progress_param;
            {
                let _guard = enter_build();
                for phase in [
                    SUBPHASE_HEAP_SCAN,
                    SUBPHASE_SEGMENT_FLUSH,
                    SUBPHASE_MERGE_FINISH,
                ] {
                    update_subphase(phase);
                    set_blocks_total(4);
                    set_blocks_done(3);
                    let current = (*pg_sys::MyBEEntry).st_progress_param;
                    assert_eq!(current[pg_sys::PROGRESS_CREATEIDX_SUBPHASE as usize], phase);
                    assert_eq!(current[pg_sys::PROGRESS_SCAN_BLOCKS_TOTAL as usize], 4);
                    assert_eq!(current[pg_sys::PROGRESS_SCAN_BLOCKS_DONE as usize], 3);
                    for slot in [
                        pg_sys::PROGRESS_CREATEIDX_TUPLES_DONE,
                        pg_sys::PROGRESS_CREATEIDX_TUPLES_TOTAL,
                        pg_sys::PROGRESS_CREATEIDX_PHASE,
                    ] {
                        assert_eq!(current[slot as usize], before[slot as usize]);
                    }
                }
            }
            let before_insert = (*pg_sys::MyBEEntry).st_progress_param;
            Spi::run("INSERT INTO hardening_progress SELECT 'needle' FROM generate_series(1, 6)")
                .unwrap();
            assert_eq!((*pg_sys::MyBEEntry).st_progress_param, before_insert);
            // Prove the insert actually traversed the run writer, not just an
            // in-memory buffer which could make the guard test vacuous.
            assert!(Spi::get_one::<i64>(
                "SELECT count(*) FROM stannum.segment_info('hardening_progress_idx') WHERE kind='immutable'"
            ).unwrap().unwrap() > 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_are_inert_outside_a_build() {
        // No panic, no observable effect: the guard is thread-local and off.
        update_subphase(SUBPHASE_HEAP_SCAN);
        set_blocks_total(3);
        set_blocks_done(1);
        assert!(!IN_INDEX_BUILD.get());
        let guard = enter_build();
        assert!(IN_INDEX_BUILD.get());
        drop(guard);
        assert!(!IN_INDEX_BUILD.get());
    }
}
