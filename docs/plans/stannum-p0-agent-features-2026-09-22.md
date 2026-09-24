# stannum P0 Agent Features: Plan

Status: final (critique applied 2026-09-22) · Basis: `main@6d02ec7` + `docs/designs/p0-agent-features.md` · 2026-09-22. All four mid-flow questions resolved to the recommended options; the design critique's verified findings (docs/reviews/p0-plan-critique-2026-09-22.md) are folded in, including six post-critique resolutions #9-#14.

## Goal

Implement the four P0 directions from `docs/designs/p0-agent-features.md` as
independently shippable extension releases plus a separately frozen LSG4
format project: (1) a standalone `stannum.search()`/`search_count()` SRF pair
and a `pgembed_stannum` Python package with RRF hybrid search and optional
retrievers; (2) multi-column BM25F on a new LSG4 segment format; (3) jieba
dictionary governance (meta-page stamp, runtime custom dictionary, `auto`
stop-word presets); (4) observability (EXPLAIN counters, create-index
progress, `index_stats`). The design document is authoritative; this plan
turns its decisions into exact signatures, SQL, byte layouts, test matrices,
and rollout steps.

## Background

Curated from four explore agents (score-reuse seams, segment-format surface,
dictionary lifecycle, pgembed packaging) plus direct code verification.
All stannum refs are `main@6d02ec7`; pgembed refs are the
`wuxianliang/pgembed` fork working tree.

### Scoring machinery (P0-1 reuse surface)

- `build_index_scorer(key, k1, b, term_add, term_replace) -> IndexScorer`
  (postgres/src/score.rs:1224) is self-sufficient from OIDs+query+flags:
  opens the index under AccessShareLock, verifies
  `IndexGetRelation(index.oid()) == heap_oid`, loads
  `storage::index_tokenizer` (storage/mod.rs:479), `options::bm25`,
  `options::score_stop_words`, parses via `parse_tinql_to_query`, snapshots
  `storage::view(oid)` (storage/mod.rs:1970), expands terms,
  `compile_scoring_terms`, aggregates per-source stats,
  `TermScorer::from_statistics`, one `SourceReader` per source. It touches no
  statement cache. Precedent for use outside any scan: pg_test at lib.rs:1241.
- Statement couplings to avoid: `note_executor_start` (score.rs:175) fires
  only from the ExecutorStart hook (customscan.rs:130-134); `SCAN_SCORERS`
  (score.rs:133), `INDEX_SCORE_CACHE`/`SCORE_CACHE` (:128-129) are
  statement-scoped; `full_score` outside a scan hits `score_context_error`.
- Candidate engines: `IndexScorer::top_k` (score.rs:951) is block-max WAND
  gated on `prunable_shape` (score.rs:524; flat conjunction/disjunction, all
  leaves scored or absent), `PRUNE_MAX_K = 4096` (score.rs:499 — re-read the
  constant before wiring). `max_score` (score.rs:1332) is the universal
  path: `plan(&query, segment, &Limits::default())` per source
  (tinql plan.rs:69, default `max_expansion = 1024`), dead-set skip,
  `visible_tids`, per-tid `score`. `sum_scores_in_order` (bm25.rs:409) is the
  canonical left-to-right f32 fold. Ordering primitive `rank()` (score.rs:493).
- **Visibility correction:** `visible_tids` (score.rs:1083) follows HOT via
  `table_index_fetch_tuple` + `call_again` but pushes the *input root tid*;
  the *visible member* (`slot.tts_tid`) is what `search_access`
  (customscan.rs:1317-1421) stores and what a JOIN on `d.ctid` needs.
- SRF/permission patterns: `require_stannum_index` (udfs.rs:303),
  `require_index_select` (udfs.rs:320: owner or table-wide SELECT, rejects
  RLS); `score_inspect` (score.rs:1743) is the closest standalone-over-index
  SRF (recovery gate `index_reads_allowed` at score.rs:352-356; diagnostic
  heap fallback `load_documents` at score.rs:1492 — excluded from `search`).
  Snippet rendering: `render_highlight`/`render_highlight_ansi`
  (highlight_udfs.rs:33-63) take `&CompiledTokenizerPipeline` from
  `tokenizer_by_oid` (storage/mod.rs:536). No helper fetches the indexed
  heap datum for an arbitrary tid — that is new code.

### Segment format surface (P0-2)

- Lengths: `BTreeMap<Tid, u32>` (segment.rs:103), `Occurrence { tid, doc_len,
  positions }` (:94-98); flat u32le table in TID order, last blob section
  (`finish_mixed` :287-308); reader enforces
  `lengths_at + doc_count*4 == total` (:415-433); `Lengths` Bytes|Lazy,
  LENGTH_CHUNK = 4096 (:722-728).
- tf: `TfBucket` 4-bit/16 buckets (tf_bucket.rs:10-51); payload entry =
  tf_bucket u8 (low 4 bits) + n varint + delta positions (payload.rs:12-29),
  format-gated skip tables (:74-103). No field id today.
- Score bounds keyed by (tf bucket × doc length) only: `BlockBound
  { min_len: [u32; 16], last: Tid }` (postings.rs:78-84); every encoder
  changes under a field dimension (`encode_term_bound` :193,
  `encode_bounds` :205, `encode_scored` :228, `encode_sparse` :255,
  `encode_grouped` :273); `PostingsBuilder::push_scored(tid, tf_bucket,
  doc_len)` (:152) is the only score-entry point.
- Dictionary: LSG3 packs `df_bucket = df << 4 | max_tf_bucket`, zigzag gap
  extents; `TermEntry { df, max_tf_bucket, postings, payload }`
  (dictionary.rs:53-60). `Format::{Lsg1,Lsg2,Lsg3}`, `CURRENT = Lsg3`
  (segment.rs:49-66); postgres never branches on Format.
- Reader API: `trait Index` (index.rs:41-54); `AreaFetch`
  (segment.rs:346-354); `MutableIndex` re-encodes via `push_scored` +
  `PayloadBuilder` (index.rs:340+), u32le lengths extent (:366-390).
  `segment/src/forward.rs` is the write-buffer forward codec — any field tag
  must land there before multi-column insert works.
- Merge round-trips: `merge.rs` (`live_lengths` :150, `push_scored` +
  `payload.push` :277-278, 6-part assemble :332-353), `direct_merge_poc.rs`,
  `maintenance/postings.rs` (BlockBound re-validation),
  `maintenance/validate.rs`, `verify.rs` oracle (:24-31, :242-271, :534-575).
- Meta page: `Meta { identity, spec: [u8; 8], buffer, next_generation,
  segments, pending }`; `META_HEADER = 8 + SPEC_BYTES + 28 + 4 + 4 + 4`
  (layout.rs:215-246); `decode` (:280+) enforces exact length — trailing
  bytes are `Err("invalid Stannum meta page")` today, which a tail-append
  must relax. `ENTRY_BYTES` = 52 (confirm constant), `PENDING_BYTES` = 16.
  Page special `magic u32, kind u8, version u8 (=2), flags u16` gated by
  equality for **every** page kind (layout.rs:95-100) — bumping version
  would refuse buffer/run pages too, not just meta.

### Dictionary lifecycle, caches, GUCs, stop words (P0-3)

- Two thread_local caches, no invalidation anywhere:
  `TOKENIZERS: HashMap<[u8; SPEC_BYTES], Rc<CompiledTokenizerPipeline>>`
  (storage/mod.rs:457-460, `tokenizer_for` :463-475) and `SPECS:
  HashMap<u32, (u32 relNumber, [u8; SPEC_BYTES])>` (:500-529).
  **Correction to the original explore claim:** today's jieba pipeline holds
  no dictionary reference — `JiebaIter::new` cuts through the global
  `LazyLock<Jieba>` at each `tokenize` (jieba.rs:33, :41-49), so a cached
  pipeline *would* observe a swapped global. Reload correctness still
  requires the pipeline to snapshot `Arc<Jieba>` at compile (one scorer
  stays on one dictionary for its whole life) **and** the cache key to gain
  the dictionary generation — the snapshot makes caching sound, the key
  makes it correct.
- All 8 `SPEC_BYTES` consumed (options.rs:407-436): tokenizer, two foldings,
  long-token mode, max_bytes u16, graphemes, position_gaps. No room without
  widening (which would invalidate every existing meta page).
