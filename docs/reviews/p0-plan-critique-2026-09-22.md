# Critique: `docs/plans/stannum-p0-agent-features-2026-09-22.md`

Status: review of the draft plan · Basis: plan @ `main@6d02ec7` + `docs/designs/p0-agent-features.md`
(authoritative) + baseline export
`prompt-exports/oracle-plan-2026-09-22-211341-stannum-p0-agent-fea-d799.md` (the `<feature_plans>`
body and the two `## Generated Plan` responses only) · 2026-09-22.

Scope of this review: only the five requested areas. The four "Resolved questions" are treated as
settled and are not reopened. Nothing here recommends deleting accurate low-level content.

**Verification method.** Every code claim below was checked by reading the file at the cited line in
`/Users/wxl/Projects/stannum` (or `/Users/wxl/Projects/pgembed` for the packaging items). Where a
plan `file:line` reference is wrong, the correct one is given. Findings are ordered by section, then
roughly by severity.

---

## 1. Implementation-bearing baseline content missing, weakened, or generalized in the plan

### 1.1 jieba owned-cut lifetime — the snapshot never reaches the iterator (contradiction, high)

The plan keeps the lifetime rule ("`cut` borrows are tied to `&self` … owned copies are mandatory",
P0-3 §Swappable dictionary holder) and `jieba_cut_owned`. Verified: `JiebaIter` holds
`std::vec::IntoIter<&'a str>` built from `JIEBA.cut(text, true)` (`tokenizer/src/tokenizers/jieba.rs:33`,
`:35-49`), so the borrow-dangling hazard is real and the plan's diagnosis is correct.

Two losses:

