#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Jieba tokenizer symmetry on a private cluster (STN3 / unittest).

Ported from pgembed's pytest fixtures onto the search_srf.py cluster pattern.
Two single-column indexes on two columns of one table; no multi-column AM.
Unittest discovery during `script/test-all quick` skips the cluster; the
cluster step runs this file as a script.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

PG_PORT = "28959"

DOCS_SQL = """
CREATE TABLE docs (id int PRIMARY KEY, body_jieba text, body_default text);
INSERT INTO docs VALUES
 (1, 'PostgreSQL supports full text search',
     'PostgreSQL supports full text search'),
 (2, 'PostgreSQL 是一个强大的开源数据库',
     'PostgreSQL 是一个强大的开源数据库'),
 (3, 'PostgreSQL数据库内核与查询优化',
     'PostgreSQL数据库内核与查询优化'),
 (4, '中文分词 让数 据库更懂中文',
     '中文分词 让数 据库更懂中文'),
 (5, 'Database kernel and query optimization',
     'Database kernel and query optimization');
CREATE INDEX docs_jieba ON docs USING stannum (body_jieba)
  WITH (tokenizer = 'jieba');
CREATE INDEX docs_default ON docs USING stannum (body_default);
ANALYZE docs;
"""


def command(args, *, env=None, input=None, check=True):
    result = subprocess.run(args, input=input, text=True, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if check and result.returncode:
        raise RuntimeError(f"{' '.join(map(str, args))} failed:\n{result.stdout}")
    return result


class JiebaTokenizerTests(unittest.TestCase):
    env = None
    data = None
    root = None
    started = False
    directory = None

    @classmethod
    def setUpClass(cls):
        if __name__ != "__main__":
            raise unittest.SkipTest(
                "private cluster; run via python3 postgres/tests/test_stannum_jieba.py"
            )
        cls.directory = tempfile.TemporaryDirectory(prefix="stannum-jieba-")
        cls.root = Path(cls.directory.name)
        cls.data = cls.root / "data"
        cls.env = dict(os.environ, PGHOST=str(cls.root), PGPORT=PG_PORT,
                       PGUSER="postgres", PGDATABASE="postgres")
        for key in ("PGSERVICE", "PGSERVICEFILE", "PGPASSWORD", "PGOPTIONS"):
            cls.env.pop(key, None)
        command(["initdb", "-D", str(cls.data), "-U", "postgres", "-A", "trust",
                 "--no-locale", "--encoding=UTF8"])
        command(["pg_ctl", "-D", str(cls.data), "-l", str(cls.root / "server.log"),
                 "-w", "-o",
                 f"-p {PG_PORT} -c listen_addresses= -c unix_socket_directories={cls.root}",
                 "start"])
        cls.started = True
        cls.sql("CREATE EXTENSION stannum;")

    @classmethod
    def tearDownClass(cls):
        if cls.started:
            command(["pg_ctl", "-D", str(cls.data), "-m", "immediate", "-w", "stop"],
                    check=False)
            cls.started = False
        if cls.directory is not None:
            cls.directory.cleanup()
            cls.directory = None

    @classmethod
    def sql(cls, text):
        return command(["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"],
                       env=cls.env, input=text).stdout.strip()

    def setUp(self):
        # Function-scoped isolation, matching the pgembed pytest fixture.
        self.sql("DROP TABLE IF EXISTS docs CASCADE;")
        self.sql("DELETE FROM stannum.jieba_words;")
        self.sql("SELECT stannum.jieba_reload_dict();")
        self.sql(DOCS_SQL)

    def matching_ids(self, column, query):
        return self.sql(
            f"SELECT string_agg(id::text, ',' ORDER BY id) FROM docs"
            f" WHERE {column} ==> '{query}';"
        )

    def test_tokenize_udf_accepts_jieba(self):
        self.assertEqual(
            self.sql("""
                SELECT string_agg(tok, '/') FROM stannum.tokenize(
                    'PostgreSQL 是开源数据库', tokenizer => 'jieba') AS t(tok);
            """),
            "postgresql/是/开源/数据库",
        )

    def test_jieba_matches_chinese_words(self):
        self.assertEqual(self.matching_ids("body_jieba", "数据库"), "2,3")

    def test_jieba_respects_word_boundaries(self):
        self.assertNotIn("4", self.matching_ids("body_jieba", "数据库").split(","))
        self.assertEqual(self.matching_ids("body_default", "数据库"), "2,3,4")

    def test_jieba_word_sequence_queries_rewrite_to_phrases(self):
        self.assertEqual(self.matching_ids("body_jieba", "开源数据库"), "2")

    def test_jieba_english_queries_and_case_folding(self):
        self.assertEqual(self.matching_ids("body_jieba", "search"), "1")
        self.assertEqual(self.matching_ids("body_jieba", "postgresql"), "1,2,3")
        self.assertEqual(self.matching_ids("body_jieba", "PostgreSQL数据库"), "3")

    def test_jieba_highlight_marks_whole_words(self):
        highlighted = self.sql("""
            SELECT stannum.highlight(body_jieba, '<mark>', '</mark>',
                                     query => '数据库')
            FROM docs WHERE id = 2;
        """)
        self.assertIn("<mark>数据库</mark>", highlighted)

    def test_jieba_full_score_ranks_matches(self):
        scored = self.sql("""
            SELECT id::text || '|' || stannum.full_score(ctid)::text FROM docs
            WHERE body_jieba ==> '数据库' ORDER BY stannum.full_score(ctid) DESC;
        """).splitlines()
        ids = {line.split("|", 1)[0] for line in scored if line}
        self.assertEqual(ids, {"2", "3"})
        for line in scored:
            if line:
                self.assertGreater(float(line.split("|", 1)[1]), 0.0)

    def test_dictionary_drift_reindex_and_presets(self):
        self.assertEqual(
            self.sql("SELECT extversion FROM pg_extension WHERE extname='stannum';"),
            "0.5.0",
        )
        self.assertEqual(
            self.sql("SELECT matches FROM stannum.index_analysis('docs_jieba');"),
            "t",
        )
        before = self.sql("SELECT stannum.jieba_dict_version();")
        self.sql("SELECT stannum.jieba_add_word('星河数据库协议', 1000000, 'n');")
        self.assertNotEqual(self.sql("SELECT stannum.jieba_dict_version();"), before)
        self.sql("SELECT stannum.jieba_reload_dict();")
        self.assertEqual(
            self.sql("""
                SELECT string_agg(tok, '/') FROM stannum.tokenize(
                    '星河数据库协议', tokenizer => 'jieba') AS t(tok);
            """),
            "星河数据库协议",
        )
        self.assertEqual(
            self.sql("SELECT matches FROM stannum.index_analysis('docs_jieba');"),
            "f",
        )
        self.assertEqual(
            self.sql("""
                SELECT matches IS NULL AND status = 'not applicable'
                FROM stannum.index_analysis('docs_default');
            """),
            "t",
        )
        self.sql("REINDEX INDEX docs_jieba;")
        self.assertEqual(
            self.sql("SELECT matches FROM stannum.index_analysis('docs_jieba');"),
            "t",
        )
        self.assertEqual(
            self.sql("""
                SELECT count(*) BETWEEN 150 AND 200
                FROM stannum.builtin_stop_words('zh');
            """),
            "t",
        )
        self.sql("""
            ALTER INDEX docs_jieba SET (score_stop_words = 'auto:zh');
            INSERT INTO docs VALUES (6, '的 数据库', '的 数据库');
        """)
        self.assertEqual(
            self.sql("""
                SELECT count(*) FROM stannum.score_inspect('docs_jieba', '的', 1.1);
            """),
            "0",
        )
        self.assertEqual(
            self.sql("""
                SELECT bool_and(stannum.score(ctid, dense_ratio => 1.1) = 0
                                AND stannum.full_score(ctid) > 0)
                FROM docs WHERE body_jieba ==> '的';
            """),
            "t",
        )
        self.sql("SELECT stannum.jieba_delete_word('星河数据库协议');")
        self.assertEqual(self.sql("SELECT stannum.jieba_dict_version();"), before)

    def test_abort_and_savepoint_discard_uncommitted_dictionary_on_next_use(self):
        baseline = self.sql("""
            SELECT string_agg(tok, '/') FROM stannum.tokenize(
                '星河数据库协议', tokenizer => 'jieba') AS t(tok);
        """)
        self.assertNotEqual(baseline, "星河数据库协议")
        abort = [
            line for line in self.sql("""
                BEGIN;
                SELECT stannum.jieba_add_word('星河数据库协议', 1000000, 'n');
                SELECT string_agg(tok, '/') FROM stannum.tokenize(
                    '星河数据库协议', tokenizer => 'jieba') AS t(tok);
                ROLLBACK;
                SELECT string_agg(tok, '/') FROM stannum.tokenize(
                    '星河数据库协议', tokenizer => 'jieba') AS t(tok);
            """).splitlines()
            if line
        ]
        self.assertEqual(abort, ["星河数据库协议", baseline])
        savepoint = [
            line for line in self.sql("""
                BEGIN;
                SELECT stannum.jieba_add_word('星河数据库协议', 1000000, 'n');
                SAVEPOINT d;
                SELECT stannum.jieba_delete_word('星河数据库协议');
                SELECT string_agg(tok, '/') FROM stannum.tokenize(
                    '星河数据库协议', tokenizer => 'jieba') AS t(tok);
                ROLLBACK TO SAVEPOINT d;
                SELECT string_agg(tok, '/') FROM stannum.tokenize(
                    '星河数据库协议', tokenizer => 'jieba') AS t(tok);
                COMMIT;
            """).splitlines()
            if line
        ]
        self.assertEqual(savepoint, [baseline, "星河数据库协议"])
        self.assertEqual(
            self.sql("""
                SELECT string_agg(tok, '/') FROM stannum.tokenize(
                    '星河数据库协议', tokenizer => 'jieba') AS t(tok);
            """),
            "星河数据库协议",
        )
        self.sql("SELECT stannum.jieba_reload_dict();")
        self.assertEqual(
            self.sql("""
                SELECT string_agg(tok, '/') FROM stannum.tokenize(
                    '星河数据库协议', tokenizer => 'jieba') AS t(tok);
            """),
            "星河数据库协议",
        )

    def test_jieba_udf_sql_identities_match_recordings(self):
        self.assertEqual(
            self.sql("""
                SELECT string_agg(format('%s|%s|%s|%s|%s',
                       p.proname,
                       pg_get_function_identity_arguments(p.oid),
                       p.provolatile,
                       p.proparallel,
                       p.proisstrict::text), E'\\n' ORDER BY p.proname)
                FROM pg_proc p
                JOIN pg_namespace n ON n.oid = p.pronamespace
                WHERE n.nspname = 'stannum'
                  AND p.proname IN (
                      'jieba_add_word',
                      'jieba_delete_word',
                      'jieba_dict_version',
                      'jieba_reload_dict'
                  );
            """),
            "\n".join([
                "jieba_add_word|word text, freq integer, tag text|v|u|false",
                "jieba_delete_word|word text|v|u|true",
                "jieba_dict_version||s|u|true",
                "jieba_reload_dict||v|u|true",
            ]),
        )

    def test_span_symmetry_after_dictionary_add(self):
        text = "星河数据库协议与开源数据库"
        self.sql("SELECT stannum.jieba_add_word('星河数据库协议', 1000000, 'n');")
        tokens = self.sql(f"""
            SELECT string_agg(tok, '/') FROM stannum.tokenize(
                '{text}', tokenizer => 'jieba') AS t(tok);
        """)
        self.assertTrue(tokens.startswith("星河数据库协议/"), tokens)
        highlighted = self.sql(f"""
            INSERT INTO docs VALUES (99, '{text}', '{text}');
            SELECT stannum.highlight(body_jieba, '<m>', '</m>',
                                     query => '星河数据库协议')
            FROM docs WHERE id = 99;
        """)
        self.assertIn("<m>星河数据库协议</m>", highlighted)
        token_set = tokens.split("/")
        self.assertIn("星河数据库协议", token_set)
        self.assertIn("开源", token_set)
        self.assertIn("数据库", token_set)


if __name__ == "__main__":
    unittest.main()