- `options::tokenizer` (options.rs:486) compiles fresh from reloptions and
  does not consult `TOKENIZERS` — stays that way for callers without a meta
  page (`score_inspect`'s non-segmented branch).
- GUC precedent: 7 storage GUCs (storage/mod.rs:89-163) +
  `stannum.enable_custom_scan` (customscan.rs:40, :106-117), all
  `GucRegistry`, `GucContext::Userset`, `GucFlags::default()`, no assign
  hooks. No relcache/syscache callbacks exist anywhere.
- Stop words: `ScoreStopWords::from_csv` (bm25.rs:104-114) splits/trims/
  dedups with **no tokenizer pass** — literal match against analyzed query
  terms; consumed by `compile_scoring_terms` (bm25.rs:227-266) at three
  scorer-build sites (score.rs:1262-1266, :1405-1409, :1788-1789).
  `key.full == true` passes `stop = None` — full scoring ignores the list
  (docs/compatibility.md). Contrast: `TermSetEdit::analyzed_with`
  (bm25.rs:143-157) is the analysis precedent.
- `_PG_init` order (lib.rs:27-35): options → storage (GUCs) → storage::wal
  → operator → customscan.

### pgembed packaging (P0-1 Python side)

- Template `pgembed_pgvector` (`__init__.py:1-29` fail-closed path helpers;
  pyproject: wheel `pgembed-pgvector`, `dependencies =
  ["pgembed>=0.3.0rc2,<0.4"]`, extras slot). Activation point:
  `EXTENSION_PACKAGES["stannum"] = None` at src/pgembed/__init__.py:61;
  `_standalone_extension_path` (:156-178) checks
  `BUILT_FOR_POSTGRES_MAJOR == BUNDLED_PG_MAJOR` and consults the package
  **only when bundled metadata does not already mark the extension built**.
  Create-name mapping already present (`get_extension_create_name`
  :234-256, stannum fallback :255; postgres_server.py:513).
- Root pyproject include glob `pgembed*` auto-bundles a new
  `src/pgembed_stannum` into the base wheel; `MANIFEST.in` grafts
  `src/pgembed/pginstall`. The standalone-wheel pipeline is net-new even for
  pgvector (built out-of-band; no in-repo target).
- `psql()` (postgres_server.py:482-491) runs `--no-psqlrc
  --set=ON_ERROR_STOP=1` via subprocess — no parameter binding; SQL errors
  raise `CalledProcessError`. Unsafe for interpolating agent query strings.
- Test conventions: `_require_extension` skip gate (test_pgembed.py:80-82),
  `tmp_postgres` fixture (:375-379), scalar parse `.splitlines()[2].strip()`;
  `tests/test_stannum_jieba.py` is the stannum fixture pattern;
  `tests/test_standalone_extensions.py:11-42` is the fail-closed contract
  (parametrize list at :12-16).
- `pgbuild/Makefile`: `STANNUM_COMMIT` pin (:278) → fork shallow-fetch +
  `cargo pgrx install --release --no-default-features --features pg18`
  (:745-780); the pin participates in the bundle stamp (:346) so a bump
  invalidates `INSTALL_PREFIX`. `STANNUM_REPO` stays
  `https://github.com/wuxianliang/stannum.git`.

## Resolved decisions

The five design open questions, implementation defaults — the four
mid-flow contract choices were user-confirmed on 2026-09-22 (all
recommended options; see Resolved questions):

1. **P0-2 scope: freeze LSG4 in an RFC this cycle; implement after
   acceptance** (§P0-2 is the RFC body). Rationale: the only feature whose
   blast radius spans the whole `segment/` crate + `tinql/` + scoring; a
   persisted format that later changes shape forces REINDEX of every
   multi-column index. P0-1/3/4 never read field metadata and are never
   blocked.
2. **`search()` retrieval: WAND first, exhaustive plan+score as the
   correctness fallback.** `top_k` returns `None` for every shape it cannot
   prune, so the two paths are complementary. WAND results are accepted only
   when complete or entirely heap-visible (the invisible-top-row underfill
   rule is mandatory — see Risks). Above `PRUNE_MAX_K` the plan path runs;
   that is success, not an error.
3. **Dictionary stamping: meta tail-append + `TOKENIZERS` key extension.**
   `SPEC_BYTES` is full; widening invalidates every existing meta page. The
   trailer is self-describing and is the same mechanism P0-2 reuses for
   field metadata. The compiled-pipeline cache key becomes `(spec,
   dictionary_generation)`; `SPECS` stays spec-only (spec bytes carry no
   dictionary identity; `tokenizer_by_oid` resolves the generation after the
   spec lookup).
4. **`pgembed_stannum`: base wheel first; standalone native wheel as a
   second packaging phase.** The `pgembed*` glob ships the Python with zero
   new build infrastructure; discovery stays metadata-first, so a bundle
   that already contains `stannum.so` is unaffected. Data path uses
   psycopg2 with `%s` parameters (never `psql()` string interpolation) —
   agent queries contain quotes.
5. **`auto` stop words: curated analysis-stable source words, filtered
   through the index tokenizer at scorer build (kept only when analysis
   yields exactly one token).** Explicit CSV entries stay literal,
   preserving the documented contract. `builtin_stop_words(preset)` SRF
   exposes the same list to Python.

Inter-plan contradictions resolved (baseline variants disagreed; the
resolution and its reason):

| # | Topic | Resolution | Why |
|---|---|---|---|
| 1 | Trailer framing | `tag u8, payload_len u32le, payload`; analysis payload = `jieba_rs_version u32le` + `reserved u32le` (0) + `dict_fingerprint u64le` = 16 bytes | Self-describing length; the reserved word reconciles the design doc's "16 bytes" with its `u32+u64` fields |
| 2 | `index_stats` shape | Wide one-row SRF (+ `index_health` view) | A metric-rows SRF re-reads the meta page once per metric from the view |
| 3 | `search()` STRICT | Not STRICT | pgrx will not mark STRICT with `Option<f32>` defaults; a hand-added STRICT would NULL the default call |
| 4 | `Heap Fetches` | Already exists (`exec.fetched`) | customscan.rs:1717-1823 |
| 5 | ctid contract | Return the visible HOT member (`slot.tts_tid`), not the index root | JOIN on `d.ctid` breaks after HOT updates otherwise; `visible_tids` pushes the root, so a new `visible_tid_pairs` helper is required |
| 6 | Pipeline/dict binding | Compiled jieba pipeline owns an `Arc<Jieba>` snapshot + generation | Statement-stable analysis; makes the cache sound |
| 7 | Vector-arm fallback | No applicable vector index at/above the seqscan threshold → `RuntimeError` | The threshold exists to prevent unbounded sequential scans; silent big-table scans are the failure mode |
| 8 | MAX_FIELDS | 16, packed entry byte `field_id << 4 \| tf_bucket` | Dense, matches the 4-bit bucket discipline, u16 masks; RFC may revisit (with `layout_revision`, widening to 32 later stays expressible) |

Post-critique resolutions (verified against code on 2026-09-22):

| # | Topic | Resolution | Why |
|---|---|---|---|
| 9 | `SECURITY DEFINER` on dict UDFs | **Dropped** — plain invoker-run functions with the Rust admin check | `extension_upgrade.py:53` asserts the generated snapshot contains no `SECURITY DEFINER`; the OID-based schema resolution + Rust-side check already cover the stated goal |
| 10 | `TOKENIZERS` key | **Fingerprint**, not a generation counter; on install, evict jieba entries whose fingerprint ≠ current | Identical content reuses the compiled pipeline (a forced reload with unchanged content costs nothing); eviction bounds memory to ~2 live `Arc<Jieba>` dictionaries (each entry pins a whole dictionary) |
| 11 | `search()` limit semantics | Counts **distinct visible documents** | Two roots can share one HOT member; the WAND acceptance rule must verify dedupe collapses nothing (else exhaustive) |
| 12 | RRF planner control | **No `SET LOCAL`** — no session GUCs; small tables seq-scan naturally, at/above the threshold an applicable vector index is required (`RuntimeError` otherwise) | `SET LOCAL` is transaction-scoped and would also disable the final join's index scans; the threshold+index requirement achieves the intent without it |
| 13 | Parallel workers × custom dictionary | v1: the custom scan **declines parallel workers** for jieba indexes with a non-empty custom dictionary; DSM snapshot transfer is future work | Workers own thread-locals; without a leader→worker snapshot they silently analyze with the embedded dictionary (correctness over parallelism, narrow condition) |
| 14 | LSG4 header | Carries `layout_revision varint = 1`; readers probe 64 bytes as today, and on `LSG4` magic re-read an extended 256-byte head | The 16-field header (~169 bytes) overruns the 64-byte probe — unreadable without this; `layout_revision` keeps future widening expressible |

## P0-3 — jieba dictionary governance (lands first)

### Module placement

New `postgres/src/dict.rs` (`mod dict;` in lib.rs); `dict::init()` called in
`_PG_init` immediately after `storage::wal::init()` and before
`operator::init()` — it registers the relcache callback and nothing else
(the dictionary stays lazy). `stannum.strict_analysis` GUC registers in
`storage::init()` beside the existing seven. `postgres/src/stopwords.rs`
holds the static lists.

### Meta tail-append (generic extension records)

After the pending-run array, a trailer of records:

```text
record := tag u8, payload_len u32le, payload[payload_len]
ANALYSIS_TAG = 0x01   payload = jieba_rs_version u32le
                                 reserved u32le (write 0, reject nonzero)
                                 dict_fingerprint u64le   (16 bytes)
FIELDS_TAG  = 0x02   defined by P0-2; unknown until that release lands
```

Decoder rules: decode the existing prefix exactly as today; no remaining
bytes → legacy `Meta` with `analysis: None`; otherwise parse complete
records to EOF; reject truncated headers/payloads, duplicate known tags,
unknown tags (fail closed — **a P0-3 decoder treats `FIELDS_TAG` as unknown
until the P0-2 release that writes it**), nonzero `reserved`. Encoder
writes the trailer only when `analysis.is_some()`, in ascending tag order,
keeping the `out.len() > CAPACITY` failure. Page
special `VERSION` stays 2 (bumping it refuses buffer/run pages too; the
trailer is the only gate). Every `Meta { … }` literal — including test
fixtures — gains `analysis: None` atomically with the struct change.

`Meta` gains `analysis: Option<AnalysisStamp>`. `FieldMeta` and
`FIELDS_TAG` are **deferred entirely to the P0-2 release** — a 0.2.0 that
ships a field type nobody writes is dead weight (extra `Meta` literals,
an untested decoder branch); the trailer loop is generic from day one, so
nothing is lost by deferring.

**Who stamps:** only the meta-initialization path used by CREATE INDEX /
REINDEX. Insert, fold, merge, and VACUUM round-trip the decoded `Option`
unchanged — an upgraded binary never rewrites a trailer on first insert,
and indexes that were not rebuilt stay readable by the old binary
(rollback-compatible). Indexes built/reindexed on the new binary carry a
trailer and will not open on the old binary (strict length check); that
rollback limit is accepted and documented.

### Analysis identity

```rust
pub(crate) struct AnalysisStamp { jieba_rs_version: u32, dict_fingerprint: u64 }
pub(crate) enum AnalysisStatus { NotApplicable, Match, MissingStamp,
                                 JiebaVersionDrift, DictionaryDrift, BothDrift }
```

- `jieba_rs_version = (major << 16) | (minor << 8) | patch`, exposed as a
  `pub const` in `tokenizer/src/tokenizers/jieba.rs`, kept in lockstep with
  the pinned `jieba-rs` (exact version pin in tokenizer/Cargo.toml; a unit
  test verifies the pinned version in Cargo.lock — dependency and const
  land atomically).
- `dict_fingerprint`: SipHash-1-3 with fixed extension-defined keys (add
  `siphasher` to postgres/Cargo.toml; **not** `DefaultHasher`, which may
  change across Rust releases). Input per row sorted by raw UTF-8 word bytes
  then freq then tag (Rust sort, never DB collation): domain marker,
  `u32le` word length, word bytes, `u32le` freq, `u8` tag-present, tag
  bytes. The empty table hashes to a defined non-special value. Exposed via
  SQL `bigint` as the bit-preserving `i64` (document hex printing).

### Swappable dictionary holder

```rust
// tokenizer/src/tokenizers/jieba.rs
pub struct JiebaHolder { inner: RwLock<Arc<Jieba>> }   // replaces LazyLock
// tokenizer/src/lib.rs (public API; postgres crate cannot see pub(crate) items)
pub fn jieba_cut_owned(text: &str) -> Vec<String>;     // owned pieces
pub fn jieba_install(words: &[(word: &str, freq: Option<usize>, tag: Option<&str>)]);
pub fn jieba_current_generation() -> u64;
```

- `jieba-rs`'s `cut` borrows are tied to `&self`; after a swap, borrowed
  `&str` pieces would dangle — **owned copies are mandatory** (confirm the
  crate signature before writing the loop). `JiebaIter::new(text,
  snapshot: &Arc<Jieba>)` takes the pipeline's snapshot explicitly
  (`compiled.rs:192-193` and `source_spans.rs:105` are the two construction
  sites — **both** must pass it; `highlight.rs:103/:116` reach the second
  via `pipeline.source_spans()`, so without this, a reload between query
  positions and document spans makes them disagree) and emits `Cow::Owned`
  tokens. `jieba_cut_owned(text)` remains the global-snapshot convenience
  API for non-pipeline callers only.
- `CompiledPipelineKind::Jieba` stores the `Arc<Jieba>` snapshot and the
  generation observed at compile; `tokenize` uses that Arc. Non-jieba
  pipelines store generation 0. `TokenizerPipelineSpec::compile()` keeps
  working for non-PostgreSQL callers via the current global snapshot; add
  an internal compile-with-snapshot entry point so storage can pair
  (cache key, snapshot) atomically.
- `jieba_install` builds a fresh `Jieba::new()` (embedded dict), applies
  words (`freq <= 0` → jieba default frequency; SQL NULL tag → none —
  confirm `add_word`'s real signature against locked jieba-rs 0.7), stores
  the Arc, increments the generation. The write lock is held only for the
  swap; iteration never locks.

### Cache key extension

```rust
pub fn tokenizer_for(spec: &[u8; SPEC_BYTES], dict_fp: u64) -> Rc<CompiledTokenizerPipeline>
// TOKENIZERS key: ([u8; SPEC_BYTES], u64) — the dictionary FINGERPRINT,
// not a generation counter (identical content reuses the entry).
```

`dict_fp = tokenizer::jieba_current_fingerprint()` for jieba specs, `0`
otherwise (no dictionary-table touch). On `jieba_install`, evict jieba
entries whose fingerprint differs from the new one — each entry pins an
entire `Arc<Jieba>`, so unbounded retention leaks a dictionary per reload
(contrast `PAGE_TABLES`' 4096-entry clear and `SEGMENT_READERS`' 64 MiB
trim, storage/mod.rs:~723, :956-969). `jieba_reload_dict` **always**
reinstalls and bumps the generation even when the fingerprint is unchanged
— that forced rebuild is its reason to exist (post-binary-upgrade embedded
dictionary); the lazy path installs only on change. Call sites to update
atomically: `index_tokenizer`, `tokenizer_by_oid`, and the `tokenizer_for`
call inside `storage::insert`. `SPECS` unchanged. `build_index_scorer`
already goes through `index_tokenizer`. `dict.rs` needs a `pub(crate)`
statement-id accessor from `score.rs` (`current_statement` is private) for
the per-statement reload dedup. **Accepted gap, documented:**
`options::tokenizer` (options.rs:486-490) compiles fresh from reloptions
and never consults `TOKENIZERS`, so `score_inspect`'s non-segmented branch
observes the current global dictionary (via `compile()`'s snapshot) without
caching — acceptable for a diagnostic path, stated so implementers do not
"fix" it silently.

### Dictionary table and SQL API

```sql
CREATE TABLE @extschema@.jieba_words (
    word text PRIMARY KEY,
    freq integer NOT NULL DEFAULT 0 CHECK (freq >= 0),
    tag  text
);
REVOKE ALL ON TABLE @extschema@.jieba_words FROM PUBLIC;

CREATE FUNCTION @extschema@.jieba_add_word(word text, freq integer DEFAULT 0,
    tag text DEFAULT NULL) RETURNS void
    LANGUAGE c VOLATILE PARALLEL UNSAFE AS 'MODULE_PATHNAME', 'jieba_add_word_wrapper';
CREATE FUNCTION @extschema@.jieba_delete_word(word text) RETURNS void …;
CREATE FUNCTION @extschema@.jieba_dict_version() RETURNS bigint
    LANGUAGE c STABLE PARALLEL UNSAFE …;
CREATE FUNCTION @extschema@.jieba_reload_dict() RETURNS void
    LANGUAGE c VOLATILE PARALLEL UNSAFE …;
CREATE FUNCTION @extschema@.builtin_stop_words(preset text) RETURNS SETOF text
    LANGUAGE c IMMUTABLE PARALLEL SAFE …;          -- 'zh' | 'en' | 'auto'
CREATE FUNCTION @extschema@.index_analysis(index regclass)
RETURNS TABLE (index_name text, recorded_jieba_version integer,
    recorded_dict_fingerprint bigint, runtime_jieba_version integer,
    runtime_dict_fingerprint bigint, matches boolean, status text)
    LANGUAGE c STABLE PARALLEL UNSAFE STRICT …;
```

- Mutating functions: Rust-side authorization (`superuser()` or
  `pg_has_role` membership of `pg_database_owner`) as plain invoker-run
  functions — **no `SECURITY DEFINER`** (`extension_upgrade.py:53` rejects
  it in the generated snapshot; the OID-based schema resolution + Rust
  check already cover the goal). Resolve the extension schema from the
  extension OID, never the caller's search_path. `jieba_add_word` upserts;
  `jieba_delete_word` is idempotent. Validation: word non-empty after
  trim, ≤ 256 bytes, no Unicode whitespace, `freq >= 0`.
- `index_analysis`: not-jieba → NULL version fields, `matches = NULL`,
  status `not applicable`; stamped+matching → `matches = true`; legacy jieba
  → `matches = false`, `missing analysis stamp; REINDEX required`; drift →
  the corresponding status. Gated by `require_stannum_index`.
- Do not grant direct DML on `jieba_words` — direct writes bypass the API's
  validation and invalidation.

### Reload protocol (cross-backend)

- `dict::init` registers `CacheRegisterRelcacheCallback`; the callback only
  sets an `AtomicBool` when the invalidated OID is `jieba_words` — no SPI,
  no allocation, no reload inside the callback.
- First jieba use in a backend (and any use with the flag set) reads
  `SELECT word, freq, tag FROM stannum.jieba_words ORDER BY word` via SPI,
  computes the fingerprint, and calls `jieba_install` only when it differs.
  A thread-local `(statement_id, fingerprint)` suppresses repeat reads
  within a statement; `dict::note_executor_start` (called next to
  `score::note_executor_start` in the executor hook) clears the per-statement
  dedup and the warned-set.
- Writers reload locally before returning (no waiting for their own sinval)
  after `CommandCounterIncrement` + `CacheInvalidateRelcacheByRelid`.
- An xact callback marks the holder dirty after commit/abort when the
  current transaction changed dictionary rows; abort restores committed
  state on next use.
- Standbys: no SQL writes; reading `jieba_words` on a standby is legal, so
  the embedded-dictionary fallback is really about **parallel workers and
  startup**, not recovery. Note the diagnostic consequence: on a hot
  standby, drift WARNINGs repeat every query and REINDEX is impossible
  until promotion — document this in `docs/compatibility.md`. The v1
  parallel-worker policy is resolution #13 (decline parallel workers for
  jieba indexes with a non-empty custom dictionary); validate on a hot
  standby.
- If a reload fails, retain the previous Arc but propagate ERROR from the
  operation that requested the refresh — never silently continue stale.
- Concurrency: backends are processes; holders/caches are backend-local;
  last-writer-wins with the fingerprint as the pure function of rows;
  duplicate/out-of-order invalidations only set the same flag.

### Drift enforcement

`dict::check_analysis(index_oid, &meta)` called at the end of
`build_index_scorer` and once inside `storage::view` (after meta decode):

| Recorded | Runtime | Tokenizer | Result |
|---|---|---|---|
| None | any | not jieba | silent |
| None | any | jieba | one WARNING per index per statement (`REINDEX` records a stamp) |
| Some == runtime | jieba | silent, `matches = true` |
| Some != runtime | jieba | WARNING naming the index and `REINDEX`; `stannum.strict_analysis = on` upgrades to ERROR |
| Some != runtime | not jieba | silent |

The warned-set is a thread-local `HashSet<u32>` of index OIDs cleared by
the executor-start hook. `strict_analysis` is a `GucSetting<bool>`, default
`false`, `Userset`, registered in `storage::init` (same shape as
`enable_custom_scan`). New inserts after drift use the current runtime
dictionary while the index keeps warning until REINDEX (by design).

### Stop-word presets

```rust
pub(crate) struct ScoreStopWords { terms: FxHashSet<String> }   // owned
pub(crate) fn from_reloption(csv: &str,
    analyze_preset: impl Fn(&str) -> Vec<String>) -> Option<ScoreStopWords>
```

- Selectors `auto` / `auto:zh` / `auto:en` (ASCII case-insensitive), mixable
  with explicit entries (`auto:zh,the,a`). `auto` = zh + en for jieba
  pipelines, en otherwise. Explicit entries stay literal after trimming.
- `analyze_preset` (provided by the three scorer-build sites +
  `score_inspect`) tokenizes with the index pipeline and keeps a word only
  when analysis yields exactly one token — this is what makes `auto:zh`
  safe on a `unicode` (per-character) index instead of stopping every 我/们.
- `key.full == true` still passes `stop = None`: `full_score` and
  `search()` ignore the reloption; the `==>`/`score()` path honors it.
- Lists: `postgres/src/stopwords.rs`, ~150-200 zh function words + a short
  en list, written for this repository, provenance + frozen version in
  `source-provenance.json`; unit test asserts every zh entry is one jieba
  token and every en entry one default-pipeline token. `builtin_stop_words`
  exists so Python never forks the list.

### Errors and edge cases

| Case | Behavior |
|---|---|
| Empty/oversized/whitespace word, negative freq | ERROR from the add function; table unchanged |
| Non-admin caller | ERROR before SPI |
| `jieba_words` missing | ERROR naming `CREATE EXTENSION` / upgrade |
| Concurrent adds in two backends | Both commit; each backend's next reload sees the committed snapshot |
| Reload mid-`tokenize` | In-flight iterator finishes on its Arc; next compile misses the cache |
| Cancellation during the dictionary-table load | Previous holder stays installed, dirty flag stays set; the ERROR propagates |
| Legacy meta + jieba | WARNING; queries run; inserts use current dictionary |
| Nonzero `reserved` in payload | Corrupt meta page (existing decode error path) |

### Tests

Tokenizer unit (holder swap preserves in-flight iteration; later pipeline
sees the new word; version const matches Cargo.lock) · layout unit (legacy
round-trip; stamped round-trip; both tags; duplicate/unknown/truncated
rejection; capacity) · `#[pg_test]` (permissions; same-session reload;
cross-session invalidation after commit; abort restores; fingerprint stable
across insertion order; missing-stamp warning; strict-mode ERROR on drift;
`index_analysis` states; `auto`/`auto:zh`/`auto:en`/mixed; `auto:zh` drops
的 from `score()` but not `full_score()`) · `postgres/tests/dict_lifecycle.py`
(two-connection add-word visibility; drift + REINDEX lifecycle) · extend
`pgembed/tests/test_stannum_jieba.py`.

## P0-1 — `stannum.search()` + `pgembed_stannum`

### Internal scoring interfaces (score.rs)

Do **not** expose `CacheKey` or `IndexScorer` fields. Add:

```rust
pub(crate) fn build_standalone_scorer(heap_oid, index_oid, query, k1, b) -> IndexScorer
// builds CacheKey { statement: 0, full: true, dense: 0, add/replace: None,
// k1/b as f32::to_bits } then calls build_index_scorer; never enters
// SCAN_SCORERS / INDEX_SCORE_CACHE / SCORE_CACHE.

pub(crate) struct PrunedCandidates { rows: Vec<RankedCandidate>, complete: bool }
pub(crate) struct RankedCandidate { indexed_tid: Tid, score: f32 }

impl IndexScorer {
    pub(crate) fn matching_tids(&self) -> BTreeSet<Tid>;   // plan loop from max_score
    pub(crate) fn pruned_top_k(&self, k: usize) -> Option<PrunedCandidates>;
    pub(crate) fn score_matching_tids(&mut self, tids) -> Vec<RankedCandidate>;
}

pub(crate) struct VisibleTid { indexed_tid: Tid, visible_tid: Tid }
pub(crate) unsafe fn visible_tid_pairs(heap_oid, tids: BTreeSet<Tid>) -> Vec<VisibleTid>;
// like visible_tids but returns slot.tts_tid (the member); one fetch object
// + slot per batch; interrupts between roots; destroys the slot/fetch state
// and closes the relation on normal completion (the leak-proofing clause);
// dedupe by visible tid (duplicate → keep first root in TID order for
// count; force exhaustive for search).
```

`full: true` is load-bearing: `search` must match `full_score` semantics
(no stop-word list, no dense elision); flipping it would make `search`
disagree with `search_count`'s `==>` match set.

### SQL surface

```sql
CREATE FUNCTION @extschema@.search(
    "index" regclass, "query" text,
    "limit" integer DEFAULT 10,
    "snippet" text DEFAULT 'html',          -- 'none' | 'html' | 'ansi'
    "begin_tag" text DEFAULT '<mark>', "end_tag" text DEFAULT '</mark>',
    "k1" real DEFAULT NULL, "b" real DEFAULT NULL
) RETURNS TABLE ("ctid" tid, "score" real, "snippet" text)
LANGUAGE c VOLATILE PARALLEL UNSAFE
AS 'MODULE_PATHNAME', 'search_wrapper';

CREATE FUNCTION @extschema@.search_count("index" regclass, "query" text)
RETURNS bigint LANGUAGE c VOLATILE PARALLEL UNSAFE
AS 'MODULE_PATHNAME', 'search_count_wrapper';
```

Not STRICT (pgrx + `Option<f32>` defaults; STRICT would NULL the default
call). `VOLATILE` (active-snapshot visibility) + `PARALLEL UNSAFE`
(relations, thread-local state — matches the other scoring functions).

### Control flow

1. `require_stannum_index(&index, "search")`; `require_index_select`.
2. `storage::present` else ERROR `stannum.search() requires a segmented
   stannum index` (no diagnostic heap fallback).
3. `limit < 0` → ERROR; `limit == 0` → empty after full validation
   (query/k1/b/snippet still validated by building the scorer).
4. `snippet` ∈ {none, html, ansi} else ERROR listing the three; `ansi`
   ignores the tags.
5. Until LSG4: `indnkeyatts != 1` → ERROR `supports a single text column`
   (indnkeyatts, not indnatts — the codebase's own suitability test uses it
   at score.rs:1947, and indnatts would wrongly reject an INCLUDE-column
   index); when `snippet != 'none'`, also require a **text-compatible
   attribute type** and a positive key attribute (`indkey.values[0] <= 0` =
   expression key) — with `snippet => 'none'` the call stays valid (the
   degraded mode); HTML/ANSI on an expression or non-text key raise an
   actionable error recommending `snippet => 'none'`.
6. `heap_oid = IndexGetRelation(...)`; `build_standalone_scorer` (drift
   check inside); `pruned_top_k(limit)`.
7. **WAND acceptance rule:** `limit` counts **distinct visible documents**.
   Accept pruned rows only when `complete == true` **and** (every row is
   heap-visible **and** deduplicating them by visible member collapses
   nothing — two roots can share one HOT member, which would silently
   underfill an all-visible result); otherwise discard and run the
   exhaustive path. Skipping the dedupe check ships wrong top-k on HOT
   chains whose root and member both rank in the top k.
8. Exhaustive: `matching_tids` → `visible_tid_pairs` → score visible roots
   → `rank` sort (score DESC, visible tid ASC) → truncate.
9. Snippet ≠ none: re-fetch under the same active snapshot, confirm the
   expected member, `slot_getattr` the indexed attribute, render with the
   **single pipeline captured once at scorer build** (generation-pinned for
   the whole call — compiling again at snippet time could straddle a reload
   and make query positions disagree with document spans) via
   `positions_from_query`/`highlight_text[_ansi]` (all three already
   `pub(crate)` — `highlight.rs:95/:110/:157`; no visibility change needed).
   NULL column → NULL snippet, row kept. Row vanished on second fetch →
   skip defensively.
10. Return `TableIterator` in rank order (SQL still requires ORDER BY for
    join stability). Returned `ctid` is the visible member.
11. `check_for_interrupts!` in candidate/visibility/scoring/snippet loops.

`search_count`: same gates + scorer build, then plan walk + visible-pair
dedupe → count. No scoring, snippets, or `top_k`. Equals the visible `==>`
match count. No artificial limit cap: a non-prunable query materializes
every match; interrupts are the escape hatch (documented).

### Errors and lifecycle

| Case | Behavior |
|---|---|
| Non-stannum regclass / no LDP2 storage / RLS / column-only privileges | existing errors from the gate helpers |
| `limit > PRUNE_MAX_K` on a flat query | plan path, no error |
| Invalid query / k1 / b | existing messages from `build_index_scorer` |
| Recovery where `view` already errors | same error (`standby_reads_allowed`) |
| HOT update | score the root, return the member |
| Deleted between plan and fetch | row omitted |

### `pgembed_stannum` package

```
src/pgembed_stannum/
├── __init__.py      # pgvector-template fail-closed helpers + re-exports
├── _index.py        # SearchHit/HybridHit/IndexAnalysis, StannumIndex
├── _hybrid.py       # RRF SQL
├── _sql.py          # identifier quoting (1-2 part table, " doubling)
├── langchain.py     # optional extra, lazy import
├── llama_index.py   # optional extra, lazy import
└── pyproject.toml   # standalone-wheel metadata (phase 2)
```

- `__init__.py` clones the pgvector helper contract (`BUILT_FOR_POSTGRES_MAJOR
  = 18`, `EXTENSION_SO = "stannum.so"`, package-local scans only). Activation
  is the one-line `EXTENSION_PACKAGES["stannum"] = "pgembed_stannum"` flip,
  atomic with the package landing.
- **Data path is psycopg2 with `%s` parameters** (`psycopg2.connect(
  server.get_uri())`; `quote_ident` for identifiers). `psql()` stays the
  test helper — it has no binding and `ON_ERROR_STOP` raises on the first
  quote-containing agent query. Optional additive improvement (not
  required): `psql(command, *, variables, tuples_only)` with validated
  `--set=name=value` args and `:'var'` quoting.
- `StannumIndex(server, table, column, *, tokenizer="jieba", id_column="id",
  index_name=None)`: `create()` (`CREATE EXTENSION IF NOT EXISTS` + `CREATE
  INDEX [IF NOT EXISTS] … USING stannum (col) WITH (tokenizer = …)`;
  default name `{table}_{column}_stannum_idx` truncated to the identifier
  limit with a deterministic hash suffix), `drop()`, `search(query, *,
  limit=5, snippet="html", drop_stop_words=False)`,
  `search_count(query)`, `analysis()`, `check_health()`,
  `hybrid_search(query, *, vector_column, query_vector, limit=5,
  candidate_limit=None, rrf_k=60, weights=(0.4, 0.6),
  seqscan_row_threshold=50_000)`. `field_weights` rejected until LSG4.
- `search()` runs one parameterized statement joining the SRF on
  `d.ctid = s.ctid`, ordered by score DESC then id; `SearchHit(id, ctid,
  score, snippet)`. `drop_stop_words=True` is a plain-text mode: it loads
  `SELECT tok FROM stannum.builtin_stop_words('auto')` **via the SRF at
  runtime** (no frozen Python mirror to drift — the SRF exists precisely so
  Python never forks the list), tokenizes the query via
  `stannum.tokenize(..., tokenizer => …)`, drops preset terms, rejoins
  remaining tokens quoted as exact terms with implicit AND; empty result →
  no hits. It never reinterprets arbitrary TINQL.
- **RRF hybrid (one transaction, parameters only; ranks are 1-based):**
  bm25 pool = `max(limit*5, 50)` (5× headroom — recall of the fused top-k
  is controlled only by this knob; recorded after the baselines disagreed
  at 4× vs 5×). Pinned SQL form (UNION ALL + GROUP BY, so a vector-only id
  reports `bm25_rank = NULL` and vice versa — what `HybridHit` depends on):

```sql
WITH bm25 AS (
    SELECT d.id, s.score, row_number() OVER (ORDER BY s.score DESC, d.id) AS rank
    FROM <table> d JOIN stannum.search(%s::regclass, %s, "limit" => %s) s
      ON d.ctid = s.ctid),
     vec AS (
    SELECT id, row_number() OVER (ORDER BY vc <=> %s::vector) AS rank
    FROM <table> ORDER BY vc <=> %s::vector LIMIT %s),
     fused AS (
    SELECT id, %s::float8/(%s::int + rank) AS c, NULL::bigint AS vrank FROM bm25
    UNION ALL
    SELECT id, NULL::bigint, %s::float8/(%s::int + rank) FROM vec)
SELECT d.*, sum(c) AS rrf_score, max(rank) AS bm25_rank, max(vrank) AS vector_rank
FROM fused JOIN <table> d USING (id) GROUP BY d.id
ORDER BY rrf_score DESC, d.id LIMIT %s;
```

  Validate limit ≥ 1, rrf_k ≥ 1, weights finite ≥ 0 and not both zero,
  vector non-empty/finite, else `ValueError` before SQL. **Planner policy
  (no session GUCs — resolution #12):** read `pg_class.reltuples`; below
  the threshold the planner picks the seq scan naturally; at/above the
  threshold require an applicable index on the vector column else
  `RuntimeError` (never a silent big-table seq scan). `HybridHit(id, score,
  bm25_rank, vector_rank, snippet)`; the snippet joins the bm25 arm.
- Retriever extras `pgembed-stannum[langchain]` / `[llama-index]`:
  `StannumRetriever.for_langchain(index)` / `for_llama_index(index)` with
  lazy imports inside the factory; map hits to `Document(page_content=
  snippet or "", metadata={id, score, ctid})` / `NodeWithScore`; async via
  framework thread offload (the data path is synchronous). Missing
  extra → `ImportError` naming the extra.

### Standalone native wheel (phase 2)

After the base-wheel API is stable: isolated staging directory outside the
source tree; copy `stannum.{so,dylib}` + control + base/update SQL under
package-local `pginstall/share/postgresql/extension`; build `pgembed-
stannum` with the same version family and PG-major attestation; package-data
for all platform suffixes; per-OS/arch CI matrix entry in
`build-and-test.yml`; run the wheel dependency audit; a test installing
into an environment where bundle metadata marks stannum not built, proving
standalone discovery.

### Tests

`#[pg_test]` in search.rs: rank order equals `ORDER BY full_score(ctid)
DESC` on a fixture; limit 0/1/default; snippet modes + custom tags + NULL
column; bad snippet value; non-stannum regclass; no-storage index;
`search_count` == visible `==>` count; k1/b change scores; syntax error;
WAND-vs-plan agreement on a flat shape (force the plan path with a non-flat
twin); HOT update keeps the joined id; `limit > PRUNE_MAX_K` returns rows;
HOT chain with root and member both in the top k returns `limit` distinct
docs (the dedupe acceptance case) · `postgres/tests/
search_srf.py`: installed SQL, permission matrix, RLS, upgrade, and the
**invisible-top-row fallback** — two connections: conn1 opens REPEATABLE
READ and acquires its snapshot, conn2 DELETEs a top-ranked row and commits,
then conn1's `search()` must still return `limit` rows (the deleted tid is
dead to conn1's snapshot but absent from the index dead set — a #[pg_test]
cannot hold a second snapshot, so this lives here) ·
`pgembed/tests/test_pgembed_stannum.py`: skip gate, create/search/count/
analysis/health, hybrid hand-computed RRF on 3 rows, quote-injection (query
containing `'` returns normally, not `CalledProcessError`),
`drop_stop_words` removes 的 · `test_standalone_extensions.py`: add
`("pgembed_stannum", "stannum.dylib", "stannum.control")` · retriever
smoke behind `importorskip`.

## P0-4 — observability

### EXPLAIN ANALYZE counters

Add to `ScanExec`: `segments_visited_immutable`, `segments_visited_buffer`,
`dictionary_pages`, `postings_blocks`, `heap_rechecks`, `dead_skipped`.
Keep `fetched` — **do not emit a second `Heap Fetches`** (it exists).

- `heap_rechecks` increments immediately before `passes_clause()` when
  recheck is required (not for plain visibility fetches) — **two call
  sites**: `customscan.rs:1416` (search) and `:1514` (count);
  `search_recheck` (`:1468-1474`) is an unconditional `true` stub, so the
  ExecScan-level recheck never fires and this counter is purely internal.
  `dead_skipped` increments on dead-set hits and `all_dead` fetches.
- `Pruned by Block-Max = candidates − scored` (saturating; tests assert
  `scored <= candidates`).
- New properties under ANALYZE (after the parallel-worker early return):
  `Segments Visited` (+ `Immutable Segments` / `Write-Buffer Segments`
  breakdown when nonzero), `Dictionary Pages Read`, `Postings Blocks Read`,
  the prune identity, `Heap Rechecks`, `Dead Skipped`.

### Page counters (the cached-reader trap)

Cached segment readers live across statements, so counters cannot sit on
the reader. Add `postgres/src/observe.rs`:

```rust
pub(crate) struct IoFrame { dictionary_pages: u64, postings_blocks: u64 }
pub(crate) fn push_io() -> IoGuard;        // Drop pops
pub(crate) fn add_pages(area: Area, blocks: u64);
```

customscan begin pushes a frame; explain reads the top; end pops; an empty
stack means "do not count" (vacuum, insert, `search` unless a test asks).
`Area` is a **new public enum in `segment/`** (`Dictionary | Postings |
Payload | Docs | Lengths | Other`) — part of the change, not a given.
Extend `segment::source::Source` with a default no-op `fn note_area(&self,
area: Area)`; `Reader` calls it immediately before the read fetching each
region; the initial 64-byte header probe (`segment.rs:417`) is read before
any area is known and counts as nothing (stated, not left to the
implementer). The postgres `Source` impl converts the byte range to pin
counts — **`div_ceil(offset_in_page + len, CHAIN_CAPACITY)`**, matching
`RunSource::read`'s real page walk (`CHAIN_CAPACITY = CAPACITY - 4`,
layout.rs:38-40; `div_ceil(len, BLCKSZ)` undercounts any range not starting
at a run-page boundary). Repeat pins counted — the **distinct-set**
alternative (`set<(source, block)>` / `set<(source, extent, ordinal)>`)
was considered and rejected: pin counts are cheaper and match the reader
cache's actual work; the choice is observable and documented here so tests
pin it.
Parallel workers own their stacks; the existing idle-leader early return
governs disclosure. Counters are Stannum pin counts, not core `BufferUsage`
(which core sums across workers and would double-count shared reader
cache hits). `Segments Visited` increments once per source on first cursor
advance / WAND open, classified by `view.immutable_sources`.

### Non-ANALYZE properties

`Segments` from a new meta-only `storage::directory_summary` (immutable
entries + nonempty buffer) — **not** `segment_rows`, which reads every
dead-list run. `Analysis`: post-P0-3 only, when `meta.analysis` is `Some`
and the tokenizer is jieba — `jieba <maj.min.patch> / dict <16 hex> /
matches|drift`; omitted when absent.

### pg_stat_progress_create_index

New `postgres/src/progress.rs`; no GUC. Core owns the progress command
lifecycle — Stannum only updates slots of the already-active command via
`pgstat_progress_update_param` with the `PROGRESS_CREATEIDX_*` constants.
**Validation requirement before wiring:** confirm constant names against
the pgrx 0.19.1 bindings and the bundled PG18 `system_views.sql`; probe
`pg_stat_get_progress_info('CREATE INDEX')`. Read `ambuild` first —
verified: it passes `progress = true` to `table_index_build_scan`
(am.rs:71-79), so core already advances `tuples_done`/`blocks_done` for
the heap scan; Stannum sets only phase + block columns (a second writer
double-counts). Phase map (1 heap scan / 2 segment flush / 3 final
merge-finish; blocks in pages, `blocks_done` advancing inside the run
writer) documented in `docs/architecture/segmented-storage.md` — **and the
mid-command denominator switch is documented there too**: phase 1 reports
heap blocks in `blocks_done`/`blocks_total`, phases 2-3 report blob pages
in the same columns, so dashboards see the unit change; the phase number is
the signal. A thread-local `IN_INDEX_BUILD`
guard set only by `ambuild` (RAII incl. panic) keeps insert-path folds from
publishing create-index progress. Use the AM subphase slot if PG18 projects
it; otherwise keep core `phase = building index` and verify the subphase
through the raw progress function — never map Stannum phases onto core's
lock-wait/validation phase numbers.

### `index_stats` / `index_health`

```sql
CREATE FUNCTION @extschema@.index_stats("index" regclass)
RETURNS TABLE (documents bigint, dead_documents bigint, dead_ratio float8,
    segments int, immutable_segments int, mutable_segments int,
    next_generation bigint, total_pages bigint, dictionary_pages bigint,
    total_length bigint, average_length float8,
    analysis_matches bool, analysis_detail text)
LANGUAGE c VOLATILE PARALLEL UNSAFE STRICT …;

CREATE VIEW @extschema@.index_health WITH (security_invoker = true) AS
SELECT c.oid::regclass AS index, s.* FROM pg_class c
CROSS JOIN LATERAL @extschema@.index_stats(c.oid) s
WHERE c.relkind = 'i' AND c.relam = (SELECT oid FROM pg_am WHERE amname = 'stannum');
```

`VOLATILE PARALLEL UNSAFE` matches `segment_info`. `security_invoker =
true` so a superuser view owner cannot bypass `require_index_select`
(all-or-nothing v1 contract: one unreadable index fails the scan).
`dictionary_pages` = pages intersected by dictionary extents, computed
from the area offset — `ceil((offset + len) / CHAIN_CAPACITY) −
floor(offset / CHAIN_CAPACITY)` per immutable segment (buffer contributes
0; `ceil(len/CAPACITY)` alone undercounts any extent not starting at a
page boundary). The column comment must state that this counts **all**
dictionary pages, whereas the explain `Dictionary Pages Read` counter
 counts pages **actually touched** — different definitions by design; they
must agree for a fully-scanned index.
average_length 0. Dead counts reuse `segment_rows` (single dead-list
decode implementation). Analysis columns NULL when the stamp is absent or
the tokenizer is not jieba; this function never WARNINGs and never consults
`strict_analysis`. `segment_info` stays the streaming detail view.

### Benchmarks and tests

`benchmarks/explain_counters.py`: parse `EXPLAIN (ANALYZE, FORMAT TEXT)`;
when `Pruned by Block-Max` + `Candidates` + `Scored Candidates` are all
present, assert the identity; called from the paired harness; persist the
fields in result JSON; never rewrite published baselines · `#[pg_test]`:
two-segment index (low `stannum.build_segment_docs`) shows `Segments`
without ANALYZE; with ANALYZE counters ≥ 1 for a term query and the prune
identity holds; legacy meta omits `Analysis` · `postgres/tests/
observability.py`: second-connection polling during CREATE INDEX (backend
appears, `tuples_total` non-null); `index_stats` agrees with `segment_info`
aggregates; `index_health` under `security_invoker` as owner and failure
without SELECT · `pgembed/tests/test_pgembed_observability.py`: smoke.

## P0-2 — multi-column BM25F / LSG4 (RFC body; implementation gated)

### RFC gate

`docs/designs/lsg4-rfc.md` freezes: max field count (16, packed entry
byte), exact segment header, length-table order, payload entry bytes,
postings-bound bytes, forward-record bytes, field-name/weight meta record,
single-column-stays-LSG3 rule, scoring formula and field-scope semantics,
old-format read behavior, and golden vectors decoded by independent test
code. `Format::CURRENT` stays `Lsg3` until acceptance; no persisted LSG4
bytes ship before the freeze; no released intermediate LSG4 interpretation.

### DDL and field plan

```sql
CREATE INDEX docs_search ON docs USING stannum (title, body)
    WITH (field_weights = 'title:3.0,body:1.0');
```

`field_weights`: string reloption (registered like `score_stop_words`).
`amoptions` checks syntax only (comma-separated `name:float`, unique names,
finite weights > 0, no `,`/`:` in names); `ambuild`/AM-validate checks
relation-aware rules: names are a permutation of the index column
`attnames`, `indnatts >= 2`, single-column + weights = ERROR
(`field_weights applies to multi-column stannum indexes`). Omitted weights
default 1.0 per column. `FieldPlan { names, weights }` in **index attribute
order**, `MAX_FIELDS = 16`. Multi-column indexes reject expression keys and
INCLUDE columns in the initial release.

### Meta fields trailer (TAG 0x02)

```text
record_version u8 = 1, field_count u8 (2..=16), reserved u16 = 0,
per field: name_len u16le, name_utf8, weight f32le
```

Written only by multi-column CREATE INDEX / REINDEX; round-tripped by
insert/merge. Opening compares recorded names with current heap attributes;
mismatch (column rename) → ERROR requiring REINDEX. **`ALTER INDEX … SET
(field_weights = …)` is fail-closed ERROR `REINDEX to change field_weights`
until an alter hook is found and covered by a test** — never a silent
reloption/meta split.

### LSG4 segment format

- `Format::Lsg4`, magic `b"LSG4"`, `has_fields() == true`,
  `has_bounds() == true`; `from_magic` accepts all four; postgres passes
  Lsg4 into finish/merge only when `field_count >= 2`. An index never mixes
  LSG3 and LSG4 segments; a single-column index writes LSG3 indefinitely.
- Header: `magic, layout_revision varint = 1, doc_count varint, total_length
  varint (unweighted Σ), field_count varint, field_total u64le × field_count,
  dictionary/postings/payload/docs lens (as LSG3), areas, lengths: doc-major
  `field_count × u32le` per document`. Invariants:
  `lengths_at + doc_count * field_count * 4 == total`;
  `Σ field_total == total_length`. LSG3's invariant stays in the LSG3 parser.
  **Head probe:** `Reader::new` reads only `min(total, 64)` bytes today
  (segment.rs:417); a 16-field LSG4 header is ~169 bytes and would fail to
  open. LSG4 readers keep the 64-byte probe, and on `LSG4` magic re-read an
  extended 256-byte head before parsing varints (covers 16 × u64 field
  totals + all varints with margin).
- Builder: `lengths: BTreeMap<Tid, Vec<u32>>`; occurrences grouped by
  `(term, field)`; `add_document_fields` takes `(field_id, term, position)`
  with positions strictly increasing **within one field** (independent
  sequences, both may start at 0). A document with every field empty is
  omitted. `add_document` remains the one-field API for LSG3.
- Payload entry (LSG4-only parse): `field_hit_count varint`, then ascending
  groups of `packed u8 = field_id << 4 | tf_bucket` + existing
  `encode_positions` output. Validation set (complete): `1 <=
  field_hit_count <= field_count`; field ids strictly increasing; bucket in
  `0..15`; `position_count > 0`; positions strictly increasing per field;
  **bucket agrees with the position count's quantization** (the
  corrupted-bucket cross-check). Skip tables stay LSG3-shaped (ordinal i =
  posting i). LSG1-3 payload bytes unchanged.