- **The plan's own mechanism is self-contradictory.** It says "`JiebaIter::new` calls
  `jieba_cut_owned`", but `jieba_cut_owned` is specified as a free function over the *global* holder.
  `JiebaIter::new(text)` is called from `compiled.rs:192-193` with only `text` in scope — it has no
  access to the `Arc<Jieba>` the plan requires `CompiledPipelineKind::Jieba` to own. As written, the
  pipeline snapshot is never used by the iterator, and the "one scorer stays on one dictionary for its
  whole life" property silently does not hold.
  **Correction:** `JiebaIter::new(text, snapshot: &Arc<Jieba>)` (or `JiebaIter::from_snapshot`), with
  the snapshot cloned into the iterator (or held by the caller for the iterator's lifetime);
  `jieba_cut_owned(text)` stays the global-snapshot convenience API for non-pipeline callers.
- **The baseline's construction-site inventory is wrong and the plan inherits the omission.** The
  `<feature_plans>` baseline claims `SourceSpanIter::new` (`source_spans.rs:97`) is "the only
  construction site". There are **two**: `compiled.rs:192-193` and `source_spans.rs:105`. The second
  matters because `SourceSpanIter::new(spec, text)` (`source_spans.rs:97-106`) rebuilds its base
  iterator from the **spec**, not from the pipeline, and `postgres/src/highlight.rs:103` and `:116`
  call `pipeline.source_spans()`. After the plan's change, `tokenize()` would use the pipeline's
  snapshot while `source_spans()` uses whatever the global holder currently holds — so a reload
  between the two calls makes query positions and document spans disagree. `source_spans.rs` is
  absent from the plan's P0-3 file list; add it, and give `SourceSpanIter` the pipeline's snapshot.

### 1.2 Trailer decode rules — kept, with one loss

Framing (`tag u8, payload_len u32le, payload`, 16-byte analysis payload with the reconciling
`reserved u32`) and the reject set (truncated, duplicate known tags, unknown tags, nonzero reserved)
are all present. Lost from Oracle 2's baseline: the explicit encoder rule "writes the trailer only
when `analysis.is_some()`" (the plan implies it) and the explicit statement that a P0-3 decoder must
reject `FIELDS_TAG` *as unknown* until P0-2 ships — the plan keeps that only in Risks, not in the
decoder rules where an implementer will look. Minor.

### 1.3 Cache-key atomicity — kept; the non-cached compile path is left with a hole

"Atomic across every `tokenizer_for` call site" is present, and the call sites are right:
`index_tokenizer` (`storage/mod.rs:478`), `tokenizer_by_oid` (`storage/mod.rs:536`), and the insert
path (`storage/mod.rs:1876+`). What the plan states but does not draw the consequence of:
`options::tokenizer` (`options.rs:486-490`) compiles fresh from reloptions and never consults
`TOKENIZERS`, so **`score_inspect`'s non-segmented branch will never observe a custom dictionary**
(no generation, no snapshot — it compiles a new pipeline from the spec each call, which *will* pick up
the current global snapshot only if `compile()` takes it). The plan should say which of the two
behaviors that path gets, because it is the one place where jieba governance silently does not apply.

### 1.4 WAND acceptance rule — present, but incomplete (see §4.1 for the correctness gap)

The rule itself ("accept only when `complete == true` or every row is heap-visible; otherwise discard
and run exhaustive") is faithfully carried. Verified supporting detail: `TopK` does carry
`complete` (`score.rs:503-513`), `PRUNE_MAX_K = 4096` (`score.rs:499`) and `prunable_shape`
(`score.rs:524`) exist as described.

### 1.5 `visible_tid_pairs` / HOT member contract — present; cleanup contract dropped

The member-vs-root correction is accurate (verified: `visible_tids` pushes the *input* tid at
`score.rs:1083-1124`; `search_access` reads `(*slot).tts_tid` at `customscan.rs:1418`). Dropped from
Oracle 1's baseline: the explicit requirement that the new helper "destroy the slot/fetch state and
close the relation on normal completion" and use one fetch object + one slot per batch. The plan keeps
"one fetch object + slot per batch" but drops the cleanup clause, which is the part that leaks if an
error path is added later.

### 1.6 Snippet expression-index handling — weakened, and now self-contradictory (medium)

Plan P0-1 step 5: "`indkey.values[0] <= 0` (expression key) → ERROR, `snippet => 'none'` is the
degraded mode for expression indexes."

Baseline Oracle 1 §3.4.5: an expression key or non-text attribute means `snippet = 'none'` **remains
valid**; only HTML/ANSI raise an actionable error recommending `snippet => 'none'`.

As written, the plan errors even when the caller explicitly asked for no snippet, and its own
"degraded mode" sentence contradicts its ERROR. **Correction:** the expression/non-text check must be
gated on `snippet != 'none'`, and the plan should restore Oracle 1's second condition — "require a
text-compatible attribute type" — because `indkey > 0` alone does not establish that `slot_getattr`
yields text.

### 1.7 RRF fusion SQL — generalized to prose; the baselines' SQL never reconciled (medium)

The plan gives the shape (bm25 arm via `stannum.search(..., limit => bm25_pool)`, `row_number() OVER
(ORDER BY score DESC, id)`, vector arm by `<=>`, full outer by id, `Σ weight_i/(rrf_k + rank_i)`,
missing arm 0, final order fused DESC then id) — that is the substance. Lost:

- **No SQL is pinned.** Oracle 1 wrote it out (`UNION ALL` + `GROUP BY d.id` + `sum(contribution)`),
  with the note that a full outer join is emulated; the plan says "full outer by id" without choosing
  a form. `SUM … GROUP BY` and `FULL OUTER JOIN … COALESCE` differ in how a vector-only id's NULL
  bm25 columns are reported, which is exactly what `HybridHit(bm25_rank=None)` depends on.
- **The two baselines disagree on the pool size** (`max(50, limit*4)` in Oracle 1 vs
  `max(limit*5, 50)` in Oracle 2) and the plan silently adopts one. It should record which and why,
  since the pool is the only knob controlling recall of the fused top-k.
- Ranks are 1-based in both baselines and the plan does not say so (it matters: `rrf_k + rank` with
  0-based ranks shifts every score).

### 1.8 io-frame / `note_area` counter design — kept, but the two baselines' semantics were never reconciled (medium)

The plan adopts Oracle 2's design (thread-local `IoFrame` stack, `push_io`/`add_pages`, default no-op
`note_area` on `Source`, pin counts including repeat pins). Two things missing:

- Oracle 1 specified **distinct** sets — `set<(source_id, relation_block)>` for dictionary pages and
  `set<(source_id, term_extent, block_ordinal)>` for postings blocks. The plan picks pin counts and
  documents that, but never records that the alternative was considered; the choice is observable
  (a query that re-reads an extent across two cursors counts once under sets, twice under pins) and
  the benchmark helper asserts only the prune identity, so nothing pins the semantics down.
- **`Area` does not exist.** Verified: `segment/src/source.rs:16-30` defines `Source` with only
  `len`/`read`/`slice`. The plan says "extend `Source` with a default no-op `fn note_area(&self,
  area: Area)`" and names `Area` as `Dictionary | Postings | Payload | Docs | Lengths | Other`, but
  never says that a **new public enum in `segment/`** is part of the change, nor which of the
  `Reader`'s reads are unnamed. `Reader::new` probes `source.read(0, min(total, 64))`
  (`segment/src/segment.rs:417`) before any area is known; that read will be attributed to whatever
  area was noted last (or to nothing), which must be stated rather than left to the implementer.

### 1.9 Progress double-count guard — present and now verifiable; the unit switch is missing

The guard ("read `ambuild` first; if core already advances `tuples_done`, set only phase + block
columns") is present. Verified: `ambuild` calls
`pg_sys::table_index_build_scan(heap, index, index_info, true, true, …)` (`postgres/src/am.rs:71-79`)
— `progress = true`, so core advances both `tuples_done` and `blocks_done` for the heap scan. The
plan's guard is therefore not hypothetical. Missing: **the denominator changes mid-command.** Phase 1
reports heap blocks in `blocks_done`/`blocks_total`; phases 2-3 report blob pages in the same
columns, so a dashboard sees `blocks_done` jump from "N of heap pages" to "M of blob pages" with no
signal. Either document the unit switch explicitly, or drive the sub-phase slot only.

### 1.10 `index_stats` `security_invoker` — present ✓

Faithfully carried, including the all-or-nothing v1 contract.

### 1.11 LSG4 header / payload / bound byte layouts — three losses (high)

Kept: magic, header field order, `Σ field_total == total_length`, doc-major `field_count × u32le`
lengths, the packed `field_id << 4 | tf_bucket` payload byte, the `field_mask`/`bucket_mask`/
`min_len` term-bound layout, `TermEntry` unchanged with aggregate df.

Lost or wrong:

- **The header does not fit the reader's probe.** `Reader::new` reads only
  `source.read(0, (total.min(64)) as usize)` (`segment/src/segment.rs:417`) and parses magic +
  varints from those 64 bytes. An LSG4 header with 16 fields is
  `4 + ≤5 + ≤10 + 2 + 16*8 + 4*≤5 ≈ 169` bytes — the varint reads run off the end and the segment
  fails to open with `Truncated`. **The plan must specify a larger head probe** (e.g. probe the field
  count first, or read a fixed 256 bytes), or LSG4 is unreadable regardless of the byte layout being
  correct.
- **Payload validation set shrunk.** Oracle 2 required `1 <= field_hit_count <= field_count`, strictly
  increasing field ids, bucket in `0..15`, `position_count > 0`, positions strictly increasing per
  field, **and "bucket agrees with the position count's quantization"**. The plan keeps only
  "ascending groups … non-empty, strictly increasing per field". The quantization cross-check is the
  one that catches a corrupted `tf_bucket` that would otherwise silently mis-score.
- **No Rust type is named for the new bounds.** The plan gives the byte layout but never names the
  type that carries it, nor the new push entry point. Verified load-bearing surface that must change
  and is unnamed in the plan: `BlockBound` (`segment/src/postings.rs:78-84`), `BlockBound::merge`
  (`:101`), `BlockBound::over` (`:118`), `PostingsBuilder::push_scored` (`:152`), and the five
  encoders (`encode_term_bound` `:193`, `encode_bounds` `:205`, `encode_scored` `:228`,
  `encode_sparse` `:255`, `encode_grouped` `:273`). Oracle 1 at least sketched `ScoreBound` /
  `FieldBlockBound` and `push_scored_fields`; the plan drops both names.
- Also dropped: Oracle 1's statement that the bound must be computed over the query's **selected**
  fields (`Σ selected_f weight[f] * bucket_upper(max_tf_bucket[f])`). Summing over all present fields
  is still a safe upper bound, but it is looser precisely on field-scoped queries — the case the
  feature exists for.

### 1.12 Multi-column operator semantics — kept; the precondition is missing (high)

The plan's rule (`title ==> 'foo'` restricts unscoped terms to `title`; a conflicting explicit field
group is rejected; all-fields queries use `search()`; heap fallback and recheck evaluate only the left
operand's field) is carried faithfully. What is missing is the fact that today a multi-column stannum
index **cannot reach that code path at all**: `score.rs:1946-1947` computes suitability as
`is_stannum && indisvalid && indisready && indnkeyatts == 1`, and `:1949` reads
`indkey.values[0]` as *the* key attribute with `var.varattno == key` at `:1955`. The "implicit outer
field scope" design has nothing to attach to until that gate is widened and the per-clause attribute
is carried. This belongs in the P0-2 phase-1 dependency list, and `score.rs:1943-1960` belongs in the
file-by-file table.

Related: "recheck evaluate only the left operand's field" needs a named code path, because
`search_recheck` (`customscan.rs:1468-1474`) is an unconditional `true` stub — the real recheck is
`passes_clause` inside `search_access` (`:1416`) and the count path (`:1514`), driven by the `clause`
/`runtime_query` `ExprState`s.

### 1.13 Forward-record discriminator — present ✓

Including the "stop if any WAL record parses postings" condition.

### 1.14 Grammar timing risk — present and verified ✓

`term_cont` (`grammar.pest:152`) excludes `(`/`)`/`[`/`]`/`"`/`~`/`^` but not `:`, and `base`
(`:68-77`) is an ordered choice with `word_primary` last, so `title:(foo)` really does parse as word
`title:` AND a group today, and inserting `field_query` ahead of `word_primary` is the right shape.
The plan's compound-atomic `field_head` note is correct.

