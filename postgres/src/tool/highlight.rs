// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! `highlight` 5-arg SQL entry points (design §3, §4.1).
//!
//! Wrappers only: the engine body lives in `crate::highlight_udfs`. This
//! module does not import `IndexScorer`, `storage::View`, or `fields`.

use crate::operator::indexed_query;
use pgrx::pg_extern;

/// The field-aware overload (RFC §5.11 highlights / design §4.1): with
/// `field` set the text is that field of a multi-column indexed document;
/// `NULL` is the single-column behavior. No argument takes a default: a
/// defaulted fifth argument would either break PostgreSQL's
/// defaults-after-defaults rule or make shorter calls ambiguous against the
/// four-argument overload. No SUPPORT.
#[pg_extern(name = "highlight", immutable, parallel_safe)]
fn highlight_field(
    text: Option<&str>,
    begin_tag: &str,
    end_tag: &str,
    query: Option<&str>,
    field: Option<&str>,
) -> Option<String> {
    crate::highlight_udfs::highlight_with_field(text, begin_tag, end_tag, query, field)
}

/// The bound field-aware overload: like [`highlight_field`] but analyzed
/// with the bound index's settings. `NULL` field keeps the single-column
/// behavior. No SUPPORT.
#[pg_extern(name = "highlight", stable, parallel_safe)]
fn highlight_bound_field(
    text: Option<&str>,
    begin_tag: &str,
    end_tag: &str,
    query: indexed_query,
    field: Option<&str>,
) -> Option<String> {
    crate::highlight_udfs::highlight_bound_with_field(text, begin_tag, end_tag, query, field)
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn text(sql: &str) -> Option<String> {
        Spi::get_one::<String>(sql).unwrap()
    }

    fn outcome(sql: &str) -> Option<(String, String)> {
        Spi::run(
            "CREATE OR REPLACE FUNCTION pg_temp.outcome(statement text) RETURNS text[]
             LANGUAGE plpgsql AS $$
             DECLARE state text; message text;
             BEGIN
                 EXECUTE statement;
                 RETURN NULL;
             EXCEPTION WHEN OTHERS THEN
                 GET STACKED DIAGNOSTICS state = RETURNED_SQLSTATE, message = MESSAGE_TEXT;
                 RETURN ARRAY[state, message];
             END $$",
        )
        .unwrap();
        Spi::get_one::<Vec<String>>(&format!("SELECT pg_temp.outcome($outcome${sql}$outcome$)"))
            .unwrap()
            .map(|pair| (pair[0].clone(), pair[1].clone()))
    }

    /// Manifest / 0.4.0 recording of the highlight family (catalog.functions).
    /// Extra functions such as `capabilities()` are out of this step's set.
    #[pg_test]
    fn highlight_family_catalog_matches_manifest() {
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
               AND p.proname IN (
                 'highlight', 'highlight_ansi', 'highlight_support')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rows,
            [
                r#"highlight|text text, begin_tag text, end_tag text, query stannum.indexed_query|text text, begin_tag text, end_tag text, query stannum.indexed_query|text|s|s|false|false|NULL|NULL"#,
                r#"highlight|text text, begin_tag text, end_tag text, query stannum.indexed_query, field text|text text, begin_tag text, end_tag text, query stannum.indexed_query, field text|text|s|s|false|false|NULL|NULL"#,
                r#"highlight|text text, begin_tag text, end_tag text, query text|text text, begin_tag text DEFAULT '<b>'::text, end_tag text DEFAULT '</b>'::text, query text DEFAULT NULL::text|text|i|s|false|false|NULL|highlight_support"#,
                r#"highlight|text text, begin_tag text, end_tag text, query text, field text|text text, begin_tag text, end_tag text, query text, field text|text|i|s|false|false|NULL|NULL"#,
                r#"highlight_ansi|text text, wrap_to integer, query stannum.indexed_query|text text, wrap_to integer, query stannum.indexed_query|text|s|s|false|false|NULL|NULL"#,
                r#"highlight_ansi|text text, wrap_to integer, query text|text text, wrap_to integer DEFAULT NULL::integer, query text DEFAULT NULL::text|text|i|s|false|false|NULL|highlight_support"#,
                r#"highlight_support|request internal|request internal|internal|i|u|false|false|NULL|NULL"#,
            ]
        );
    }

    /// Tags, query marking, and a NULL fifth argument leaving the query
    /// unchanged (Appendix A / 0.4.0 single-column).
    #[pg_test]
    fn five_arg_null_field_leaves_query_unchanged() {
        assert_eq!(
            text("SELECT stannum.highlight('Hi there', '<b>', '</b>', 'hi', NULL)"),
            Some("<b>Hi</b> there".into())
        );
        assert_eq!(
            text("SELECT stannum.highlight('Hi there', '[', ']', 'hi')"),
            Some("[Hi] there".into())
        );
        // Named field, unscoped query: still marks (no project_to_field).
        assert_eq!(
            text("SELECT stannum.highlight('alpha pad', '<mark>', '</mark>', 'alpha', 'nope')"),
            Some("<mark>alpha</mark> pad".into())
        );
        assert_eq!(
            text("SELECT stannum.highlight(NULL, '<b>', '</b>', 'a', NULL)"),
            None
        );
        assert_eq!(
            text("SELECT stannum.highlight('x', '<b>', '</b>', NULL, NULL)"),
            Some("x".into())
        );
    }

    #[pg_test]
    fn highlight_ansi_wraps_and_rejects_non_positive_width() {
        let ansi = text("SELECT stannum.highlight_ansi('hi there', NULL, 'hi')").unwrap();
        assert!(ansi.contains("\x1b["), "{ansi}");
        assert!(ansi.contains("hi"), "{ansi}");
        let wrapped =
            text("SELECT stannum.highlight_ansi('one two three four five apple', 10, 'apple')")
                .unwrap();
        assert!(wrapped.contains('\n'), "{wrapped}");
        assert_eq!(
            outcome("SELECT stannum.highlight_ansi('apple', 0, 'apple')"),
            Some(("XX000".into(), "wrap_to must be positive".into()))
        );
        assert_eq!(
            outcome("SELECT stannum.highlight_ansi('apple', -1, 'apple')"),
            Some(("XX000".into(), "wrap_to must be positive".into()))
        );
    }

    #[pg_test]
    fn bound_five_arg_unknown_field_matches_0_4_0_text() {
        Spi::run(
            "CREATE TABLE hl_field (id int PRIMARY KEY, body text);
             INSERT INTO hl_field VALUES (1, 'Hi there');
             CREATE INDEX hl_field_idx ON hl_field USING stannum (body)",
        )
        .unwrap();
        let bound = "stannum.bind_query('hi', 'hl_field_idx'::regclass::oid)";
        assert_eq!(
            text(&format!(
                "SELECT stannum.highlight('Hi there', '<b>', '</b>', {bound}, NULL)"
            )),
            Some("<b>Hi</b> there".into())
        );
        assert_eq!(
            text(&format!(
                "SELECT stannum.highlight('Hi there', '<b>', '</b>', {bound})"
            )),
            Some("<b>Hi</b> there".into())
        );
        assert_eq!(
            outcome(&format!(
                "SELECT stannum.highlight('Hi there', '<b>', '</b>', {bound}, 'nope')"
            )),
            Some((
                "XX000".into(),
                "stannum.highlight(): unknown field 'nope'".into()
            ))
        );
        assert_eq!(
            outcome(&format!(
                "SELECT stannum.highlight('Hi there', '<b>', '</b>', {bound}, 'body')"
            )),
            Some((
                "XX000".into(),
                "stannum.highlight(): unknown field 'body'".into()
            ))
        );
    }

    #[pg_test]
    fn column_field_name_returns_none_for_non_positive_attnum() {
        use crate::highlight_udfs::field_name_for_attnum;
        let names = ["title".to_string(), "body".to_string()];
        assert_eq!(field_name_for_attnum(0, Some(&names)), None);
        assert_eq!(field_name_for_attnum(-1, Some(&names)), None);
        assert_eq!(field_name_for_attnum(1, Some(&names)), None);
    }
}