- Bounds: new types `FieldBlockBound { field_count: u8, present_fields:
  u16, max_tf_bucket: [u8; 16], min_doc_length: [u32; 16], last: Tid }`
  (and `ScoreBound::{Single(BlockBound), Fields(FieldBlockBound)}`), with
  `PostingsBuilder::push_scored_fields(tid, &[(field, bucket)],
  field_lens)` as the LSG4 entry point (`push_scored` unchanged for LSG3;
  also touched: `BlockBound::merge` :101, `BlockBound::over` :118). LSG4
  term-bound payload = `field_mask varint (bit i = field i present)` then
  per set bit: `bucket_mask varint` + `min_len varint` per set bucket;
  envelope unchanged. Safe looser bound, computed over the **query's
  selected fields**: `max tf* = Σ_selected w_f · dequant(max_tf_bucket_f)`,
  `min len* = Σ_selected w_f · min_doc_length_f` — summing over all
  present fields is also safe but looser exactly on field-scoped queries,
  the case the feature exists for. Minima from different documents can
  underestimate length → overestimate score → safe for pruning. Expect
  fewer prunes than LSG3; measured via the explain identity, not a bug.
- `TermEntry` unchanged: `df` = documents containing the term in any field;
  `max_tf_bucket` = max across fields; **no per-field df** (design
  decision: dictionary bloat).