---

## 2. Under-specified seams, contradictions, wrong references, missing dependencies

### 2.1 `SECURITY DEFINER` contradicts an existing release gate (high)

The plan (and both baselines) require the mutating dictionary functions to be `SECURITY DEFINER` in
SQL "so the definer property cannot broaden access". `postgres/tests/extension_upgrade.py:53` asserts:

```python
assert 'SECURITY DEFINER' not in actual.upper()
```

where `actual` is the normalized generated snapshot `postgres/sql/stannum--<version>.sql`. Shipping
the plan as written fails the SQL gate on the first P0-3 merge, and the plan's cross-cutting section
mentions only snapshot drift. **Resolution:** the definer property is not needed for the stated goal —
the Rust-side admin check (`superuser()` / `pg_database_owner`) runs as the invoker either way, and
the plan already resolves the extension schema from the extension OID rather than the caller's
`search_path`, which is the only thing `SECURITY DEFINER` + `SET search_path` would have bought.
Drop it and record the decision, or explicitly relax `:53` with a justification. Do not leave it
implicit.

### 2.2 Incorrect / stale `file:line` references (low individually, collectively corrosive)

| Plan says | Actual |
|---|---|
| `require_stannum_index` (udfs.rs:328) | `postgres/src/udfs.rs:303` |
| `require_index_select` (udfs.rs:335) | `postgres/src/udfs.rs:320` |
| `EXTENSION_PACKAGES["stannum"] = None` at `__init__.py:63` | `pgembed/src/pgembed/__init__.py:61` |
| `_standalone_extension_path` (:165-186) | `pgembed/src/pgembed/__init__.py:156-178` |
| "Create-name mapping already present (:272; …)" | `get_extension_create_name` at `__init__.py:234-256`, stannum fallback at `:255`; there is no `:272` mapping |
| `encode_term_bound` (postings.rs:187) | `segment/src/postings.rs:193` |
| `encode_bounds` (:207) / `encode_scored` (:230) | `:205` / `:228` |

