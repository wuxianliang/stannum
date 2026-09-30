// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Search-family surface pg_tests (execution plan 2.4).
//!
//! The `#[pg_extern]` bodies stay in `crate::search`. This module does not
//! import `IndexScorer`, `storage::View`, or `fields`.

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
