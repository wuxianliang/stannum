# P0 Agent-Scenario Design: search(), BM25F, jieba governance, observability

Status: proposal (uncommitted) · Basis: `main@6d02ec7` · 2026-09-22

Scope: interface and storage design for the four P0 directions. All file
references are to the current tree.

## P0-1 One-stop `stannum.search()` + `pgembed_stannum` package

Today's three-piece pattern (==> scan, `full_score(ctid)`, `highlight`)
couples through a statement-scoped scorer cache (`ScoreCorpus` keyed by
statement/heap/index/query; `scorer_for_scan`; `note_executor_start`).
`stannum.search()` removes the coupling by opening the named index itself and
reusing the existing `IndexScorer` infrastructure for the lifetime of the
call.

```sql
CREATE FUNCTION stannum.search(
    index regclass, query text,
    limit int DEFAULT 10,
    snippet text DEFAULT 'html',        -- 'none' | 'html' | 'ansi'
    begin_tag text DEFAULT '<mark>', end_tag text DEFAULT '</mark>',
    k1 real DEFAULT NULL, b real DEFAULT NULL
) RETURNS TABLE (ctid tid, score real, snippet text);
-- volatile, parallel_unsafe; validate_stannum_index + storage::present;
-- heap visibility per candidate (bitmap-recheck standard); snippet rendered
-- only for returned rows with the index's own tokenizer (tokenizer_by_oid).
-- Companion: stannum.search_count(index, query) RETURNS bigint.
```

Usage:

```sql
SELECT d.id, s.score, s.snippet
FROM docs d
JOIN stannum.search('docs_search', '数据库', limit => 5) s ON d.ctid = s.ctid
ORDER BY s.score DESC;
```

Deliberately excluded: planner rewriting of `search()` into a custom scan, and
returning heap columns from the UDF (ctid join preserves type safety).

`pgembed_stannum` standalone wheel (modeled on `pgembed_pgvector`: own
pyproject, `BUILT_FOR_POSTGRES_MAJOR`, fail-closed path helpers):

- `StannumIndex(server, table, column, tokenizer='jieba')` with `.create()`
- `.search(query, limit=5)` → `[SearchHit(id, score, snippet)]`
- `.hybrid_search(query, vector_column=…, query_vector=…, rrf_k=60,
  weights=(0.4,0.6))` — RRF fused **in SQL** (bm25 ranks via
  `stannum.search`, vector ranks via `<=>`; seq scan under a row threshold,
  vchord index scan above)
- Optional extras `pgembed-stannum[langchain]` /
  `[llama-index]`: `BaseRetriever` subclasses returning
  `Document(page_content=snippet, metadata={"id", "score"})`

Phasing: (1) SRF + tests ~1w; (2) package + hybrid + pytest 1–2w;
(3) adapters + notebook ~1w.

## P0-2 Multi-column BM25F (format work; RFC first)

Constraints in the current tree: single-column assumption in
`storage::Builder::add` (`String::from_datum(values[0])`); one
`lengths: BTreeMap<Tid, u32>` per document; tf bucketing in payloads; LSG3
just shipped with LSG1/2/3 read compatibility; TINQL has no field syntax and
`:` bridges inside ASCII words.

Design:

1. DDL: `CREATE INDEX ... USING stannum (title, body) WITH
   (field_weights='title:3.0,body:1.0')`; string reloption validated against
   index columns in `amoptions`; single-column indexes keep today's format.
2. LSG4 format: meta gains field count/names/weights (tail-append + meta
   version bump); per-document lengths become `[u32; N]`; payloads encode
   sparse `(field_id, tf_bucket)` pairs (most terms hit few fields);
   **per-field independent position sequences** (Lucene semantics — cross-field
   phrases never match; rejected the concatenated+gap alternative for false
   adjacency and position-operator complexity).
3. Scoring: Robertson-style fused BM25F (effective tf = Σ w_f·tf_f, effective
   length = Σ w_f·len_f, aggregate-corpus idf; per-field df rejected to avoid
   dictionary bloat).
