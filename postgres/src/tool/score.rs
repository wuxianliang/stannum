// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Score-family surface and Appendix A pg_tests (execution plan 2.3).
//!
//! The `#[pg_extern]` bodies stay in `crate::score` (owned shim). This
//! module does not import `IndexScorer`, `storage::View`, or `fields`.

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn text(sql: &str) -> String {
        Spi::get_one::<String>(sql).unwrap().unwrap()
    }

    fn force_index_scan() {
        // score/full_score are SUPPORT-rewritten; a seq scan hits the stub
        // and errors "cannot be used in this query context".
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
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

    /// Manifest / 0.4.0 recording of the score family (catalog.functions +
    /// catalog.acls rows). Extra functions such as `capabilities()` are out
    /// of this step's identity set.
    #[pg_test]
    fn score_family_catalog_matches_manifest() {
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
                 'full_score', 'max_score', 'score', 'score_bound',
                 'score_bound_indexed', 'score_inspect', 'score_support')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            rows,
            [
                "full_score|ctid tid|ctid tid|real|i|u|true|false|NULL|score_support",
                "full_score|ctid tid, k1 real, b real|ctid tid, k1 real, b real|real|i|u|false|false|NULL|score_support",
                "max_score|ctid tid|ctid tid|real|i|u|true|false|NULL|score_support",
                "score|ctid tid, dense_ratio real, k1 real, b real, term_add text[], term_replace text[]|ctid tid, dense_ratio real DEFAULT 0.10, k1 real DEFAULT NULL::real, b real DEFAULT NULL::real, term_add text[] DEFAULT NULL::text[], term_replace text[] DEFAULT NULL::text[]|real|i|u|false|false|NULL|score_support",
                "score_bound|document text, query text, heap_oid integer, index_oid integer, mode integer, dense_ratio real, k1 real, b real, term_add text[], term_replace text[]|document text, query text, heap_oid integer, index_oid integer, mode integer, dense_ratio real, k1 real, b real, term_add text[], term_replace text[]|real|v|u|false|false|{<owner>=X/<owner>}|NULL",
                "score_bound_indexed|ctid tid, query text, heap_oid integer, index_oid integer, mode integer, dense_ratio real, k1 real, b real, term_add text[], term_replace text[]|ctid tid, query text, heap_oid integer, index_oid integer, mode integer, dense_ratio real, k1 real, b real, term_add text[], term_replace text[]|real|v|u|false|false|{<owner>=X/<owner>}|NULL",
                "score_inspect|index regclass, query text, dense_ratio real, term_add text[], term_replace text[]|index regclass, query text, dense_ratio real DEFAULT 0.10, term_add text[] DEFAULT NULL::text[], term_replace text[] DEFAULT NULL::text[]|TABLE(term text, weight real)|v|u|false|false|NULL|NULL",
                "score_support|request internal|request internal|internal|i|u|false|false|NULL|NULL",
            ]
        );
        let public_execute = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(format('%s=%s', p.proname, has_function_privilege('public', p.oid, 'EXECUTE')::text)
             ORDER BY p.proname, pg_get_function_identity_arguments(p.oid))
             FROM pg_proc p
             JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname = 'stannum'
               AND p.proname IN (
                 'full_score', 'max_score', 'score', 'score_bound',
                 'score_bound_indexed', 'score_inspect', 'score_support')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            public_execute,
            [
                "full_score=true",
                "full_score=true",
                "max_score=true",
                "score=true",
                "score_bound=false",
                "score_bound_indexed=false",
                "score_inspect=true",
                "score_support=true",
            ]
        );
    }

    /// arithmetic.row3 bits via `==>` + `full_score` (search() is step 2.4).
    /// Dense elision would zero these; `search` uses the full program.
    #[pg_test]
    fn arithmetic_row3_full_score_bits_match_0_4_0() {
        Spi::run(
            "CREATE TABLE row3 (id int PRIMARY KEY, body text);
             INSERT INTO row3 VALUES (1, 'needle'), (2, 'needle needle'), (3, 'pad');
             CREATE INDEX row3_idx ON row3 USING stannum (body)",
        )
        .unwrap();
        force_index_scan();
        let ranked = text(
            "SELECT coalesce(json_agg(json_build_array(id, encode(float4send(score), 'hex'))
             ORDER BY score DESC, ctid), '[]')::text
             FROM (SELECT id, ctid, stannum.full_score(ctid) AS score
                   FROM row3 WHERE body ==> 'needle') s",
        );
        assert_eq!(ranked, r#"[[2, "3f110b5e"], [1, "3f060744"]]"#);
    }

    #[pg_test]
    fn term_add_and_term_replace_follow_appendix_a() {
        Spi::run(
            "CREATE TABLE edits (id int PRIMARY KEY, body text);
             INSERT INTO edits VALUES (1, 'alpha gamma'), (2, 'alpha');
             INSERT INTO edits SELECT 100 + n, 'pad' || n FROM generate_series(1, 20) n;
             CREATE INDEX edits_idx ON edits USING stannum (body)",
        )
        .unwrap();
        force_index_scan();
        let inspect = |sql: &str| text(sql);
        assert_eq!(
            inspect(
                "SELECT coalesce(json_agg(json_build_array(term, encode(float4send(weight), 'hex'))
                 ORDER BY term), '[]')::text
                 FROM stannum.score_inspect('edits_idx', 'alpha^2.0',
                     term_add => ARRAY['alpha', 'Gamma', 'Gamma'])"
            ),
            r#"[["alpha", "40000000"], ["gamma", "3f800000"]]"#
        );
        assert_eq!(
            inspect(
                "SELECT coalesce(json_agg(json_build_array(term, encode(float4send(weight), 'hex'))
                 ORDER BY term), '[]')::text
                 FROM stannum.score_inspect('edits_idx', 'alpha',
                     term_replace => ARRAY['Gamma'])"
            ),
            r#"[["gamma", "3f800000"]]"#
        );
        assert_eq!(
            outcome(
                "SELECT stannum.score(ctid, term_add => ARRAY['gamma'], term_replace => ARRAY['gamma'])
                 FROM edits WHERE body ==> 'alpha' LIMIT 1"
            ),
            Some((
                "XX000".into(),
                "stannum.score(): term_add and term_replace cannot both be non-NULL".into()
            ))
        );
        assert_eq!(
            outcome(
                "SELECT * FROM stannum.score_inspect('edits_idx', 'alpha',
                     term_add => ARRAY['gamma'], term_replace => ARRAY['gamma'])"
            ),
            Some((
                "XX000".into(),
                "stannum.score_inspect(): term_add and term_replace cannot both be non-NULL".into()
            ))
        );
        let added = Spi::get_one::<Vec<f32>>(
            "SELECT array_agg(stannum.score(ctid, term_add => ARRAY['Gamma']) ORDER BY id)
             FROM edits WHERE body ==> 'alpha' AND id <= 2",
        )
        .unwrap()
        .unwrap();
        let added_again = Spi::get_one::<Vec<f32>>(
            "SELECT array_agg(stannum.score(ctid, term_add => ARRAY['Gamma', 'gamma']) ORDER BY id)
             FROM edits WHERE body ==> 'alpha' AND id <= 2",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            added.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            added_again.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        );
    }

    #[pg_test]
    fn stop_words_filter_only_compile_scoring_terms() {
        Spi::run(
            "CREATE TABLE stops (id int PRIMARY KEY, body text);
             INSERT INTO stops VALUES (1, 'the needle'), (2, 'needle');
             INSERT INTO stops SELECT 100 + n, 'pad' || n FROM generate_series(1, 20) n;
             CREATE INDEX stops_idx ON stops USING stannum (body)
                 WITH (score_stop_words = 'the')",
        )
        .unwrap();
        force_index_scan();
        let matched = Spi::get_one::<Vec<i32>>(
            "SELECT array_agg(id ORDER BY id) FROM stops WHERE body ==> 'the'",
        )
        .unwrap();
        assert_eq!(matched, Some(vec![1]));
        assert_eq!(
            text(
                "SELECT coalesce(json_agg(term ORDER BY term), '[]')::text
                 FROM stannum.score_inspect('stops_idx', 'the OR needle')"
            ),
            r#"["needle"]"#
        );
        let scores = Spi::get_two::<f32, f32>(
            "SELECT stannum.score(ctid), stannum.full_score(ctid)
             FROM stops WHERE body ==> 'the'",
        )
        .unwrap();
        assert_eq!(scores.0.unwrap().to_bits(), 0);
        assert!(
            scores.1.unwrap() > 0.0,
            "full_score must ignore the stop-word list, got {:?}",
            scores.1
        );
    }

    #[pg_test]
    fn score_inspect_is_volatile_table_and_rejects_null_elements() {
        Spi::run(
            "CREATE TABLE inspect_docs (id int PRIMARY KEY, body text);
             INSERT INTO inspect_docs VALUES (1, 'common rare'), (2, 'common'), (3, 'common');
             CREATE INDEX inspect_idx ON inspect_docs USING stannum (body)",
        )
        .unwrap();
        assert_eq!(
            text(
                "SELECT pg_get_function_result(p.oid)
                 FROM pg_proc p
                 JOIN pg_namespace n ON n.oid = p.pronamespace
                 WHERE n.nspname = 'stannum' AND p.proname = 'score_inspect'"
            ),
            "TABLE(term text, weight real)"
        );
        let flags = Spi::get_one::<Vec<String>>(
            "SELECT ARRAY[p.provolatile::text, p.proparallel::text, p.proisstrict::text]
             FROM pg_proc p
             JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname = 'stannum' AND p.proname = 'score_inspect'",
        )
        .unwrap()
        .unwrap();
        assert_eq!(flags, ["v", "u", "false"]);
        assert_eq!(
            text(
                "SELECT coalesce(json_agg(json_build_array(term, encode(float4send(weight), 'hex'))
                 ORDER BY term), '[]')::text
                 FROM stannum.score_inspect('inspect_idx', 'common OR rare', dense_ratio => 0.5)"
            ),
            r#"[["rare", "3f800000"]]"#
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM stannum.score_inspect(NULL, 'rare')")
                .unwrap(),
            Some(0)
        );
        assert_eq!(
            outcome(
                "SELECT * FROM stannum.score_inspect('inspect_idx', 'rare',
                     term_add => ARRAY[NULL]::text[])"
            ),
            Some((
                "XX000".into(),
                "stannum.score_inspect() term_add array elements must not be NULL".into()
            ))
        );
    }
}
