// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! L2 SQL entry points for diagnostics, TIN helpers, and stop-word presets
//! (design sections 3 and 4.1). Thin `#[pg_extern]` wrappers; engine bodies
//! live in `crate::udfs`, `crate::dict`, and `crate::stopwords`. This module
//! does not import `IndexScorer` or `storage` internals.

use pgrx::iter::{SetOfIterator, TableIterator};
use pgrx::{PgRelation, default, name, pg_extern};

#[pg_extern(stable, parallel_unsafe)]
#[expect(clippy::too_many_arguments, reason = "TIN-compatible SQL signature")]
fn tokenize<'a>(
    text: Option<&'a str>,
    tokenizer: default!(&str, "'unicode'"),
    case_folding: default!(&str, "'fold'"),
    accent_folding: default!(&str, "'fold'"),
    long_tokens: default!(&str, "'split'"),
    max_token_bytes: default!(i32, 256),
    graphemes: default!(&str, "'emoji'"),
    position_gaps: default!(&str, "'preserve'"),
) -> SetOfIterator<'a, String> {
    crate::udfs::tokenize(
        text,
        tokenizer,
        case_folding,
        accent_folding,
        long_tokens,
        max_token_bytes,
        graphemes,
        position_gaps,
    )
}

#[pg_extern(stable, parallel_unsafe)]
#[expect(clippy::too_many_arguments, reason = "TIN-compatible SQL signature")]
fn ql_parse(
    query: Option<&str>,
    surface: default!(bool, true),
    tokenizer: default!(&str, "'unicode'"),
    case_folding: default!(&str, "'fold'"),
    accent_folding: default!(&str, "'fold'"),
    long_tokens: default!(&str, "'split'"),
    max_token_bytes: default!(i32, 256),
    graphemes: default!(&str, "'emoji'"),
    position_gaps: default!(&str, "'preserve'"),
) -> Option<String> {
    crate::udfs::ql_parse(
        query,
        surface,
        tokenizer,
        case_folding,
        accent_folding,
        long_tokens,
        max_token_bytes,
        graphemes,
        position_gaps,
    )
}

#[pg_extern(immutable, parallel_safe)]
fn builtin_stop_words(preset: &str) -> SetOfIterator<'static, &'static str> {
    SetOfIterator::new(crate::stopwords::words(preset))
}

/// One health row per stannum index: the aggregates `segment_info`
/// streams per source, plus the dictionary page coverage and the analysis
/// identity. `VOLATILE PARALLEL UNSAFE` matches `segment_info`; the SQL
/// (function, column comment and the `index_health` view) is pinned in the
/// `sql` attribute so fresh installs and upgrade scripts stay identical.
#[pg_extern(
    volatile,
    parallel_unsafe,
    sql = "
    CREATE FUNCTION @extschema@.index_stats(\"index\" regclass)
    RETURNS TABLE (documents bigint, dead_documents bigint, dead_ratio float8,
        segments int, immutable_segments int, mutable_segments int,
        next_generation bigint, total_pages bigint, dictionary_pages bigint,
        total_length bigint, average_length float8,
        analysis_matches bool, analysis_detail text)
    STRICT VOLATILE PARALLEL UNSAFE
    LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';
    CREATE VIEW @extschema@.index_health WITH (security_invoker = true) AS
    SELECT c.oid::regclass AS index, s.* FROM pg_class c
    CROSS JOIN LATERAL @extschema@.index_stats(c.oid) s
    WHERE c.relkind = 'i' AND c.relam = (SELECT oid FROM pg_am WHERE amname = 'stannum');
    COMMENT ON COLUMN @extschema@.index_health.dictionary_pages IS 'Pages intersected by every immutable segment''s dictionary extents, whether or not this backend ever read them. EXPLAIN''s Dictionary Pages Read counts only the pages a given scan actually pinned; the two differ by design and agree only for a fully-scanned index.';
"
)]
#[allow(clippy::type_complexity)]
fn index_stats(
    index: PgRelation,
) -> TableIterator<
    'static,
    (
        name!(documents, i64),
        name!(dead_documents, i64),
        name!(dead_ratio, f64),
        name!(segments, i32),
        name!(immutable_segments, i32),
        name!(mutable_segments, i32),
        name!(next_generation, i64),
        name!(total_pages, i64),
        name!(dictionary_pages, i64),
        name!(total_length, i64),
        name!(average_length, f64),
        name!(analysis_matches, Option<bool>),
        name!(analysis_detail, Option<String>),
    ),