4. Query syntax: Lucene-style `field:( or_expr )`, parsed only when `ident`
   is followed by `(`; `title:foo` keeps today's single-token semantics — zero
   backward-compat break. Chosen over `IN FIELD x (...)` for stronger
   agent/LLM priors on Lucene syntax. Field dimension flows through AST →
   subtokenize → plan/eval → span operators (same-field positions).
5. Phasing (each with a REINDEX boundary): format + field-scoped terms →
   BM25F scoring → field phrases/NEAR + highlight fields; extend
   `extension_upgrade.py`, `verify`, and the oracle benchmark with a field
   dimension each phase.

Risk: the only quarter-scale project (touches the whole segment crate);
freeze LSG4 in an RFC before implementation.

## P0-3 jieba dictionary governance (smallest, first)

Meta page stores only `SPEC_BYTES` today; no dictionary version, immutable
`LazyLock<Jieba>`, `score_stop_words` is a reloption used at scoring time
only (ALTER-able, no reindex needed by design).

1. **Analysis version in meta**: append 16 bytes — `jieba_rs_version: u32` +
   `dict_fingerprint: u64` (SipHash of the custom dictionary); tail-append +
   version gate (same technique as P0-2, landable independently). Mismatch
   behavior: `WARNING` naming the REINDEX fix; GUC `stannum.strict_analysis`
   upgrades to ERROR. New `stannum.index_analysis(index)` returns recorded vs
   runtime versions and a matches flag; `pgembed_stannum` runs it as a
   startup health check.
2. **Custom dictionary SQL API** (zhparser pattern): extension-schema table
   `stannum.jieba_words(word PK, freq, tag)`; `jieba_add_word /
   jieba_delete_word / jieba_dict_version / jieba_reload_dict`. Swap
   `LazyLock` for `RwLock<Arc<Jieba>>` (lock-free read snapshots); writes
   publish relcache invalidations so other backends reload lazily; the
   fingerprint is stamped into meta at build time. Documented semantics: new
   words affect new documents and query analysis immediately; historical
   consistency requires REINDEX (which the fingerprint warning enforces).
   Permissions: superuser / pg_database_owner.
3. **Stop words**: extend `score_stop_words` syntax to
   `'auto' | 'auto:zh' | 'auto:en' | explicit list` (combinable). Built-in
   curated ~200-word Chinese list frozen per extension version; scoring-only
   contract unchanged; `pgembed_stannum.search(drop_stop_words=True)` strips
   them at query analysis for agent-generated queries.
4. Tests: meta round-trip; drift warning and strict mode; word add →
   invalidation → cross-session effect; fingerprint stability; auto-stopword
   expansion compatibility.

## P0-4 Observability

Existing EXPLAIN already reports Index/Query/Order/Top-K plus ANALYZE
counters (Candidate Strategy, Candidates Visited, Count Strategy, Candidates,
Pruning: block-max, Scored Candidates, Exhaustive Score Calls) with correct
parallel-worker disclosure. Additions:

1. ANALYZE counters: `Segments Visited` (immutable/buffer split),
   `Dictionary Pages Read`, `Postings Blocks Read` (own counters in the
   reader path, not core BufferUsage, avoiding parallel aggregation
   ambiguity), `Pruned by Block-Max` (= Candidates − Scored),
   `Heap Fetches`/`Rechecks`, `Dead Skipped`. Non-ANALYZE adds `Segments`
   and `Analysis` (dictionary version summary, tied to P0-3).
2. `pg_stat_progress_create_index`: phase reporting across
   scan→build→merge→finish using core generic slots; the segment merge/flush
   inside `builder.finish()` gets its own sub-phase with bytes done.
3. `stannum.index_stats(index)`: aggregate health view (docs, dead ratio,
   segments/generations, dictionary pages, analysis drift) for monitoring and
   package-level health checks; `segment_info` stays streaming-detail.
4. Benchmark integration: assert the new EXPLAIN counters in the paired
   harness so pruning behavior changes surface in perf runs.

## Sequencing

```
P0-3 (2-3w, independent, establishes the meta tail-append technique)
P0-1 (3-4w, parallel with P0-3; package hybrid depends on search())
P0-4 (1-2w, insertable anywhere; index_stats consumes P0-3 fields)
P0-2 (RFC first; 1-2 quarters; never blocks P0-1/3/4 value)
```
