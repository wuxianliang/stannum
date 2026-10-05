#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Post-rebuild definition preservation and interrupted REINDEX INDEX.

A multi-column index cannot carry an expression key (storage/mod.rs rejects
`attnum <= 0` when `key_count >= 2`). Two indexes on one heap cover the
guarantee together:

- docs_expr_idx: `stannum((lower(body))) WHERE published`, whitespace+
  preserve folding so the expression is not redundant with analysis.
- docs_idx: `stannum(title, body)` with `field_weights` and jieba.

Interrupted REINDEX uses a separate bulk table sized so the rebuild has a
real window. Modeled on merge_lifecycle.py's timed kill. Install a release
build first; run under script/pgrx-lock.py on a shared machine.
"""
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import time

PAGE_SIZE = 8192
PAGE_HEADER = 24
SPECIAL_SIZE = 8
MAGIC = 0x4C445032
VERSION = 2
KIND_ENVELOPE = 5
SPEC_BYTES = 8
STNM_MAGIC = b"STNM"
ENTRY_BYTES = 68
PENDING_BYTES = 20
META_HEADER = 8 + SPEC_BYTES + 28 + 12
RETRY_BOUND = 5


def main():
    root = Path(tempfile.mkdtemp(prefix="stannum-rebuild-"))
    data = root / "data"
    env = dict(os.environ, PGHOST=str(root), PGPORT="28932", PGUSER="postgres",
               PGDATABASE="postgres",
               PGOPTIONS="-c enable_seqscan=off -c statement_timeout=120000")
    for name in ("PGSERVICE", "PGSERVICEFILE", "PGPASSWORD"):
        env.pop(name, None)

    def command(args, **kwargs):
        try:
            return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kwargs)
        except subprocess.CalledProcessError as error:
            raise AssertionError(error.output) from error

    def sql(query):
        return command(["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"], input=query, env=env).strip()

    def start():
        command(["pg_ctl", "-D", str(data), "-l", str(root / "server.log"), "-w", "start"])

    started = False
    try:
        command(["initdb", "-D", str(data), "-U", "postgres", "-A", "trust",
                 "--no-locale", "--encoding=UTF8", "--data-checksums"])
        with (data / "postgresql.conf").open("a") as handle:
            handle.write(f"\nlisten_addresses=''\nport=28932\nunix_socket_directories='{root}'\n"
                         "shared_buffers='64MB'\nautovacuum=off\nshared_preload_libraries=''\n")
        start()
        started = True
        sql("CREATE EXTENSION stannum;")
        procs = sql("SELECT count(*) FROM pg_proc WHERE pronamespace = 'stannum'::regnamespace")
        assert procs == "42", procs
        hidden = sql("SELECT coalesce(string_agg(proname, ',' ORDER BY proname), '') FROM pg_proc "
                     "WHERE proname IN ('corrupt_index_page', 'index_page_kinds')")
        assert hidden == "", hidden

        sql("""
            CREATE TABLE docs(
                id int PRIMARY KEY,
                title text,
                body text,
                published bool NOT NULL
            );
            INSERT INTO docs VALUES
              (1, 'alpha beta', 'other', true),
              (2, 'alpha', 'beta', true),
              (3, 'beta alpha', 'pad', true),
              (4, 'alpha xx beta', 'pad', true),
              (5, NULL, 'needle here', true),
              (6, 'needle', NULL, true),
              (7, 'pad', 'alpha beta', true),
              (8, 'secretunpublished', 'WINE wine', false),
              (20, 'needle', 'pad', true),
              (21, 'pad', 'needle needle needle needle', true),
              (30, '开源数据库', 'pad', true),
              (31, '中文分词 让数 据库更懂中文', 'pad', true),
              (40, 'pad', 'WINE', true),
              (41, 'pad', 'wine', true),
              (42, 'pad', 'alpha THEN beta', true);
            CREATE INDEX docs_expr_idx ON docs USING stannum((lower(body)))
              WITH (tokenizer = whitespace, case_folding = preserve, k1 = 1.3, b = 0.5)
              WHERE published;
            CREATE INDEX docs_idx ON docs USING stannum(title, body)
              WITH (field_weights = 'title:3,body:1', tokenizer = jieba, k1 = 1.3, b = 0.5);
        """)

        expr_before = capture_definition(sql, data, "docs_expr_idx")
        field_before = capture_definition(sql, data, "docs_idx")
        assert expr_before["indexprs"], expr_before
        assert expr_before["indpred"], expr_before
        assert expr_before["envelope_fields"] == [("expr", 1.0)], expr_before["envelope_fields"]
        assert field_before["field_weights"] == "title:3,body:1", field_before
        assert field_before["envelope_fields"] == [("title", 3.0), ("body", 1.0)], (
            field_before["envelope_fields"])
        assert field_before["indexprs"] == ""
        assert field_before["indpred"] == ""

        expr_answers = freeze_answers(sql, EXPR_QUERIES)
        field_answers = freeze_answers(sql, FIELD_QUERIES)
        assert_nontrivial(expr_answers, field_answers)

        sql("REINDEX INDEX docs_expr_idx; REINDEX INDEX docs_idx;")

        expr_after = capture_definition(sql, data, "docs_expr_idx")
        field_after = capture_definition(sql, data, "docs_idx")
        assert_same_definition(expr_before, expr_after, keep_filenode=False)
        assert_same_definition(field_before, field_after, keep_filenode=False)
        assert freeze_answers(sql, EXPR_QUERIES) == expr_answers, (
            expr_answers, freeze_answers(sql, EXPR_QUERIES))
        assert freeze_answers(sql, FIELD_QUERIES) == field_answers, (
            field_answers, freeze_answers(sql, FIELD_QUERIES))
        assert_clean(sql, "docs_expr_idx")
        assert_clean(sql, "docs_idx")

        alter = subprocess.run(
            ["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"],
            input=("SELECT stannum.search_count('docs_idx'::regclass, 'needle');\n"
                   "ALTER INDEX docs_idx SET (field_weights = 'title:2,body:1');"),
            env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        assert alter.returncode != 0, alter.stdout
        assert "REINDEX to change field_weights" in alter.stdout, alter.stdout

        print("gap1 definition docs_expr_idx:")
        print_definition(expr_before)
        print("gap1 definition docs_idx:")
        print_definition(field_before)
        print("gap1 answers docs_expr_idx:", expr_answers)
        print("gap1 answers docs_idx:", field_answers)
        print("gap1 ALTER INDEX field_weights rejected; both indexes clean after REINDEX")

        sql("""
            CREATE TABLE bulk(id int PRIMARY KEY, title text, body text);
            INSERT INTO bulk VALUES
              (1000001, 'needle', 'pad'),
              (1000002, 'pad', 'needle needle needle needle');
            INSERT INTO bulk SELECT n,
              'title pad ' || n,
              'body filler ' || repeat('token' || (n % 47) || ' ', 30)
              FROM generate_series(1, 4000) n;
            SET stannum.build_segment_docs = 32;
            CREATE INDEX bulk_idx ON bulk USING stannum(title, body)
              WITH (field_weights = 'title:3,body:1', tokenizer = jieba, k1 = 1.3, b = 0.5);
            RESET stannum.build_segment_docs;
        """)
        bulk_answers = freeze_answers(sql, BULK_QUERIES)
        heap_before = heap_fingerprint(sql, "bulk")
        bulk_answers, heap_before = crash_during_reindex(
            sql, command, env, data, root, start, bulk_answers, heap_before)
        assert freeze_answers(sql, BULK_QUERIES) == bulk_answers
        assert heap_fingerprint(sql, "bulk") == heap_before
        assert_clean(sql, "bulk_idx")
        sql("SET stannum.build_segment_docs = 32; REINDEX INDEX bulk_idx; RESET stannum.build_segment_docs;")
        assert freeze_answers(sql, BULK_QUERIES) == bulk_answers
        assert_clean(sql, "bulk_idx")
        print("gap2 crash: old index unpublished, heap intact, subsequent REINDEX succeeded")

        bulk_answers = freeze_answers(sql, BULK_QUERIES)
        heap_before = heap_fingerprint(sql, "bulk")
        bulk_answers, heap_before = terminate_during_reindex(
            sql, env, data, bulk_answers, heap_before)
        assert freeze_answers(sql, BULK_QUERIES) == bulk_answers
        assert heap_fingerprint(sql, "bulk") == heap_before
        assert_clean(sql, "bulk_idx")
        sql("SET stannum.build_segment_docs = 32; REINDEX INDEX bulk_idx; RESET stannum.build_segment_docs;")
        assert freeze_answers(sql, BULK_QUERIES) == bulk_answers
        assert_clean(sql, "bulk_idx")
        print("gap2 terminate: old index unpublished, heap intact, subsequent REINDEX succeeded")
        print(f"rebuild guarantees passed: {root}")
    finally:
        if started or (data / "postmaster.pid").exists():
            command(["pg_ctl", "-D", str(data), "-m", "fast", "-w", "stop"])


EXPR_QUERIES = {
    "expr_wine": "SELECT coalesce(string_agg(id::text, ',' ORDER BY id), '') FROM docs "
                  "WHERE lower(body) ==> 'wine' AND published",
    "expr_search_wine": "SELECT coalesce(string_agg(d.id::text, ',' ORDER BY d.id), '') FROM docs d "
                        "JOIN stannum.search('docs_expr_idx'::regclass, 'wine', snippet => 'none') s "
                        "ON d.ctid = s.ctid",
    "expr_phrase": "SELECT coalesce(string_agg(id::text, ',' ORDER BY id), '') FROM docs "
                   "WHERE lower(body) ==> '\"alpha beta\"' AND published",
    "expr_then": "SELECT coalesce(string_agg(id::text, ',' ORDER BY id), '') FROM docs "
                 "WHERE lower(body) ==> 'alpha THEN/0 beta' AND published",
    "expr_near": "SELECT coalesce(string_agg(id::text, ',' ORDER BY id), '') FROM docs "
                 "WHERE lower(body) ==> 'alpha NEAR/1 beta' AND published",
    "expr_scores_wine": "SELECT coalesce(string_agg(id::text || ':' || stannum.full_score(ctid)::text, ',' "
                        "ORDER BY stannum.full_score(ctid) DESC, id), '') FROM docs "
                        "WHERE lower(body) ==> 'wine' AND published",
}

FIELD_QUERIES = {
    "op_title_alpha": "SELECT coalesce(string_agg(id::text, ',' ORDER BY id), '') FROM docs "
                      "WHERE title ==> 'alpha'",
    "op_body_alpha": "SELECT coalesce(string_agg(id::text, ',' ORDER BY id), '') FROM docs "
                     "WHERE body ==> 'alpha'",
    "search_phrase": "SELECT coalesce(string_agg(d.id::text, ',' ORDER BY d.id), '') FROM docs d "
                     "JOIN stannum.search('docs_idx'::regclass, '\"alpha beta\"', snippet => 'none') s "
                     "ON d.ctid = s.ctid",
    "search_title_phrase": "SELECT coalesce(string_agg(d.id::text, ',' ORDER BY d.id), '') FROM docs d "
                           "JOIN stannum.search('docs_idx'::regclass, 'title:(\"alpha beta\")', snippet => 'none') s "
                           "ON d.ctid = s.ctid",
    "search_then": "SELECT coalesce(string_agg(d.id::text, ',' ORDER BY d.id), '') FROM docs d "
                   "JOIN stannum.search('docs_idx'::regclass, 'alpha THEN/0 beta', snippet => 'none') s "
                   "ON d.ctid = s.ctid",
    "search_near": "SELECT coalesce(string_agg(d.id::text, ',' ORDER BY d.id), '') FROM docs d "
                   "JOIN stannum.search('docs_idx'::regclass, 'alpha NEAR/1 beta', snippet => 'none') s "
                   "ON d.ctid = s.ctid",
    "search_jieba": "SELECT coalesce(string_agg(d.id::text, ',' ORDER BY d.id), '') FROM docs d "
                    "JOIN stannum.search('docs_idx'::regclass, '数据库', snippet => 'none') s "
                    "ON d.ctid = s.ctid",
    "search_unpublished": "SELECT coalesce(string_agg(d.id::text, ',' ORDER BY d.id), '') FROM docs d "
                          "JOIN stannum.search('docs_idx'::regclass, 'secretunpublished', snippet => 'none') s "
                          "ON d.ctid = s.ctid",
    "op_scores_needle": "SELECT coalesce(string_agg(id::text || ':' || stannum.full_score(ctid)::text, ',' "
                        "ORDER BY stannum.full_score(ctid) DESC, id), '') FROM docs "
                        "WHERE title ==> 'needle'",
    "search_scores_needle": "SELECT coalesce(string_agg(d.id::text || ':' || s.score::text, ',' "
                            "ORDER BY s.score DESC, d.id), '') FROM docs d "
                            "JOIN stannum.search('docs_idx'::regclass, 'needle', snippet => 'none') s "
                            "ON d.ctid = s.ctid",
}

BULK_QUERIES = {
    "bulk_needle": "SELECT coalesce(string_agg(id::text, ',' ORDER BY id), '') FROM bulk "
                   "WHERE title ==> 'needle' OR body ==> 'needle'",
    "bulk_search_needle": "SELECT coalesce(string_agg(d.id::text, ',' ORDER BY d.id), '') FROM bulk d "
                          "JOIN stannum.search('bulk_idx'::regclass, 'needle', snippet => 'none') s "
                          "ON d.ctid = s.ctid",
    "bulk_scores": "SELECT coalesce(string_agg(d.id::text || ':' || s.score::text, ',' "
                   "ORDER BY s.score DESC, d.id), '') FROM bulk d "
                   "JOIN stannum.search('bulk_idx'::regclass, 'needle', snippet => 'none') s "
                   "ON d.ctid = s.ctid",
    "bulk_title_scope": "SELECT coalesce(string_agg(d.id::text, ',' ORDER BY d.id), '') FROM bulk d "
                        "JOIN stannum.search('bulk_idx'::regclass, 'title:(needle)', snippet => 'none') s "
                        "ON d.ctid = s.ctid",
}


def freeze_answers(sql, queries):
    return {name: sql(query) for name, query in queries.items()}


def capture_definition(sql, data, index):
    indexdef = sql(f"SELECT pg_get_indexdef('{index}'::regclass)")
    reloptions = sql(f"SELECT coalesce(reloptions::text, '{{}}') FROM pg_class WHERE oid = '{index}'::regclass")
    indexprs = sql(f"SELECT coalesce(pg_get_expr(indexprs, indrelid), '') FROM pg_index "
                   f"WHERE indexrelid = '{index}'::regclass")
    indpred = sql(f"SELECT coalesce(pg_get_expr(indpred, indrelid), '') FROM pg_index "
                  f"WHERE indexrelid = '{index}'::regclass")
    field_weights = sql(
        f"SELECT coalesce((SELECT option_value FROM pg_options_to_table(("
        f"SELECT reloptions FROM pg_class WHERE oid = '{index}'::regclass)) "
        f"WHERE option_name = 'field_weights'), '')")
    identity = sql(f"SELECT oid::text || ':' || relfilenode::text FROM pg_class "
                   f"WHERE oid = '{index}'::regclass")
    oid, relfilenode = identity.split(":", 1)
    fields, spec = read_envelope(sql, data, index)
    return {
        "indexdef": indexdef,
        "reloptions": reloptions,
        "indexprs": indexprs,
        "indpred": indpred,
        "field_weights": field_weights,
        "oid": oid,
        "relfilenode": relfilenode,
        "envelope_fields": fields,
        "spec": spec,
    }


def assert_same_definition(before, after, keep_filenode):
    for key in ("indexdef", "reloptions", "indexprs", "indpred", "field_weights",
                "oid", "envelope_fields", "spec"):
        assert before[key] == after[key], (key, before[key], after[key])
    if keep_filenode:
        assert before["relfilenode"] == after["relfilenode"], (before, after)


def assert_clean(sql, index):
    findings = sql(f"SELECT coalesce(string_agg(severity || ': ' || location || ': ' || message, E'\\n'), '') "
                   f"FROM stannum.verify_index('{index}', true)")
    assert findings == "", (index, findings)


def assert_nontrivial(expr_answers, field_answers):
    assert expr_answers["expr_search_wine"], expr_answers
    assert "8" not in expr_answers["expr_search_wine"].split(","), expr_answers
    assert expr_answers["expr_wine"] != "", expr_answers
    assert expr_answers["expr_phrase"] != "", expr_answers
    assert field_answers["op_title_alpha"] != field_answers["op_body_alpha"], field_answers
    assert field_answers["search_phrase"] != field_answers["search_title_phrase"], field_answers
    assert field_answers["search_jieba"] != "", field_answers
    assert "8" in field_answers["search_unpublished"].split(","), field_answers
    assert field_answers["search_scores_needle"] != "", field_answers
    ranked = [piece.split(":")[0] for piece in field_answers["search_scores_needle"].split(",") if piece]
    assert "20" in ranked and "21" in ranked, field_answers["search_scores_needle"]
    assert ranked.index("20") < ranked.index("21"), field_answers["search_scores_needle"]


def print_definition(captured):
    for key in ("indexdef", "reloptions", "indexprs", "indpred", "field_weights",
                "envelope_fields", "spec", "oid"):
        print(f"  {key}: {captured[key]!r}")


def heap_fingerprint(sql, table):
    return sql(
        f"SELECT count(*)::text || ':' || md5(coalesce(string_agg("
        f"id::text || chr(9) || coalesce(title, '') || chr(9) || coalesce(body, '')"
        f"{' || chr(9) || published::text' if table == 'docs' else ''}, chr(10) ORDER BY id), '')) "
        f"FROM {table}")


def read_envelope(sql, data, index):
    sql("CHECKPOINT")
    relative = sql(f"SELECT pg_relation_filepath('{index}'::regclass)")
    page = (Path(sql("SHOW data_directory")) / relative).read_bytes()[:PAGE_SIZE]
    if len(page) != PAGE_SIZE:
        raise AssertionError((index, "short relation file", relative))
    lower = struct.unpack_from("<H", page, 12)[0]
    special = PAGE_SIZE - SPECIAL_SIZE
    magic, kind, version = struct.unpack_from("<IBB", page, special)
    assert magic == MAGIC and version == VERSION and kind == KIND_ENVELOPE, (index, magic, kind, version)
    payload = page[PAGE_HEADER:lower]
    _identity, spec = struct.unpack_from("<Q8s", payload, 0)
    segment_count, pending_count = struct.unpack_from("<II", payload, 8 + SPEC_BYTES + 28 + 4)
    image_len = META_HEADER + segment_count * ENTRY_BYTES + pending_count * PENDING_BYTES
    rest = payload[image_len:]
    assert rest[:4] == STNM_MAGIC, (index, rest[:8])
    body_len = struct.unpack_from("<I", rest, 5)[0]
    body = rest[9:9 + body_len]
    count = body[0]
    at = 1
    fields = []
    for _ in range(count):
        name_len = struct.unpack_from("<H", body, at)[0]
        at += 2
        name = body[at:at + name_len].decode("utf-8")
        at += name_len
        weight, = struct.unpack_from("<f", body, at)
        at += 4
        fields.append((name, weight))
    return fields, spec.hex()


def relation_dir(sql, data, index):
    relative = sql(f"SELECT pg_relation_filepath('{index}'::regclass)")
    path = Path(sql("SHOW data_directory")) / relative
    return path.parent, path.name


def wait_for_rebuild(sql, env, folder, directory, ignore_name, before_names):
    watcher = subprocess.Popen(
        ["psql", "-XqAt"], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL, text=True, bufsize=1)
    deadline = time.monotonic() + 120
    grown = False
    try:
        while time.monotonic() < deadline and folder.poll() is None:
            watcher.stdin.write(
                "SELECT coalesce((SELECT tuples_done FROM pg_stat_progress_create_index "
                "WHERE command = 'REINDEX' AND index_relid = 'bulk_idx'::regclass), 0);\n")
            watcher.stdin.flush()
            line = watcher.stdout.readline().strip()
            tuples_done = int(line) if line.isdigit() else 0
            new_bytes = 0
            for path in directory.iterdir():
                if path.name.isdigit() and path.name not in before_names:
                    new_bytes = max(new_bytes, path.stat().st_size)
            if tuples_done > 0 or new_bytes >= 2 * PAGE_SIZE:
                grown = True
                break
    finally:
        watcher.kill()
    return grown


def start_reindex(env):
    return subprocess.Popen(
        ["psql", "-XqAt", "-v", "ON_ERROR_STOP=1", "-c",
         "SET statement_timeout = 0; SET stannum.build_segment_docs = 32; "
         "REINDEX INDEX bulk_idx;"],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)


def enlarge_bulk(sql, attempt):
    sql(f"""
        INSERT INTO bulk SELECT n,
          'title pad ' || n,
          'body filler ' || repeat('token' || (n % 47) || ' ', 30)
          FROM generate_series({5000 + attempt * 4000}, {5000 + attempt * 4000 + 3999}) n;
    """)


def crash_during_reindex(sql, command, env, data, root, start, answers, heap_before):
    for attempt in range(RETRY_BOUND):
        if attempt:
            enlarge_bulk(sql, attempt)
            assert heap_fingerprint(sql, "bulk") != heap_before
            heap_before = heap_fingerprint(sql, "bulk")
            answers = freeze_answers(sql, BULK_QUERIES)
        before = capture_definition(sql, data, "bulk_idx")
        directory, ignore_name = relation_dir(sql, data, "bulk_idx")
        before_names = {path.name for path in directory.iterdir() if path.name.isdigit()}
        folder = start_reindex(env)
        grown = wait_for_rebuild(sql, env, folder, directory, ignore_name, before_names)
        command(["pg_ctl", "-D", str(data), "-m", "immediate", "-w", "stop"])
        folder.wait()
        start()
        after = capture_definition(sql, data, "bulk_idx")
        if after["relfilenode"] != before["relfilenode"]:
            # A completed rebuild legitimately swaps the relfilenode, so only an
            # *interrupted* one that still published is a defect. Without this
            # the test false-fails whenever the kill lands after publication.
            if grown and folder.returncode != 0:
                raise AssertionError(
                    "interrupted REINDEX published a new relfilenode: %s -> %s (%s)"
                    % (before["relfilenode"], after["relfilenode"], root))
            continue
        if not grown:
            continue
        assert_same_definition(before, after, keep_filenode=True)
        assert freeze_answers(sql, BULK_QUERIES) == answers
        assert heap_fingerprint(sql, "bulk") == heap_before
        assert_clean(sql, "bulk_idx")
        return answers, heap_before
    raise AssertionError("no crash attempt interrupted REINDEX INDEX: %s" % root)


def terminate_during_reindex(sql, env, data, answers, heap_before):
    for attempt in range(RETRY_BOUND):
        if attempt:
            enlarge_bulk(sql, attempt + RETRY_BOUND)
            heap_before = heap_fingerprint(sql, "bulk")
            answers = freeze_answers(sql, BULK_QUERIES)
        before = capture_definition(sql, data, "bulk_idx")
        directory, ignore_name = relation_dir(sql, data, "bulk_idx")
        before_names = {path.name for path in directory.iterdir() if path.name.isdigit()}
        folder = start_reindex(env)
        grown = wait_for_rebuild(sql, env, folder, directory, ignore_name, before_names)
        if not grown:
            sql("SELECT pg_terminate_backend(pid) FROM pg_stat_activity "
                "WHERE pid <> pg_backend_pid() AND query ILIKE '%REINDEX INDEX bulk_idx%'")
            folder.wait(timeout=30)
            continue
        sql("SELECT pg_terminate_backend(pid) FROM pg_stat_progress_create_index "
            "WHERE command = 'REINDEX' AND index_relid = 'bulk_idx'::regclass")
        folder.wait(timeout=30)
        after = capture_definition(sql, data, "bulk_idx")
        if after["relfilenode"] != before["relfilenode"]:
            # Same rule as the crash case: a REINDEX that ran to completion
            # legitimately swaps the relfilenode. Only one that was actually
            # terminated and still published is a defect.
            if folder.returncode != 0:
                raise AssertionError(
                    "terminated REINDEX published a new relfilenode: %s -> %s"
                    % (before["relfilenode"], after["relfilenode"]))
            continue
        if folder.returncode == 0:
            continue
        assert_same_definition(before, after, keep_filenode=True)
        assert freeze_answers(sql, BULK_QUERIES) == answers
        assert heap_fingerprint(sql, "bulk") == heap_before
        assert_clean(sql, "bulk_idx")
        return answers, heap_before
    raise AssertionError("no terminate attempt interrupted REINDEX INDEX")


if __name__ == "__main__":
    main()