Correct and worth closing as "confirmed": `PRUNE_MAX_K = 4096` (`score.rs:499`), `rank`
(`score.rs:493`), `visible_tids` (`score.rs:1083`), `build_index_scorer` (`score.rs:1224`),
`ScoreStopWords::from_csv` (`bm25.rs:104`), `push_scored` (`postings.rs:152`), `present`
(`storage/mod.rs:370`), `ENTRY_BYTES = 52` / `PENDING_BYTES = 16` (`layout.rs:197`, decode offsets
`:311-322`), page-special version gate for every kind (`layout.rs:95-100`), `Heap Fetches` from
`exec.fetched` (`customscan.rs:1806-1812`), `segment_rows` reading every dead run
(`storage/mod.rs:2832-2839`), reader cache across statements (`storage/mod.rs:869-954`),
`stannum.build_segment_docs` (`storage/mod.rs:132-141`), pgembed `psql()` with no kwargs
(`postgres_server.py:481`), `test_standalone_extensions.py:11-15` (pgvector only), `pgbuild/Makefile:278`
/ `:346` / `:756-766`.

### 2.3 A work item the code already satisfies

The plan asks to "widen `positions_from_query` / `highlight_text[_ansi]` to `pub(crate)`". All three
are already `pub(crate)`: `highlight.rs:95`, `:110`, `:157`. Mark it verified-not-needed so the
implementer does not churn visibility for nothing.

