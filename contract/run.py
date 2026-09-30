#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Run the stannum tool-contract suite against one engine.

    python3 contract/run.py --engine tin|stannum [--dsn-env NAME]
        [--record OUTDIR | --check EXPECTED_DIR | --shape] [--area A] [--case ID]
        [--skip-crash] [--host-note TEXT]

The connection string is read from the environment variable named by
--dsn-env (default CONFORMANCE_DSN), never from the command line. Every run
works in a fresh scratch schema, contract_<random>, which is dropped at
the end, also when the run fails. An empty case list is a successful run:
record and check both report 0 cases and 0 failures, including a missing or
empty expected directory. See contract/README.md.
"""

import argparse
import datetime
import decimal
import fnmatch
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import time

try:
    import psycopg
    import yaml
except ImportError as error:  # pragma: no cover - environment guidance
    sys.exit(f"{error}; install the dependencies with: pip install -r benchmarks/requirements.txt")

RUNNER_VERSION = "2"
SUITE = Path(__file__).resolve().parent
CAPTURES = ("ids", "count", "ranked", "scores", "highlight", "value", "error", "script", "notices")
MANIFEST = SUITE.parent / "docs" / "tool-contract.manifest.json"
JIEBA_WORDS = "stannum.jieba_words"
PAD_ROWS_START = 1000
DEFAULT_SCORE = "{engine}.full_score(ctid)"
DEFAULT_TIMEOUT = "60s"
RECONNECT_SECONDS = 120


class CaseError(Exception):
    """A case file is malformed."""


# ---------------------------------------------------------------- case files


def load_cases(directory):
    """Read every cases/*.yaml file: returns (corpora by name, cases in file order)."""
    corpora, cases, seen = {}, [], set()
    for path in sorted(directory.glob("*.yaml")):
        document = yaml.safe_load(path.read_text()) or {}
        area = document.get("area")
        if not area:
            raise CaseError(f"{path.name}: missing top-level 'area'")
        # cases/<area>.yaml pairs with expected/<engine>-<version>/<area>.json.
        if path.stem != area:
            raise CaseError(f"{path.name}: area {area!r} must be the file name ({area}.yaml)")
        for name, corpus in (document.get("corpora") or {}).items():
            if name in corpora:
                raise CaseError(f"{path.name}: corpus {name!r} is defined twice")
            if not re.fullmatch(r"[a-z][a-z0-9_]*", name):
                raise CaseError(f"{path.name}: corpus name {name!r} must be [a-z][a-z0-9_]*")
            corpora[name] = dict(corpus, name=name, file=path.name)
        defaults = document.get("defaults") or {}
        for raw in document.get("cases") or []:
            case = merge_defaults(defaults, raw)
            case.setdefault("area", area)
            case["file"] = path.name
            validate_case(case, seen)
            seen.add(case["id"])
            cases.append(case)
    for case in cases:
        for name in case_corpora(case):
            if name not in corpora:
                raise CaseError(f"{case['id']}: unknown corpus {name!r}")
    return corpora, cases


def case_corpora(case):
    """The corpora a case uses: its own and any a capture names."""
    names = [case["corpus"]] if case.get("corpus") else []
    names += [capture["corpus"] for capture in case["capture"] if capture.get("corpus")]
    return list(dict.fromkeys(names))


def merge_defaults(defaults, case):
    merged = dict(defaults)
    merged.update(case)
    if "settings" in defaults or "settings" in case:
        merged["settings"] = {**defaults.get("settings", {}), **case.get("settings", {})}
    return merged


def validate_case(case, seen):
    case_id = case.get("id")
    if not case_id or not re.fullmatch(r"[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)+", case_id):
        raise CaseError(f"{case_id!r}: ids are dotted words, e.g. span.then_phrase_operand.1 or catalog.F-02")
    if case_id in seen:
        raise CaseError(f"{case_id}: duplicate case id")
    for field in ("description", "capture"):
        if not case.get(field):
            raise CaseError(f"{case_id}: missing {field!r}")
    if "query" in case and "query_sql" in case:
        raise CaseError(f"{case_id}: give 'query' or 'query_sql', not both")
    case["capture"] = [normalize_capture(case_id, item) for item in case["capture"]]
    names = [capture["as"] for capture in case["capture"]]
    if len(set(names)) != len(names):
        raise CaseError(f"{case_id}: two captures share a name; use 'as' to rename one")
    has_query = any(key in case for key in ("query", "query_sql"))
    for capture in case["capture"]:
        if "query" in capture and "query_sql" in capture:
            raise CaseError(f"{case_id}: capture {capture['as']}: give 'query' or 'query_sql', not both")
        own_query = any(key in capture for key in ("query", "query_sql"))
        needs_query = capture["kind"] in ("ids", "count", "ranked", "scores") and "sql" not in capture
        if needs_query and not (has_query or own_query):
            raise CaseError(f"{case_id}: capture {capture['as']} needs 'query' or 'query_sql'")
        if capture["kind"] == "value" and "sql" not in capture and "sql" not in case:
            raise CaseError(f"{case_id}: capture value needs 'sql'")
        if capture["kind"] == "script":
            steps = capture.get("steps")
            if not steps or not all(isinstance(step, dict) and "sql" in step for step in steps):
                raise CaseError(f"{case_id}: capture script needs 'steps', each with 'sql'")
        if capture["kind"] == "notices" and "sql" not in capture and "sql" not in case:
            raise CaseError(f"{case_id}: capture notices needs 'sql'")
    if case.get("shape"):
        for capture in case["capture"]:
            if "expect" not in capture:
                raise CaseError(f"{case_id}: shape capture {capture['as']} needs embedded 'expect'")
    elif any("expect" in capture for capture in case["capture"]):
        raise CaseError(f"{case_id}: 'expect' is only valid on a shape case")


def normalize_capture(case_id, item):
    """A capture is a name ('ids') or a one-key mapping ({'ranked': {'k': 1}})."""
    if isinstance(item, str):
        kind, params = item, {}
    elif isinstance(item, dict) and len(item) == 1:
        kind, params = next(iter(item.items()))
        params = dict(params or {})
    else:
        raise CaseError(f"{case_id}: capture {item!r} must be a name or a one-key mapping")
    if kind not in CAPTURES:
        raise CaseError(f"{case_id}: unknown capture {kind!r}; known: {', '.join(CAPTURES)}")
    params["kind"] = kind
    params.setdefault("as", kind)
    return params


def selected(cases, areas, patterns):
    chosen = []
    for case in cases:
        if areas and case["area"] not in areas:
            continue
        if patterns and not any(fnmatch.fnmatchcase(case["id"], p) for p in patterns):
            continue
        chosen.append(case)
    return chosen


def crash_tagged(case, engine, version):
    tags = case.get("crashes") or []
    return engine in tags or f"{engine}-{version}" in tags


# ---------------------------------------------------------------- SQL


def literal(text):
    return "'" + str(text).replace("'", "''") + "'"


def substitute(sql, values):
    """Replace {name} placeholders we know; leave any other braces (array literals) alone."""
    def replace(match):
        name = match.group(1)
        return values[name] if name in values else match.group(0)
    previous = None
    while previous != sql:  # placeholders may expand to placeholders ({score} -> {engine})
        previous, sql = sql, re.sub(r"\{([a-z_]+)\}", replace, sql)
    return sql


def jsonable(value):
    if isinstance(value, (bool, int, str)) or value is None:
        return value
    if isinstance(value, float):
        return value
    if isinstance(value, decimal.Decimal):
        return str(value)
    if isinstance(value, (list, tuple)):
        return [jsonable(item) for item in value]
    if isinstance(value, dict):
        return {str(key): jsonable(item) for key, item in value.items()}
    if isinstance(value, (bytes, memoryview)):
        return bytes(value).hex()
    return str(value)


class ConnectionLost(Exception):
    """The session's connection died while running a statement."""


class Session:
    """A working connection plus an idle sentinel that dies when PostgreSQL reinitializes."""

    def __init__(self, dsn, schema):
        self.dsn, self.schema = dsn, schema
        self.connection = self.sentinel = None
        self.connect()

    def open(self):
        return psycopg.connect(self.dsn, connect_timeout=20, application_name="stannum-contract")

    def connect(self):
        self.connection = self.open()
        self.connection.autocommit = True
        if self.schema:
            self.connection.execute(f"SET search_path = {self.schema}, public")
        self.sentinel = self.open()
        self.sentinel.autocommit = True

    def close(self):
        for connection in (self.connection, self.sentinel):
            try:
                if connection is not None:
                    connection.close()
            except Exception:
                pass

    def sentinel_alive(self):
        try:
            self.sentinel.execute("SELECT 1").fetchone()
            return True
        except psycopg.Error:
            return False

    def server_reinitialized(self, grace=5.0):
        """After a lost connection: did the whole server restart? The postmaster
        signals the other backends asynchronously, so give the sentinel a moment."""
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline:
            if not self.sentinel_alive():
                return True
            time.sleep(0.25)
        return False

    def reconnect_after_crash(self, delay=2.0):
        """Wait for the server to finish crash recovery, then open fresh connections."""
        self.close()
        deadline = time.monotonic() + RECONNECT_SECONDS
        time.sleep(delay)
        while True:
            try:
                self.connect()
                return
            except psycopg.OperationalError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(1)

    def run(self, sql, settings, fetch=True):
        """Run sql in its own transaction with SET LOCAL settings; always roll back."""
        connection = self.connection
        try:
            connection.autocommit = False
            try:
                with connection.cursor() as cursor:
                    cursor.execute("SELECT set_config('statement_timeout', %s, true)",
                                   (settings.get("statement_timeout", DEFAULT_TIMEOUT),))
                    for name, value in settings.items():
                        if name != "statement_timeout":
                            cursor.execute("SELECT set_config(%s, %s, true)", (name, str(value)))
                    cursor.execute(sql)
                    rows = cursor.fetchall() if fetch and cursor.description else []
            finally:
                if not connection.closed:
                    connection.rollback()
                    connection.autocommit = True
            return rows
        except psycopg.OperationalError as error:
            if connection.closed or connection.broken:
                raise ConnectionLost(str(error)) from error
            raise


# ---------------------------------------------------------------- engine facts


def engine_facts(session, engine):
    row = session.connection.execute(
        "SELECT extversion FROM pg_extension WHERE extname = %s", (engine,)).fetchone()
    if row is None:
        try:
            session.connection.execute(f"CREATE EXTENSION IF NOT EXISTS {engine}")
        except psycopg.Error as error:
            sys.exit(f"extension {engine} is not installed and could not be created: {error}")
        row = session.connection.execute(
            "SELECT extversion FROM pg_extension WHERE extname = %s", (engine,)).fetchone()
    # Load the library in this backend so _PG_init registers GUCs and the
    # field_weights utility hook. A catalog probe does not load it.
    session.connection.execute(f"LOAD '{engine}'")
    server = session.connection.execute("SELECT version()").fetchone()[0]
    return row[0], server


def suite_commit():
    """Commit of the suite sources. Ignores contract/expected so a later
    recording commit does not change the stamp and a re-record stays
    byte-identical. Appends -dirty when those sources differ from HEAD."""
    repo = SUITE.parent
    paths = [
        "contract/run.py",
        "contract/cases",
        "contract/exclusions.yaml",
        "contract/divergences",
        "contract/README.md",
    ]
    try:
        commit = subprocess.check_output(
            ["git", "log", "-1", "--format=%H", "--", *paths],
            cwd=repo, text=True, stderr=subprocess.DEVNULL).strip()
        dirty = subprocess.check_output(
            ["git", "status", "--porcelain", "--", *paths],
            cwd=repo, text=True, stderr=subprocess.DEVNULL).strip()
        if not commit:
            commit = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=repo, text=True,
                stderr=subprocess.DEVNULL).strip()
        return commit + ("-dirty" if dirty else "")
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def extension_commit():
    """Last commit that touched postgres/, the extension tree this recording ran.
    Appends -dirty when those sources differ from HEAD."""
    repo = SUITE.parent
    try:
        commit = subprocess.check_output(
            ["git", "log", "-1", "--format=%H", "--", "postgres"],
            cwd=repo, text=True, stderr=subprocess.DEVNULL).strip()
        dirty = subprocess.check_output(
            ["git", "status", "--porcelain", "--", "postgres"],
            cwd=repo, text=True, stderr=subprocess.DEVNULL).strip()
        return (commit or "unknown") + ("-dirty" if dirty else "")
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


# ---------------------------------------------------------------- corpora


def corpus_table(name):
    return f"corpus_{name}"


def corpus_values(engine, name, schema=None):
    values = {"engine": engine}
    if name:
        table = corpus_table(name)
        values.update(table=table, index=f"{table}_idx")
    if schema:
        values["schema"] = schema
    return values


def column_names(columns_sql):
    """Names from a CREATE TABLE column list, splitting on commas at depth 0."""
    parts, buf, depth = [], [], 0
    for char in columns_sql:
        if char == "(":
            depth += 1
        elif char == ")":
            depth = max(0, depth - 1)
        if char == "," and depth == 0:
            parts.append("".join(buf))
            buf = []
        else:
            buf.append(char)
    parts.append("".join(buf))
    names = []
    for part in parts:
        token = part.strip()
        if not token:
            continue
        if token.startswith('"'):
            end = token.find('"', 1)
            names.append(token[1:end] if end > 0 else token.strip('"'))
        else:
            names.append(token.split()[0])
    if not names:
        raise CaseError(f"could not parse column names from {columns_sql!r}")
    return names


def row_values(names, row, where):
    if isinstance(row, dict):
        missing = [name for name in names if name not in row]
        if missing:
            raise CaseError(f"{where}: row missing columns {missing}")
        return tuple(row[name] for name in names)
    if not isinstance(row, (list, tuple)) or len(row) != len(names):
        raise CaseError(f"{where}: row {row!r} must have {len(names)} values for {names}")
    return tuple(row)


def build_corpus(session, engine, corpus):
    """Create the corpus table, fill it, and build its index.

    Defaults: columns `id int PRIMARY KEY, body text`, rows matching those
    columns (a list, or a dict keyed by column name). `pad: N` adds
    (1000 + n, 'pad' || n) and is only valid for columns (id, body). One
    index `USING {engine}(body)` is built with `index_options` unless
    `index_sql` replaces it (a list; empty for no index). `setup_sql` runs
    before the index, `after_index_sql` after it. Multi-column corpora must
    set `index_sql`. `index_options` values are spliced verbatim: enum values
    bare, string values already quoted in the YAML.
    """
    values = corpus_values(engine, corpus["name"], session.schema)
    table = values["table"]
    connection = session.connection
    columns = corpus.get("columns") or "id int PRIMARY KEY, body text"
    names = column_names(columns)
    connection.execute(f"CREATE TABLE {table} ({substitute(columns, values)})")
    rows = [row_values(names, row, corpus["name"]) for row in corpus.get("rows") or []]
    pad = int(corpus.get("pad", 0))
    if pad:
        if names != ["id", "body"]:
            raise CaseError(f"{corpus['name']}: pad requires columns id, body; list multi-column rows explicitly")
        rows += [(PAD_ROWS_START + n, f"pad{n}") for n in range(1, pad + 1)]
    if rows:
        quoted = ", ".join(names)
        placeholders = ", ".join(["%s"] * len(names))
        with connection.cursor() as cursor:
            cursor.executemany(f"INSERT INTO {table} ({quoted}) VALUES ({placeholders})", rows)
    if "index_sql" not in corpus and "body" not in names:
        raise CaseError(f"{corpus['name']}: set index_sql; the default index is on body")
    for statement in corpus.get("setup_sql") or []:
        connection.execute(substitute(statement, values))
    if "index_sql" in corpus:
        statements = corpus["index_sql"] or []
    else:
        options = corpus.get("index_options") or {}
        with_clause = ""
        if options:
            with_clause = " WITH (" + ", ".join(f"{key} = {value}" for key, value in options.items()) + ")"
        statements = [f"CREATE INDEX {{index}} ON {{table}} USING {{engine}}(body){with_clause}"]
    for statement in statements:
        connection.execute(substitute(statement, values))
    for statement in corpus.get("after_index_sql") or []:
        connection.execute(substitute(statement, values))
    connection.execute(f"ANALYZE {table}")


# ---------------------------------------------------------------- captures


def capture_sql(case, capture, values):
    kind = capture["kind"]
    if "sql" in capture:
        return capture["sql"]
    if kind == "ids":
        return "SELECT id FROM {table} WHERE body ==> {query} ORDER BY id"
    if kind == "count":
        return "SELECT count(*) FROM {table} WHERE body ==> {query}"
    if kind == "ranked":
        # Tie order is ascending ctid (design §7). Score bits travel with the id so
        # f32::to_bits identity is in the recording, not only a separate scores capture.
        return ("SELECT id, encode(float4send((s)::real), 'hex') FROM ("
                "SELECT id, ctid, {score} AS s FROM {table} WHERE body ==> {query}) ranked "
                "ORDER BY s DESC, ctid ASC LIMIT {k}")
    if kind == "scores":
        return ("SELECT id, encode(float4send(({score})::real), 'hex') FROM {table} "
                "WHERE body ==> {query} ORDER BY id")
    if kind == "highlight":
        return case.get("sql") or "SELECT id, {engine}.highlight(body) FROM {table} WHERE body ==> {query} ORDER BY id"
    if kind in ("value", "error"):
        return case.get("sql") or "SELECT id FROM {table} WHERE body ==> {query} ORDER BY id"
    raise AssertionError(kind)


def shape(kind, rows):
    if kind == "ids":
        return [row[0] for row in rows]
    if kind == "ranked":
        return [[row[0], row[1]] if len(row) > 1 else row[0] for row in rows]
    if kind == "count":
        return rows[0][0]
    if kind == "scores":
        return {str(row[0]): row[1] for row in rows}
    if kind == "highlight":
        return [jsonable(list(row)) for row in rows]
    if len(rows) == 1 and len(rows[0]) == 1:
        return jsonable(rows[0][0])
    return [jsonable(list(row)) for row in rows]


def query_value(owner, values):
    """The SQL for {query}: a literal of 'query', or the expression 'query_sql'."""
    if "query" in owner:
        return literal(owner["query"])
    if "query_sql" in owner:
        return "(" + substitute(owner["query_sql"], values) + ")"
    return None


def error_of(error):
    return {"sqlstate": error.sqlstate, "message": error.diag.message_primary or str(error)}


def run_script(session, capture, values, settings):
    """Ordered steps over named sessions (fresh autocommit connections, closed at
    the end, which rolls back anything left open). Returns one entry per step
    with capture: true: its rows (shaped like value) or its ERROR."""
    connections, results = {}, []
    try:
        for step in capture["steps"]:
            name = str(step.get("session", "a"))
            if name not in connections:
                connection = session.open()
                connection.autocommit = True
                connection.execute(f"SET search_path = {session.schema}, public")
                connection.execute("SELECT set_config('statement_timeout', %s, false)",
                                   (settings.get("statement_timeout", DEFAULT_TIMEOUT),))
                for key, value in settings.items():
                    if key != "statement_timeout":
                        connection.execute("SELECT set_config(%s, %s, false)", (key, str(value)))
                connections[name] = connection
            sql = substitute(step["sql"], values)
            try:
                with connections[name].cursor() as cursor:
                    cursor.execute(sql)
                    rows = cursor.fetchall() if cursor.description else []
                outcome = shape("value", rows) if rows else None
                if step.get("normalize") == "explain_parallel":
                    outcome = explain_parallel_result(rows)
                elif step.get("normalize"):
                    outcome = normalized(step, outcome)
            except psycopg.OperationalError as error:
                if connections[name].closed or connections[name].broken:
                    raise ConnectionLost(str(error)) from error
                outcome = {"error": error_of(error)}
            except psycopg.Error as error:
                outcome = {"error": error_of(error)}
            if step.get("capture", True):
                results.append(outcome)
    finally:
        for connection in connections.values():
            try:
                connection.close()
            except Exception:
                pass
    return results


def run_notices(session, sql, settings):
    """Run sql and return the notice messages it raised (the statement rolls back)."""
    notices = []

    def handler(diag):
        notices.append(diag.message_primary or str(diag))

    session.connection.add_notice_handler(handler)
    try:
        try:
            session.run(sql, settings)
        except psycopg.Error as error:
            return {"error": error_of(error), "notices": list(notices)}
        return list(notices)
    finally:
        session.connection.remove_notice_handler(handler)


def snapshot_jieba(connection):
    """Rows of the extension-global dictionary, so a case can restore them."""
    return connection.execute(
        f"SELECT word, freq, tag FROM {JIEBA_WORDS} ORDER BY word").fetchall()


def restore_jieba(connection, rows):
    """Replace jieba_words in one transaction so a failure cannot empty it."""
    previous = connection.autocommit
    connection.autocommit = False
    try:
        with connection.cursor() as cursor:
            cursor.execute(f"DELETE FROM {JIEBA_WORDS}")
            if rows:
                cursor.executemany(
                    f"INSERT INTO {JIEBA_WORDS} (word, freq, tag) VALUES (%s, %s, %s)",
                    list(rows))
        connection.commit()
    except Exception:
        if not connection.closed:
            connection.rollback()
        raise
    finally:
        if not connection.closed:
            connection.autocommit = previous


def run_capture(session, case, capture, values, settings):
    """Returns the capture's JSON result; an ERROR becomes {"error": {...}}."""
    local = dict(values)
    if capture.get("corpus"):
        local.update(corpus_values(values["engine"], capture["corpus"], values.get("schema")))
    own_query = query_value(capture, local)
    if own_query is not None:
        local["query"] = own_query
    local["score"] = capture.get("score", case.get("score", DEFAULT_SCORE))
    local["k"] = str(int(capture.get("k", case.get("k", 10))))
    settings = {**settings, **{substitute(name, values): value
                               for name, value in (capture.get("settings") or {}).items()}}
    if capture["kind"] == "script":
        return run_script(session, capture, local, settings)
    sql = substitute(capture_sql(case, capture, local), local)
    if capture["kind"] == "notices":
        return run_notices(session, sql, settings)
    try:
        rows = session.run(sql, settings)
    except ConnectionLost:
        raise
    except psycopg.Error as error:
        failure = error_of(error)
        return failure if capture["kind"] == "error" else {"error": failure}
    if capture["kind"] == "error":
        return {"no_error": shape("value", rows)}
    if capture.get("normalize") == "explain_parallel":
        return explain_parallel_result(rows)
    return shape(capture["kind"], rows)


def run_case(session, engine, case):
    """Returns the case record: {"captures": {...}} or {"server_crashed": true, ...}."""
    values = corpus_values(engine, case.get("corpus"), session.schema)
    query = query_value(case, values)
    if query is not None:
        values["query"] = query
    base = {substitute(name, values): value for name, value in (case.get("settings") or {}).items()}
    variants = case.get("variants") or {}
    variant_settings = [
        {**base, **{substitute(name, values): value for name, value in variant.items()}}
        for variant in variants.get("settings") or []
    ]
    varied = set(variants.get("captures") or [])
    captures = {}
    for capture in case["capture"]:
        name = capture["as"]
        if name in varied and variant_settings:
            answers = [run_capture(session, case, capture, values, s) for s in variant_settings]
            if all(answer == answers[0] for answer in answers):
                captures[name] = answers[0]
            else:
                captures[name] = {"variants_disagree": [
                    {"settings": s, "answer": a} for s, a in zip(variant_settings, answers)]}
        else:
            captures[name] = run_capture(session, case, capture, values, base)
    return {"captures": captures}


def execute_case(session, engine, case, risky):
    """Run a case with crash detection; reconnects when the server reinitialized."""
    if risky and not session.sentinel_alive():
        session.reconnect_after_crash()
    try:
        record = run_case(session, engine, case)
        lost = None
    except ConnectionLost as error:
        record, lost = None, str(error)
    if lost is not None:
        crashed = session.server_reinitialized()
    else:
        crashed = risky and not session.sentinel_alive()
    if crashed:
        session.reconnect_after_crash()
        detail = "the sentinel connection died too (the server reinitialized)"
        return {"server_crashed": True, "detail": detail}
    if lost is not None:
        # The backend died but the server did not reinitialize (terminated from outside).
        session.reconnect_after_crash(delay=0)
        return {"captures": {"connection_lost": lost}}
    return record


# ---------------------------------------------------------------- comparison


def normalized(capture, value):
    """Apply a declared normalization before comparison.

    `version` replaces a non-empty string with `<version>`. `explain_parallel`
    is reduced once, at capture time; an already-reduced object is not
    re-derived (that regexed the reduced form and was always false).
    """
    mode = capture.get("normalize")
    if mode in (None, "none", "message", "ignore_message"):
        return value
    if mode == "version":
        return "<version>" if isinstance(value, str) and value else value
    if mode == "explain_parallel":
        if isinstance(value, dict) and set(value) == {"has_parallel_worker"}:
            return value
        return explain_parallel_result(value if isinstance(value, list) else [value])
    raise CaseError(f"unknown normalize {mode!r}")


def explain_line_has_worker(line):
    """A planned parallel worker, not 'Parallel Aware' or 'Workers Planned: 0'."""
    text = str(line).strip()
    if re.search(r"Workers Planned:\s*[1-9]", text):
        return True
    node = re.sub(r"^->\s*", "", text)
    if re.match(r"Gather\b", node):
        return True
    if re.match(r"Parallel (?!Aware\b)", node):
        return True
    return False


def has_parallel_worker(value):
    if isinstance(value, dict) and set(value) == {"has_parallel_worker"}:
        return bool(value["has_parallel_worker"])
    lines = value if isinstance(value, list) else [value]
    return any(explain_line_has_worker(line[0] if isinstance(line, (list, tuple)) and line else line)
               for line in lines)


def explain_parallel_result(rows):
    lines = []
    for row in rows or []:
        lines.append(row[0] if isinstance(row, (list, tuple)) and row else row)
    return {"has_parallel_worker": any(explain_line_has_worker(line) for line in lines)}


def compare_capture(case, capture, want, got):
    """Returns (status, detail) for one capture: PASS, DIFF (compatible) or FAIL."""
    if capture["kind"] == "error":
        if "no_error" in got:
            if "no_error" in want and want == got:
                return "PASS", ""
            return "FAIL", f"expected ERROR {want.get('sqlstate')}, got an answer {got['no_error']!r:.80}"
        if "no_error" in want:
            return "FAIL", f"expected answer {want['no_error']!r:.80}, got ERROR {got['sqlstate']}"
        return compare_errors(capture.get("message_prefix"), want, got, message_mode(capture))
    if isinstance(want, dict) and "error" in want and isinstance(got, dict) and "error" in got:
        return compare_errors(capture.get("message_prefix"), want["error"], got["error"],
                              message_mode(capture))
    if capture.get("identity_set"):
        return compare_identity_set(capture, want, got)
    want_n, got_n = normalized(capture, want), normalized(capture, got)
    if want_n == got_n:
        return "PASS", ""
    mode = message_mode(capture)
    if mode == "ignore" and without_messages(want) == without_messages(got):
        return "PASS", ""
    if mode == "prefix":
        if prefix_tree_matches(want, got, capture.get("message_prefix")):
            return "PASS", ""
        return "FAIL", f"{capture['as']}: nested ERROR lacks prefix {capture.get('message_prefix')!r}"
    if without_messages(want) == without_messages(got):
        return "FAIL", f"{capture['as']}: same answers and SQLSTATEs, different ERROR messages"
    return "FAIL", f"{capture['as']}: got {brief(got)}, recorded {brief(want)}"


def row_list(value):
    if not isinstance(value, list):
        return None
    rows = []
    for row in value:
        rows.append(tuple(row) if isinstance(row, list) else (row,))
    return rows


def compare_identity_set(capture, want, got):
    """Recorded rows must all be present. Extra rows fail, and only a divergence
    entry's expected.extra may document that addition."""
    want_rows, got_rows = row_list(want), row_list(got)
    if want_rows is None or got_rows is None:
        return "FAIL", f"{capture['as']}: identity_set capture is not a row list"
    missing = [list(row) for row in want_rows if row not in got_rows]
    extra = [list(row) for row in got_rows if row not in want_rows]
    if missing:
        return "FAIL", f"{capture['as']}: identity set lost rows: {brief(missing)}"
    if extra:
        return "FAIL", f"{capture['as']}: identity set gained rows: {brief(extra)}"
    return "PASS", ""


def walk_errors(value):
    found = []

    def walk(node):
        if isinstance(node, dict):
            if "sqlstate" in node and "message" in node:
                found.append(node)
            for key, item in node.items():
                if key != "error":
                    walk(item)
            if isinstance(node.get("error"), dict):
                walk(node["error"])
        elif isinstance(node, list):
            for item in node:
                walk(item)

    walk(value)
    return found


def prefix_tree_matches(want, got, prefix):
    if without_messages(want) != without_messages(got):
        return False
    needed = prefix or ""
    return all(str(payload.get("message") or "").startswith(needed) for payload in walk_errors(got))


def without_messages(value):
    """The value with every ERROR's message removed, keeping its SQLSTATE."""
    if isinstance(value, dict):
        if "error" in value and isinstance(value["error"], dict):
            return {"error": {"sqlstate": value["error"].get("sqlstate")}}
        return {key: without_messages(item) for key, item in value.items()}
    if isinstance(value, list):
        return [without_messages(item) for item in value]
    return value


def message_mode(capture):
    """How ERROR text is compared. Default is exact, and a mismatch FAILs.

    `message_prefix` passes when the live message starts with that prefix.
    `normalize: message` (or `ignore_message`) compares SQLSTATE only. A
    divergence entry's `expected` text can consume a remaining mismatch.
    """
    if capture.get("normalize") in ("message", "ignore_message"):
        return "ignore"
    if capture.get("message_prefix") is not None:
        return "prefix"
    return "exact"


def compare_errors(prefix, want, got, mode="exact"):
    if got.get("sqlstate") != want.get("sqlstate"):
        return "FAIL", f"ERROR {got.get('sqlstate')} {got.get('message')!r:.60}, recorded {want.get('sqlstate')}"
    if mode == "ignore":
        return "PASS", ""
    if mode == "prefix" or prefix is not None:
        needed = prefix if prefix is not None else ""
        if not str(got.get("message") or "").startswith(needed):
            return "FAIL", f"message {got.get('message')!r:.60} lacks prefix {needed!r}"
        if mode == "prefix":
            return "PASS", ""
    if got.get("message") != want.get("message"):
        return "FAIL", (f"same SQLSTATE {got.get('sqlstate')}, message "
                         f"{got.get('message')!r:.60} vs {want.get('message')!r:.60}")
    return "PASS", ""


def brief(value):
    text = json.dumps(value, sort_keys=True)
    return text if len(text) <= 100 else text[:97] + "..."


def only_errors(record):
    captures = record.get("captures") or {}
    return captures and all(isinstance(v, dict) and ("error" in v or "sqlstate" in v)
                            for v in captures.values())


def capture_set_mismatch(case, want, got):
    """Three-way equality of capture names. A divergence must not consume this."""
    case_names = [capture["as"] for capture in case["capture"]]
    want_names = set((want or {}).get("captures") or {})
    got_names = set((got or {}).get("captures") or {})
    if set(case_names) != want_names or set(case_names) != got_names:
        return "FAIL", (
            f"capture set mismatch; case {case_names}; recorded {sorted(want_names)}; "
            f"live {sorted(got_names)}")
    return None


def compare_case(case, want, got):
    if "corpus_error" in got or "corpus_error" in want:
        if "corpus_error" not in got:
            return "FAIL", f"recorded engine could not build the corpus: {brief(want['corpus_error'])}"
        if "corpus_error" not in want:
            return "FAIL", f"could not build the corpus: {brief(got['corpus_error'])}"
        return compare_errors(None, want["corpus_error"], got["corpus_error"])
    if got.get("server_crashed"):
        return "FAIL", "the server crashed" + (" (as the recorded engine did)" if want.get("server_crashed") else "")
    if "connection_lost" in (got.get("captures") or {}):
        return "FAIL", f"connection lost: {got['captures']['connection_lost']}"
    if want.get("server_crashed"):
        # The recorded engine crashed: an ERROR or a correct answer passes; a crash fails.
        if only_errors(got):
            states = sorted({(v.get("error") or v)["sqlstate"] for v in got["captures"].values()})
            return "PASS", f"recorded engine crashed; answered ERROR {', '.join(states)}"
        expect = case.get("expect_if_answered")
        if expect is None:
            return "PASS", "recorded engine crashed; answered (no reference answer to check)"
        wrong = [name for name, value in expect.items()
                 if name in got["captures"] and got["captures"][name] != value]
        if wrong:
            return "FAIL", "recorded engine crashed; answered wrongly: " + ", ".join(
                f"{name} {brief(got['captures'][name])}, correct {brief(expect[name])}" for name in wrong)
        return "PASS", "recorded engine crashed; answered correctly"
    mismatch = capture_set_mismatch(case, want, got)
    if mismatch:
        return mismatch
    statuses, details = [], []
    for capture in case["capture"]:
        name = capture["as"]
        status, detail = compare_capture(case, capture, want["captures"][name], got["captures"][name])
        statuses.append(status)
        if detail:
            details.append(detail)
    for status in ("FAIL", "DIFF"):
        if status in statuses:
            return status, "; ".join(details)
    return "PASS", ""


def compare_shape(case, got):
    """Compare a shape case to the spec embedded in its captures. No recording."""
    if "corpus_error" in got:
        return "FAIL", f"could not build the corpus: {brief(got['corpus_error'])}"
    if got.get("server_crashed"):
        return "FAIL", "the server crashed"
    if "connection_lost" in (got.get("captures") or {}):
        return "FAIL", f"connection lost: {got['captures']['connection_lost']}"
    for capture in case["capture"]:
        name = capture["as"]
        live = (got.get("captures") or {}).get(name)
        if isinstance(live, dict) and "error" in live:
            error = live["error"]
            return "FAIL", f"{name}: ERROR {error.get('sqlstate')} {error.get('message')}"
        expect = capture.get("expect")
        if expect is None:
            return "FAIL", f"{name}: shape capture has no embedded expect"
        if normalized(capture, expect) != normalized(capture, live):
            return "FAIL", f"{name}: got {brief(live)}, spec {brief(expect)}"
    return "PASS", "shape"


# ---------------------------------------------------------------- divergences

DIVERGENCE_KINDS = {"improvement": "IMPROVED", "gap": "GAP"}


def read_divergences(engine, recorded):
    """The documented divergences of `engine` from the recorded engine-version:
    {case id: entry}, from divergences/<engine>.yaml."""
    path = SUITE / "divergences" / f"{engine}.yaml"
    if not path.exists():
        return {}
    chosen = {}
    for entry in (yaml.safe_load(path.read_text()) or {}).get("divergences") or []:
        if not entry.get("captures"):
            raise CaseError(f"{path}: {entry.get('id')}: list the captures that diverge")
        expected = entry.get("expected")
        if not isinstance(expected, dict) or not expected:
            raise CaseError(f"{path}: {entry.get('id')}: expected is required and must document the difference")
        missing = set(entry["captures"]) - set(expected)
        if missing:
            raise CaseError(f"{path}: {entry.get('id')}: expected missing captures {sorted(missing)}")
        if entry.get("kind") not in DIVERGENCE_KINDS:
            raise CaseError(f"{path}: {entry.get('id')}: kind must be one of {', '.join(DIVERGENCE_KINDS)}")
        if entry.get("against") == recorded:
            chosen[entry["id"]] = entry
    return chosen


def identity_delta(want, got):
    want_rows, got_rows = row_list(want), row_list(got)
    if want_rows is None or got_rows is None:
        return [want], [got]
    missing = [list(row) for row in want_rows if row not in got_rows]
    extra = [list(row) for row in got_rows if row not in want_rows]
    return missing, extra


def error_payload(value):
    """The {{sqlstate, message}} dict, whether the capture is kind=error or wrapped."""
    if isinstance(value, dict) and "sqlstate" in value and "message" in value:
        return value
    if isinstance(value, dict) and isinstance(value.get("error"), dict):
        return value["error"]
    return None


def is_error(value):
    return isinstance(value, dict) and ("error" in value or "sqlstate" in value)


def recorded_an_error(value):
    """The recorded engine raised an ERROR here, in every variant or in some."""
    if is_error(value):
        return True
    if isinstance(value, dict) and "variants_disagree" in value:
        return any(is_error(variant.get("answer")) for variant in value["variants_disagree"])
    return False


def compare_divergent(case, want, got, entry):
    """Checks a documented divergence: exactly the listed captures differ, and
    for an improvement each is one the recorded engine refused and this one
    answered. Anything else fails, so the file cannot hide a regression."""
    mismatch = capture_set_mismatch(case, want, got)
    if mismatch:
        return mismatch
    listed = set(entry["captures"])
    captures = {capture["as"]: capture for capture in case["capture"]}
    unknown = listed - set(captures)
    if unknown:
        return "FAIL", f"divergence lists unknown captures: {', '.join(sorted(unknown))}"
    differing = set()
    for name, capture in captures.items():
        if name not in (want.get("captures") or {}) or name not in (got.get("captures") or {}):
            return "FAIL", f"capture set mismatch at {name}; a divergence cannot hide that"
        status, detail = compare_capture(case, capture, want["captures"][name], got["captures"][name])
        if status == "FAIL":
            if name not in listed:
                return "FAIL", detail
            differing.add(name)
    if differing != listed:
        return "FAIL", ("documented divergence no longer holds for "
                        + ", ".join(sorted(listed - differing)) + "; update divergences/")
    expected = entry["expected"]
    for name in sorted(listed):
        spec = expected[name]
        capture = captures[name]
        if capture.get("identity_set"):
            missing, extra = identity_delta(want["captures"][name], got["captures"][name])
            if missing:
                return "FAIL", f"{name}: identity set lost rows; a divergence cannot hide that: {brief(missing)}"
            documented = spec.get("extra")
            if documented is None:
                return "FAIL", f"{name}: identity-set divergence must set expected.extra to the added rows"
            if extra != documented:
                return "FAIL", f"{name}: gained rows do not match expected.extra: {brief(extra)}"
            continue
        payload = error_payload(got["captures"][name])
        if payload is None:
            return "FAIL", f"{name}: divergence expected an ERROR, got {brief(got['captures'][name])}"
        prefix = spec.get("message_prefix")
        mode = "prefix" if prefix is not None else "exact"
        want_error = {"sqlstate": spec.get("sqlstate", payload.get("sqlstate")),
                      "message": spec.get("message", payload.get("message"))}
        if "sqlstate" in spec and payload.get("sqlstate") != spec["sqlstate"]:
            return "FAIL", f"{name}: divergence expected SQLSTATE {spec['sqlstate']}, got {payload.get('sqlstate')}"
        status, detail = compare_errors(prefix, want_error, payload, mode)
        if status != "PASS":
            return "FAIL", f"{name}: divergence expected error text not produced: {detail}"
    if entry["kind"] == "improvement":
        for name in sorted(listed):
            recorded, answer = want["captures"][name], got["captures"][name]
            if not recorded_an_error(recorded) or is_error(answer):
                return "FAIL", f"{name}: an improvement must answer where the recorded engine raised an ERROR"
    return DIVERGENCE_KINDS[entry["kind"]], entry["summary"]


# ---------------------------------------------------------------- expected files


def read_expected(directory):
    source, records = None, {}
    files = sorted(Path(directory).glob("*.json"))
    if not files:
        sys.exit(f"no recorded answers (*.json) in {directory}")
    for path in files:
        document = json.loads(path.read_text())
        header = document.get("source") or {}
        if source is None:
            source = header
        elif (header.get("engine"), header.get("extension_version")) != (
                source.get("engine"), source.get("extension_version")):
            sys.exit(f"{path}: recorded from {header.get('engine')} {header.get('extension_version')}, "
                     f"but other files in {directory} from {source.get('engine')} {source.get('extension_version')}")
        for record in document.get("cases") or []:
            records[record["id"]] = dict(record, recorded_in=path.name, source=header)
    return source, records


def header_mismatch(path, header):
    """Refuse to merge a file recorded from a different engine or version."""
    existing = json.loads(path.read_text()).get("source") or {}
    for key in ("engine", "extension_version"):
        if existing.get(key) != header.get(key):
            return (f"{path}: refusing --merge: existing {key}={existing.get(key)!r}, "
                    f"this run {header.get(key)!r}")
    return None


def write_expected(directory, header, results, cases, merge):
    directory.mkdir(parents=True, exist_ok=True)
    by_area = {}
    for case in cases:
        if case["id"] in results:
            by_area.setdefault(case["area"], []).append(dict(id=case["id"], **results[case["id"]]))
    if merge:
        for area in by_area:
            path = directory / f"{area}.json"
            if path.exists():
                mismatch = header_mismatch(path, header)
                if mismatch:
                    sys.exit(mismatch)
    written = []
    for area, records in by_area.items():
        path = directory / f"{area}.json"
        if merge and path.exists():
            kept = {r["id"]: r for r in json.loads(path.read_text()).get("cases") or []}
            kept.update({r["id"]: r for r in records})
            order = [c["id"] for c in cases if c["id"] in kept]
            records = [kept[i] for i in order] + [r for i, r in kept.items() if i not in order]
        path.write_text(json.dumps({"source": header, "cases": records}, indent=1) + "\n")
        written.append(path)
    return written


# ---------------------------------------------------------------- exclusions / coverage


def load_exclusions(path=None):
    """Case ids deliberately not recorded, plus object-coverage waivers.

    ``excluded`` is a list of ``{id, reason}`` (or a mapping of id to reason).
    An excluded case skips in check mode and does not fail for a missing
    recording. ``engines``, when set, scopes that skip to those live
    extension versions during ``--check`` and ``--record`` only; other
    engines still run the case. ``objects`` waives manifest coverage.
    """
    path = Path(path) if path else SUITE / "exclusions.yaml"
    if not path.exists():
        return {}, {}
    document = yaml.safe_load(path.read_text()) or {}
    cases, objects = {}, {}
    raw = document.get("excluded") or []
    if isinstance(raw, dict):
        raw = [{"id": key, "reason": value} for key, value in raw.items()]
    for entry in raw:
        if isinstance(entry, str):
            sys.exit(f"{path}: exclusion {entry!r} needs a reason")
        case_id, reason = entry.get("id"), entry.get("reason")
        if not case_id or not reason:
            sys.exit(f"{path}: each exclusion needs id and reason")
        extra = set(entry) - {"id", "reason", "engines"}
        if extra:
            sys.exit(f"{path}: exclusion {case_id} has unknown keys {sorted(extra)}")
        engines = entry.get("engines")
        if engines is not None:
            if (not isinstance(engines, list) or not engines
                    or not all(isinstance(item, str) and item for item in engines)):
                sys.exit(f"{path}: exclusion {case_id}: engines must be a non-empty list of versions")
            engines = tuple(engines)
        if case_id in cases:
            sys.exit(f"{path}: duplicate exclusion {case_id}")
        cases[case_id] = {"reason": reason, "engines": engines}
    for entry in document.get("objects") or []:
        object_id, reason = entry.get("id"), entry.get("reason")
        if not object_id or not reason:
            sys.exit(f"{path}: each objects entry needs id and reason")
        if object_id in objects:
            sys.exit(f"{path}: duplicate object exclusion {object_id}")
        objects[object_id] = reason
    return cases, objects


def required_objects(manifest):
    """Design §7 coverage ids: functions, reloptions, GUCs, operators,
    opclass, SUPPORT attachments, jieba_words ACL, index_health security_invoker."""
    required = []
    for function in manifest.get("functions") or []:
        required.append(f"function:{function['identity']}")
        if function.get("support"):
            required.append(f"support:{function['identity']}->{function['support']}")
        if function.get("acl"):
            required.append(f"acl:function:{function['identity']}")
    for operator in manifest.get("operators") or []:
        required.append(f"operator:{operator['identity']}")
        if operator.get("procedure_support"):
            required.append(f"support:operator:{operator['identity']}->{operator['procedure_support']}")
        if operator.get("restrict"):
            required.append(f"restrict:operator:{operator['identity']}->{operator['restrict']}")
    for reloption in manifest.get("reloptions") or []:
        required.append(f"reloption:{reloption['name']}")
    for guc in manifest.get("gucs") or []:
        required.append(f"guc:{guc['name']}")
    for opclass in manifest.get("operator_classes") or []:
        required.append(f"opclass:{opclass['name']}")
    for table in manifest.get("tables") or []:
        if table.get("acl"):
            required.append(f"acl:table:{table.get('schema', 'stannum')}.{table['name']}")
    for view in manifest.get("views") or []:
        if (view.get("options") or {}).get("security_invoker"):
            required.append(f"view:{view['name']}.security_invoker")
    return required


def coverage_gaps(cases, waived):
    """Object ids in the manifest that no case covers and no waiver lists."""
    if not MANIFEST.exists():
        return []
    manifest = json.loads(MANIFEST.read_text())
    covered = set(waived)
    for case in cases:
        for object_id in case.get("covers") or []:
            covered.add(object_id)
    return [object_id for object_id in required_objects(manifest) if object_id not in covered]


# ---------------------------------------------------------------- main


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--engine", required=True, choices=["tin", "stannum"])
    parser.add_argument("--dsn-env", default="CONFORMANCE_DSN",
                        help="environment variable holding the connection string (default CONFORMANCE_DSN)")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--record", metavar="OUTDIR", help="write answers to OUTDIR/<engine>-<version>/<area>.json")
    mode.add_argument("--check", metavar="EXPECTED_DIR", help="compare answers with a recorded directory")
    mode.add_argument("--shape", action="store_true",
                      help="run shape-gated cases against the spec embedded in the case; no recording")
    parser.add_argument("--area", action="append", default=[], help="only this area (repeatable)")
    parser.add_argument("--case", action="append", default=[], help="only this case id or glob (repeatable)")
    parser.add_argument("--skip-crash", action="store_true",
                        help="skip cases tagged as crashing the engine under test")
    parser.add_argument("--merge", action="store_true",
                        help="merge --record into an existing file; refuse if its engine/version differs")
    parser.add_argument("--host-note", default="", help="free-text host description for the source header")
    parser.add_argument("--cases-dir", default=str(SUITE / "cases"), help=argparse.SUPPRESS)
    args = parser.parse_args()

    dsn = os.environ.get(args.dsn_env)
    if not dsn:
        sys.exit(f"set the connection string in the environment variable {args.dsn_env}")
    try:
        corpora, all_cases = load_cases(Path(args.cases_dir))
    except CaseError as error:
        sys.exit(f"invalid case file: {error}")
    cases = selected(all_cases, set(args.area), args.case)
    if args.shape:
        if not args.area and not args.case:
            cases = [case for case in cases if case.get("shape")]
        else:
            plain = [case["id"] for case in cases if not case.get("shape")]
            if plain:
                sys.exit("--shape cannot run non-shape cases: " + ", ".join(plain))
        if not cases:
            sys.exit("no shape case matches the --area/--case filters")
    elif not cases and all_cases:
        sys.exit("no case matches the --area/--case filters")

    expected_source, expected, divergences = (None, {}, {})
    expected_dir = Path(args.check) if args.check else None
    has_recordings = bool(args.check) and expected_dir.is_dir() and any(expected_dir.glob("*.json"))
    if args.check and (cases or has_recordings):
        expected_source, expected = read_expected(args.check)
        try:
            divergences = read_divergences(
                args.engine, f"{expected_source.get('engine')}-{expected_source.get('extension_version')}")
        except CaseError as error:
            sys.exit(f"invalid divergence file: {error}")
        print(f"Comparing against {expected_source.get('engine')} {expected_source.get('extension_version')} "
              f"recorded in {args.check}")
        for key in ("postgres", "server_version", "host", "measured", "date", "measured_by", "imported_from"):
            if expected_source.get(key):
                print(f"  {key}: {expected_source[key]}")

    excluded, waived_objects = load_exclusions()
    gaps = coverage_gaps(all_cases, waived_objects) if all_cases else []
    if gaps:
        print(f"Coverage gaps ({len(gaps)}): " + ", ".join(gaps), file=sys.stderr)

    schema = f"contract_{secrets.token_hex(4)}"
    session = Session(dsn, None)
    schema_created = False
    jieba_baseline = None
    version = server = None
    results, rows = {}, []
    width = max((len(case["id"]) for case in cases), default=0)

    def emit(status, case_id, detail=""):
        rows.append((status, case_id, detail))
        print(f"{status:<8} {case_id:<{width}}  {detail}".rstrip(), flush=True)

    print()
    try:
        version, server = engine_facts(session, args.engine)
        print(f"Engine under test: {args.engine} {version}; {server}")
        jieba_baseline = snapshot_jieba(session.connection)
        session.connection.execute(f"CREATE SCHEMA {schema}")
        schema_created = True
        session.schema = schema
        session.connection.execute(f"SET search_path = {schema}, public")
        print(f"Scratch schema: {schema}")

        def skip_reason(case):
            entry = excluded.get(case["id"])
            if entry:
                engines = entry["engines"]
                # Global exclusions always skip. Engine-scoped ones skip only a
                # versioned replay of that extension version (design §7 item 3).
                if engines is None or ((args.check or args.record) and version in engines):
                    return "EXCLUDED", f"excluded: {entry['reason']}"
            if crash_tagged(case, args.engine, version) and args.skip_crash:
                return "SKIP", f"tagged crashes: {args.engine}-{version}"
            return None

        corpus_errors = {}
        runnable = [case for case in cases if skip_reason(case) is None]
        for name in dict.fromkeys(name for case in runnable for name in case_corpora(case)):
            try:
                build_corpus(session, args.engine, corpora[name])
            except psycopg.Error as error:
                corpus_errors[name] = dict(error_of(error), corpus=name)
        for case in cases:
            tagged = crash_tagged(case, args.engine, version)
            reason = skip_reason(case)
            if reason:
                emit(reason[0], case["id"], reason[1])
                continue
            if args.check and case["id"] not in expected and not case.get("shape"):
                emit("FAIL", case["id"], "no expectation recorded")
                continue
            failed = [corpus_errors[name] for name in case_corpora(case) if name in corpus_errors]
            if failed:
                record = {"corpus_error": failed[0]}
            else:
                record = execute_case(session, args.engine, case, risky=bool(case.get("risky") or tagged))
            results[case["id"]] = record
            if args.check and case["id"] in expected:
                status, detail = compare_case(case, expected[case["id"]], record)
                entry = divergences.get(case["id"])
                # A capture-set mismatch is structural. Divergence dispatch must
                # not replace it with GAP/IMPROVED.
                if entry and status != "PASS" and not capture_set_mismatch(case, expected[case["id"]], record):
                    status, detail = compare_divergent(case, expected[case["id"]], record, entry)
                elif entry and status == "PASS":
                    status, detail = "FAIL", "matches the recorded engine; remove the documented divergence"
                emit(status, case["id"], detail)
            elif case.get("shape"):
                # 0.5.0-only gate (design §7 item 3): the spec is in the case, not a recording.
                status, detail = compare_shape(case, record)
                emit(status, case["id"], detail)
            elif args.check:
                emit("FAIL", case["id"], "no expectation recorded")
            else:
                if "corpus_error" in record:
                    error = record["corpus_error"]
                    emit("ERROR", case["id"], f"corpus {error['corpus']}: {error['sqlstate']} {error['message']}")
                elif record.get("server_crashed"):
                    emit("CRASH", case["id"], record.get("detail", ""))
                elif "connection_lost" in record["captures"]:
                    emit("LOST", case["id"], record["captures"]["connection_lost"])
                else:
                    errors = [n for n, v in record["captures"].items()
                              if isinstance(v, dict) and ("error" in v or "sqlstate" in v or "variants_disagree" in v)]
                    emit("OK", case["id"], ("ERROR or disagreement in: " + ", ".join(errors)) if errors else "")
            if jieba_baseline is not None:
                restore_jieba(session.connection, jieba_baseline)
    finally:
        try:
            if not session.sentinel_alive() or session.connection.closed:
                session.reconnect_after_crash()
            if jieba_baseline is not None:
                restore_jieba(session.connection, jieba_baseline)
        except Exception as error:
            print(f"WARNING: could not restore jieba_words: {error}", file=sys.stderr)
        try:
            if schema_created:
                if not session.sentinel_alive() or session.connection.closed:
                    session.reconnect_after_crash()
                session.connection.execute(f"DROP SCHEMA IF EXISTS {schema} CASCADE")
                print(f"Dropped scratch schema {schema}")
        except Exception as error:  # report, but do not mask the run's own failure
            print(f"WARNING: could not drop scratch schema {schema}: {error}", file=sys.stderr)
        session.close()

    counts = {}
    for status, _, _ in rows:
        counts[status] = counts.get(status, 0) + 1
    print()
    if not rows:
        print("Summary: 0 cases / 0 failures")
    else:
        print("Summary: " + ", ".join(f"{counts[s]} {s}" for s in sorted(counts)) + f" of {len(rows)} cases")
    if args.check:
        unknown = sorted(set(expected) - {case["id"] for case in all_cases}) if all_cases else []
        if unknown:
            print(f"Recorded answers without a case in the suite: {', '.join(unknown)}")
        if divergences:
            print(f"IMPROVED and GAP are divergences documented in divergences/{args.engine}.yaml")
        if expected_source is not None:
            print(f"Compared {args.engine} {version} against {expected_source.get('engine')} "
                  f"{expected_source.get('extension_version')} ({args.check})")
        failed = (counts.get("FAIL", 0) + counts.get("DIFF", 0) + counts.get("SKIP", 0)
                  + len(unknown) + len(gaps))
        return 1 if failed else 0
    if args.record and cases and version is not None:
        header = {
            "engine": args.engine,
            "extension_version": version,
            "server_version": server,
            "host": args.host_note,
            "suite_commit": suite_commit(),
            "extension_commit": extension_commit(),
            "runner_version": RUNNER_VERSION,
        }
        directory = Path(args.record) / f"{args.engine}-{version}"
        written = write_expected(directory, header, results, cases,
                                 merge=bool(args.merge or args.case or args.area))
        for path in written:
            print(f"Recorded {path}")
    record_bad = (counts.get("CRASH", 0) + counts.get("ERROR", 0) + counts.get("LOST", 0)
                  + counts.get("FAIL", 0) + len(gaps))
    return 1 if record_bad else 0


if __name__ == "__main__":
    sys.exit(main())
