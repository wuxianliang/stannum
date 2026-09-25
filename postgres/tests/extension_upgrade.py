#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Check release SQL drift, snapshot installs, and every retained upgrade path.

Run after cargo pgrx install with matching pg_config/server binaries on PATH.
Callers sharing a PostgreSQL installation must hold the machine-wide pgrx lock.
"""
import difflib
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]


def normalized(sql):
    # pgrx's dependency graph can emit independent objects in different orders.
    # Keep each connected object's SQL intact, ignore source-location lines,
    # and sort objects. Executing snapshots below still verifies dependencies.
    objects = []
    for block in sql.split('/* <begin connected objects> */'):
        block = block.replace('/* </end connected objects> */', '')
        text = '\n'.join(line.rstrip() for line in block.splitlines()
                         if line.strip() and not line.lstrip().startswith('--'))
        if text:
            objects.append(text)
    return '\n\n'.join(sorted(objects)) + '\n'


def command(args, **kwargs):
    try:
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kwargs)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"{' '.join(map(str, args))} failed:\n{error.output}") from error


def main():
    version = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
    shared = Path(command(['pg_config', '--sharedir']).strip()) / 'extension'
    installed = shared / f'stannum--{version}.sql'
    snapshot = ROOT / 'postgres/sql' / installed.name
    actual, expected = normalized(installed.read_text()), normalized(snapshot.read_text())
    if actual != expected:
        raise AssertionError('Release schema drift: bump version and add snapshot/upgrade SQL.\n' +
                             ''.join(difflib.unified_diff(expected.splitlines(True), actual.splitlines(True))))
    assert 'corrupt_index_page' not in actual and 'index_page_kinds' not in actual
    assert 'SECURITY DEFINER' not in actual.upper()
    snapshots = sorted((ROOT / 'postgres/sql').glob('stannum--*.sql'))
    snapshots = [p for p in snapshots if p.name.count('--') == 1]
    backups = {shared / p.name: (shared / p.name).read_bytes() if (shared / p.name).exists() else None
               for p in snapshots}
    with tempfile.TemporaryDirectory(prefix='stannum-upgrade-') as directory:
        root = Path(directory)
        data = root / 'data'
        env = dict(os.environ, PGHOST=str(root), PGPORT='28938', PGUSER='postgres', PGDATABASE='postgres')
        for key in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD', 'PGOPTIONS'):
            env.pop(key, None)
        def sql(text, database='postgres'):
            return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], input=text,
                           env=dict(env, PGDATABASE=database)).strip()
        def fingerprint(database):
            return sql("""SELECT pg_describe_object(d.classid,d.objid,d.objsubid) || '|' ||
                CASE
                  WHEN d.classid='pg_proc'::regclass THEN
                    pg_get_functiondef(d.objid) || COALESCE((SELECT proacl::text FROM pg_proc WHERE oid=d.objid), 'DEFAULT')
                  WHEN d.classid='pg_type'::regclass THEN
                    (SELECT jsonb_build_array(typtype, typlen, typbyval, typalign, typstorage,
                      typinput::regproc::text, typoutput::regproc::text, typreceive::regproc::text,
                      typsend::regproc::text, typacl)::text FROM pg_type WHERE oid=d.objid)
                  WHEN d.classid='pg_operator'::regclass THEN
                    (SELECT jsonb_build_array(oprcode::regproc::text, oprrest::regproc::text,
                      oprjoin::regproc::text, oprcanmerge, oprcanhash)::text FROM pg_operator WHERE oid=d.objid)
                  WHEN d.classid='pg_am'::regclass THEN
                    (SELECT amhandler::regproc::text || amtype::text FROM pg_am WHERE oid=d.objid)
                  WHEN d.classid='pg_opclass'::regclass THEN
                    (SELECT jsonb_build_array(c.opcintype::regtype::text, c.opckeytype::regtype::text,
                       c.opcdefault,
                       (SELECT jsonb_agg(jsonb_build_array(a.amopstrategy, a.amoppurpose,
                          a.amoplefttype::regtype::text, a.amoprighttype::regtype::text,
                          a.amopopr::regoperator::text) ORDER BY a.amopstrategy,
                          a.amoplefttype::regtype::text, a.amoprighttype::regtype::text)
                        FROM pg_amop a WHERE a.amopfamily=c.opcfamily),
                       (SELECT jsonb_agg(jsonb_build_array(p.amprocnum,
                          p.amproclefttype::regtype::text, p.amprocrighttype::regtype::text,
                          p.amproc::regprocedure::text) ORDER BY p.amprocnum,
                          p.amproclefttype::regtype::text, p.amprocrighttype::regtype::text)
                        FROM pg_amproc p WHERE p.amprocfamily=c.opcfamily))::text
                     FROM pg_opclass c WHERE c.oid=d.objid)
                  ELSE '' END
                FROM pg_depend d JOIN pg_extension e ON e.oid=d.refobjid
                WHERE d.refclassid='pg_extension'::regclass AND e.extname='stannum'
                  AND d.deptype='e' ORDER BY 1;""", database)
        started = False
        try:
            command(['initdb', '-D', str(data), '-U', 'postgres', '-A', 'trust', '--no-locale', '--encoding=UTF8'])
            command(['pg_ctl', '-D', str(data), '-l', str(root / 'server.log'), '-w',
                     '-o', f'-p 28938 -c listen_addresses= -c unix_socket_directories={root}', 'start'])
            started = True
            sql('CREATE DATABASE fresh')
            sql('CREATE EXTENSION stannum', 'fresh')
            fresh = fingerprint('fresh')
            assert sql('SELECT stannum.version()', 'fresh') == version
            # Bootstrap compares the baseline snapshot with the generated fresh
            # install. Once a second version exists, every old snapshot must have
            # a real ALTER EXTENSION UPDATE path to the current release.
            for number, path in enumerate(snapshots):
                previous = path.stem.split('--')[1]
                assert re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', previous)
                (shared / path.name).write_bytes(path.read_bytes())
                database = f'upgrade_{number}'
                sql(f'CREATE DATABASE {database}')
                sql(f"CREATE EXTENSION stannum VERSION '{previous}'", database)
                # A single-column index built by the previous version: its LSG3
                # bytes are never rewritten, so it must read identically after
                # the upgrade (LSG4 RFC §5.9).
                lsg3 = previous == '0.3.0'
                if lsg3:
                    sql('CREATE TABLE lsg3_docs(id int primary key, body text); '
                        "INSERT INTO lsg3_docs VALUES (1, 'needle pad'), (2, 'pad needle'); "
                        'CREATE INDEX lsg3_docs_idx ON lsg3_docs USING stannum(body)', database)
                if previous != version:
                    sql(f"ALTER EXTENSION stannum UPDATE TO '{version}'", database)
                assert sql("SELECT extversion FROM pg_extension WHERE extname='stannum'", database) == version
                assert fingerprint(database) == fresh, f'{previous} upgrade differs from fresh installation'
                if lsg3:
                    assert sql("SELECT count(*) FROM lsg3_docs WHERE body ==> 'needle'", database) == '2'
                    assert sql("SELECT stannum.search_count('lsg3_docs_idx', '\"needle pad\"')", database) == '1'
                    # The 0.4.0 objects in the upgraded database: a multi-column
                    # field-weighted index, same-field phrases, snippets, and the
                    # highlight field overload (RFC §5.11).
                    sql('CREATE TABLE fields_docs(id int primary key, title text, body text); '
                        "INSERT INTO fields_docs VALUES "
                        "(1, '甲 乙', 'pad'), (2, '甲', '乙'), (3, 'needle 甲 乙', 'pad'); "
                        "CREATE INDEX fields_docs_idx ON fields_docs USING stannum(title, body) "
                        "WITH (field_weights = 'title:3.0,body:1.0')", database)
                    # Same-field phrases: 甲 in title plus 乙 in body (row 2) never matches.
                    assert sql("SELECT count(*) FROM fields_docs WHERE title ==> 'title:(\"甲 乙\")'", database) == '2'
                    assert sql("SELECT count(*) FROM fields_docs WHERE title ==> '\"甲 乙\"'", database) == '2'
                    assert sql("SELECT stannum.search_count('fields_docs_idx', '\"甲 乙\"')", database) == '2'
                    assert sql("SELECT stannum.search_count('fields_docs_idx', 'title:(甲) AND body:(乙)')", database) == '1'
                    # The snippet renders the wrapper's field with confined marks.
                    assert sql("SELECT s.snippet FROM fields_docs d JOIN "
                               "stannum.search('fields_docs_idx', 'title:(needle)', 1) s "
                               'ON d.ctid = s.ctid', database) == '<mark>needle</mark> 甲 乙'
                    # The new highlight overload answers in the upgraded database.
                    assert sql("SELECT stannum.highlight(title, '<b>', '</b>', 'title:(needle)', 'title') "
                               'FROM fields_docs WHERE id = 3', database) == '<b>needle</b> 甲 乙'
            print(f'Release schema {version}: drift, snapshot install, and {len(snapshots)-1} upgrade paths passed')
        finally:
            if started:
                command(['pg_ctl', '-D', str(data), '-m', 'immediate', '-w', 'stop'])
            for path, backup in backups.items():
                if backup is None:
                    path.unlink(missing_ok=True)
                else:
                    path.write_bytes(backup)


if __name__ == '__main__':
    main()