- `trait Index` gains `field_count() (default 1)`, `field_length(ordinal,
  field)`, `field_total(field)`; `AreaFetch`/`Lengths` learn a field
  argument; `length(ordinal)` remains field-0 for LSG3 callers.
  `Lengths::Bytes(&[u8])` has no field count in the type today
  (segment.rs:~720-742, indexes `ordinal * 4`) — it becomes
  `Lengths::Fields { bytes, field_count }` (or takes the count from the
  reader). Note `LENGTH_CHUNK = 4096` is byte-granular and the reader
  caches one chunk: a document's field row can straddle a chunk boundary,
  so `field_length` fetches the whole document row (or a two-chunk window),
  not a single u32.
- `MutableIndex`/`ForwardRecord`: field id on each term group; old forward
  bytes have no field id and decode as field 0; the codec discriminator is
  "meta has a fields trailer", not a byte-layout change. Confirm
  `storage/wal.rs` treats run/buffer pages as opaque — stop if any WAL
  record parses postings.
- Merge/direct-merge/maintenance/verify branch on `has_fields()`;
  `verify_segment` asserts per-field length sums, per-field position
  counts, field-id range, and per-field (not global) strictly-increasing
  positions.

### Postgres write path

The datum plumbing already works — `build_callback` (am.rs:90-104) and
`aminsert` (:117-129) forward the whole `values`/`isnull` arrays; the
take-first-datum happens inside `storage::Builder::add`
(storage/mod.rs:1837, incl. the early `if *isnull` return that becomes a
loop over key attrs) and `storage::insert` (:1886). **The P0-2 change is
confined to `storage/mod.rs`** (loop `0..indnkeyatts`, skip NULLs,
tokenize each datum, tag with the attribute ordinal; all-NULL rows index
nothing); `am.rs` keeps only name/column validation and scan-key
propagation. The insert-path meta recheck compares the fields-tag bytes
next to `(identity, spec)` so a concurrent REINDEX changing field count
cannot publish a mismatched forward record — and by the same token, the
insert path never re-stamps `analysis` (it round-trips what it decoded),
so a retry cannot publish a stamp the rebuilt index does not have.

