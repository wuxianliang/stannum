// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use crate::highlight::{highlight_text, highlight_text_ansi, positions_from_query};
use crate::score::{
    PRUNE_MAX_K, PrunedCandidates, VisibleTid, build_standalone_scorer, visible_tid_pairs,
};
use pgrx::iter::TableIterator;
use pgrx::{FromDatum, PgRelation, default, name, pg_extern, pg_sys};
use rustc_hash::FxHashMap;
use segment::Tid;
use std::collections::BTreeSet;
use tokenizer::CompiledTokenizerPipeline;

type SearchRow = (pg_sys::ItemPointerData, f32, Option<String>);

fn validate_shape(index: &PgRelation, snippets: bool) -> i16 {
    unsafe {
        let metadata = &*(*index.as_ptr()).rd_index;
        if metadata.indnkeyatts != 1 {
            pgrx::error!("stannum.search() supports a single text column until LSG4");
        }
        let key = *metadata.indkey.values.as_ptr();
        if !snippets {
            return key;
        }
        if key <= 0 {
            pgrx::error!(
                "stannum.search() snippets require a positive text key; use snippet => 'none' for an expression index"
            );
        }
        let heap_oid = pg_sys::IndexGetRelation(index.oid(), false);
        let heap = pg_sys::table_open(heap_oid, pg_sys::AccessShareLock as _);
        let attribute = pg_sys::TupleDescAttr((*heap).rd_att, i32::from(key - 1));
        let typid = pg_sys::getBaseType((*attribute).atttypid);
        let text_compatible = matches!(
            typid,
            pg_sys::TEXTOID | pg_sys::VARCHAROID | pg_sys::BPCHAROID | pg_sys::NAMEOID
        );
        pg_sys::table_close(heap, pg_sys::AccessShareLock as _);
        if !text_compatible {
            pgrx::error!(
                "stannum.search() snippets require a text-compatible key column; use snippet => 'none' for degraded mode"
            );
        }
        key
    }
}

fn validate_snippet(snippet: &str) -> &'static str {
    match snippet {
        "none" => "none",
        "html" => "html",
        "ansi" => "ansi",
        _ => pgrx::error!("stannum.search() snippet must be one of: none, html, ansi"),
    }
}

fn pointer_of(tid: Tid) -> pg_sys::ItemPointerData {
    pg_sys::ItemPointerData {
        ip_blkid: pg_sys::BlockIdData {
            bi_hi: (tid.block >> 16) as u16,
            bi_lo: tid.block as u16,
        },
        ip_posid: tid.offset,
    }
}

fn tid_of(pointer: pg_sys::ItemPointerData) -> Tid {
    let block = (u32::from(pointer.ip_blkid.bi_hi) << 16) | u32::from(pointer.ip_blkid.bi_lo);
    Tid::new(block, pointer.ip_posid)
        .unwrap_or_else(|_| pgrx::error!("invalid visible heap tuple location"))
}

fn rank_rows(rows: &mut [(f32, VisibleTid)]) {
    rows.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .total_cmp(left_score)
            .then(left.visible_tid.cmp(&right.visible_tid))
    });
}

fn exhaustive_rows(
    scorer: &mut crate::score::IndexScorer,
    heap_oid: pg_sys::Oid,
) -> Vec<(f32, VisibleTid)> {
    let roots = scorer.matching_tids();
    let visible = unsafe { visible_tid_pairs(heap_oid, roots) };
    let visible_roots: BTreeSet<Tid> = visible.iter().map(|row| row.indexed_tid).collect();
    let scored = scorer.score_matching_tids(&visible_roots);
    let scores: FxHashMap<Tid, f32> = scored
        .into_iter()
        .map(|row| (row.indexed_tid, row.score))
        .collect();
    let mut rows: Vec<_> = visible
        .into_iter()
        .filter_map(|row| {
            scores
                .get(&row.indexed_tid)
                .copied()
                .map(|score| (score, row))
        })
        .collect();
    rank_rows(&mut rows);
    rows
}