### 2.4 `fields: Option<FieldMeta>` in the 0.2.0 `Meta` is dead weight

The plan adds the field structurally now ("used by P0-2"), but the settled decision makes P0-2 an
RFC-only deliverable this cycle. That means 0.2.0 ships a `FieldMeta` type, a decoder branch for a tag
no writer emits, a second `Meta` literal in every test fixture, and a capacity accounting path — all
untested by any writer. It also contradicts the plan's own Risks note ("`FIELDS_TAG` ships in the
release that writes it"). The trailer loop is generic from day one, so nothing is lost by deferring
the field.

### 2.5 `Lengths` / `AreaFetch` field argument: the type change is unspecified

The plan says "`AreaFetch`/`Lengths` learn a field argument". Verified shapes that must change:
`AreaFetch::length(&self, ordinal) -> u32` (`segment/src/segment.rs:346-354`) and
`Lengths::Bytes(&'a [u8])`, whose `get` indexes `ordinal * 4` with **no field count in the type**
(`segment/src/segment.rs:~720-742`). `Lengths::Bytes` therefore cannot serve LSG4 without becoming
`Lengths::Fields { bytes, field_count }` (or taking the count from the reader). Say so.

### 2.6 Length chunking is byte-granular; LSG4 rows straddle chunks

`LENGTH_CHUNK = 4096` is a **byte** granularity and `Reader` caches one chunk by offset
(`segment/src/segment.rs:~690-715`). With doc-major `field_count × u32le` rows, one document's fields
can straddle a chunk boundary, so a single `field_length(ordinal, field)` may need two chunks while
the single-slot `last_chunk` cache holds one. The plan's "learn a field argument" does not cover this;
name the fix (fetch the whole document row, or a two-chunk window).

### 2.7 Missing dependency: statement identity for the dictionary dedup

The plan's reload protocol uses "a thread-local `(statement_id, fingerprint)`" in `dict.rs`.
`statement_id` comes from the statement counter owned by `score.rs` (`current_statement()` is private
and `note_executor_start` lives in `score.rs`). `dict.rs` needs a `pub(crate)` accessor (or its own
counter bumped by the same hook). Not listed anywhere.

### 2.8 `indnatts` vs `indnkeyatts` in the `search()` gate

The plan gates `search()` on `indnatts != 1`. The codebase's own suitability test uses
`indnkeyatts` (`score.rs:1947`), and `indnatts` counts INCLUDE columns, so `search()` would reject a
one-key index that has INCLUDE columns. Use `indnkeyatts` (and note the interaction with P0-2's
"reject INCLUDE columns").

### 2.9 The `pgembed_stannum` stop-word mirror has no parity mechanism

