#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""STN4 C.2 per-field-payload latency GATE harness (Item 1).

Sibling of ``benchmarks/stn3_fielded_poc.py``. Imports the 4.6 English
generator, quantiles, dump/breakdown helpers, and the shared STNF prefix
parser (v1 + v2). Adds a locked Chinese corpus, jieba index SQL, channel-
directory labeling, and dual-corpus merge whose top-level decision is
stay|escalate.

Subcommands:

  generate   write or hash a locked CSV (english|chinese; QUOTE_ALL, LF)
  run        load/index/search one system using PG* (writes a result fragment)
  merge      combine fragments, apply per-corpus gates, write the POC JSON
"""
from __future__ import annotations

import argparse
import csv
import hashlib
import io
import json
import os
from pathlib import Path
import random
import re
import subprocess
import sys
import tempfile
import time

import stn3_fielded_poc as stn3

ROOT = stn3.ROOT
DEFAULT_QUERIES_EN = stn3.DEFAULT_QUERIES
DEFAULT_QUERIES_ZH = ROOT / "docs" / "benchmarks" / "stn4-per-field-poc-queries-zh.txt"
DEFAULT_JSON = ROOT / "docs" / "benchmarks" / "stn4-per-field-poc.json"
ZH_VOCAB_PATH = Path(__file__).with_name("stn4_zh_vocab.txt")
COMPARISON_4_6_JSON = ROOT / "docs" / "benchmarks" / "stn3-fielded-poc.json"

ENGLISH_SHA256 = "52775c8a17077f09f028f5758eb53df65f94dc37c5a1f3e800990f5a39f0bbeb"
CHINESE_SHA256 = "0b2e37b54dde76b3cd59603681dd4580b3b36f690f738f22530489a483141dd4"
ZH_ROWS = stn3.ROWS
ZH_SEED = 20261003
SYSTEMS = ("stn4_multi", "stn3_single", "v040")
LANGS = ("english", "chinese")
MANDATORY_THRESHOLD = stn3.MANDATORY_THRESHOLD
ADVISORY_P50_THRESHOLD = stn3.ADVISORY_P50_THRESHOLD
DECISION_SCOPE = (
    "per-corpus `decision` reports the MANDATORY dictionary/build gate only; "
    "`c2_complete` additionally requires the advisory p50 gate to pass or carry a "
    "named waiver in this corpus object"
)
JIEBA_RS_VERSION = (0 << 16) | (7 << 8) | 4
JIEBA_EMPTY_DICT_FINGERPRINT = 0x6855A0736155F3DD
FIELD_WEIGHTS = "defaults"
CHANNEL_DIRECTORY_NOTE = (
    "informational, not in dict_bytes. Channel directory lives in posting "
    "areas (ordinals_head + payload directory). Parsed from breakdown 'all' "
    "row ord_head when present; otherwise null and item 2 fills it from live "
    "breakdown. Do not invent a number."
)
JIEBA_NOTE = (
    "empty jieba_words table pin 0x6855a0736155f3dd; jieba_rs_version packed "
    "(0<<16)|(7<<8)|4. Item 2 overwrites from index_analysis / runtime fingerprint."
)
BREAKDOWN_ALL_ROW = re.compile(
    r"^  all\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\b",
    re.M,
)


def load_zh_vocab(path=ZH_VOCAB_PATH):
    words = []
    for raw in Path(path).read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        words.append(line)
    return tuple(words)


ZH_VOCAB = load_zh_vocab()
if len(ZH_VOCAB) != 2048 or len(set(ZH_VOCAB)) != 2048:
    raise RuntimeError(
        f"locked Chinese vocab must be 2048 unique words, got {len(ZH_VOCAB)} "
        f"({len(set(ZH_VOCAB))} unique)"
    )


def zh_generate_rows(rows=ZH_ROWS, seed=ZH_SEED):
    rng = random.Random(seed)
    weights = stn3.zipf_weights(len(ZH_VOCAB))
    out = []
    for doc_id in range(1, rows + 1):
        n_title = rng.randint(*stn3.TITLE_TOKENS)
        n_body = rng.randint(*stn3.BODY_TOKENS)
        title = " ".join(rng.choices(ZH_VOCAB, weights=weights, k=n_title))
        body = " ".join(rng.choices(ZH_VOCAB, weights=weights, k=n_body))
        out.append((str(doc_id), title, body))
    return out


def zh_corpus_csv_bytes(rows=ZH_ROWS, seed=ZH_SEED):
    buf = io.StringIO()
    writer = csv.writer(buf, quoting=csv.QUOTE_ALL, lineterminator="\n")
    writer.writerow(["id", "title", "body"])
    writer.writerows(zh_generate_rows(rows, seed))
    return buf.getvalue().encode("utf-8")


def zh_corpus_sha256(rows=ZH_ROWS, seed=ZH_SEED, data=None):
    payload = zh_corpus_csv_bytes(rows, seed) if data is None else data
    return hashlib.sha256(payload).hexdigest(), payload


def corpus_sha256(lang, rows=None, seed=None, data=None):
    if lang == "english":
        kw = {}
        if rows is not None:
            kw["rows"] = rows
        if seed is not None:
            kw["seed"] = seed
        if data is not None:
            kw["data"] = data
        return stn3.corpus_sha256(**kw)
    kw = {}
    if rows is not None:
        kw["rows"] = rows
    if seed is not None:
        kw["seed"] = seed
    if data is not None:
        kw["data"] = data
    return zh_corpus_sha256(**kw)


def load_queries(path):
    queries = stn3.load_queries(path)
    for line in queries:
        if re.search(r"[:：]\(", line):
            raise ValueError(f"field syntax is not allowed in GATE queries: {line}")
    return queries


def queries_for_lang(lang):
    return DEFAULT_QUERIES_EN if lang == "english" else DEFAULT_QUERIES_ZH


def index_sql(system, lang):
    columns = "(concat)" if system == "stn3_single" else "(title, body)"
    sql = f"CREATE INDEX idx ON documents USING stannum {columns}"
    if lang == "chinese":
        sql += " WITH (tokenizer='jieba')"
    return sql


def jieba_schema(live=None):
    block = {
        "jieba_rs_version": JIEBA_RS_VERSION,
        "dict_fingerprint": JIEBA_EMPTY_DICT_FINGERPRINT,
        "dict_fingerprint_hex": format(JIEBA_EMPTY_DICT_FINGERPRINT, "x"),
        "note": JIEBA_NOTE,
    }
    if live:
        for key, value in live.items():
            if value is not None:
                block[key] = value
    return block


def jieba_stability_sql():
    """SQL item 2 runs against a live cluster. Empty result set means stable.

    Bind the locked vocab as a text[] (see ``jieba_stability_words``). Each
    word must tokenize as exactly one jieba token equal to the word.
    """
    return (
        "SELECT word, array_agg(tok ORDER BY tok) AS tokens "
        "FROM unnest(%s::text[]) AS word, "
        "LATERAL stannum.tokenize(word, tokenizer => 'jieba') AS t(tok) "
        "GROUP BY word "
        "HAVING count(*) <> 1 OR min(tok) IS DISTINCT FROM word "
        "OR max(tok) IS DISTINCT FROM word"
    )


def jieba_stability_words():
    return list(ZH_VOCAB)


def parse_ordinals_head(text):
    """Channel-directory bytes from breakdown ordinals_head, or None.

    Does not invent a number. The 'all' row is
    terms / count / bitmaps / dictionary / ord_head / …
    """
    if not text:
        return None
    block = text
    if "as stored:" in text:
        block = text.split("as stored:", 1)[1]
        if "\nre-encoded" in block:
            block = block.split("\nre-encoded", 1)[0]
    match = BREAKDOWN_ALL_ROW.search(block)
    if not match:
        return None
    return int(match.group(4))


def measure_dictionary(blobs, repo=ROOT):
    """dict_bytes = sections.dictionary + stnf_df_len. Norms never in the sum.

    Channel-directory bytes are labeled from ordinals_head and are not part
    of dict_bytes. trailer_count increments when norms_len > 0.
    """
    per_segment = []
    stnf_df = 0
    stnf_norms = 0
    trailers = 0
    segment_bytes = 0
    for path in blobs:
        blob = Path(path).read_bytes()
        segment_bytes += len(blob)
        norms_len, df_len = stn3.stnf_sections(blob)
        if norms_len > 0:
            trailers += 1
        stnf_df += df_len
        stnf_norms += norms_len
        per_segment.append(
            {
                "path": Path(path).name,
                "bytes": len(blob),
                "stnf_norms_len": norms_len,
                "stnf_df_len": df_len,
            }
        )
    dictionary = 0
    breakdown_bytes = None
    channel_directory_bytes = None
    if blobs:
        (dictionary, breakdown_bytes), stdout = stn3.run_breakdown(blobs, repo=repo)
        channel_directory_bytes = parse_ordinals_head(stdout)
    dict_bytes = dictionary + stnf_df
    return {
        "dict_bytes": dict_bytes,
        "channel_directory_bytes": channel_directory_bytes,
        "dict_breakdown": {
            "sections_dictionary": dictionary,
            "stnf_df_len": stnf_df,
            "stnf_norms_len": stnf_norms,
            "trailer_count": trailers,
            "channel_directory_bytes": channel_directory_bytes,
            "channel_directory_note": CHANNEL_DIRECTORY_NOTE,
            "breakdown_blob_bytes": breakdown_bytes,
            "per_segment": per_segment,
        },
        "segment_bytes": segment_bytes,
        "stnf": {
            "trailer_count": trailers,
            "df_len_sum": stnf_df,
            "norms_len_sum": stnf_norms,
        },
    }


def mean_query_metric(system, key):
    if not system:
        return None
    values = [q[key] for q in (system.get("queries") or {}).values() if key in q]
    return stn3.mean(values)


def p99_block(multi, single):
    multi_p99 = mean_query_metric(multi, "p99_s")
    single_p99 = mean_query_metric(single, "p99_s")
    ratio = None
    if multi_p99 is not None and single_p99:
        ratio = multi_p99 / single_p99
    return {
        "p99_s": multi_p99,
        "p99_ratio": ratio,
        "p99_note": (
            "informational; mean of per-query p99_s (nearest-rank) on "
            "stn4_multi, ratio vs stn3_single. Not a gate."
        ),
    }


def decide_corpus(multi, single):
    """Per-corpus gates. Mandatory miss → escalate. Waiver is never invented."""
    missing = []
    if multi is None:
        missing.append("stn4_multi")
    if single is None:
        missing.append("stn3_single")
    empty = {
        "decision": "escalate",
        "decision_scope": DECISION_SCOPE,
        "c2_complete": False,
        "mandatory_gate": {
            "pass": False,
            "dict_ratio": None,
            "build_ratio": None,
            "threshold": MANDATORY_THRESHOLD,
        },
        "advisory_p50_gate": {
            "pass": False,
            "ratio": None,
            "threshold": ADVISORY_P50_THRESHOLD,
            "waiver": None,
        },
        **p99_block(multi, single),
    }
    if missing:
        empty["blocked"] = missing
        return empty
    dict_den = single.get("dict_bytes") or 0
    build_den = single.get("build_s") or 0
    if dict_den <= 0 or build_den <= 0:
        empty["blocked"] = ["cannot measure: zero single-column denominator"]
        return empty
    dict_ratio = multi["dict_bytes"] / dict_den
    build_ratio = multi["build_s"] / build_den
    mandatory = dict_ratio <= MANDATORY_THRESHOLD and build_ratio <= MANDATORY_THRESHOLD
    p50_num = mean_query_metric(multi, "p50_s")
    p50_den = mean_query_metric(single, "p50_s")
    p50_ratio = None
    if p50_num is not None and p50_den:
        p50_ratio = p50_num / p50_den
    advisory = p50_ratio is not None and p50_ratio <= ADVISORY_P50_THRESHOLD
    labeled = p99_block(multi, single)
    return {
        # `decision` is scoped to the MANDATORY dictionary/build gate only. The plan
        # says "no waiver, no pass" for C.2 as a whole, so `c2_complete` is the field
        # that decides whether the step holds; it is False whenever the advisory
        # p50 gate misses without a named waiver in this object.
        "decision": "stay" if mandatory else "escalate",
        "decision_scope": DECISION_SCOPE,
        "c2_complete": bool(mandatory and advisory),
        "mandatory_gate": {
            "pass": mandatory,
            "dict_ratio": dict_ratio,
            "build_ratio": build_ratio,
            "threshold": MANDATORY_THRESHOLD,
        },
        "advisory_p50_gate": {
            "pass": advisory,
            "ratio": p50_ratio,
            "threshold": ADVISORY_P50_THRESHOLD,
            "waiver": None,
        },
        **labeled,
    }


def top_level_decision(english, chinese):
    """stay only if both corpora pass mandatory 1.8×; else escalate."""
    if english["mandatory_gate"]["pass"] and chinese["mandatory_gate"]["pass"]:
        return "stay"
    return "escalate"


def channel_directory_basis(system):
    """What `channel_directory_bytes` actually measures on this system.

    The two lineages do not report the same quantity. On STN4 multi-column the
    breakdown example synthesizes the FCH1 directory cost as `2 * (5 + 5n)` bytes
    per entry (two extents, 5-byte FCH1 header + one record per channel); on
    stock STN3 single-column it reads the measured `ordinals_head` column. The
    field is informational and never enters `dict_bytes`, so it must not be
    compared across lineages.
    """
    if system == "stn4_multi":
        return "derived: 2*(5+5n) FCH1 directory bytes per entry, summed by segment/examples/breakdown.rs term.channels() path"
    return "measured: stock ordinals_head column of the breakdown 'all' row"


def benchmark_tree(repo=ROOT):
    """Identify the exact tree that was benchmarked, not just the commit.

    The C.2 run installed an uncommitted working tree (the harness itself plus the
    `segment/examples/breakdown.rs` FCH1 directory path), so the commit SHA alone
    cannot reproduce it. Record the dirty paths and a digest of their contents.
    """
    def git(*argv):
        proc = subprocess.run(
            ["git", *argv], cwd=repo, capture_output=True, text=True, check=True
        )
        return proc.stdout

    porcelain = git("status", "--porcelain").splitlines()
    digest = hashlib.sha256()
    for line in porcelain:
        path = line[3:].strip()
        if " -> " in path:
            path = path.split(" -> ", 1)[1]
        if not path or not (repo / path).is_file():
            continue
        digest.update(path.encode("utf-8") + b"\0")
        digest.update((repo / path).read_bytes())
    return {
        "commit": git("rev-parse", "HEAD").strip(),
        "dirty_paths": sorted(line[3:].strip() for line in porcelain if line[3:].strip()),
        "dirty_content_sha256": digest.hexdigest(),
        "note": "recorded at merge time; the benchmark ran on this same uncommitted tree",
    }


def semantic_divergences(systems):
    """Per-query row counts that differ between stn4_multi and the baselines.

    Latency ratios are only evidence when the systems answer the same query the
    same way. A row-count mismatch means the STN4 number may come from a different
    code path (or from a semantics bug), so it is recorded instead of hidden.
    """
    multi = systems.get("stn4_multi") or {}
    out = []
    for name, baseline in (("stn3_single", systems.get("stn3_single")), ("v040", systems.get("v040"))):
        base = baseline or {}
        for query, record in sorted((multi.get("queries") or {}).items()):
            other = (base.get("queries") or {}).get(query) or {}
            rows, ref = record.get("rows"), other.get("rows")
            if rows is None or ref is None or rows == ref:
                continue
            out.append(
                {
                    "query": query,
                    "baseline": name,
                    "stn4_multi_rows": rows,
                    "baseline_rows": ref,
                }
            )
    return out


def ratios_block(systems):
    multi = systems.get("stn4_multi") or {}
    single = systems.get("stn3_single") or {}
    v040 = systems.get("v040") or {}

    def ratio(num, den, key):
        n, d = num.get(key), den.get(key)
        if n is None or not d:
            return None
        return n / d

    def qratio(num, den, key):
        n = mean_query_metric(num, key)
        d = mean_query_metric(den, key)
        if n is None or not d:
            return None
        return n / d

    return {
        "dict_multi_over_single": ratio(multi, single, "dict_bytes"),
        "build_multi_over_single": ratio(multi, single, "build_s"),
        "p50_multi_over_single": qratio(multi, single, "p50_s"),
        "p99_multi_over_single": qratio(multi, single, "p99_s"),
        "dict_multi_over_v040": ratio(multi, v040, "dict_bytes"),
        "build_multi_over_v040": ratio(multi, v040, "build_s"),
        "p50_multi_over_v040": qratio(multi, v040, "p50_s"),
        "p50_single_over_v040": qratio(single, v040, "p50_s"),
        "note": "0.4.0 comparison is reported, not gated",
    }


def load_fragment(path):
    if path is None:
        return None
    return json.loads(Path(path).read_text(encoding="utf-8"))


def comparison_4_6(english_obj):
    fielded = {
        "dict_ratio": 3.3391858068315665,
        "build_ratio": 1.950900178269241,
        "p50_ratio": 5.613628719223553,
        "labeled": "3.34×/1.95×/5.61×",
        "sha256": ENGLISH_SHA256,
    }
    if COMPARISON_4_6_JSON.exists():
        data = json.loads(COMPARISON_4_6_JSON.read_text(encoding="utf-8"))
        fielded["dict_ratio"] = data["mandatory_gate"]["dict_ratio"]
        fielded["build_ratio"] = data["mandatory_gate"]["build_ratio"]
        fielded["p50_ratio"] = data["advisory_p50_gate"]["ratio"]
        fielded["sha256"] = data.get("corpus", {}).get("sha256", ENGLISH_SHA256)
    stn4_english = {
        "dict_ratio": None,
        "build_ratio": None,
        "p50_ratio": None,
    }
    if english_obj:
        mg = english_obj.get("mandatory_gate") or {}
        ag = english_obj.get("advisory_p50_gate") or {}
        stn4_english = {
            "dict_ratio": mg.get("dict_ratio"),
            "build_ratio": mg.get("build_ratio"),
            "p50_ratio": ag.get("ratio"),
        }
    return {
        "artifact": "docs/benchmarks/stn3-fielded-poc.json",
        "baseline_system": "stn3_fielded",
        "english_sha256": ENGLISH_SHA256,
        "controlled": False,
        "gate_evidence": False,
        "stn3_fielded": fielded,
        "stn4_multi": stn4_english,
        "note": (
            "Historical context only: same corpus sha256, but the fielded-terms run "
            "was a different commit, install, and machine moment. C.2 evidence is the "
            "in-run stn4_multi vs stn3_single comparison above, not this block."
        ),
    }


def locked_corpus_meta(lang):
    if lang == "english":
        return {
            "language": "english",
            "rows": stn3.ROWS,
            "seed": stn3.SEED,
            "sha256": ENGLISH_SHA256,
            "columns": ["id", "title", "body"],
        }
    return {
        "language": "chinese",
        "rows": ZH_ROWS,
        "seed": ZH_SEED,
        "sha256": CHINESE_SHA256,
        "columns": ["id", "title", "body"],
    }


def corpus_object(lang, systems, gate):
    corpus = None
    for fragment in systems.values():
        if fragment and fragment.get("corpus"):
            corpus = dict(fragment["corpus"])
            break
    if corpus is None:
        corpus = locked_corpus_meta(lang)
    corpus.setdefault("language", lang)
    obj = {
        "language": lang,
        "decision": gate["decision"],
        "decision_scope": DECISION_SCOPE,
        "c2_complete": gate["c2_complete"],
        "mandatory_gate": gate["mandatory_gate"],
        "advisory_p50_gate": gate["advisory_p50_gate"],
        "p99_s": gate.get("p99_s"),
        "p99_ratio": gate.get("p99_ratio"),
        "p99_note": gate.get("p99_note"),
        "corpus": corpus,
        "systems": {
            "stn4_multi": systems.get("stn4_multi"),
            "stn3_single": systems.get("stn3_single"),
            "v040": systems.get("v040"),
        },
        "semantic_divergences": semantic_divergences(systems),
        "ratios": ratios_block(systems),
        "field_weights": FIELD_WEIGHTS,
    }
    if gate.get("blocked"):
        obj["blocked"] = gate["blocked"]
    if lang == "chinese":
        live = None
        for fragment in systems.values():
            if fragment and fragment.get("jieba"):
                live = fragment["jieba"]
                break
        obj["jieba"] = jieba_schema(live)
    return obj


def cmd_generate(args):
    rows = args.rows
    seed = args.seed
    if rows is None:
        rows = stn3.ROWS if args.lang == "english" else ZH_ROWS
    if seed is None:
        seed = stn3.SEED if args.lang == "english" else ZH_SEED
    sha, payload = corpus_sha256(args.lang, rows=rows, seed=seed)
    if args.lang == "english" and rows == stn3.ROWS and seed == stn3.SEED:
        if sha != ENGLISH_SHA256:
            raise SystemExit(
                f"english corpus sha256 {sha} != locked {ENGLISH_SHA256}"
            )
    if args.lang == "chinese" and rows == ZH_ROWS and seed == ZH_SEED:
        if sha != CHINESE_SHA256:
            raise SystemExit(
                f"chinese corpus sha256 {sha} != locked {CHINESE_SHA256}"
            )
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_bytes(payload)
        print(f"path: {args.out}")
    print(f"lang: {args.lang}")
    print(f"sha256: {sha}")
    print(f"rows: {rows}")
    print(f"seed: {seed}")
    return 0


def cmd_run(args):
    lang = args.lang
    sha, payload = corpus_sha256(lang)
    if args.corpus:
        got = Path(args.corpus).read_bytes()
        got_sha = hashlib.sha256(got).hexdigest()
        if got_sha != sha:
            raise SystemExit(f"corpus sha256 {got_sha} != locked {sha}")
        payload = got
    queries = load_queries(args.queries or queries_for_lang(lang))
    live_jieba = None
    conn = stn3.connect()
    try:
        server_version = conn.execute("SHOW server_version").fetchone()[0]
        conn.execute("CREATE EXTENSION IF NOT EXISTS stannum")
        stn3.load_table(conn, payload)
        create_sql = index_sql(args.system, lang)
        started = time.perf_counter()
        conn.execute(create_sql)
        build_s = time.perf_counter() - started
        conn.execute("CHECKPOINT")
        with tempfile.TemporaryDirectory(prefix="stn4-c2-blobs-") as raw:
            out_dir = Path(raw)
            stn3.dump_index_blobs(
                out_dir,
                index="idx",
                dump_segments=args.dump_segments,
                dbname=os.environ.get("PGDATABASE"),
            )
            blobs = sorted(out_dir.glob("*.segment"))
            sizes = measure_dictionary(blobs, repo=args.repo)
        timed = stn3.measure_queries(conn, queries)
        if lang == "chinese":
            row = conn.execute(
                "SELECT recorded_jieba_version, recorded_dict_fingerprint, "
                "runtime_jieba_version, runtime_dict_fingerprint "
                "FROM stannum.index_analysis('idx'::regclass)"
            ).fetchone()
            if row is not None:
                rec_ver, rec_fp, run_ver, run_fp = row
                fp = int(run_fp if run_fp is not None else rec_fp or 0)
                ver = int(run_ver if run_ver is not None else rec_ver or JIEBA_RS_VERSION)
                live_jieba = {
                    "jieba_rs_version": ver,
                    "dict_fingerprint": fp,
                    "dict_fingerprint_hex": format(fp, "x"),
                    "recorded_jieba_version": int(rec_ver) if rec_ver is not None else None,
                    "recorded_dict_fingerprint": int(rec_fp) if rec_fp is not None else None,
                    "runtime_jieba_version": int(run_ver) if run_ver is not None else None,
                    "runtime_dict_fingerprint": int(run_fp) if run_fp is not None else None,
                }
    finally:
        conn.close()
    fragment = {
        "system": args.system,
        "language": lang,
        "server_version": server_version,
        "corpus": locked_corpus_meta(lang) | {"sha256": sha},
        "create_index_sql": create_sql,
        "field_weights": FIELD_WEIGHTS,
        "timed_sql": stn3.TIMED_SQL,
        "build_s": build_s,
        **sizes,
        "queries": timed,
        "protocol": {
            "warmup": stn3.WARMUP_RUNS,
            "timed_runs": stn3.TIMED_RUNS,
            "concurrency": 1,
            "gucs": "defaults",
            "nearest_rank": "p50=ceil(0.50*n)th, p99=ceil(0.99*n)th; n=20 → 10th and 20th",
            "cold": (
                "first SELECT of each query after CREATE INDEX and CHECKPOINT; "
                "not mixed into the 20 timed samples"
            ),
        },
    }
    if lang == "chinese":
        fragment["jieba"] = jieba_schema(live=live_jieba)
    args.fragment.parent.mkdir(parents=True, exist_ok=True)
    args.fragment.write_text(json.dumps(fragment, indent=2) + "\n")
    print(f"wrote {args.fragment}")
    print(
        f"build_s={build_s:.4f} dict_bytes={sizes['dict_bytes']} "
        f"segments={len(sizes['dict_breakdown']['per_segment'])} "
        f"channel_directory_bytes={sizes['channel_directory_bytes']}"
    )
    return 0


def cmd_merge(args):
    english_systems = {
        "stn4_multi": load_fragment(args.english_multi),
        "stn3_single": load_fragment(args.english_single),
        "v040": load_fragment(args.english_v040),
    }
    chinese_systems = {
        "stn4_multi": load_fragment(args.chinese_multi),
        "stn3_single": load_fragment(args.chinese_single),
        "v040": load_fragment(args.chinese_v040),
    }
    english_gate = decide_corpus(
        english_systems.get("stn4_multi"), english_systems.get("stn3_single")
    )
    chinese_gate = decide_corpus(
        chinese_systems.get("stn4_multi"), chinese_systems.get("stn3_single")
    )
    english_obj = corpus_object("english", english_systems, english_gate)
    chinese_obj = corpus_object("chinese", chinese_systems, chinese_gate)
    for obj in (english_obj, chinese_obj):
        for name, system in (obj.get("systems") or {}).items():
            if system is None:
                continue
            system["channel_directory_basis"] = channel_directory_basis(name)
            breakdown = system.get("dict_breakdown")
            if breakdown is not None:
                breakdown["channel_directory_note"] = (
                    CHANNEL_DIRECTORY_NOTE + " Basis: " + channel_directory_basis(name)
                )
    decision = top_level_decision(english_gate, chinese_gate)
    pg = args.pg_version
    for group in (english_systems, chinese_systems):
        for fragment in group.values():
            if fragment and fragment.get("server_version"):
                pg = pg or fragment["server_version"]
                break
        if pg:
            break
    result = {
        "decision": decision,
        "corpora": {
            "english": english_obj,
            "chinese": chinese_obj,
        },
        "comparison_4_6": comparison_4_6(english_obj),
        "protocol": {
            "pg": pg or "17.11",
            "concurrency": 1,
            "gucs": "defaults",
            "warmup": stn3.WARMUP_RUNS,
            "timed_runs": stn3.TIMED_RUNS,
            "nearest_rank": (
                "p50=ceil(0.50*n)th, p99=ceil(0.99*n)th of sorted samples; "
                "n=20 → 10th and 20th"
            ),
            "cold": (
                "labeled first SELECT of each query after CREATE INDEX and CHECKPOINT; "
                "not mixed into the 20"
            ),
            "server_topology": args.topology,
            "timed_sql": stn3.TIMED_SQL,
            "search_limit": stn3.SEARCH_LIMIT,
            "field_weights": FIELD_WEIGHTS,
            "build_protocol": {
                "runs": 1,
                "interval": (
                    "wall clock of one `CREATE INDEX` statement per system on a fresh "
                    "table load; no median over repeated builds"
                ),
                "note": (
                    "Inherited from the 4.6 protocol, which also measured build as a "
                    "single scalar. sub-second single-shot timings are sensitive to "
                    "machine state, so a near-threshold ratio is a single-run gate "
                    "observation, not a stable estimate; repeated builds would need a "
                    "protocol amendment applied to both systems."
                ),
            },
        },
        "git": {
            "stn4": args.stn4_sha or stn3.git_sha(),
            "v040": args.v040_sha,
            "benchmark_tree": benchmark_tree(),
        },
    }
    if args.pg18_remeasurement and args.pg18_clean_baseline:
        raise SystemExit(
            "merge: use only one of --pg18-remeasurement / --pg18-clean-baseline"
        )
    if args.pg18_remeasurement:
        if not args.existing:
            raise SystemExit("merge --pg18-remeasurement requires --existing")
        existing = json.loads(args.existing.read_text(encoding="utf-8"))
        if "decision" not in existing:
            raise SystemExit(f"{args.existing} has no top-level decision to preserve")
        profile = None
        if args.profile:
            profile = json.loads(args.profile.read_text(encoding="utf-8"))
        existing["superseded_by"] = "pg18_remeasurement.decision_pg18"
        existing["pg18_remeasurement"] = {
            "engine": pg or "",
            "decision_pg18": decision,
            "corpora": result["corpora"],
            "paired_build_profile": profile,
            "protocol": result["protocol"],
            "git": result["git"],
        }
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(existing, indent=2, ensure_ascii=False) + "\n")
        print(f"wrote {args.out}")
        print(
            f"decision_pg18={decision} "
            f"(PG17 decision={existing['decision']} preserved)"
        )
        print(
            "english mandatory="
            f"{english_obj['mandatory_gate']['pass']} "
            "chinese mandatory="
            f"{chinese_obj['mandatory_gate']['pass']}"
        )
        return 0
    if args.pg18_clean_baseline:
        if not args.existing:
            raise SystemExit("merge --pg18-clean-baseline requires --existing")
        existing = json.loads(args.existing.read_text(encoding="utf-8"))
        if "decision" not in existing:
            raise SystemExit(f"{args.existing} has no top-level decision to preserve")
        environment = None
        if args.environment:
            environment = json.loads(args.environment.read_text(encoding="utf-8"))
        english_build = (english_obj.get("mandatory_gate") or {}).get("build_ratio")
        existing["pg18_clean_baseline"] = {
            "engine": pg or "",
            "decision_clean": decision,
            "english_oneshot_build_exceeds_1_8": bool(
                english_build is not None and english_build > 1.8
            ),
            "corpora": result["corpora"],
            "protocol": result["protocol"],
            "git": result["git"],
            "environment": environment,
        }
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(existing, indent=2, ensure_ascii=False) + "\n")
        print(f"wrote {args.out}")
        print(
            f"decision_clean={decision} "
            f"(PG17 decision={existing['decision']} and pg18_remeasurement preserved)"
        )
        print(
            "english mandatory="
            f"{english_obj['mandatory_gate']['pass']} "
            "chinese mandatory="
            f"{chinese_obj['mandatory_gate']['pass']}"
        )
        return 0

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n")
    print(f"wrote {args.out}")
    print(f"decision={result['decision']}")
    print(
        "english mandatory="
        f"{english_obj['mandatory_gate']['pass']} "
        "chinese mandatory="
        f"{chinese_obj['mandatory_gate']['pass']}"
    )
    return 0


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)

    gen = sub.add_parser("generate", help="write or hash a locked synthetic CSV")
    gen.add_argument("--lang", required=True, choices=LANGS)
    gen.add_argument("--out", type=Path, help="CSV path; omit to hash only (dry-run)")
    gen.add_argument("--rows", type=int, default=None)
    gen.add_argument("--seed", type=int, default=None)
    gen.set_defaults(func=cmd_generate)

    run = sub.add_parser("run", help="load, CREATE INDEX, dump, and time one system")
    run.add_argument("--system", required=True, choices=SYSTEMS)
    run.add_argument("--lang", required=True, choices=LANGS)
    run.add_argument("--fragment", type=Path, required=True, help="write this system JSON fragment")
    run.add_argument("--corpus", type=Path, help="locked CSV; omit to generate in memory")
    run.add_argument("--queries", type=Path, default=None)
    run.add_argument("--dump-segments", type=Path, default=stn3.DUMP_SEGMENTS)
    run.add_argument("--repo", type=Path, default=ROOT, help="cargo working directory")
    run.set_defaults(func=cmd_run)

    merge = sub.add_parser("merge", help="merge fragments and apply the dual-corpus GATE")
    merge.add_argument("--english-multi", type=Path, help="english stn4_multi fragment")
    merge.add_argument("--english-single", type=Path, help="english stn3_single fragment")
    merge.add_argument("--english-v040", type=Path, help="english v040 fragment")
    merge.add_argument("--chinese-multi", type=Path, help="chinese stn4_multi fragment")
    merge.add_argument("--chinese-single", type=Path, help="chinese stn3_single fragment")
    merge.add_argument("--chinese-v040", type=Path, help="chinese v040 fragment")
    merge.add_argument("--out", type=Path, default=DEFAULT_JSON)
    merge.add_argument("--stn4-sha", help="stn4 HEAD SHA (default: git rev-parse HEAD)")
    merge.add_argument("--v040-sha", default=stn3.V040_PIN)
    merge.add_argument("--pg-version", help="PostgreSQL version string (default: from fragments or 17.11)")
    merge.add_argument(
        "--topology",
        default="unspecified; Item 2 records Homebrew sequential vs isolated prefixes",
        help="server topology notes for the JSON protocol block",
    )
    merge.add_argument(
        "--pg18-remeasurement",
        action="store_true",
        help=(
            "preserve the existing PG17 JSON (top-level decision/corpora/protocol) "
            "and write a pg18_remeasurement object plus superseded_by"
        ),
    )
    merge.add_argument(
        "--existing",
        type=Path,
        help="JSON to preserve when using --pg18-remeasurement or --pg18-clean-baseline",
    )
    merge.add_argument(
        "--profile",
        type=Path,
        help="optional paired build-profile JSON nested under pg18_remeasurement",
    )
    merge.add_argument(
        "--pg18-clean-baseline",
        action="store_true",
        help=(
            "preserve the existing JSON (including pg18_remeasurement and PG17 objects) "
            "and write a sibling pg18_clean_baseline object with decision_clean"
        ),
    )
    merge.add_argument(
        "--environment",
        type=Path,
        help="optional environment JSON nested under pg18_clean_baseline",
    )
    merge.set_defaults(func=cmd_merge)
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