### BM25F scoring

Per term contribution on an LSG4 source:

```text
tf*  = Σ_f w_f · dequantize(tf_bucket_f)      // one dequantize, never re-quantized
len* = Σ_f w_f · length_f
saturation = the same f32 expression TermScorer uses for one (tf, length)
contribution = idf · boost · saturation        // idf stays aggregate-corpus
```

Weighted average length = `Σ_f w_f · field_total_f / document_count`,
computed once in `build_index_scorer`. Terms still combine through
`sum_scores_in_order` in canonical `(term bytes, field mask)` order. A
single-field weight-1.0 LSG4 fixture must be **bit-equal** to the LSG3
scorer on the same tokens (pins operation order to `TermScorer`).
`TermScoreModel::{Bm25, Bm25f}` enum in the score readers.

### Field-aware scoring terms and query language

- Scoring key widens to `(text, FieldMask)`; unscoped term = all fields;
  `(text, mask)` duplicates combine boosts; identical text with different
  masks stays separate; scoped idf remains aggregate.
- Grammar: `field_head = ${ field_name ~ "(" }` (compound-atomic — a space
  in `title: (foo)` is not field syntax), first alternative of `base`
  before `word_primary`; `field_name = bare_ident | quoted_ident`;
  `bare_ident = @{ ASCII_ALPHA ~ (ASCII_ALPHANUM | "_")* }` matched with
  PostgreSQL ASCII case-fold; quoted identifiers use the phrase escape rule
  and match bytes exactly. `title:foo` is unchanged (one word). **Do not
  ship the grammar change before the executor can return the unknown-field
  error**, or those queries change meaning and then fail inside eval.