`_stopwords.py` "mirrors `builtin_stop_words`", but nothing keeps them in step: a change to
`postgres/src/stopwords.rs` silently diverges from the frozen Python copy. The plan already ships
`builtin_stop_words` precisely so Python never forks the list — the package should *call* it (as
`drop_stop_words` already does) and the mirror should either be deleted or covered by a parity test.

### 2.10 Under-specified test: the invisible-top-row fallback

"Delete a top row under a snapshot where the tid is dead to the heap and absent from the dead set"
requires holding an older snapshot than the active one. Nothing in the plan (or the baseline) says how
a `#[pg_test]` obtains one. Without a concrete mechanism this acceptance-rule test — the one that
keeps WAND correct — will not get written.

---

## 3. Details the code disproves, the task does not require, or a simpler design replaces

### 3.1 `am.rs build_callback` already passes the whole arrays

The plan lists "`am.rs` `build_callback` passes the whole `values`/`isnull` arrays" as a required P0-2
change. Verified: `build_callback` (`am.rs:90-104`) forwards `values`/`isnull` straight to
`state.builder.add(index, values, isnull, tid)`, and `aminsert` (`:117-129`) forwards them to
`storage::insert`. The deref-and-take-first-datum happens inside `storage::Builder::add`
(`storage/mod.rs:1837`) and `storage::insert` (`storage/mod.rs:1886`), including the early
`if *isnull` return that must become a loop over all key attrs. **Correction:** the P0-2 change is
confined to `storage/mod.rs`; drop the `am.rs` datum-plumbing item (keep only `am.rs`'s name/column
validation and the scan-key propagation).

### 3.2 The explain pin-count arithmetic is wrong

The plan: "converts the byte range to `div_ceil(len, BLCKSZ)` **pin counts**". Verified
`RunSource::read` (`storage/mod.rs:751-774`) walks pages with `at / CHAIN_CAPACITY` and
`at % CHAIN_CAPACITY`, one pin per page, where
`CHAIN_CAPACITY = CAPACITY - 4 = BLCKSZ - PAGE_HEADER - SPECIAL_SIZE - 4` (`layout.rs:38-40`).
`div_ceil(len, BLCKSZ)` undercounts any range that does not start at a run-page boundary: offset 4000,
len 4100 pins 2 pages but reports 1.
**Correction:** `div_ceil(offset % CHAIN_CAPACITY + len, CHAIN_CAPACITY)`. (This also matches the
existing doc comment on `RunSource`, "one buffer pin per page touched".)

### 3.3 `index_stats.dictionary_pages` undercounts the same way

The plan: `Σ ceil(dictionary_len / CAPACITY)` per immutable segment header. The dictionary extent
starts at an arbitrary offset inside the run (after the header and any preceding areas), so
`ceil(len/CAPACITY)` misses the page the extent starts on whenever it is not page-aligned — e.g. a
dictionary at run offset 8000 with len 400 intersects 2 pages but computes 1.
**Correction:** count intersected pages from the area offset,
`ceil((offset + len)/CHAIN_CAPACITY) - floor(offset/CHAIN_CAPACITY)`, or relabel the column as a
lower-bound estimate. Note also that this column and the explain `Dictionary Pages Read` counter then
have *different* definitions (all dictionary pages vs pages actually touched) — say which is which in
the column comment, because `index_stats` and `segment_info` are expected to agree.

### 3.4 `jieba_reload_dict` loses its force semantics

The plan's protocol installs "only when [the fingerprint] differs". That makes
`jieba_reload_dict()` a no-op whenever the table content is unchanged — including after a binary
upgrade that changed the *embedded* dictionary, which is exactly when an operator needs a forced
rebuild (the baseline's stated reason). **Correction:** keep "install only on change" for the lazy
path; make `jieba_reload_dict` always reinstall and bump the generation.

### 3.5 `full: true` is load-bearing — keep, and add the second half of the invariant

The plan correctly preserves `full: true` for `search`/`search_count`. The invariant it does not state
is the *snippet* half: `search()` must use **one** `Rc<CompiledTokenizerPipeline>` for both
`positions_from_query` and the highlight renderer. The plan's control flow compiles the scorer (which
captures generation N) and then calls `tokenizer_by_oid` again at snippet time (generation N+1 if a
reload landed in between), so query positions and document spans can be computed under two different
dictionaries. Capture the pipeline once.