fn accepted_pruned_rows(
    pruned: PrunedCandidates,
    heap_oid: pg_sys::Oid,
    limit: usize,
) -> Option<Vec<(f32, VisibleTid)>> {
    if !pruned.complete {
        return None;
    }
    let roots: BTreeSet<Tid> = pruned.rows.iter().map(|row| row.indexed_tid).collect();
    let visible = unsafe { visible_tid_pairs(heap_oid, roots) };
    // This simultaneously rejects invisible top rows and any HOT dedupe that
    // would make the distinct-visible-document result underfill.
    if visible.len() != pruned.rows.len() {
        return None;
    }
    let scores: FxHashMap<Tid, f32> = pruned
        .rows
        .into_iter()
        .map(|row| (row.indexed_tid, row.score))
        .collect();
    let mut rows: Vec<_> = visible
        .into_iter()
        .filter_map(|row| {
            scores
                .get(&row.indexed_tid)
                .copied()
                .map(|score| (score, row))
        })
        .collect();
    rank_rows(&mut rows);
    rows.truncate(limit);
    Some(rows)
}

#[allow(clippy::too_many_arguments)] // snippet refetch bundles its fixed context
fn fetch_snippet(
    heap_oid: pg_sys::Oid,
    attnum: i16,
    pipeline: &CompiledTokenizerPipeline,
    query: &str,
    mode: &str,
    begin_tag: &str,
    end_tag: &str,
    rows: Vec<(f32, VisibleTid)>,
) -> Vec<SearchRow> {
    if mode == "none" {
        return rows
            .into_iter()
            .map(|(score, row)| (pointer_of(row.visible_tid), score, None))
            .collect();
    }
    unsafe {
        let heap = pg_sys::table_open(heap_oid, pg_sys::AccessShareLock as _);
        let fetch = pg_sys::table_index_fetch_begin(heap);
        let slot = pg_sys::table_slot_create(heap, std::ptr::null_mut());
        let snapshot = pg_sys::GetActiveSnapshot();
        let mut output = Vec::with_capacity(rows.len());
        for (score, row) in rows {
            pgrx::check_for_interrupts!();
            let mut pointer = pointer_of(row.indexed_tid);
            let mut call_again = false;
            let mut all_dead = false;
            let mut found = false;
            loop {
                if pg_sys::table_index_fetch_tuple(
                    fetch,
                    &mut pointer,
                    snapshot,
                    slot,
                    &mut call_again,
                    &mut all_dead,
                ) {
                    found = true;
                    break;
                }
                if !call_again {
                    break;
                }
            }
            if !found || tid_of((*slot).tts_tid) != row.visible_tid {
                continue;
            }
            let mut isnull = false;
            let datum = pg_sys::slot_getattr(slot, i32::from(attnum), &mut isnull);
            let snippet = if isnull {
                None
            } else {
                let text = String::from_datum(datum, false)
                    .unwrap_or_else(|| pgrx::error!("stannum.search() indexed column is not text"));
                let positions = positions_from_query(pipeline, query, &text);
                let rendered = if mode == "ansi" {
                    highlight_text_ansi(pipeline, &text, &positions)
                } else {
                    highlight_text(pipeline, &text, begin_tag, end_tag, &positions)
                };
                Some(rendered.unwrap_or_else(|error| {
                    pgrx::error!("stannum.search() snippet rendering failed: {error}")
                }))
            };
            output.push((pointer_of(row.visible_tid), score, snippet));
        }
        pg_sys::ExecDropSingleTupleTableSlot(slot);
        pg_sys::table_index_fetch_end(fetch);
        pg_sys::table_close(heap, pg_sys::AccessShareLock as _);
        output
    }
}

#[pg_extern(volatile, parallel_unsafe)]
#[expect(
    clippy::too_many_arguments,
    reason = "SQL surface is intentionally explicit"
)]
fn search(
    index: PgRelation,
    query: Option<&str>,
    limit: default!(i32, 10),
    snippet: default!(&str, "'html'"),
    begin_tag: default!(&str, "'<mark>'"),
    end_tag: default!(&str, "'</mark>'"),
    k1: default!(Option<f32>, "NULL"),
    b: default!(Option<f32>, "NULL"),
) -> TableIterator<
    'static,
    (
        name!(ctid, pg_sys::ItemPointerData),
        name!(score, f32),
        name!(snippet, Option<String>),
    ),
