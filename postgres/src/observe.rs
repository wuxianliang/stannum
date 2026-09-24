// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Per-scan I/O observation, decoupled from the cached segment readers.
//!
//! Readers live across statements, so page counters cannot sit on them, and
//! the pin happens deep under [`crate::storage::RunSource::read`] where no
//! executor state is reachable. A thread-local stack of [`IoFrame`]s is the
//! transport instead: a custom scan's begin pushes a frame
//! ([`push_io`]), every pinned read adds to the top frame
//! ([`add_pages`]), EXPLAIN reads the top frame back, and the guard's drop
//! (end of scan, including error paths) pops it. An empty stack means "do
//! not count": vacuum, insert folds and `stannum.search()` observe nothing.
//! Parallel workers own their stacks, and the custom scan's existing
//! idle-leader early return governs disclosure.
//!
//! Counted values are Stannum pin counts, not core `BufferUsage` (which core
//! sums across workers and would double-count reader-cache hits). Repeat
//! pins are counted: a distinct-set alternative (`set<(source, block)>`)
//! was considered and rejected — pin counts are cheaper and match the reader
//! cache's actual work. The choice is observable (a query re-reading an
//! extent across two cursors counts twice) and is pinned by the
//! observability tests.
//!
//! With two Stannum scan nodes in one plan, both frames are live during
//! execution and reads attribute to whichever scan began last; the
//! single-scan plans the counters are documented for are exact.

use std::cell::RefCell;

use segment::source::Area;

/// One scan's page counters, accumulated from `RunSource` pin walks.
pub(crate) struct IoFrame {
    id: u64,
    dictionary_pages: u64,
    postings_blocks: u64,
}

thread_local! {
    static STACK: RefCell<Vec<IoFrame>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: RefCell<u64> = const { RefCell::new(0) };
}

/// Owns a pushed frame; dropping it removes that frame from the stack.
pub(crate) struct IoGuard {
    id: u64,
}

/// Pushes a counting frame for the current scan.
pub(crate) fn push_io() -> IoGuard {
    let id = NEXT_ID.with_borrow_mut(|next| {
        *next += 1;
        *next
    });
    STACK.with_borrow_mut(|stack| {
        stack.push(IoFrame {
            id,
            dictionary_pages: 0,
            postings_blocks: 0,
        })
    });
    IoGuard { id }
}

impl Drop for IoGuard {
    fn drop(&mut self) {
        // Frames end out of order when sibling scans end in plan order, so
        // remove this guard's frame by identity rather than popping the top.
        STACK.with_borrow_mut(|stack| {
            if let Some(at) = stack.iter().rposition(|frame| frame.id == self.id) {
                stack.remove(at);
            }
        });
    }
}

/// Adds `blocks` pinned pages of `area` to the top frame, if any.
pub(crate) fn add_pages(area: Area, blocks: u64) {
    STACK.with_borrow_mut(|stack| {
        let Some(frame) = stack.last_mut() else {
            return;
        };
        match area {
            Area::Dictionary => frame.dictionary_pages += blocks,
            Area::Postings => frame.postings_blocks += blocks,
            // Payload, docs, lengths and the header probe are read but not
            // reported; the frame carries only the two query-shaped areas.
            _ => {}
        }
    });
}

/// The top frame's `(dictionary_pages, postings_blocks)`, for EXPLAIN.
pub(crate) fn top_pages() -> Option<(u64, u64)> {
    STACK.with_borrow(|stack| {
        stack
            .last()
            .map(|frame| (frame.dictionary_pages, frame.postings_blocks))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_count_only_while_pushed_and_only_two_areas() {
        // Empty stack: nothing counted, nothing observable.
        add_pages(Area::Dictionary, 3);
        assert_eq!(top_pages(), None);
        let guard = push_io();
        add_pages(Area::Dictionary, 2);
        add_pages(Area::Postings, 5);
        add_pages(Area::Payload, 100);
        add_pages(Area::Other, 100);
        assert_eq!(top_pages(), Some((2, 5)));
        // Repeat pins accumulate rather than deduplicate.
        add_pages(Area::Dictionary, 2);
        assert_eq!(top_pages(), Some((4, 5)));
        drop(guard);
        assert_eq!(top_pages(), None);
    }

    #[test]
    fn frames_remove_by_identity_when_siblings_end_in_order() {
        let first = push_io();
        add_pages(Area::Dictionary, 1);
        let second = push_io();
        add_pages(Area::Dictionary, 10);
        assert_eq!(top_pages(), Some((10, 0)));
        drop(first);
        assert_eq!(top_pages(), Some((10, 0)));
        drop(second);
        assert_eq!(top_pages(), None);
    }
}