### 3.6 Not required by the task

`Meta.fields` (see §2.4) and the `am.rs` datum plumbing (see §3.1) are the two clear cases of work the
settled scope does not need. Nothing else in the plan looks like scope creep; the specificity of the
byte layouts, decode rules, and acceptance rules is warranted and should stay.

---

## 4. Requirements, edge cases, dependencies, and architectural problems absent from both

### 4.1 WAND acceptance is unsound for HOT duplicates (high)

`top_k` returns **root** tids. Two roots can map to the same visible HOT member, so `limit` pruned rows
can be fewer than `limit` distinct visible documents even when every row is heap-visible — the plan's
acceptance rule ("complete or all visible") passes that result through and `search()` underfills. The
plan's own `visible_tid_pairs` comment ("duplicate → keep first root in TID order for count; force
exhaustive for search") fixes this only on the exhaustive path.
**Correction:** the acceptance rule must additionally require that deduplicating the pruned rows by
visible member collapses **nothing**; otherwise discard and run exhaustive. Add a `#[pg_test]` with a
HOT chain whose root and member both rank in the top k.

### 4.2 Parallel workers silently diverge from the leader on the dictionary (high)

The plan's fallback is "during recovery or where SPI cannot run (parallel worker), keep the embedded
dictionary and emit one WARNING per backend". Unaddressed: a parallel worker has its **own**
thread-locals (`TOKENIZERS`, the holder), so once it compiles a jieba pipeline it analyzes with the
embedded dictionary for the life of the worker — and its WARNING may never reach the client session.
The `==>` custom scan is the path that runs in workers, so this is reachable in production, not just
in tests. Either the custom scan must be `PARALLEL UNSAFE` for jieba indexes, or the worker must
obtain the leader's snapshot (parallel workers are forked after planning, so a DSM/shm_toc slot or a
leader-side pre-compile is required — that is a design decision, not a detail).

### 4.3 `TOKENIZERS` grows without bound across reloads (medium)

The cache key gains the generation, so every reload inserts a new entry per spec and nothing evicts
the old ones. Contrast the neighbouring caches, which do bound themselves: `PAGE_TABLES` clears past
4096 entries (`storage/mod.rs:~723`) and `SEGMENT_READERS` trims past 64 MiB
(`storage/mod.rs:956-969`). A long-lived backend that reloads often (agent adds words per request)
leaks one compiled pipeline per reload. Add the same trim, or key by fingerprint so identical content
reuses the entry.

### 4.4 The RRF planner policy cannot be scoped to the vector arm (medium)

`SET LOCAL enable_indexscan = off, enable_bitmapscan = off` is transaction-scoped and applies to the
**whole** fusion statement: it also disables the primary-key index scan on the final
`JOIN <table> d USING (id)` and any index the bm25 arm's `d.ctid = s.ctid` join would use. The
baselines' "for the vector arm only" intent is not expressible in one statement. Options: run the
vector arm as its own statement and fuse in a second (two round trips, one transaction), or accept
whole-statement disabling and document the join cost, or force materialization of the vector arm in a
`MATERIALIZED` CTE and leave scans enabled. Pick one before the Python code is written.

### 4.5 Upgrade-script fingerprint equality is an unlisted gate (medium)

`extension_upgrade.py` compares a post-upgrade object fingerprint against a fresh-install fingerprint
(`:66-106`, asserted at `:122`). Any asymmetry between `stannum--0.1.0--0.2.0.sql` and
`stannum--0.2.0.sql` — a `GRANT … TO pg_database_owner` present in one and not the other, the
`REVOKE … FROM PUBLIC`, the view definition — fails the gate. The plan's cross-cutting section
mentions only snapshot drift and "read the test first".

### 4.6 Insert path and a concurrent REINDEX (low)