> {
    crate::udfs::require_stannum_index(&index, "search");
    let query = query.unwrap_or_else(|| pgrx::error!("stannum.search() query must not be NULL"));
    if !unsafe { crate::storage::present(index.as_ptr()) } {
        pgrx::error!("stannum.search() requires a segmented stannum index");
    }
    if limit < 0 {
        pgrx::error!("stannum.search() limit must be non-negative");
    }
    let mode = validate_snippet(snippet);
    let attnum = validate_shape(&index, mode != "none");
    let heap_oid = unsafe { pg_sys::IndexGetRelation(index.oid(), false) };
    let index_oid = index.oid();
    let mut scorer = build_standalone_scorer(heap_oid, index_oid, query, k1, b);
    if limit == 0 {
        return TableIterator::new(Vec::new());
    }
    let limit = usize::try_from(limit).expect("non-negative limit fits usize");
    let rows = if limit <= PRUNE_MAX_K {
        let pruned = scorer
            .pruned_top_k(limit)
            .and_then(|pruned| accepted_pruned_rows(pruned, heap_oid, limit));
        pruned.unwrap_or_else(|| exhaustive_rows(&mut scorer, heap_oid))
    } else {
        exhaustive_rows(&mut scorer, heap_oid)
    };
    let rows = rows.into_iter().take(limit).collect();
    let pipeline = scorer.pipeline();
    let rows = fetch_snippet(
        heap_oid, attnum, pipeline, query, mode, begin_tag, end_tag, rows,
    );
    TableIterator::new(rows)
}

