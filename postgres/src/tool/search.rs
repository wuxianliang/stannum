// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! `search` / `search_count` SQL entry points (design §3, §4.1).
//!
//! Wrappers only: the engine body lives in `crate::search`. This module does
//! not import `IndexScorer`, `storage::View`, or `fields`.

use pgrx::iter::TableIterator;
use pgrx::{PgRelation, default, name, pg_extern, pg_sys};

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
    crate::search::search(index, query, limit, snippet, begin_tag, end_tag, k1, b)
}

#[pg_extern(volatile, parallel_unsafe)]
fn search_count(index: PgRelation, query: Option<&str>) -> i64 {
    crate::search::search_count(index, query)
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    /// Manifest / 0.4.0 recording of search and search_count (catalog.functions).
    #[pg_test]
    fn search_family_catalog_matches_manifest() {
        let rows = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(format(
                 '%s|%s|%s|%s|%s|%s|%s|%s|%s|%s',
                 p.proname,
                 pg_get_function_identity_arguments(p.oid),
                 pg_get_function_arguments(p.oid),
                 pg_get_function_result(p.oid),
                 p.provolatile,
                 p.proparallel,
                 p.proisstrict::text,
                 p.prosecdef::text,
                 coalesce(replace(p.proacl::text, current_user, '<owner>'), 'NULL'),
                 coalesce(s.proname, 'NULL')
             ) ORDER BY p.proname, pg_get_function_identity_arguments(p.oid))
             FROM pg_proc p
             JOIN pg_namespace n ON n.oid = p.pronamespace
             LEFT JOIN pg_proc s ON s.oid = p.prosupport
             WHERE n.nspname = 'stannum'
               AND p.proname IN ('search', 'search_count')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rows,
            [
                r#"search|index regclass, query text, "limit" integer, snippet text, begin_tag text, end_tag text, k1 real, b real|index regclass, query text, "limit" integer DEFAULT 10, snippet text DEFAULT 'html'::text, begin_tag text DEFAULT '<mark>'::text, end_tag text DEFAULT '</mark>'::text, k1 real DEFAULT NULL::real, b real DEFAULT NULL::real|TABLE(ctid tid, score real, snippet text)|v|u|false|false|NULL|NULL"#,
                "search_count|index regclass, query text|index regclass, query text|bigint|v|u|false|false|NULL|NULL",
            ]
        );
    }
}