The plan says insert/fold/merge round-trip `analysis` unchanged, which is right. The insert loop
re-reads `(identity, spec)` and retries on mismatch (`storage/mod.rs:1887-1894`); a concurrent REINDEX
can change the *stamp* between that read and the meta write. State explicitly that the insert path
never re-stamps (it round-trips what it decoded), so a retry cannot publish a stamp the rebuilt index
does not have — and that P0-2's fields-tag comparison joins the same tuple.

### 4.7 Standby drift is permanent and undocumented (low)

On a hot standby, `check_analysis` in `storage::view` will WARNING on every query after a dictionary
change, and REINDEX is impossible until promotion. The plan's standby paragraph covers reload but not
the diagnostic consequence. Also, the plan conflates "recovery in progress" with "SPI cannot run":
reading `jieba_words` on a standby is legal, so the embedded-dictionary fallback is really only about
parallel workers and startup — say which.

### 4.8 Cancellation inside the dictionary read (low)

The plan puts `check_for_interrupts!` in the search loops; the baselines also require that
cancellation during the dictionary-table load leaves the previous holder installed with the dirty
flag set. The plan's error table omits it.

### 4.9 `Heap Rechecks` has two call sites (low)

`passes_clause` is invoked at `customscan.rs:1416` (search) and `:1514` (count). The plan should say
both, and note that `search_recheck` (`:1468-1474`) is a `true` stub, so the ExecScan-level recheck
never fires and the counter is purely the internal one.

---

## 5. Questions whose answers would materially change the design or implementation order

1. **`TOKENIZERS` key: generation counter or fingerprint?** The plan uses the generation. If the key
   were the fingerprint, an identical reinstall (or a second backend reaching the same content) would
   reuse the compiled pipeline instead of recompiling it, and a forced `jieba_reload_dict` could bump
   the generation without invalidating the cache. Answer changes §1.3, §3.4 and §4.3 together.
2. **`MAX_FIELDS = 16` (packed `field_id << 4 | tf_bucket`) or 32 (`INDEX_MAX_KEYS`)?** The packed
   byte is what forces 16; Oracle 2's baseline chose 32 for parity with PostgreSQL. This is an RFC
   freeze decision that changes the payload layout, the bound masks (`u16` today), and the trailer's
   `field_count u8` range — and it cannot be revisited after bytes ship.
3. **Does the LSG4 header carry a `layout_revision varint`?** Oracle 1 had one, Oracle 2 and the plan
   do not. It interacts with §1.11's head-probe fix (the probe size depends on the answer) and is
   cheap to decide now, expensive later.
4. **How is the vector arm's plan controlled in `hybrid_search` — one statement or two?** See §4.4.
   This decides the Python structure (one `psycopg2` execute vs two plus a temp/CTE), the
   transaction shape, and whether `SET LOCAL` is used at all.
5. **What does `search()`'s `limit` count — pruned rows or distinct visible documents?** See §4.1.
   If distinct documents, the acceptance rule needs the dedupe check and `top_k` must be called with
   a larger k; if rows, the HOT case must be documented as "may return fewer than `limit` distinct
   ids", which changes the Python contract too.
6. **Do parallel workers need the custom dictionary?** See §4.2. A "no" makes the custom scan
   `PARALLEL UNSAFE` for jieba indexes (a visible planner/performance change that should be decided
   before P0-3 lands, not after); a "yes" requires a leader-to-worker snapshot mechanism that no part
   of the plan currently budgets for.

---

## Recommended next actions (ordered)

1. Fix the three correctness defects that are cheap now and expensive later: the WAND/HOT acceptance
   rule (§4.1), the pipeline snapshot reaching `JiebaIter` and `SourceSpanIter` (§1.1), and the LSG4
   head probe (§1.11).
2. Resolve the `SECURITY DEFINER` contradiction with `extension_upgrade.py:53` (§2.1) before any P0-3
   SQL is written.
3. Correct the two arithmetic formulas (§3.2, §3.3) and drop the two items the code already satisfies
   or the scope does not need (§3.1, §2.4).
4. Record the six decisions in §5 — at minimum #2, #3 and #4, which block the RFC freeze and the
   Python implementation respectively.