> {
    crate::udfs::index_stats(index)
}

#[pg_extern(stable, parallel_unsafe, strict)]
#[allow(clippy::type_complexity)] // the SRF row shape is fixed public SQL surface
fn index_analysis(
    index: PgRelation,
) -> TableIterator<
    'static,
    (
        name!(index_name, String),
        name!(recorded_jieba_version, Option<i32>),
        name!(recorded_dict_fingerprint, Option<i64>),
        name!(runtime_jieba_version, Option<i32>),
        name!(runtime_dict_fingerprint, Option<i64>),
        name!(matches, Option<bool>),
        name!(status, String),
    ),
> {
    crate::dict::index_analysis(index)
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn tokenize_and_ql_parse_are_stable_parallel_unsafe() {
        let rows = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(format(
                 '%s|%s|%s|%s',
                 p.proname,
                 p.provolatile,
                 p.proparallel,
                 p.proisstrict::text
             ) ORDER BY p.proname, pg_get_function_identity_arguments(p.oid))
             FROM pg_proc p
             JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname = 'stannum'
               AND p.proname IN ('tokenize', 'ql_parse')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(rows, ["ql_parse|s|u|false", "tokenize|s|u|false",]);
    }

    #[pg_test]
    fn builtin_stop_words_catalog_and_auto_en_zh() {
        let row = Spi::get_one::<String>(
            "SELECT format('%s|%s|%s|%s|%s',
                 p.proname,
                 pg_get_function_identity_arguments(p.oid),
                 p.provolatile,
                 p.proparallel,
                 p.proisstrict::text)
             FROM pg_proc p
             JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname = 'stannum' AND p.proname = 'builtin_stop_words'",
        )
        .unwrap()
        .unwrap();
        assert_eq!(row, "builtin_stop_words|preset text|i|s|true");
        let count = Spi::get_one::<i64>("SELECT count(*) FROM stannum.builtin_stop_words('auto')")
            .unwrap()
            .unwrap();
        assert!(count > 200, "auto preset too small: {count}");
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT 'the' IN (SELECT * FROM stannum.builtin_stop_words('en'))"
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT '的' IN (SELECT * FROM stannum.builtin_stop_words('zh'))")
                .unwrap(),
            Some(true)
        );
    }

    #[pg_test]
    fn index_stats_reports_zero_average_and_null_analysis_for_unicode() {
        Spi::run(
            "CREATE TABLE diag_stats(body text);
             INSERT INTO diag_stats VALUES ('needle'), ('pad');
             CREATE INDEX diag_stats_idx ON diag_stats USING stannum(body);",
        )
        .unwrap();
        let row = Spi::get_one::<String>(
            "SELECT format('%s|%s|%s|%s|%s',
                 documents, dead_documents, average_length::text,
                 coalesce(analysis_matches::text, 'NULL'),
                 coalesce(analysis_detail, 'NULL'))
             FROM stannum.index_stats('diag_stats_idx')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(row, "2|0|0|NULL|NULL");
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT relrowsecurity IS NOT TRUE AND relkind = 'v'
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = 'stannum' AND c.relname = 'index_health'"
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT reloptions::text LIKE '%security_invoker=true%'
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = 'stannum' AND c.relname = 'index_health'"
            )
            .unwrap(),
            Some(true)
        );
    }

    #[pg_test]
    fn index_analysis_is_not_applicable_for_unicode() {
        Spi::run(
            "CREATE TABLE diag_analysis(body text);
             INSERT INTO diag_analysis VALUES ('needle');
             CREATE INDEX diag_analysis_idx ON diag_analysis USING stannum(body);",
        )
        .unwrap();
        let status = Spi::get_one::<String>(
            "SELECT status FROM stannum.index_analysis('diag_analysis_idx')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(status, "not applicable");
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT matches IS NULL FROM stannum.index_analysis('diag_analysis_idx')"
            )
            .unwrap(),
            Some(true)
        );
    }

    #[pg_test]
    fn jieba_words_is_empty_and_revoked_from_public() {
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM stannum.jieba_words").unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT NOT has_table_privilege('public', 'stannum.jieba_words', 'SELECT')"
            )
            .unwrap(),
            Some(true)
        );
    }
}
