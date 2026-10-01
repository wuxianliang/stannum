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

    /// arithmetic.row3 bits via `stannum.search`, matching
    /// `contract/cases/arithmetic.yaml` (`encode(float4send(s.score::real),'hex')`,
    /// ORDER BY score DESC, ctid). `full_score` is pinned in `tool/score.rs`.
    #[pg_test]
    fn arithmetic_row3_search_bits_match_0_4_0() {
        Spi::run(
            "CREATE TABLE row3_search (id int PRIMARY KEY, body text);
             INSERT INTO row3_search VALUES (1, 'needle'), (2, 'needle needle'), (3, 'pad');
             CREATE INDEX row3_search_idx ON row3_search USING stannum (body)",
        )
        .unwrap();
        let ranked = Spi::get_one::<String>(
            r#"SELECT coalesce(json_agg(json_build_array(d.id, encode(float4send(s.score::real), 'hex'))
                 ORDER BY s.score DESC, d.ctid), '[]')::text
               FROM row3_search d
               JOIN stannum.search('row3_search_idx'::regclass, 'needle', "limit" => 10, snippet => 'none') s
                 ON d.ctid = s.ctid"#,
        )
        .unwrap()
        .unwrap();
        let recorded = r#"[[2, "3f110b5e"], [1, "3f060744"]]"#;
        eprintln!("arithmetic.row3_single_column computed {ranked} vs recorded {recorded}");
        assert_eq!(ranked, recorded, "computed {ranked} vs recorded {recorded}");
    }
}
