#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Installed SQL contract and MVCC/permission checks for search() SRFs.

Run after cargo pgrx install with the matching PostgreSQL binaries on PATH.
"""
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
PG_PORT = "28958"


def command(args, *, env=None, input=None, check=True):
    result = subprocess.run(args, input=input, text=True, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if check and result.returncode:
        raise RuntimeError(f"{' '.join(map(str, args))} failed:\n{result.stdout}")
    return result


class Session:
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


def main():
    with tempfile.TemporaryDirectory(prefix="stannum-search-") as directory:
        root = Path(directory)
        data = root / "data"
        env = dict(os.environ, PGHOST=str(root), PGPORT=PG_PORT,
                   PGUSER="postgres", PGDATABASE="postgres")
        for key in ("PGSERVICE", "PGSERVICEFILE", "PGPASSWORD", "PGOPTIONS"):
            env.pop(key, None)
        started = False
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

            sql("""
                CREATE EXTENSION stannum;
                CREATE TABLE docs (id integer PRIMARY KEY, body text);
                INSERT INTO docs VALUES
                  (1, 'needle needle needle'),
                  (2, 'needle common'),
                  (3, 'needle other'),
                  (4, 'unrelated');
                CREATE INDEX docs_idx ON docs USING stannum (body);
            """)

            contract = sql("""
                SELECT p.proname || '|' || p.provolatile::text || '|' || p.proparallel::text || '|' || p.proisstrict::text
                FROM pg_proc p
                WHERE p.oid IN (
                  'stannum.search(regclass,text,integer,text,text,text,real,real)'::regprocedure,
                  'stannum.search_count(regclass,text)'::regprocedure)
                ORDER BY p.proname;
            """).splitlines()
            assert contract == ["search|v|u|false", "search_count|v|u|false"], contract
            assert sql("SELECT stannum.search_count('public.docs_idx', 'needle');") == "3"
            assert sql("""
                SELECT count(*) FROM stannum.search(
                    'public.docs_idx', 'needle', 2, 'html', '<b>', '</b>'
                );
            """) == "2"
            assert "<b>" in sql("""
                SELECT snippet FROM stannum.search(
                    'public.docs_idx', 'needle', 1, 'html', '<b>', '</b>'
                );
            """)
            assert sql("SELECT count(*) FROM stannum.search('public.docs_idx', 'needle', 0);") == "0"
            fails("SELECT * FROM stannum.search('public.docs_idx', 'needle', -1);")
            fails("SELECT * FROM stannum.search('public.docs_idx', 'needle', 1, 'bad');")

            sql("""
                CREATE ROLE search_reader;
                GRANT USAGE ON SCHEMA stannum TO search_reader;
                GRANT SELECT (id) ON docs TO search_reader;
            """)
            fails("""
                SET ROLE search_reader;
                SELECT stannum.search_count('public.docs_idx', 'needle');
            """)
            sql("GRANT SELECT ON docs TO search_reader;")
            sql("""
                SET ROLE search_reader;
                SELECT stannum.search_count('public.docs_idx', 'needle');
                RESET ROLE;
            """)
            sql("ALTER TABLE docs ENABLE ROW LEVEL SECURITY;")
            fails("""
                SET ROLE search_reader;
                SELECT stannum.search_count('public.docs_idx', 'needle');
            """)
            sql("""
                ALTER TABLE docs DISABLE ROW LEVEL SECURITY;
                REVOKE ALL ON TABLE docs FROM search_reader;
                REVOKE ALL ON SCHEMA stannum FROM search_reader;
                DROP ROLE search_reader;
            """)

            # The first command on conn1 is BEGIN, so its REPEATABLE READ
            # snapshot is taken by search() after conn2 commits the delete. The
            # index dead set has not been vacuum-maintained, so this exercises
            # the required invisible-top-row exhaustive fallback.
            top_id = int(sql("""
                SELECT d.id FROM docs d
                JOIN stannum.search('public.docs_idx', 'needle', 3, 'none') s
                  ON d.ctid = s.ctid
                ORDER BY s.score DESC, d.id
                LIMIT 1;
            """))
            conn1 = Session(env)
            conn2 = Session(env)
            try:
                conn1.sql("BEGIN ISOLATION LEVEL REPEATABLE READ;")
                conn2.sql(f"BEGIN; DELETE FROM docs WHERE id = {top_id}; COMMIT;")
                assert conn1.sql(
                    "SELECT count(*) FROM stannum.search('public.docs_idx', 'needle', 2, 'none');"
                ) == "2"
                conn1.sql("ROLLBACK;")
            finally:
                conn1.close()
                conn2.close()

            print("search SRF installed SQL, permissions, RLS, and MVCC checks passed")
        finally:
            if started:
                command(["pg_ctl", "-D", str(data), "-m", "immediate", "-w", "stop"])


if __name__ == "__main__":
    main()