- `Expr::Field { name, inner }` — every `match` on `Expr` updated (no
  wildcard arms): subtokenize (rewrite inner, keep wrapper; no name prefix
  on terms — the dictionary stays one entry per term string), simplify,
  display/quote, estimate, retrieval, plan/eval, span_expr, position
  filters. Name resolution at scorer build: unknown field → ERROR
  `stannum: unknown field '<name>'`; field syntax on a fieldless index →
  ERROR requiring a multi-column index.
- Field-qualified non-positional terms need a **payload-filter cursor**
  (postings alone cannot field-restrict because df is aggregate); estimates
  use aggregate df as a conservative upper bound. Phrases/spans: partition
  positions by field, run the boldi-vigna solver per field, union matches,
  never compare intervals across fields (no field axis added to
  `boldi-vigna::Interval`). An unscoped phrase matches when any ONE field
  contains it (Lucene rule; never across fields).

### Multi-column operator semantics

**Precondition (phase 1 dependency):** today a multi-column stannum index
cannot reach the operator path at all — suitability is `is_stannum &&
indisvalid && indisready && indnkeyatts == 1` (score.rs:1946-1947) and the
clause matcher reads `indkey.values[0]` as *the* key attribute at
:1949/:1955. Widening that gate and carrying the per-clause attribute is
part of P0-2 phase 1 (score.rs:1943-1960 joins the file list).

The SQL operator still has one text left operand: `title ==> 'foo'`
restricts unscoped terms to `title` (the scan key's attribute defines an
implicit outer field scope); an explicit field group naming another field
in that operator context is rejected; all-fields queries use
`stannum.search()`. The scan-key attribute number is carried through
custom-scan private state and bitmap keys — a `body` match must never
satisfy `title ==> …`. Heap fallback and recheck evaluate only the left
operand's field — the concrete path is `passes_clause` inside
`search_access` (customscan.rs:1416) and the count path (:1514);
`search_recheck` (:1468-1474) is a `true` stub.

### Highlights (phase 3)

Internal highlight positions gain a field id; `stannum.highlight` gains an
optional `field text` overload (NULL = single-column behavior; multi-column
without a field highlights each field separately, joined by one newline,
marks confined to the matching field); `search()` on a multi-column index
highlights the field named by a single top-level `Field` wrapper, else the
first index column (first matching field, else first non-NULL field —
preserves the scalar return type).

### Phases (each an LSG4 REINDEX boundary; LSG3 indexes never rewritten)

1. **Format + field-scoped terms**: LSG4 read/write, forward records,
   builder/insert, meta trailer, `field:(…)` parse/eval, weights-applied
   scoring with the LSG3 bit-equality test; `search()` keeps rejecting
   `indnatts != 1` until parity is green.
2. **BM25F bounds**: per-field BlockBound, WAND on LSG4, benchmark prune
   rate; relax `search()`'s single-column check.
3. **Phrases + highlights**: same-field spans, highlight overload,
   `extension_upgrade.py` field fixture, oracle/fuzz field dimension.

### Tests (when implementation starts)

