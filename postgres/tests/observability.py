#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""P0-4 observability: CREATE INDEX progress polling, index_stats/index_health.

Run after cargo pgrx install with the matching PostgreSQL binaries on PATH.
"""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[2]
PG_PORT = "28968"


def command(args, *, env=None, input=None, check=True):
    result = subprocess.run(args, input=input, text=True, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if check and result.returncode:
        raise RuntimeError(f"{' '.join(map(str, args))} failed:\n{result.stdout}")
    return result


class Session:
    """A persistent psql process whose statements return synchronously."""

    def __init__(self, env):
        self.process = subprocess.Popen(
            ["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, env=env, bufsize=1,
        )
        self.serial = 0

    def sql(self, text):
        self.serial += 1
        marker = f"__STANNUM_MARKER_{self.serial}__"
        self.process.stdin.write(text.rstrip() + f"\n\\echo {marker}\n")
        self.process.stdin.flush()
        rows = []
        for line in self.process.stdout:
            line = line.rstrip("\n")
            if line == marker:
                return "\n".join(rows).strip()
            rows.append(line)
        raise RuntimeError("persistent psql exited unexpectedly")

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.write("\\q\n")
            self.process.stdin.flush()
            self.process.wait(timeout=10)


def load_explain_counters():
    spec = importlib.util.spec_from_file_location(
        "explain_counters", ROOT / "benchmarks" / "explain_counters.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def poll_progress(session, stop, samples):
    """Samples the raw CREATE INDEX progress command until told to stop."""
    while not stop.is_set():
        row = session.sql(
            "SELECT coalesce(max((s.param12 IS NOT NULL)::int), 0)::text || '|' ||"
            "       count(*)::text || '|' ||"
            "       coalesce(string_agg(DISTINCT command_text, ','), '')"
            " FROM pg_stat_get_progress_info('CREATE INDEX') s,"
            "      LATERAL (SELECT CASE s.param1 WHEN 1 THEN 'CREATE INDEX'"
            "                            WHEN 3 THEN 'REINDEX' END AS command_text) c;")
        samples.append(row)


def main():
    explain_counters = load_explain_counters()
    with tempfile.TemporaryDirectory(prefix="stannum-observability-") as directory:
        root = Path(directory)
        data = root / "data"
        env = dict(os.environ, PGHOST=str(root), PGPORT=PG_PORT,
                   PGUSER="postgres", PGDATABASE="postgres")
        for key in ("PGSERVICE", "PGSERVICEFILE", "PGPASSWORD", "PGOPTIONS"):
            env.pop(key, None)
        started = False
        builder = None
        poller = None
        try:
            command(["initdb", "-D", str(data), "-U", "postgres", "-A", "trust",
                     "--no-locale", "--encoding=UTF8"])
            command(["pg_ctl", "-D", str(data), "-l", str(root / "server.log"), "-w",
                     "-o", f"-p {PG_PORT} -c listen_addresses= -c unix_socket_directories={root}",
                     "start"])
            started = True

            def sql(text):
                return command(["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"],
                               env=env, input=text).stdout.strip()

            def fails(text):
                result = command(["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"],
                                 env=env, input=text, check=False)
                if result.returncode == 0:
                    raise AssertionError(f"expected SQL failure:\n{text}")
                return result.stdout

            sql("CREATE EXTENSION stannum;")

            # --- CREATE INDEX progress, polled from a second connection ----
            sql("""
                CREATE TABLE progress_docs (id integer PRIMARY KEY, body text);
                INSERT INTO progress_docs
                SELECT n, repeat('filler ', 12) ||
                       CASE WHEN n % 500 = 0 THEN 'needle ' ELSE '' END
                FROM generate_series(1, 120000) n;
            """)
            builder = Session(env)
            poller = Session(env)
            samples = []
            stop = threading.Event()
            thread = threading.Thread(
                target=poll_progress, args=(poller, stop, samples), daemon=True)
            thread.start()
            builder.sql("CREATE INDEX progress_idx ON progress_docs USING stannum (body);")
            stop.set()
            thread.join(timeout=10)
            # The backend appears in the raw command with a non-null
            # tuples_total slot (core zeroes it at build start; the heap scan
            # fills blocks_total and advances blocks_done).
            assert samples, "progress polling produced no samples"
            nonnull = max(int(row.split("|")[0]) for row in samples)
            assert nonnull == 1, "tuples_total slot never observed non-null"
            with_backend = [row for row in samples if int(row.split("|")[1]) > 0]
            assert with_backend, "CREATE INDEX backend never appeared"
            assert any("CREATE INDEX" in row for row in with_backend), samples[:5]
            assert sql("SELECT count(*) FROM progress_docs WHERE body ==> 'needle';") == "240"
            poller.close()
            poller = None
            builder.close()
            builder = None

            # --- EXPLAIN counters, parsed by the benchmark helper ----------
            counters = explain_counters.observe(
                lambda statement: command(
                    ["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"],
                    env=dict(env, PGDATABASE="postgres"), input=statement).stdout,
                "SELECT id FROM progress_docs WHERE body ==> 'needle' ORDER BY"
                " stannum.full_score(ctid) DESC LIMIT 50")
            assert counters.get("Segments Visited", 0) >= 1, counters
            assert counters.get("Postings Blocks Read", 0) >= 1, counters
            assert counters.get("Heap Fetches", 0) >= 1, counters
            if all(key in counters for key in
                   ("Candidates", "Scored Candidates", "Pruned by Block-Max")):
                # The helper already asserted the identity; checked again to
                # pin it in this test's output on failure.
                assert (counters["Scored Candidates"] <= counters["Candidates"]
                        and counters["Pruned by Block-Max"]
                        == counters["Candidates"] - counters["Scored Candidates"])

            # --- index_stats agrees with segment_info aggregates ----------
            sql("""
                CREATE TABLE health (id integer PRIMARY KEY, body text);
                SET stannum.build_segment_docs = 10;
                INSERT INTO health
                SELECT n, 'common ' || CASE WHEN n % 2 = 0 THEN 'beer' ELSE 'wine' END
                FROM generate_series(1, 20) n;
                CREATE INDEX health_idx ON health USING stannum (body);
                DELETE FROM health WHERE id IN (2, 4);
                VACUUM health;
                INSERT INTO health VALUES (100, 'beer fresh');
            """)
            agreement = sql("""
                SELECT (SELECT sum(docs - dead_docs) FROM stannum.segment_info('health_idx'))
                    || '|' || (SELECT sum(dead_docs) FROM stannum.segment_info('health_idx'))
                    || '|' || (SELECT count(*) FROM stannum.segment_info('health_idx'))
                    || '|' || (SELECT count(*) FROM stannum.segment_info('health_idx')
                               WHERE kind = 'immutable')
                    || '|' || (SELECT count(*) FROM stannum.segment_info('health_idx')
                               WHERE kind = 'mutable')
                    || '|' || (SELECT sum(total_pages) FROM stannum.segment_info('health_idx'))
                    || '|' || (SELECT sum(sum_doc_lengths) FROM stannum.segment_info('health_idx'))
            """)
            row = sql("""
                SELECT documents || '|' || dead_documents || '|' || segments || '|'
                    || immutable_segments || '|' || mutable_segments || '|'
                    || total_pages || '|' || total_length || '|' || average_length
                FROM stannum.index_stats('health_idx');
            """)
            live, dead, segments, immutable, mutable, pages, lengths = agreement.split("|")
            documents, dead_documents, all_segments, immutable_segments, \
                mutable_segments, total_pages, total_length, average_length = row.split("|")
            assert documents == live, (row, agreement)
            assert dead_documents == dead, (row, agreement)
            assert all_segments == segments == str(int(immutable) + int(mutable)), (row, agreement)
            assert immutable_segments == immutable and mutable_segments == mutable, (row, agreement)
            assert total_pages == pages, (row, agreement)
            assert total_length == lengths, (row, agreement)
            assert average_length == "0"
            stats = sql("""
                SELECT dead_ratio::text || '|' || (next_generation > 0)::text || '|'
                    || (dictionary_pages >= immutable_segments::bigint)::text || '|'
                    || (analysis_matches IS NULL)::text || '|'
                    || (analysis_detail IS NULL)::text
                FROM stannum.index_stats('health_idx');
            """)
            ratio, generation, dictionaries, matches_null, detail_null = stats.split("|")
            posted = int(documents) + int(dead_documents)
            expected = (int(dead_documents) / posted) if posted else 0.0
            assert abs(float(ratio) - expected) < 1e-9, stats
            assert generation == "true", stats
            assert dictionaries == "true", stats
            # A unicode index is never stamped: analysis columns are NULL.
            assert matches_null == "true" and detail_null == "true", stats

            # --- index_health under security_invoker ----------------------
            view_options = sql(
                "SELECT reloptions::text FROM pg_class"
                " WHERE oid = 'stannum.index_health'::regclass;")
            assert "security_invoker=true" in view_options, view_options
            assert sql("""
                SELECT count(*) FROM stannum.index_health
                WHERE index = 'health_idx'::regclass AND dead_documents = 2;
            """) == "1"
            comment = sql("""
                SELECT col_description('stannum.index_health'::regclass, 10);
            """)
            assert "actually pinned" in comment and "fully-scanned" in comment, comment
            sql("""
                CREATE ROLE health_reader;
                GRANT USAGE ON SCHEMA stannum TO health_reader;
                GRANT SELECT ON stannum.index_health TO health_reader;
            """)
            # The view executes as the invoker, not its owner: without table
            # access the diagnostics fail, and one unreadable index fails the
            # whole scan (all-or-nothing in v1).
            denial = fails("""
                SET ROLE health_reader;
                SELECT dead_documents FROM stannum.index_health
                WHERE index = 'health_idx'::regclass;
            """)
            assert "permission denied for table health" in denial, denial
            sql("GRANT SELECT ON health TO health_reader;")
            assert sql("""
                SET ROLE health_reader;
                SELECT dead_documents FROM stannum.index_health
                WHERE index = 'health_idx'::regclass;
                RESET ROLE;
            """) == "2"
            print("observability: progress polling, explain counters, index_stats"
                  " agreement and index_health security all passed")
        finally:
            if poller:
                poller.close()
            if builder:
                builder.close()
            if started:
                command(["pg_ctl", "-D", str(data), "-m", "immediate", "-w", "stop"])


if __name__ == "__main__":
    main()