#[pg_extern(volatile, parallel_unsafe)]
fn search_count(index: PgRelation, query: Option<&str>) -> i64 {
    crate::udfs::require_stannum_index(&index, "search_count");
    let query =
        query.unwrap_or_else(|| pgrx::error!("stannum.search_count() query must not be NULL"));
    if !unsafe { crate::storage::present(index.as_ptr()) } {
        pgrx::error!("stannum.search_count() requires a segmented stannum index");
    }
    validate_shape(&index, false);
    let heap_oid = unsafe { pg_sys::IndexGetRelation(index.oid(), false) };
    let scorer = build_standalone_scorer(heap_oid, index.oid(), query, None, None);
    let roots = scorer.matching_tids();
    let visible = unsafe { visible_tid_pairs(heap_oid, roots) };
    i64::try_from(visible.len())
        .unwrap_or_else(|_| pgrx::error!("stannum.search_count() result is too large"))
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use crate::score::RankedCandidate;
    use pgrx::prelude::*;

    fn oid(name: &str) -> pg_sys::Oid {
        Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{name}'::regclass::oid"))
            .unwrap()
            .unwrap()
    }

    fn row_tid(id: i32) -> Tid {
        tid_of(
            Spi::get_one::<pg_sys::ItemPointerData>(&format!(
                "SELECT ctid FROM hardening_docs WHERE id = {id}"
            ))
            .unwrap()
            .unwrap(),
        )
    }

    struct CurrentSnapshot;

    impl CurrentSnapshot {
        fn push() -> Self {
            // The outer pg_test SELECT predates all of our SPI writes. Private
            // heap-refetch helpers need a fresh command snapshot, just as a
            // subsequent SQL search() invocation would receive.
            unsafe {
                pg_sys::CommandCounterIncrement();
                pg_sys::PushActiveSnapshot(pg_sys::GetTransactionSnapshot());
            }
            Self
        }
    }

    impl Drop for CurrentSnapshot {
        fn drop(&mut self) {
            unsafe { pg_sys::PopActiveSnapshot() };
        }
    }

    fn fixture() {
        Spi::run(
            "CREATE TABLE hardening_docs(id int, body text, revision int DEFAULT 0)
               WITH (fillfactor=50);
             INSERT INTO hardening_docs SELECT n, 'needle ' || repeat('pad ', n), 0
               FROM generate_series(1, 20) n;
             CREATE INDEX hardening_idx ON hardening_docs USING stannum(body);",
        )
        .unwrap();
    }

    #[pg_test]
    fn search_shape_accepts_include_catalog_layout() {
        fixture();
        // Stannum does not advertise amcaninclude yet. Use a real INCLUDE
        // descriptor to test the shape validator independently of the AM gate,
        // rather than mutating pg_index or pretending this is end-to-end support.
        Spi::run("CREATE INDEX hardening_include ON hardening_docs(body) INCLUDE(revision)")
            .unwrap();
        let index = unsafe {
            PgRelation::with_lock(oid("hardening_include"), pg_sys::AccessShareLock as _)
        };
        let metadata = unsafe { &*(*index.as_ptr()).rd_index };
        assert_eq!(metadata.indnatts, 2);
        assert_eq!(metadata.indnkeyatts, 1);
        assert_eq!(validate_shape(&index, true), 2);
    }

    #[pg_test(error = "stannum.search() supports a single text column until LSG4")]
    fn search_shape_rejects_multiple_keys_even_without_snippets() {
        fixture();
        // Likewise exercise the defensive SRF check even though CREATE INDEX
        // itself currently rejects multicolumn Stannum indexes.
        Spi::run("CREATE INDEX hardening_multikey ON hardening_docs(body, revision)").unwrap();
        let index = unsafe {
            PgRelation::with_lock(oid("hardening_multikey"), pg_sys::AccessShareLock as _)
        };
        validate_shape(&index, false);
    }

    #[pg_test]
    fn search_zero_limit_still_validates_query_mode_and_bm25() {
        fixture();
        // Catch errors in real SQL subtransactions, then continue using the
        // same index. Raising our own error is outside the caught block.
        for (arguments, message) in [
            (
                "'needle', 0, 'bogus'",
                "stannum.search() snippet must be one of: none, html, ansi",
            ),
            (
                "'needle', 0, 'none', '<m>', '</m>', -1, NULL",
                "stannum score parameters: invalid BM25 parameters",
            ),
            (
                "'needle', 0, 'none', '<m>', '</m>', NULL, 'NaN'::real",
                "stannum score parameters: invalid BM25 parameters",
            ),
            (
                "'needle', 0, 'none', '<m>', '</m>', 'Infinity'::real, NULL",
                "stannum score parameters: invalid BM25 parameters",
            ),
        ] {
            let message = message.replace('\'', "''");
            Spi::run(&format!(
                "DO $test$ DECLARE caught text; BEGIN
                   BEGIN PERFORM * FROM stannum.search('hardening_idx', {arguments});
                   EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
                   IF caught IS DISTINCT FROM '{message}' THEN
                     RAISE EXCEPTION 'expected %, got %', '{message}', caught;
                   END IF;
                 END $test$;"
            ))
            .unwrap();
        }
        Spi::run(
            "DO $test$ DECLARE caught boolean := false; BEGIN
               BEGIN PERFORM * FROM stannum.search('hardening_idx', '(', 0);
               EXCEPTION WHEN OTHERS THEN caught := true; END;
               IF NOT caught THEN RAISE EXCEPTION 'zero limit bypassed parser'; END IF;
             END $test$;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM stannum.search('hardening_idx', 'needle', 0)"
            )
            .unwrap(),
            Some(0)
        );
    }

    #[pg_test]
    fn search_large_limit_and_expression_degraded_mode_keep_visible_matches() {
        fixture();
        Spi::run(
            "CREATE INDEX hardening_expr ON hardening_docs USING stannum(lower(body));
             DELETE FROM hardening_docs WHERE id IN (1, 4, 7);",
        )
        .unwrap();
        let large_limit = PRUNE_MAX_K + 1;
        for index in ["hardening_idx", "hardening_expr"] {
            assert_eq!(
                Spi::get_one::<bool>(&format!(
                    "SELECT stannum.search_count('{index}', 'needle') =
                   (SELECT count(*) FROM hardening_docs WHERE body ==> 'needle')"
                ))
                .unwrap(),
                Some(true)
            );
            assert_eq!(
                Spi::get_one::<i64>(&format!(
                    "SELECT count(*) FROM hardening_docs d JOIN
                   stannum.search('{index}', 'needle', {large_limit}, 'none') s
                   ON d.ctid = s.ctid WHERE s.snippet IS NULL"
                ))
                .unwrap(),
                Some(17)
            );
        }
        // The default limit is a real ten-row bound, not all remaining rows.
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM stannum.search('hardening_idx', 'needle')")
                .unwrap(),
            Some(10)
        );
        Spi::run(
            "DO $test$ DECLARE caught text; BEGIN
               BEGIN PERFORM * FROM stannum.search('hardening_expr', 'needle', 0, 'html');
               EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
               IF caught IS NULL OR position('use snippet => ''none''' in caught) = 0 THEN
                 RAISE EXCEPTION 'missing actionable expression-key error: %', caught;
               END IF;
             END $test$;",
        )
        .unwrap();
    }

    #[pg_test]
    fn search_acceptance_rejects_invisible_top_row_before_exhaustive_fallback() {
        fixture();
        let heap = oid("hardening_docs");
        let index = oid("hardening_idx");
        let mut scorer = build_standalone_scorer(heap, index, "needle", None, None);
        // complete means every matching candidate fits (strictly fewer than
        // k), not merely that the WAND traversal completed. Use an oversized
        // pool to reach the visibility guard rather than the incomplete guard.
        let pruned = scorer.pruned_top_k(21).unwrap();
        assert!(pruned.complete);
        assert_eq!(pruned.rows.len(), 20);
        {
            let _snapshot = CurrentSnapshot::push();
            assert_eq!(
                accepted_pruned_rows(scorer.pruned_top_k(21).unwrap(), heap, 3)
                    .unwrap()
                    .len(),
                3
            );
        }
        let deleted = pruned.rows[0].indexed_tid;
        Spi::run(&format!(
            "DELETE FROM hardening_docs WHERE ctid = '({},{})'::tid",
            deleted.block, deleted.offset
        ))
        .unwrap();
        let _snapshot = CurrentSnapshot::push();
        assert!(accepted_pruned_rows(pruned, heap, 3).is_none());
        let expected = exhaustive_rows(&mut scorer, heap);
        assert!(expected.len() >= 3);
        assert!(expected.iter().all(|(_, row)| row.visible_tid != deleted));
        let actual: Vec<_> = search(
            unsafe { PgRelation::with_lock(index, pg_sys::AccessShareLock as _) },
            Some("needle"),
            3,
            "none",
            "",
            "",
            None,
            None,
        )
        .map(|(tid, score, _)| (tid_of(tid), score.to_bits()))
        .collect();
        assert_eq!(
            actual,
            expected[..3]
                .iter()
                .map(|(score, row)| (row.visible_tid, score.to_bits()))
                .collect::<Vec<_>>()
        );
    }

    #[pg_test]
    fn search_acceptance_rejects_hot_root_and_member_underfill() {
        fixture();
        let heap = oid("hardening_docs");
        let root = row_tid(1);
        Spi::run("UPDATE hardening_docs SET revision = 1 WHERE id = 1").unwrap();
        let member = row_tid(1);
        assert_ne!(root, member);
        let _snapshot = CurrentSnapshot::push();
        let visible = unsafe { visible_tid_pairs(heap, BTreeSet::from([root, member])) };
        assert_eq!(visible.len(), 1, "fixture must be an actual HOT chain");
        assert_eq!(visible[0].visible_tid, member);
        assert_eq!(
            visible[0].indexed_tid, root,
            "the indexed root must follow the HOT chain"
        );
        let member_only = unsafe { visible_tid_pairs(heap, BTreeSet::from([member])) };
        assert!(
            member_only.is_empty(),
            "a heap-only member is not an index root"
        );
        // A plain HOT update only leaves the root indexed. Inject both
        // candidates to exercise underfill rejection: PG follows the real
        // root to the member, but cannot fetch the heap-only member as another
        // independent index root. Do not mistake this for two visible roots.
        let pruned = PrunedCandidates {
            complete: true,
            rows: vec![
                RankedCandidate {
                    indexed_tid: root,
                    score: 2.0,
                },
                RankedCandidate {
                    indexed_tid: member,
                    score: 1.0,
                },
            ],
        };
        assert!(accepted_pruned_rows(pruned, heap, 2).is_none());
        let incomplete = PrunedCandidates {
            complete: false,
            rows: vec![RankedCandidate {
                indexed_tid: root,
                score: 2.0,
            }],
        };
        assert!(accepted_pruned_rows(incomplete, heap, 1).is_none());
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(DISTINCT d.id) FROM hardening_docs d JOIN
               stannum.search('hardening_idx', 'needle', 2, 'none') s ON d.ctid = s.ctid"
            )
            .unwrap(),
            Some(2)
        );
    }

    #[pg_test]
    fn snippet_refetch_keeps_null_but_skips_missing_and_changed_members() {
        fixture();
        Spi::run("UPDATE hardening_docs SET body = NULL WHERE id = 1").unwrap();
        let null_tid = row_tid(1);
        let gone = row_tid(2);
        let changed = row_tid(3);
        Spi::run(
            "DELETE FROM hardening_docs WHERE id = 2;
             UPDATE hardening_docs SET revision = 1 WHERE id = 3;",
        )
        .unwrap();
        assert_ne!(row_tid(3), changed);
        let rows = [null_tid, gone, changed]
            .into_iter()
            .map(|tid| {
                (
                    1.0,
                    VisibleTid {
                        indexed_tid: tid,
                        visible_tid: tid,
                    },
                )
            })
            .collect();
        let pipeline = tokenizer::TokenizerPipelineSpec::default()
            .compile()
            .unwrap();
        let _snapshot = CurrentSnapshot::push();
        let output = fetch_snippet(
            oid("hardening_docs"),
            2,
            &pipeline,
            "needle",
            "html",
            "<b>",
            "</b>",
            rows,
        );
        assert_eq!(output.len(), 1);
        assert_eq!(tid_of(output[0].0), null_tid);
        assert_eq!(output[0].1, 1.0);
        assert_eq!(output[0].2, None);
    }
}