LSG3 byte fixtures unchanged; LSG4 round-trip; payload ordinals align with
postings; cross-field positions both-0 parse cleanly; LSG4+LSG4 merge
preserves field lengths; refuse LSG3×LSG4 merges; weights change ranking
(title weight 10 ranks the title hit first); `title:(数据库)` does not match
a body-only hit; `title:("甲 乙")` does not match 甲 in title + 乙 in body;
`title:foo` stays a single-token lookup; ALTER fail-closed;
property tests 1-16 fields/NULL/empty/merges/deletes; golden vectors;
`ranked_fuzz.py` + `oracle.py` field column (phase 3); upgrade test reads
an LSG3 index built by the previous version.

## Cross-cutting: SQL versioning and state flows

**Read `postgres/tests/extension_upgrade.py` before any SQL change** — it
fails on snapshot drift and defines the normalization. Every merge adding
SQL objects: bump the workspace version (root `Cargo.toml`; minor version
per release — see Resolved questions), regenerate `postgres/sql/stannum--<new>.sql`
with the existing pgrx schema command, hand-write
`stannum--<old>--<new>.sql` (new functions, `jieba_words`, grants, view)
following the test's assertions, advance `default_version` in
`stannum.control`. Never edit a released script in place. This is the
single most common breakage when adding `#[pg_extern]` surface. The test
also asserts **fingerprint equality between upgrade and fresh install**
(extension_upgrade.py:66-106, asserted at :122): every `GRANT`/`REVOKE`/
view clause in `stannum--0.1.0--0.2.0.sql` must match `stannum--0.2.0.sql`
exactly — asymmetry fails the gate even when the snapshot itself is clean.

**Dictionary write flow:** `jieba_add_word` → admin check → SPI upsert →
command counter → `CacheInvalidateRelcacheByRelid` → local fingerprint read
→ `jieba_install` on change → generation increment. Other primaries: sinval
sets the flag; next jieba `tokenizer_for` reads the table. Standby: next
jieba `tokenizer_for` reads WAL-visible rows. A scorer's compiled pipeline
keeps its Arc until drop; the next `open` misses `TOKENIZERS` because the
generation changed.

**search flow:** SQL call → gates → `build_standalone_scorer` → view +
drift check → `pruned_top_k`/WAND-acceptance → heap member fetch (maybe
full recollect) → score → snippet → `TableIterator`. The scorer is a local;
drop order stays "sources before view" (already encoded by field order).

## File-by-file impact

### P0-3

`tokenizer/src/tokenizers/jieba.rs` (holder, owned cut, version const) ·
`tokenizer/src/lib.rs` + `tokenizers.rs` (public jieba API; keep jieba-rs
private) · `tokenizer/src/compiled.rs` (pipeline owns Arc + generation;
compile-with-snapshot) · `tokenizer/src/source_spans.rs` (**second
`JiebaIter` construction site — must pass the pipeline's snapshot**) ·
`tokenizer/src/spec.rs` (snapshot compile path) ·
`tokenizer/Cargo.toml` (exact pin) · root `Cargo.toml`/`Cargo.lock` ·
`postgres/Cargo.toml` (`siphasher`) · `postgres/src/storage/layout.rs`
(`AnalysisStamp`, trailer codec, tests; every `Meta` literal) ·
`postgres/src/storage/mod.rs` (`tokenizer_for(spec, gen)`; generation from
`index_tokenizer`/`tokenizer_by_oid`/`insert`; `check_analysis` in `view`;
`STRICT_ANALYSIS` GUC; stamping in the build/init path) ·
`postgres/src/dict.rs` **new** · `postgres/src/lib.rs` (`mod dict;`,
`dict::init()` placement) · `postgres/src/customscan.rs`
(`dict::note_executor_start` in the hook) · `postgres/src/bm25.rs` (owned
`ScoreStopWords`, `from_reloption`) · `postgres/src/stopwords.rs` **new** ·
`postgres/src/score.rs` (three stop-word sites take the parser;
`build_index_scorer` calls `check_analysis`) · `postgres/src/options.rs`
(help text) · SQL + control + root version · `docs/compatibility.md` ·
`source-provenance.json` · `postgres/tests/dict_lifecycle.py` **new** ·
`pgembed/tests/test_stannum_jieba.py` (extend).

### P0-1

`postgres/src/score.rs` (`build_standalone_scorer`, `matching_tids`,
`pruned_top_k`, `score_matching_tids`, `visible_tid_pairs`) ·
`postgres/src/search.rs` **new** · `postgres/src/highlight.rs`/
`highlight_udfs.rs` (`pub(crate)` renderers) · `postgres/src/lib.rs` ·
SQL/control/version · `pgembed/src/pgembed_stannum/*` **new** ·
`pgembed/src/pgembed/__init__.py` (activation flip, atomic) ·
`pgembed/tests/test_standalone_extensions.py` (parametrize row) ·
`pgembed/tests/test_pgembed_stannum.py` **new** ·
`postgres/tests/search_srf.py` **new**.

### P0-4

`segment/src/source.rs` (default `note_area`) · `segment/src/segment.rs`
(area calls before region reads) · `postgres/src/observe.rs` **new** ·
`postgres/src/storage/mod.rs` (Source::read pins; `directory_summary`) ·
`postgres/src/customscan.rs` (fields, properties, io frame, increments) ·
`postgres/src/progress.rs` **new** · `postgres/src/am.rs` (build guard,
phase 1, tuple policy after reading the heap-scan call) ·
`storage/mod.rs` Builder::flush/finish + run writer (phases 2-3,
blocks_done) · `postgres/src/stats.rs` or udfs (`index_stats`) ·
`postgres/src/lib.rs` · SQL/control (`index_health` view) ·
`benchmarks/explain_counters.py` **new** + paired harness call site ·
`docs/architecture/segmented-storage.md` (phase map) ·
`postgres/tests/observability.py` **new** ·
`pgembed/tests/test_pgembed_observability.py` **new**.

### P0-2

`docs/designs/lsg4-rfc.md` **new** (phase 0) · `segment/src/{segment,
payload, postings, dictionary, index, forward, merge, direct_merge_poc,
maintenance/*, verify, error, format_tests, random_tests}.rs` ·
`segment/tests/fixtures/lsg4.segment` **new** ·
`postgres/src/{options, storage/layout, storage/mod, storage/verify, am
(name validation + scan-key propagation only), bm25, score (incl. the
indnkeyatts suitability gate at :1943-1960), operator, customscan,
highlight, highlight_udfs, search}.rs` ·
`tinql/src/{ast, parser/pest_parser/grammar.pest, parser mod, runtime/*,
error, quote, tests}` · `boldi-vigna` (same-field spans, only if the solver
assumes one position space — spot-check) · docs (query-language field page,
compatibility, segmented-storage) · SQL/control ·
`postgres/tests/{ranked_fuzz, postings_lifecycle, extension_upgrade}.py`.

### pgembed pin

`pgbuild/Makefile` `STANNUM_COMMIT` bump only after the phase's commit is
on `wuxianliang/stannum` (bumping before the push fails
`STANNUM_SOURCE_VERIFIED`; bumping wipes `INSTALL_PREFIX` via the stamp) ·
`.github/workflows/build-and-test.yml` (standalone wheel job, phase 2) ·
README/examples/notebook (final adapter phase).

## Risks and migration

- **Meta trailer rollback:** old binaries reject trailing bytes; indexes
  never reindexed keep `analysis: None` and stay old-readable; reindexed
  ones require the new binary. No on-disk downgrade. Page `VERSION` stays
  2; unknown trailer tags fail closed (a P0-3 decoder knows only
  `ANALYSIS_TAG`; `FIELDS_TAG` ships in the release that writes it).
- **`full: true` is load-bearing** for `search`/`full_score`/`search_count`
  agreement; agent stop-word removal is the Python flag +
  `builtin_stop_words`.
- **HOT ctid:** emitting the index root breaks JOINs after HOT updates;
  the member ctid is the contract (`visible_tid_pairs`).
- **WAND + dead tuples:** the all-visible-or-complete acceptance rule is
  mandatory; skipping it ships wrong top-k.
- **Grammar timing:** `title:(foo)` today parses as word `title:` AND a
  group (`:` is a word char); after P0-2 it is a field query. Ship grammar
  only with the executor-side unknown-field error ready; release-note it.
- **jieba lifetimes:** `cut` borrows are tied to the segmenter; owned
  copies are mandatory (use-after-free on reload otherwise).
- **Presets vs unicode indexes:** the single-token filter prevents
  `auto:zh` from stopping every character on a per-character index.
- **`index_health` privileges:** `security_invoker = true` or the check is
  bypassed by the view owner.
- **Progress phase names:** PG18's view may only name builtin AM phases;
  dashboards can show integers. Confirm before documenting names.
- **LSG4 blast radius:** merges, direct merge, maintenance, verify, the
  mutable buffer, and every TINQL `Expr` match move together; a partial
  land that flips `CURRENT` would rewrite single-column indexes into an
  unreadable format. `CURRENT` stays `Lsg3`. WAND recall on LSG4 is weaker
  (measured, expected).
- **Forward-record compatibility:** the old codec must stay for metas
  without a fields trailer, or existing write buffers become undecodable.
- **Python injection:** the package uses psycopg2 parameters; tests include
  a quote character.
- **Bundle pin:** bump `STANNUM_COMMIT` only after the fork push; the
  hot-install loop covers local iteration.
- **Upgrade script discipline:** new functions ship with a new version +
  upgrade script; never edit a released SQL file in place.

## Implementation order (with gates)

"Rust gate" = `cargo test --workspace` + `cargo test -p stannum --features
pg18`. "SQL gate" = `extension_upgrade.py` + the feature's Python tests.
"Install gate" = `cargo pgrx install --release --no-default-features
--features pg18 --pg-config <pgembed pginstall>/bin/pg_config` (toolchain +
`PGRX_HOME` per `pgbuild/Makefile:745-780`).

1. **P0-3 tokenizer holder + owned cut** (tokenizer crate only; in-flight
   iteration survives install; generation increments). Rust gate.
2. **P0-3 meta trailer** (atomic with every `Meta` literal; legacy bytes
   decode `analysis: None`; stamped round-trips; bad tag/short payload
   error). Rust gate. No stamping from insert yet.
3. **P0-3 cache key + snapshot-in-compile** (atomic across every
   `tokenizer_for` call site; pg_test: two generations, cached pipeline's
   cut matches its captured generation).
4. **P0-3 SQL, GUC, drift, stop words** (one version bump; stopwords.rs,
   from_reloption, dict.rs, upgrade script, compatibility doc, provenance).
   SQL gate + pg_test matrix + dict_lifecycle.py (two connections). Install
   gate.
5. **P0-1 scorer interfaces + search/search_count** (version bump if 4
   shipped; pg_test matrix incl. HOT and the invisible-top-row fallback;
   search_srf.py permissions/RLS/upgrade). SQL gate.
6. **`pgembed_stannum` phase 1** (package, activation flip, standalone
   helper test, test_pgembed_stannum.py, hybrid fixture). Pytest. No pin
   bump until the stannum commit is pushed.
7. **P0-4** in three separately-compilable chunks: io stack + explain
   counters + `directory_summary` (benchmark helper) → progress reporting
   (after reading ambuild) → `index_stats`/`index_health` (analysis columns
   compile against `Option`). observability.py. SQL gate when SQL lands.
8. **Retriever extras + example notebook** (importorskip tests).
9. **Pin bump** (`STANNUM_COMMIT` → fork sha; bundle rebuild; full pgembed
   stannum test set).
10. **P0-2 phase 0:** `docs/designs/lsg4-rfc.md` (this §P0-2 + golden-vector
    schema). No code. Stop until accepted.
11. **P0-2 phase 1** (atomic writer+reader; LSG3 fixtures stay green;
    upgrade test reads old LSG3 index).
12. **P0-2 phase 2** (LSG4 bounds + WAND; prune-rate recorded).
13. **P0-2 phase 3** (same-field phrases, highlight overload, search()
    multi-column, fuzz/oracle field fixtures, docs).

Steps 1-3 are separately compilable; 4 and 5 are each atomic with their
upgrade script; 11-13 are each atomic across the bytes they introduce
(never merge a writer without its reader).

## Verification

Gate order per phase: (1) `cargo test --workspace` (segment/tinql/
tokenizer unit + proptests); (2) `cargo test -p stannum --features pg18`
(`#[pg_test]`); (3) `python3 postgres/tests/extension_upgrade.py` — **any
new `#[pg_extern]` fails this until the SQL snapshot is regenerated and the
version bumped**; (4) `postgres/tests/{postings_lifecycle,ranked_fuzz,
extension_upgrade}.py`; (5) pgembed pytest
(`test_stannum_jieba/test_standalone_extensions/test_pgembed_stannum/
test_pgembed_observability`); (6) benchmark/oracle checks when ranking or
observability changes.

**Correctness oracles:** `segment/src/verify.rs::verify_segment` (any LSG4
change), `postgres/src/storage/verify.rs::verify` (meta changes),
`benchmarks/oracle.py` (ranking). **Iteration loop:** the established
hot-install into pgembed's pginstall prefix (commands in
`pgbuild/Makefile:745-780`); then relativize
(`tools/relativize_native_install.sh`) when the prefix will feed a wheel.

