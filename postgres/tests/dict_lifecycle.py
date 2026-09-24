#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Installed P0-3 checks using persistent psql connections and a private cluster.

Run with bundled PG18 bin on PATH after cargo pgrx install. No Python driver
is required. Like extension_upgrade.py, callers must serialize install/test
runs sharing a PostgreSQL installation.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import time


def command(args, **kwargs):
    try:
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kwargs)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f'{args}:\n{error.output}') from error


class Connection:
    def __init__(self, env, name):
        self.process = subprocess.Popen(
            ['psql', '-XqAt', '-v', 'ON_ERROR_STOP=0'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, env=dict(env, PGAPPNAME=name), bufsize=1)
        self.serial = 0

    def send(self, sql):
        self.serial += 1
        self.marker = f'__dict_end_{self.serial}__'
        self.process.stdin.write(sql + '\n\\echo ' + self.marker + '\n')
        self.process.stdin.flush()

    def receive(self):
        lines = []
        while True:
            line = self.process.stdout.readline()
            if not line:
                raise AssertionError('psql exited: ' + ''.join(lines))
            if line.strip() == self.marker:
                return '\n'.join(lines).strip()
            lines.append(line.rstrip())

    def sql(self, sql, error=None):
        self.send(sql)
        result = self.receive()
        if error is None:
            assert 'ERROR:' not in result and 'FATAL:' not in result, result
        else:
            assert 'ERROR:' in result and error in result, result
        return result

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.write('\\q\n')
            self.process.stdin.flush()
            self.process.wait(timeout=10)
        self.process.stdin.close()
        self.process.stdout.close()


WORD = '星河数据库协议'
TOKENIZE = f"SELECT string_agg(tok, '/') FROM stannum.tokenize('{WORD}', tokenizer => 'jieba') tok;"
VERSION = 'SELECT stannum.jieba_dict_version();'
ANALYSIS = "SELECT matches, status FROM stannum.index_analysis('docs_idx');"
QUERY = "SELECT stannum.full_score(ctid) FROM docs WHERE body ==> '数据库';"


def wait_for(conn, sql):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if conn.sql(sql) == 't':
            return
        time.sleep(.05)
    raise AssertionError('timed out: ' + sql)


def main():
    with tempfile.TemporaryDirectory(prefix='stannum-dict-') as directory:
        root = Path(directory)
        data, standby = root / 'data', root / 'standby'
        env = dict(os.environ, PGHOST=str(root), PGPORT='28948', PGUSER='postgres',
                   PGDATABASE='postgres', PGOPTIONS='-c statement_timeout=30000 -c enable_seqscan=off')
        for key in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
            env.pop(key, None)
        connections = []
        started = standby_started = False
        try:
            command(['initdb', '-D', str(data), '-U', 'postgres', '-A', 'trust', '--no-locale', '--encoding=UTF8'])
            with (data / 'postgresql.conf').open('a') as config:
                config.write(f"\nlisten_addresses=''\nport=28948\nunix_socket_directories='{root}'\nshared_preload_libraries='stannum'\nwal_level=replica\nmax_wal_senders=4\n")
            command(['pg_ctl', '-D', str(data), '-l', str(root / 'primary.log'), '-w', 'start'])
            started = True
            a, b, observer = [Connection(env, name) for name in ('dict-writer', 'dict-reader', 'dict-observer')]
            connections.extend([a, b, observer])
            a.sql("CREATE EXTENSION stannum; CREATE TABLE docs(body text); INSERT INTO docs VALUES ('的 数据库'), ('数据库'), ('的'); CREATE INDEX docs_idx ON docs USING stannum(body) WITH(tokenizer='jieba', score_stop_words='auto:zh'); CREATE TABLE unicode_docs(body text); CREATE INDEX unicode_idx ON unicode_docs USING stannum(body);")
            empty = b.sql(VERSION)
            assert empty != '0'
            baseline = b.sql(TOKENIZE)
            assert baseline != WORD, baseline
            assert b.sql(ANALYSIS) == 't|matches'
            assert b.sql("SELECT matches IS NULL AND recorded_jieba_version IS NULL AND runtime_dict_fingerprint IS NULL AND status='not applicable' FROM stannum.index_analysis('unicode_idx');") == 't'
            # Force a real worker with the empty dictionary, including strict
            # stamping, then retain the generic plan across a dictionary change.
            b.sql("SET debug_parallel_query=on; SET stannum.strict_analysis=on; SET plan_cache_mode=force_generic_plan; PREPARE dict_parallel AS SELECT body FROM docs WHERE body ==> '数据库';")
            assert '数据库' in b.sql('EXECUTE dict_parallel;')
            empty_plan = b.sql('EXPLAIN (ANALYZE, COSTS OFF) EXECUTE dict_parallel;')
            assert 'Gather' in empty_plan and 'Workers Launched: 1' in empty_plan, empty_plan
            b.sql('SET stannum.strict_analysis=off;')
            a.sql(f"BEGIN; SELECT stannum.jieba_add_word('{WORD}', 1000000, 'n');")
            assert a.sql(TOKENIZE) == WORD
            assert b.sql(TOKENIZE) == baseline  # not visible before commit
            a.sql('COMMIT;')
            assert b.sql(TOKENIZE) == WORD  # same backend, committed invalidation
            changed = b.sql(VERSION)
            assert changed != empty
            # Relcache invalidation must invalidate the saved plan's worker
            # eligibility, not just the dictionary holder.
            custom_plan = b.sql('EXPLAIN (ANALYZE, COSTS OFF) EXECUTE dict_parallel;')
            assert 'Gather' not in custom_plan, custom_plan
            b.sql('SET stannum.enable_custom_scan=off;')
            bitmap_plan = b.sql("EXPLAIN (ANALYZE, COSTS OFF) SELECT body FROM docs WHERE body ==> '数据库';")
            assert 'Gather' not in bitmap_plan, bitmap_plan
            b.sql('RESET stannum.enable_custom_scan;')
            b.sql('PREPARE dict_parameter(text) AS SELECT body FROM docs WHERE body ==> $1;')
            parameter_plan = b.sql("EXPLAIN (ANALYZE, COSTS OFF) EXECUTE dict_parameter('数据库');")
            assert 'Gather' not in parameter_plan, parameter_plan
            b.sql('DEALLOCATE dict_parameter;')
            b.sql('DEALLOCATE dict_parallel; RESET debug_parallel_query; RESET plan_cache_mode;')
            assert 'dictionary drift' in b.sql(ANALYSIS)
            for _ in range(2):
                output = b.sql(QUERY)
                assert output.count('WARNING:') == 1 and 'REINDEX' in output, output
            b.sql('SET stannum.strict_analysis=on;')
            b.sql(QUERY, error='REINDEX')
            # Inspection reports drift rather than raising strict-mode errors.
            assert 'dictionary drift' in b.sql(ANALYSIS)
            a.sql('REINDEX INDEX docs_idx;')
            assert b.sql(ANALYSIS) == 't|matches'
            assert 'WARNING:' not in b.sql(QUERY)

            # Rollback and savepoint rollback restore the visible row set locally.
            a.sql(f"BEGIN; SELECT stannum.jieba_delete_word('{WORD}');")
            assert a.sql(TOKENIZE) == baseline
            a.sql('ROLLBACK;')
            assert a.sql(TOKENIZE) == WORD
            a.sql(f"BEGIN; SAVEPOINT d; SELECT stannum.jieba_delete_word('{WORD}'); ROLLBACK TO d;")
            assert a.sql(TOKENIZE) == WORD
            a.sql('COMMIT;')
            a.sql('SELECT stannum.jieba_reload_dict();')
            assert a.sql(VERSION) == changed

            # Deterministic content identity is independent of insertion order.
            a.sql(f"SELECT stannum.jieba_add_word('星河检索协议', 7, 'x');")
            pair = a.sql(VERSION)
            a.sql(f"SELECT stannum.jieba_delete_word('{WORD}'); SELECT stannum.jieba_delete_word('星河检索协议'); SELECT stannum.jieba_add_word('星河检索协议', 7, 'x'); SELECT stannum.jieba_add_word('{WORD}', 1000000, 'n');")
            assert a.sql(VERSION) == pair
            a.sql("SELECT stannum.jieba_delete_word('星河检索协议'); SELECT stannum.jieba_delete_word('星河检索协议');")
            for word, freq in [('', 0), ('a b', 0), ('a\u3000b', 0), ('x' * 257, 0), ('valid', -1)]:
                a.sql(f"SELECT stannum.jieba_add_word('{word}', {freq});", error='dictionary')
                assert a.sql(VERSION) == changed

            # Permissions, including a database owner who does not own extension objects.
            a.sql('CREATE ROLE dict_reader; CREATE ROLE dict_owner; GRANT USAGE ON SCHEMA stannum TO dict_reader, dict_owner; GRANT SELECT ON docs TO dict_reader;')
            b.sql('SET ROLE dict_reader;')
            assert b.sql(TOKENIZE) == WORD
            b.sql("SELECT stannum.jieba_add_word('denied');", error='superuser or pg_database_owner')
            b.sql('SELECT stannum.jieba_reload_dict();', error='superuser or pg_database_owner')
            b.sql("INSERT INTO stannum.jieba_words VALUES ('denied',0,NULL);", error='permission denied')
            assert b.sql("SELECT current_user;") == 'dict_reader'  # internal owner restored
            b.sql('RESET ROLE;')
            a.sql('ALTER DATABASE postgres OWNER TO dict_owner; SET ROLE dict_owner;')
            a.sql("SELECT stannum.jieba_add_word('授权测试词', 7); SELECT stannum.jieba_delete_word('授权测试词');")
            assert a.sql('SELECT current_user;') == 'dict_owner'
            a.sql('RESET ROLE;')
            # Neither pg_temp nor a caller schema may hijack the private SPI path.
            a.sql("CREATE TEMP TABLE jieba_words(word text, freq int, tag text); INSERT INTO jieba_words VALUES ('伪造词',1,NULL); SET search_path=pg_temp, public;")
            assert a.sql(VERSION) == changed
            a.sql('RESET search_path;')
            # Qualifying the table is not enough: a caller's operator must not
            # run under internal table-owner access during DELETE.
            a.sql("CREATE SCHEMA dict_spoof; CREATE FUNCTION dict_spoof.eq(text,text) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'operator hijacked'; END$$; CREATE OPERATOR dict_spoof.= (LEFTARG=text, RIGHTARG=text, FUNCTION=dict_spoof.eq); SET search_path=dict_spoof,pg_catalog;")
            a.sql("SELECT stannum.jieba_delete_word('nonexistent');")
            a.sql('RESET search_path; DROP SCHEMA dict_spoof CASCADE;')

            # An invalidation arriving DURING a load must survive that load.
            # Reader's first statement has an older snapshot while its relation
            # lock waits for the writer; its next READ COMMITTED statement must
            # refresh rather than remain stuck on the old holder until COMMIT.
            a.sql("BEGIN; LOCK TABLE stannum.jieba_words IN ACCESS EXCLUSIVE MODE; SELECT stannum.jieba_add_word('并发刷新词', 42);")
            b.sql('BEGIN;')
            b.send(VERSION)
            wait_for(observer, "SELECT EXISTS(SELECT FROM pg_stat_activity WHERE application_name='dict-reader' AND wait_event_type='Lock');")
            a.sql('COMMIT;')
            first_snapshot = b.receive()
            assert 'ERROR:' not in first_snapshot, first_snapshot
            assert b.sql(VERSION) == a.sql(VERSION)
            b.sql('COMMIT;')
            a.sql("SELECT stannum.jieba_delete_word('并发刷新词');")
            assert a.sql(VERSION) == changed

            # Cancel a load blocked on the table lock, then use the same backend.
            a.sql('BEGIN; LOCK TABLE stannum.jieba_words IN ACCESS EXCLUSIVE MODE;')
            b.send(VERSION)
            wait_for(observer, "SELECT EXISTS(SELECT FROM pg_stat_activity WHERE application_name='dict-reader' AND wait_event_type='Lock');")
            assert observer.sql("SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE application_name='dict-reader';") == 't'
            assert 'canceling statement' in b.receive()
            a.sql('ROLLBACK;')
            assert b.sql(TOKENIZE) == WORD
            assert b.sql('SELECT current_user;') == 'postgres'

            # Source presets and analyzed scoring semantics.
            a.sql('REINDEX INDEX docs_idx;')
            assert a.sql("SELECT count(*) BETWEEN 150 AND 200 FROM stannum.builtin_stop_words('zh');") == 't'
            assert a.sql("SELECT count(*) FROM stannum.score_inspect('docs_idx', '的', 1.1);") == '0'
            assert a.sql("SELECT bool_and(stannum.score(ctid, dense_ratio => 1.1)=0 AND stannum.full_score(ctid)>0) FROM docs WHERE body ==> '的';") == 't'
            a.sql("SELECT * FROM stannum.builtin_stop_words('missing');", error='expected zh, en, or auto')
            a.sql("SELECT * FROM stannum.index_analysis('docs');", error='requires a stannum index')

            # A standby refreshes from WAL-visible custom rows and repeats drift
            # warnings, but cannot REINDEX. Promotion is the local repair boundary.
            command(['pg_basebackup', '-D', str(standby), '-h', str(root), '-p', '28948', '-U', 'postgres', '-X', 'stream', '-R'])
            with (standby / 'postgresql.conf').open('a') as config:
                config.write('\nport=28949\n')
            command(['pg_ctl', '-D', str(standby), '-l', str(root / 'standby.log'), '-w', 'start'])
            standby_started = True
            replica = Connection(dict(env, PGPORT='28949'), 'dict-standby')
            connections.append(replica)
            assert replica.sql(TOKENIZE) == WORD
            a.sql("SELECT stannum.jieba_add_word('备用检索词', 42);")
            lsn = a.sql('SELECT pg_current_wal_lsn();')
            wait_for(replica, f"SELECT pg_last_wal_replay_lsn() >= '{lsn}'::pg_lsn;")
            assert replica.sql(VERSION) == a.sql(VERSION)
            for _ in range(2):
                output = replica.sql(QUERY)
                assert output.count('WARNING:') == 1 and 'REINDEX' in output, output
            replica.sql('REINDEX INDEX docs_idx;', error='recovery')
            print('Dictionary lifecycle: visibility, drift/REINDEX, rollback, permissions, cancellation, presets, and hot standby passed')
        finally:
            for connection in reversed(connections):
                connection.close()
            if standby_started:
                command(['pg_ctl', '-D', str(standby), '-m', 'immediate', '-w', 'stop'])
            if started:
                command(['pg_ctl', '-D', str(data), '-m', 'immediate', '-w', 'stop'])


if __name__ == '__main__':
    main()