## Rollout

- stannum phases land on `wuxianliang/stannum` `main` (never push
  upstream); pgembed's `STANNUM_COMMIT` bumps per bundled phase; the pin
  flows into the bundle stamp and build metadata (rebuild wipes the prefix
  by design).
- pgembed packaging: phase 1 base wheel (`pgembed*` glob + activation
  flip); phase 2 standalone `pgembed-stannum` wheel (staging outside the
  source tree, CI matrix, audit, standalone-discovery test).
- Release cadence and version scheme: minor releases per feature set —
  `0.2.0` = P0-3, `0.3.0` = P0-1 + P0-4 (+ the Python package),
  `0.4.0` = P0-2 (user-confirmed).

## Resolved questions (user-confirmed 2026-09-22)

1. **P0-2 scope: RFC + format freeze only this cycle.** `docs/designs/
   lsg4-rfc.md` (step 10) is this cycle's P0-2 deliverable; implementation
   phases 11-13 start only after the RFC is accepted. P0-1/3/4 ship
   independently on LSG3.
2. **Version scheme: minor releases** — `0.2.0` (P0-3 governance) →
   `0.3.0` (P0-1 search + P0-4 observability + `pgembed_stannum`) →
   `0.4.0` (P0-2/LSG4). Implementation-order steps 4 → 0.2.0;
   steps 5-8 → 0.3.0; steps 10-13 → 0.4.0.
3. **`pgembed_stannum` data path: psycopg2 with `%s` parameters** (hard
   dependency on `psycopg2-binary` in the package). Injection safety for
   agent queries outweighs the dependency cost; `psql()` stays a test
   helper.
4. **`stannum.strict_analysis` on unstamped legacy jieba indexes: stays
   WARNING** — enabling the GUC must not break every existing jieba index
   on upgrade; strict mode applies only to stamped-but-drifted indexes.

## References

- Design: `docs/designs/p0-agent-features.md` (authoritative)
- jieba baseline: commit `6d02ec7` (both forks' main); pgembed wiring
  `dd6ee09`
- Baseline export (until fidelity check passes):
  `prompt-exports/oracle-plan-2026-09-22-211341-stannum-p0-agent-fea-d799.md`
- `docs/compatibility.md`; `benchmarks/{oracle,paired_libraries}.py`;
`postgres/tests/extension_upgrade.py`; `pgbuild/Makefile:745-780`

## Orchestration progress (2026-09-22)

Work-item mapping (implementation-order steps → agent dispatches):

- [x] WI-1: steps 1-3 — P0-3 Rust core (holder + owned cut, meta trailer, fingerprint cache key). DONE 2026-09-23: gates green (34 pg18 tests, workspace all-pass, fmt clean); opaque snapshot carries dictionary+generation+fingerprint atomically; eviction helper + current_statement() pub(crate) landed for WI-2's use; four tokenizer_for call sites incl. amrescan.
- [x] WI-2: step 4 — P0-3 SQL/GUC/drift/stop words → release 0.2.0. DONE 2026-09-23: all 6 gates green (38 feature pg_tests, 136 full pg_test suite, install, extension_upgrade fingerprint-equal, dict_lifecycle incl. standby/parallel/cancellation, 8 pgembed jieba tests); 200 zh + 59 en stop words; tokenize/ql_parse reclassified STABLE PARALLEL UNSAFE; load-race fix + plan-cache parallel-eligibility invalidation + pg_catalog.= hardening.
- [x] WI-3: step 5 — P0-1 search()/search_count() → starts release 0.3.0. DONE 2026-09-23: all gates green (139-test pgrx suite per agent; workspace, release install, extension_upgrade with 2 upgrade paths, search_srf SQL/permissions/RLS/MVCC — all confirmed by orchestrator). Note: Codex pair backend quota-exhausted until 09:50 Beijing; WI-4/WI-5 dispatched on engineer (Claude Opus) instead.
- [x] WI-4: step 7 — P0-4 observability (io stack + explain counters → progress → index_stats/index_health) → completes 0.3.0. DONE 2026-09-24 (orchestrator-verified after executor sessions died): workspace + pg18 (41/41) + release install + extension_upgrade (2 paths) + observability.py (progress polling, explain counters, index_stats agreement, index_health security) all green.
- [x] WI-5: steps 6+8 — pgembed_stannum package + retriever extras (pgembed repo). DONE 2026-09-24 (orchestrator-verified): pytest 64 passed / 2 skipped (importorskip retriever smokes) across test_pgembed_stannum (20 tests), test_pgembed_observability, test_standalone_extensions, test_stannum_jieba, test_pgembed; activation flip at __init__.py:61; pinned RRF SQL verified.

ALL SIX WORK ITEMS COMPLETE 2026-09-24. Deferred: step 9 pin bump (needs fork push first, user decision). Remaining plan steps 11-13 gated on lsg4-rfc.md acceptance.
- [x] WI-6: step 10 — docs/designs/lsg4-rfc.md (RFC only, no code). DONE 2026-09-22: 760-line RFC, all 12 freeze items covered; judgment call: field_count==1 LSG4 blobs valid on disk (write path requires ≥2) to enable the bit-equality fixture. Pending user acceptance.
- Deferred: step 9 (STANNUM_COMMIT pin bump) — DONE 2026-09-24: pinned 7f58bbe, full bundle rebuilt, pin test fixed (stale e163585 assertion), pushed f85ff10.
- Out of scope this cycle: steps 11-13 (LSG4 implementation, gated on RFC acceptance per resolved question 1). → RFC ACCEPTED by user 2026-09-24 (incl. the field_count==1 valid-on-disk judgment); steps 11-13 now unblocked.

## 0.4.0 wave — LSG4 implementation (2026-09-24, post-acceptance)

- [x] WI-11: step 11 — phase 1: LSG4 format + field-scoped terms. DONE 2026-09-25 (three dispatches: WI-11 foundations + WI-11b2 segment completion + WI-11c postgres half): full LSG4 read/write per accepted RFC, 24 golden vectors + independent decoder, BM25F with double-level bit-equality, Expr::Field grammar shipped WITH executor errors, multi-column operator scoping, amcanmulticol enabled, ALTER/rename guards; workspace 579→green, pg18 46/46, full pg_test 169, clippy clean (orchestrator fixed 4 pre-existing lints). search() still rejects multi-column (phase 2 relaxes).
- [ ] WI-12: step 12 — phase 2: BM25F bounds + WAND on LSG4; prune-rate recorded; relax search() single-column check.
- [ ] WI-13: step 13 — phase 3: same-field phrases, highlight overload + field fixture, fuzz/oracle field dimension, docs; release 0.4.0 SQL/version.
- [ ] WI-14: final gates + commit + push 0.4.0 (stannum; then pgembed pin bump as its own follow-up).

Dispatch order: WI-1 ∥ WI-6 → WI-2 → WI-3 → (WI-4 ∥ WI-5).

## Hardening phase (2026-09-24, user-requested)

- [x] WI-A: stannum additional tests — DONE 2026-09-24: 15 new tests (13 #[pg_test] + 2 unit) across 6 files; trailer fuzz over all 5,355 single-byte mutations; gates: workspace 545/6-ignored, pg18 43, full pg_test 161, install, extension_upgrade — all green. No product bugs found. Reported pre-existing: benchmarks/run.py:369-390 duplicate EXPLAIN instrumentation.
- [x] WI-B: pgembed additional tests — DONE 2026-09-24: 47 new test items (34 pure-unit + 13 server) in test_pgembed_stannum.py; gate 78 passed / 2 skipped. Found + fixed real bug: schema-qualified CREATE INDEX rejected by PostgreSQL → create() now emits unqualified name. Reported only: begin_tag/end_tag not exposed in wrapper (API addition). Environmental (pre-existing): test_postgres_build stannum pin e163585 vs HEAD 6d02ec7 — resolves via step 9 after push.
- [x] WI-C: full gate re-run + commit + push both repos. DONE 2026-09-24 10:19: FINAL GATES GREEN (pgembed 78/2 against release install); COMMITTED + PUSHED: stannum 6d02ec7..db68b07 (49 files, +8751) and pgembed dd6ee09..5032c6e (14 files, +1959), both to origin main on the forks. STANNUM_COMMIT pin bump (step 9) now UNBLOCKED — user decision (rebuilds the bundle prefix).