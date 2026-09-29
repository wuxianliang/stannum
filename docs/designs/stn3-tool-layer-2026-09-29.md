# STN3 Rebase: Stable Tool Layer over the Upstream Engine

Status: proposal, revised · Basis: `main@24f5c02` (0.4.0) vs `upstream/main@d57ef58` (STN3) · 2026-09-29

Rebuild this fork's agent-facing capabilities — multi-field BM25F, same-field
phrases, field-aware highlights, jieba, `search()`, `pgembed_stannum` — on top
of upstream's STN3 engine, so that **the layer pg-agent consumes stays stable
no matter how the engine changes underneath**.

This revision settles three choices. Every gate below uses them.

- Scoring is 0.4.0's fused BM25F (§5.1), not a sum of per-field scores.
  Ordering parity against recorded 0.4.0 answers is the gate. A different
  rank order is a contract bump, and it says so.
- Cutover is a downtime window with a clean shutdown and a server restart
  (§8). Replacing `stannum.so` under a running postmaster is not a cutover.
  Two copies of one extension in one cluster is not a plan.
- The frozen surface is the 0.4.0 snapshot in §4.1, not a prose sketch.

## 1. Why a rebase, not a merge

- Upstream replaced the segment format and execution model (STN3: ordinal
  streams per term, 65,536-doc chunk folds, page tables, dead-doc bitmaps,
  block-max WAND with per-chunk/sub-block bounds, hot-standby reads). A dry-run
  merge reports 31 conflicting files across `segment/src`, `postgres/src`,
  `tinql/src`, and the conflicts are semantic, not textual: two execution
  models cannot be interleaved.
- Upstream does not read `LSG1`–`LSG5`; our LSG4 (multi-field) and upstream's
  transient "LSG4" (an STN3 predecessor) share only a name.
- Our 0.4.0 features exist nowhere upstream; upstream's perf, memory, fault
  tolerance and conformance work lands only on STN3. The gap widens with every
  upstream release. Continuing on the LSG3/LSG4 lineage forfeits all of it.
- What must survive is not the LSG4 *format* — it is the *behavior* pg-agent
  depends on. That is a contract, and contracts can be re-implemented.

## 2. Design principles

1. **The contract is SQL, not Rust.** pg-agent touches operators, functions,
   index options and GUCs. That surface is the stability boundary. Everything
   below it (segment format, plan shapes, scoring internals) may churn, except
   where a churn would change a recorded answer.
2. **Narrowest seam that still owns the behavior.** Features enter through
   tokenizer plugins, the fused scorer, and tool UDFs. Owned shims, in files
   upstream also edits, are `operator.rs`, `score.rs` (`scope_scan_query`),
   `highlight_udfs.rs`, `customscan.rs`, `tinql/`, `am.rs`, `options.rs`,
   and `storage/layout.rs` (§3). Reloption registration is not
   upstream-stable. The `segment` crate stays structurally upstream except
   the files that write and read `STNF`: `Reader::new` in
   `segment/src/segment.rs` (stock checks stop at `pages_end`; only the
   trailer API reads past it), plus merge, verify, and the mutable index
   (§5.1, §9). The format delta is that reader edit, the trailer, and the
   meta envelope, not a fork of the key type.
3. **The contract is enforced by tests, not intent.** A declarative contract
   suite records 0.4.0's answers and gates every future build. Upstream merges
   are safe exactly when the suite is green.
4. **0.4.0 is the oracle, not the codebase.** Parity means matching recorded
   answers under the §5.1 formula, not porting LSG4 bytes and not inventing a
   new score. A wanted divergence is a `contract_version` bump with an entry
   whose oracle is not `stannum-0.4.0`.

## 3. Layer architecture

```
L3  packaging      pgembed_stannum wheel, standalone binaries
                    (depends only on L2's SQL, never on Rust types)
L2  tool contract  the §4.1 surface, in postgres/src/tool/
L1  field model    fielded streams + fused BM25F (§5). postgres/src/fields/
                    owns the codec, the scorer, and the sidecar readers
L0  engine         upstream STN3, merged on our cadence
```

`IndexScorer` and `storage::View` are 0.4.0 internals. They are not a seam
that survives the rebase, and L2 does not import them.

Owned, fork-controlled:

- `postgres/src/tool/` — the SQL entry points in §4.1, including `capabilities()`.
- `postgres/src/fields/` — fielded-term codec, fused scorer, norms and
  aggregate-df readers. L2 calls this. It does not reach into segment
  internals except through the sidecar API this module owns.

Consumed as upstream-stable, except for the jieba exceptions in §6:

- Tokenizer plugin API (`Index::term(&str)` and the `Window` / `expand`
  shapes). The expansion adapter in §5.1 sits on top of that API. It does
  not replace it.

Not upstream-stable: the 16 reloptions and the AM flags. Upstream has no
`field_weights`, `jieba`, `score_stop_words`, or the five ignored TIN
options. `amcanmulticol = true` in `am.rs` is what permits
`CREATE INDEX (title, body)`. A merge that keeps upstream `am.rs` or
`options.rs` drops BM25F at DDL.

Shared files upstream also edits. These shims are owned surface. A merge
conflict in them is expected and reviewed, not evidence the seam slipped:

- `operator.rs` — `check_clause_field_scope`, and the suitability path.
  Upstream `d57ef58` still requires `indnkeyatts == 1`
  (`score.rs` around the index match). This fork's
  `matching_stannum_indexes` already walks every key and returns the
  matched ordinal. The rebase keeps that widening. Dropping it means a
  multi-column index is never chosen for `==>`. Widening without carrying
  the ordinal makes every clause the first key. The ordinal rides through
  custom scan, bitmap, cost, heap fallback, and recheck. WS4 tests
  `title ==>`, `body ==>`, bitmap execution, and `enable_custom_scan = off`.
- `score.rs` — `scope_scan_query` applies the implicit `==>` scope. `Private.field`
  only carries the scan key's field into that call. Without the application,
  a `body` hit can satisfy `title ==> …`.
- `highlight_udfs.rs` — column-to-field rewrite, the unknown-field error,
  and `SUPPORT` only on the forms §4.1 lists.
- `customscan.rs` — `Private.field` is the carrier, not the enforcer.
- `am.rs` — `amcanmulticol`, and the build/insert entry that reads every key
  column. `ambuild` / `build_empty` replace meta and read the heap; they do
  not run the migration check (§8).
- `options.rs` — all 16 reloptions, including the five ignored TIN options
  and their warning. Not an upstream pass-through.
- `storage/layout.rs` — page-kind constants and `kind()`. `d57ef58` defines
  only `KIND_META = 1`, `KIND_BUFFER = 2`, `KIND_RUN = 3`, `KIND_FREE = 4`
  (`postgres/src/storage/layout.rs`). Kind 5 is free there. `verify.rs`'s
  `kind_name` match names an unknown kind as `unknown kind {other}` and must
  grow an arm. Meta load in `storage/mod.rs` compares `kind == KIND_META`
  (the checks around the meta read, the free/run/buffer paths, `spec_by_oid`,
  `fields_meta`, builder init, and WAL redo of the meta page). Those callers
  go through one loader. An arm that omits kind 5 treats a current index as
  corruption. A future upstream snapshot that assigns kind 5 is a merge
  conflict, checked when we pin that snapshot. We do not share the kind.
- `tinql/` — `Expr::Field` grammar and AST, `project_to_field`
  (`tinql/src/runtime/eval.rs`), and per-field phrase/span planning. §1's
  dry-run already conflicts in `tinql/src`. A `fields/` module does not
  absorb it.

The segment crate is not pristine. The format delta is `Reader::new` in
`segment/src/segment.rs` (stock checks stop at `pages_end`; only the trailer
API reads past it), the `STNF` trailer, and the meta envelope. Not a fork of
the key type. Pinned `d57ef58` keeps `Index::term(&str)`. Encoding and
decoding sit in `fields/`. Writers we own: index build, the insert buffer,
merge, and verify. `fields/` does not absorb the shims.

## 4. The tool contract (L2)

### 4.1 Frozen surface

This section is the readable summary. The authority is
`postgres/sql/stannum--0.4.0.sql` plus a generated manifest of the 16
reloptions in `postgres/src/options.rs` and the GUC registrations in
`storage/mod.rs`, `storage/wal.rs`, and `customscan.rs` (types, domains,
defaults, overload identity, SET/RESET). WS0 writes that manifest.
Disposition: **keep** (0.4.0 behavior), **planner** (not a user call;
required for plans), **new** (0.5.0 only). Where this summary and the
snapshot disagree, the snapshot wins.

`pgembed_stannum` calls `search`, `search_count`, `index_analysis`,
`builtin_stop_words('auto')`, and `tokenize(text, tokenizer => …)`.
`verify_index` is `StannumIndex.check_health` in `_index.py` only;
`_hybrid.py` does not call it. WS5 checks those call sites against this
list before the wheel moves. The extension-contract floor is PostgreSQL 17.
The wheel is PostgreSQL 18 only (`BUILT_FOR_POSTGRES_MAJOR = 18`); WS5
targets 18, and a green PG17 contract replay is not that wheel. There
is no `tokenize(index, …)` overload. Index settings reach highlights through
the `indexed_query` overloads and `search` snippets.

**Access method and operators (keep).** `amhandler(internal) → index_am_handler`
IMMUTABLE STRICT PARALLEL SAFE; access method `stannum`.
`==>` (`text, text`) via `stannum_text_cmpfunc` IMMUTABLE STRICT PARALLEL SAFE,
`SUPPORT stannum_text_cmpfunc_support`. `==>` (`text, indexed_query`) via
`stannum_text_cmpfunc_indexed` STABLE STRICT PARALLEL SAFE, no SUPPORT.
Both `RESTRICT stannum_text_restrict(internal, oid, internal, integer) → float8`
PARALLEL SAFE STRICT. Opclass `stannum_text_ops` DEFAULT FOR `text`,
operators 1 and 2, STORAGE `text`. Type `indexed_query` (variable, extended)
with `indexed_query_in` / `indexed_query_out`. `bind_query(query text, index oid) → indexed_query`
IMMUTABLE STRICT PARALLEL SAFE.

**Search and score (keep).** `search(index regclass, query text, "limit" int = 10, snippet text = 'html', begin_tag text = '<mark>', end_tag text = '</mark>', k1 real = NULL, b real = NULL) → TABLE(ctid tid, score real, snippet text)`
VOLATILE PARALLEL UNSAFE. The tags are `begin_tag` and `end_tag`, not one
`tags` argument. `search_count(index regclass, query text) → bigint`
VOLATILE PARALLEL UNSAFE.
`score(ctid tid, dense_ratio real = 0.10, k1 real = NULL, b real = NULL, term_add text[] = NULL, term_replace text[] = NULL) → real`
IMMUTABLE PARALLEL UNSAFE, SUPPORT `score_support`.
`full_score(ctid)` IMMUTABLE STRICT PARALLEL UNSAFE, and
`full_score(ctid, k1, b)` IMMUTABLE PARALLEL UNSAFE, both SUPPORT `score_support`.
`max_score(ctid) → real` IMMUTABLE STRICT PARALLEL UNSAFE, SUPPORT `score_support`.
`score_inspect(index regclass, query text, dense_ratio real = 0.10, term_add text[] = NULL, term_replace text[] = NULL) → TABLE(term text, weight real)`
VOLATILE PARALLEL UNSAFE.

**Highlight (keep).** `highlight(text, begin_tag = '<b>', end_tag = '</b>', query = NULL)`
IMMUTABLE PARALLEL SAFE, SUPPORT `highlight_support`.
`highlight(text, begin_tag, end_tag, query, field)` IMMUTABLE PARALLEL SAFE,
no SUPPORT. `highlight(text, begin_tag, end_tag, query indexed_query)` and
`highlight(text, begin_tag, end_tag, query indexed_query, field)` are STABLE
PARALLEL SAFE, no SUPPORT. `highlight_ansi(text, wrap_to = NULL, query = NULL)`
IMMUTABLE PARALLEL SAFE, SUPPORT `highlight_support`.
`highlight_ansi(text, wrap_to, query indexed_query)` STABLE PARALLEL SAFE,
no SUPPORT. Unknown field on the bound form errors
`stannum.highlight(): unknown field '<name>'`.

**Planner-only (planner).** `highlight_support(internal)`, `score_support(internal)`,
`stannum_text_cmpfunc_support(internal)`, all IMMUTABLE PARALLEL UNSAFE.
`score_bound(document text, query text, heap_oid int, index_oid int, mode int, dense_ratio real, k1 real, b real, term_add text[], term_replace text[])`
and `score_bound_indexed(ctid, …)` the same tail, both VOLATILE PARALLEL UNSAFE,
`REVOKE ALL` FROM PUBLIC. No SUPPORT clause on either.

**Dictionary, analysis, TIN helpers (keep).** Table `jieba_words(word text PK, freq int NOT NULL DEFAULT 0 CHECK (freq >= 0), tag text)`,
`REVOKE ALL` FROM PUBLIC. `jieba_add_word(word text, freq int = 0, tag text = NULL) → void`
VOLATILE PARALLEL UNSAFE. `jieba_delete_word(word) → void` STRICT VOLATILE
PARALLEL UNSAFE. `jieba_dict_version() → bigint` STRICT STABLE PARALLEL UNSAFE.
`jieba_reload_dict() → void` STRICT VOLATILE PARALLEL UNSAFE.
`index_analysis(index regclass) → TABLE(index_name, recorded_jieba_version, recorded_dict_fingerprint, runtime_jieba_version, runtime_dict_fingerprint, matches, status)`
STRICT STABLE PARALLEL UNSAFE.
`builtin_stop_words(preset text) → SETOF text` IMMUTABLE STRICT PARALLEL SAFE.
`tokenize(text, tokenizer = 'unicode', case_folding = 'fold', accent_folding = 'fold', long_tokens = 'split', max_token_bytes = 256, graphemes = 'emoji', position_gaps = 'preserve') → SETOF text`
STABLE PARALLEL UNSAFE.
`ql_parse` takes the same option tail plus `surface bool = true`, returns
text, STABLE PARALLEL UNSAFE. `maybe_quote(text) → text` IMMUTABLE PARALLEL SAFE.

**Diagnostics (keep).** `verify_index(index regclass, heap_check bool = false) → TABLE(severity, location, message)`
STRICT VOLATILE PARALLEL UNSAFE. `segment_info(index) → TABLE(ordinal, kind, root_block, docs, dead_docs, sum_doc_lengths, total_pages, generation)`
STRICT VOLATILE PARALLEL UNSAFE.
`index_stats(index) → TABLE(documents bigint, dead_documents bigint, dead_ratio float8, segments int, immutable_segments int, mutable_segments int, next_generation bigint, total_pages bigint, dictionary_pages bigint, total_length bigint, average_length float8, analysis_matches bool, analysis_detail text)`
STRICT VOLATILE PARALLEL UNSAFE. `average_length` is hard-coded `0.0`
(`udfs.rs`: averages are reserved; v1 reports zero rather than a misleading
mean). A fixture that expects a real mean is wrong; the recording is `0.0`.
View `index_health` is `security_invoker`. `version() → text` IMMUTABLE STRICT
PARALLEL SAFE. `index_reads_allowed(index) → bool` and
`logs_removal_horizons(index) → bool`, both STRICT VOLATILE PARALLEL SAFE.
`wal_rmgr_id() → int` STRICT STABLE PARALLEL SAFE is a **function**. The GUC
of the same name is listed below and is not this function.

**New.** `capabilities() → jsonb` (§4.2). It does not exist in 0.4.0. It must
appear, byte-identical, in `stannum--0.5.0.sql` and in the upgrade script (§8).

**Reloptions (16), keep.** Five are accepted and ignored, with the warning
`Stannum accepts {name} for TIN compatibility but ignores it; Stannum's storage and maintenance settings apply`
(`options.rs`):
`initial_segment_count` (int, default 1, 1..4096), `target_segment_count`
(1, 1..4096), `max_mutable_segment_size` (4194304, 131072..max),
`max_merged_segment_size` (2000, 100..max), `dead_percent_threshold`
(0.5, 0..1). If the STN3 port honors any of them, that is a meaning change
and a `contract/divergences` entry, not a silent pass-through.

The other eleven are behavior: `tokenizer` enum `unicode | whitespace | jieba`
(default `unicode`); `case_folding` and `accent_folding` enum `preserve | fold`
(default `fold`); `long_tokens` enum `truncate | discard | split` (default
`split`); `max_token_bytes` (default 256, range 4..2692); `graphemes` enum
`discard | emoji | retain` (default `emoji`); `position_gaps` enum
`collapse | preserve` (default `preserve`); `k1` (default 1.2, 0..10000);
`b` (default 0.75, 0..1); `field_weights` (string; `ALTER INDEX … SET` is
rejected, `REINDEX to change field_weights`); `score_stop_words` (string;
`auto` / `auto:zh` / `auto:en` plus literals; `full_score` ignores the list).

**DDL limits (keep).** At most 16 key columns. The cap lives on the meta
envelope (§8), not on the legacy TAG `0x02` trailer. Every 0.5.0 index
writes the envelope; a single-column index has `field_count` 1 and no `STNF`
trailer; a multi-column index has `field_count` `2..=16` and a trailer.
Multi-column indexes reject expression keys
(`attnum <= 0`). `amcaninclude` is not set, so INCLUDE is not supported — the
INCLUDE shape test builds a non-stannum index and is not evidence otherwise.
Dropping the 16-column cap is a contract bump: the mask is `u16`, and fielded
terms no longer need LSG4's packed nibble, but the cap stays until that bump.

**GUCs that keep their 0.4.0 meaning.** `stannum.strict_analysis` (bool,
default false, userset): stamped drift errors when on; a missing stamp still
warns. `stannum.enable_custom_scan` (bool, default true, userset).
`stannum.experimental_vacuum_merge_strategy` (enum `auto | direct | reconstruct`,
default `auto`). `stannum.write_buffer_bytes` (1048576, 1024..67108864),
`stannum.write_buffer_docs` (512, 1..1000000), `stannum.build_segment_docs`
(32768, 1..10000000), `stannum.max_merge_docs` (1024, 0..max),
`stannum.max_segments` (128, 1..128), `stannum.merge_tier_factor` (8, 2..64).
`stannum.wal_rmgr_id` is both. The SQL function is above. The GUC is int,
postmaster, default `RM_MIN_CUSTOM_ID` (128), registered in `wal.rs::init`
only while `shared_preload_libraries` is loading. It is not function-only.

`stannum.reader_cache_mb` and `stannum.count_fold` are not 0.4.0 GUCs and are
not frozen. A GUC that arrives with upstream is newly exposed: it may be
added, and it does not inherit a 0.4.0 meaning it never had.

### 4.2 Capability negotiation

```sql
CREATE FUNCTION stannum.capabilities() RETURNS jsonb
  IMMUTABLE STRICT PARALLEL SAFE;
-- {"contract_version": 1,
--  "engine": {"name": "stannum", "format": "STN3", "version": "0.5.0"},
--  "features": {"bm25f": true, "field_phrases": true, "field_highlights": true,
--               "highlight_field_arity": 5,
--               "tokenizers": ["unicode", "whitespace", "jieba"],
--               "stop_word_presets": ["auto", "auto:zh", "auto:en"],
--               "jieba_ddl": true, "standby_reads": true},
--  "limits": {"max_expansion": 1024}}
```

The base is `d57ef58`, so #88's engine protections are present. They are not
optional. `capabilities().limits` reports the values 0.5.0 actually enforces,
not a sketch. Where an enforced limit or its error text differs from 0.4.0 —
`Limits::default().max_expansion` is 1024 (`tinql/src/runtime/plan.rs`) —
`contract/divergences/stannum.yaml` carries an expected-error fixture. A
tighter #88 limit is a recorded divergence, not a silent change, and the
jsonb shows the tighter value. `regex_max_bytes` is omitted until that entry
exists.

No special ACL. Acceptance compares the parsed key set and the order of
arrays. `jsonb` sorts object keys by length then bytes, so the sketch order
is not a byte contract. A fixture that wants text records PostgreSQL's
actual `jsonb` output, never the sketch. The return type stays `jsonb`.
An extra key is a contract bug. The WS0 manifest records this. pg-agent
calls `capabilities()` once at startup and feature-gates. `contract_version`
1 means the §5.1 formula and the §4.1 surface.

### 4.3 Stability policy

- `contract_version` bumps only with a breaking change to the frozen surface
  or to recorded rank order. Deprecations keep old spellings for one version
  with a warning.
- The normative surface is `postgres/sql/stannum--0.4.0.sql` plus the
  generated reloption/GUC manifest (§4.1). `docs/tool-contract.md` is a WS0
  deliverable generated from those two. It explains them. It is not edited
  ahead of them. The contract suite (§7) is the executable form. A change
  that alters recorded answers without a contract bump is a bug. The
  float-tail caveat in §7 does not cover a different formula.

## 5. Multi-field BM25F on STN3 (L1)

### 5.1 Fielded streams, fused score

**Choice: parity.** The score is 0.4.0's fused BM25F. The previous per-field
formula — per-field idf, per-field saturation, raw tf, no `(k1+1)` — is
withdrawn. It cannot meet the ordering gate, and it is not what ships.

A multi-column index tokenizes each column into **fielded terms**. Storage
is stock STN3 per term: its own ordinal stream, tf, block bounds, and
position stream. The dictionary key stays a `String`. Pinned upstream
`d57ef58` (`segment/src/index.rs`) exposes `Index::term(&str)`,
`Window::Prefix` / `Window::Range` of `&str`, `expand`'s `Fn(&str)` filter,
and `Expanded::Terms(Vec<(String, …)>)`. This fork's mutable index is
`FxHashMap<String, TermData>`. A `String` cannot hold `0xFF`. That prefix
is withdrawn.

The key is UTF-8 and injective. `ordinal` is the 0-based key-column index,
not `attnum`, one hex digit under the 16-column cap:

```
fielded_key(ordinal, token) = "~" + format!("{:x}", ordinal) + "~" + escape(token)
escape: every '~' in the payload doubles ('~' → '~~')
```

The header is exactly one lowercase nibble (`{:x}`, never decimal, never
`{:02x}`). Every tilde in the payload doubles, including ones that look like
a header. Token `~0~foo` in field 0 encodes `~0~~~0~~foo`, not `~0~~0~foo`.
Multi-column dictionaries store only encoded keys. Single-column indexes do
not use this encoding. The surface spelling `title:foo` is still one word
(`:` is a word character); field syntax is `title:(foo)`. Errors and
`score_inspect` show the surface token, never the encoded key.

The expansion adapter in `fields/` is the only caller of `Index::term`,
`Window`, and `expand` for a multi-column index. It does not fork the key
type. Its result is one of:

```
FieldTerm<'a> { field: u8, term: Term<'a> }
LogicalTerm<'a> { text: String, mask: u16, df_agg: u64, streams: Vec<FieldTerm<'a>> }
Lookup<'a> = Term(LogicalTerm<'a>) | Terms(Vec<LogicalTerm<'a>>) | Overflow
LogicalPostingCursor<'a> { current_ordinal, advance, field_hits, next_bound_interval }
```

`text` is an owned `String`, the decoded surface token. It is not a slice
of the encoded key. Every `~` is unescaped. `<'a>` constrains only the
`Term` streams. The cursor wraps those streams. It does not copy them.
Planner and scorer consume `LogicalPostingCursor`, not the struct alone.
`current_ordinal` is the document-table ordinal. `advance` steps every
stream to the next ordinal at or after the target. Equal ordinals coalesce
into one candidate. `field_hits` returns each field's payload and positions
at that ordinal. `next_bound_interval` is the fused interval end from the
truncation rule. A direct lookup absent from every selected field is
`Lookup::Term` with empty streams: no candidates, not an error. Fixtures
cover a token in both fields, in one field, and in none.

Both lookup and expansion open only fields in the scope mask. An unscoped
query's mask is every field (`u16::MAX` when `field_count` is 16, because
`1u16 << 16` does not fit; `all_fields_mask` in `score.rs` already does
this). A scoped query's mask is `1 << ordinal`. The `LogicalTerm`'s mask is
that scope. There is one fused scoring key per decoded token, never one key
per field.

| Where the token occurs | Scope | Streams opened | `df_agg` |
|---|---|---|---|
| title only | unscoped | title | unscoped sidecar |
| body only | unscoped | body | same sidecar value |
| both | unscoped | title and body, one key | same sidecar value |
| either | `title:(…)` | title only | same sidecar value; `tf*` is title only |

A field with no posting at ordinal `o` does not remove `o` from the
candidate union and does not contribute `tf*`.

- Direct lookup calls `term` once per field in the mask.
- Expansion runs one `expand` per field in the mask, on that field's escaped
  window, then groups by decoded text. The collapsed mask is the scope, not
  the set of fields that happened to hit. Order is `(decoded text, mask)`.
  `max_expansion` counts logical tokens once, globally.
- Expansion is two stages, and it does not need a new `expand` seam.
  Upstream `expand` already takes `limit: usize` and returns
  `Expanded::Overflow` (`segment/src/index.rs` on `d57ef58`). Each field in
  the mask is expanded with limit `max_expansion`. A field's decoded tokens
  cannot outnumber the union, so a per-field overflow is a real global
  overflow. The adapter then merges, dedups, and counts decoded tokens.
  Field copies are counted once. If that count exceeds `max_expansion`, the
  result is `Lookup::Overflow` and the lists are discarded.
- `Lookup::Overflow` is not an error and not an empty result. The plan is
  `Plan { cursor, exact: false }`. On 0.4.0 that cursor is
  `inexact_universe`: every document (`plan.rs`). `AND` intersects and stays
  inexact if any child is. `OR` unions. `NOT` of an inexact plan is the
  universe again. There is no second evaluator.
- Three SQL outcomes, matching `24f5c02`, not a recheck on every path.
  `==>` on the custom scan is exact because `candidates_in_view` and
  `stream.rs` set `exec.recheck` when `!planned.exact`. Those sites are not
  the bitmap path. With `enable_custom_scan` off, `amgetbitmap` adds TIDs
  to the PostgreSQL bitmap and must pass `recheck = !planned.exact` into
  `tbm_add_tuples`. Otherwise `BitmapHeapScan` does not re-run `==>`, and
  an `inexact_universe` cursor returns every document. `search` and `search_count` call `matching_tids`,
  which walks `planned.cursor` and never reads `planned.exact`. The recorded
  count and membership are that cursor. No recheck, no error. `score` and
  `score_bound_indexed` compile terms through `expand_in`, which calls
  `expand` with `usize::MAX` and keeps `Expanded::Terms` only. A scan-cap
  overflow does not truncate that list. The discarded scan list is not the
  scoring set. The over-limit fixture locks the recording: superset count,
  superset membership, and those scores. It does not assert the word
  "exact." Wanting per-document exact evaluation on `search` is a
  `contract/divergences` entry, and a `contract_version` bump if answers
  change (§4.3). §5.2 and WS3 are the arithmetic smoke and the WAND
  invariant. They are not this fallback.
- A missing range bound does not cross fields. The fence is the field
  header: lower-open starts at `~{h}~`; upper-open ends before `~{next}~`
  (`~f~`'s upper fence is the end of that prefix, not `Window::All`).
- Regex and fuzzy use `Window::All` inside the mask, with the same two-stage
  limit. They do not keep a prefix of the matches.
- A malformed encoded key at query time is `error`, not `Error::Corrupt`.
  Verify still treats a malformed key in a stored dictionary as corruption:
  header not `~` + one lowercase hex nibble + `~`, a trailing incomplete
  escape, or an ordinal greater than 15 or not less than `field_count`.
- Phrase and span expansion keeps each field's positions on that field.
  Intervals are not compared across fields.
- The token-byte limit applies to the analyzed token. The encoded key is
  longer and is not truncated.

Those streams are the posting representation, not independent score leaves.
Saturation is nonlinear: `sat(Σ w·tf, Σ w·len)` is not `Σ w·sat(tf, len)`.
An unscoped term is one scoring key. The scorer opens every fielded stream
under that key's mask, fuses them, and applies one saturation.

```
tf*    = Σ_{f ∈ mask ∩ present} w_f · representative_count(bucket_f)
         left to right in field order; one dequantize; never re-quantized; never raw tf
len*   = Σ_{all index fields} w_f · length_f          exact u32, left-to-right f32
         the mask does not apply: a title-scoped term still sees body length
avgdl* = (Σ_{all index fields} w_f * (field_total_f as f32)) / (N as f32)
         left-to-right f32, the build_index_scorer fold; not an f64 sum
         N = Σ segment document_count, including dead, until rewrite; N = 0 yields 1.0
idf    = bm25_idf(N, df_agg) as f64, then cast to f32
score  = (idf_f32 * boost * tf* * (k1 + 1))
         / ((tf* + k1 * (1 - b)) + (k1 * b / avgdl*) * len*)
```

`bm25_idf` is `f64`. The cast is before `multiplier = idf_f32 * boost`,
matching `Bm25fScorer::from_statistics`. The product inside `saturate` is
`f32`. Multiplying in `f64` and casting the score is a different model.
Dequantize is `TfBucket::representative_count` of the bucket
`TfBucket::from_count` would assign. If STN3's native tf is not that bucket,
the scorer still quantizes with 0.4.0's. Adopting a different bucket is a
recorded divergence, not the default. Terms still combine by
`sum_scores_in_order` in `(decoded token, mask)` order. R-BIT — bit-equality
with `TermScorer` — holds only for a single-column index, where there is one
field, weight 1.0, and no fielded key. A one-field mask on a multi-column
index still uses full `len*` (case 5). The product never writes a one-field
fielded segment; that LSG4 fixture is a codec/scorer property test, not a
SQL smoke. `weighted_tf` skips fields outside the mask; `weighted_length`
does not.

**`df_agg` is stored. It does not fall out of the fielded dictionary.**
Each fielded entry has a field-local df, because it is an STN3 term. Summing
those dfs double-counts a document that holds the term in more than one
field. The idf input is the index-time union cardinality: distinct document
ordinals that contain the token in any field, inside one segment. It is a
`u64` per unscoped token, keyed by the analyzed text, not by the fielded key.
Build and flush count those ordinals across the token's field streams,
including dead ordinals the segment still stores. Merge **recounts** the
output segment from the merged streams. Addition is forbidden only as that
merge substitute: field sets overlap, and a sum of input `df_agg` would keep
dead ordinals the merge dropped. Across segments, ordinals are disjoint.
Query time sums, the same way `build_index_scorer` does: `total_df` is the
sum of per-segment `df_agg`, and `N` is the sum of per-segment
`document_count`, dead included, until that segment is rewritten. A dead doc
stays in both until then. Summing field-local dfs is still wrong, in one
segment or across them. §5.2 case 4 catches the field sum. §7 catches a
second segment and a dead doc.

This reverses lsg4-rfc §5.5's dictionary decision, not its scoring rule.
§5.5 stores one `TermEntry` per term text, puts the aggregate df on that
entry, and forbids per-field df to avoid dictionary bloat. Fielded terms
accept that bloat: each `(field, text)` is its own entry, so a field-local
df exists and is not the idf input. §5.10's formula is unchanged. idf is
still aggregate. The aggregate simply no longer lives on the entry, because
the entry's df is no longer the aggregate.

**WAND bound is the derivative of this saturation, not a scale factor.**
Each fielded stream is indexed with **that field's raw `u32` length** as
STN3's document length for the stream. Pinned `d57ef58` does not expose a
raw count in the bound. `segment::bound::BlockBound::max_tf_bucket` is the
highest bucket in the run; `buckets` yields `(bucket, min_len)`; `shortest`
is the run's minimum length. `segment::ordinals::ChunkBound` stores the same
per-bucket `min_len` and `subs` (one past the max bucket in each sub-block).
Upstream's `REPRESENTATIVE_COUNTS` matches 0.4.0, so the max representative
is `TfBucket::new(max_tf_bucket()).representative_count()`. `from_count` is
the index-time quantizer of a raw position count, not a bound-time
translation. Because the stream's document length is the field's raw length,
`shortest()` is `min_doc_length(f)`. A stock score bound is not an input.

```
max_tf*  = Σ_{f ∈ mask ∩ present} w_f · representative_count(max_tf_bucket_f)
min_len* = min over mask ∩ present of w_f · shortest_f
bound    = saturate(max_tf*, min_len*)
```

The minimum, not the sum, is the 2026-09-25 erratum. Candidates are the **union** of the field streams in the scope mask. A
title-only document is a candidate. The bound at document-table ordinal `o`
uses only the member blocks that actually cover `o`. A field with no posting
at `o` does not gate candidacy and does not contribute `tf*`. An empty
intersection of some members is fine. The ordinal is the document-table
index, not a posting index and not a TID. The fused interval truncates at the earlier of the end of the currently
covering blocks and the start of the next block, chunk, or sub-block of any
mask-internal stream, including a stream that does not yet cover the pivot. `max_tf*` and
`min_len*` over that interval account for every block that intersects it,
not only the fields posting at the pivot. A body block that starts at 500
inside a title block covering 0..1000 raises `tf*` at 500. Holding the
title-only bound across that start breaks `exact_score ≤ fused_bound`, and
`search()` would accept the wrong top-k. A sub-block bound
(`ChunkBound.subs`, one past the max bucket) feeds the same formula. WS3's
property test includes that 0..1000 / 500 case, one intra-block sub-block
step, and a conjunction. Identical
top-k rows and ordering, not the same prune count, is the gate. Same completion, or the same prune
count, is not the gate. Scaling a stock per-field bound by `w_f · max_idf`
is not this expression.

**Norms and `df_agg` are a trailer, not a length-class byte.** This is a
reader delta in `segment/src/segment.rs`, not an additive blob a stock
reader ignores. Stock `Reader::new` requires `pages_at + pages_len == total`.
The fork sets `pages_end = pages_at + pages_len` and runs every stock
end-of-blob check against `pages_end`. Only the trailer API reads
`pages_end..total`. A short trailer, a long trailer, or a bad `STNF` magic
is corruption. No trailer on a multi-column index is corruption. A
single-column segment has `total == pages_end` and no trailer. Trailer bytes
are charged to the reader-cache budget, the same way dictionary bytes are.
The mutable index exposes the same tables in memory, before any immutable
section exists. The immutable reader exposes them only through the trailer
API. A lossy byte cannot reconstruct `field_total` or R-BIT, and it is not
accepted.

```
trailer  := magic "STNF", version u8 = 1, norms_len u32le, df_len u32le, norms, df_agg
norms    := field_count u8 (2..=16), field_total u64le × field_count,
            rows u32le × field_count × document_count,
            crc32 u32le
df_agg   := count u32le, entry*
entry    := token_len u32le, token utf-8, df u64le
```

`STNF` is not `STN3` and not `LDP2`. The version byte sits inside the
magic-prefixed payload. Doc-major, as lsg4-rfc §5.2: ordinal `o`, field `f`
is at `(o * field_count + f) * 4` from the start of the rows, not from the
start of `norms`. The checksum is CRC-32/ISO-HDLC, the zlib `crc32`: polynomial `0x04C11DB7`
reflected as `0xEDB88320`, reflected input and output, init `0xFFFFFFFF`,
final XOR `0xFFFFFFFF`. It is `u32le` over the row bytes in file order, and
it sits after the rows, inside `norms_len`. Rows include dead ordinals. Entries are in decoded-token byte order, unique, nonempty;
`df = 0` is omitted, not stored. `count` equals the entry count. Lengths
must sum to the remainder; a short or long trailer is corruption, not a
migration error. The mutable index holds the same tables before any
immutable section exists, and it checks the same CRC-32 before flush. Flush
writes the trailer. Merge rewrites both
sections; `df_agg` is recounted, not added. Those checks run at the one sidecar-open step, before planning or scoring,
not only in `verify_index`. Upstream posting streams do not store an exact
length per posting. Recomputed bounds cannot see a corrupted length that is not an extremum.
Three checks, and they prove different things. The CRC-32/ISO-HDLC proves
the row bytes were not torn. It does not prove the rows match the indexed
positions, and it does not cover `field_total`. The once-per-reader-cache-entry
pass proves the positions. Each cell is the token count: the sum, across
every term, of that field's position-list length at that ordinal, which is
what `verify.rs` compares to the length row. It is not `max(position)+1`.
Those differ when `position_gaps` is `preserve`, and that reading would
reject a legal index. The same pass recomputes each decoded token's union
df from the field postings (`0 < df ≤ document_count`; a missing, extra, or
mismatched entry fails). With checked `u64` arithmetic,
`field_total[f] == Σ_o rows[o][f]`. A mismatch on a row or a total is
corruption. `STNF.field_total` is what `avgdl*` uses. Upstream's header
stores one `total_length` (`segment.rs`), the sum of the stock per-document
lengths, not a per-field total. Single-column scoring uses that header
value. There is no `STNF` to disagree with. Multi-column `avgdl*` does not
read it. The bound recompute still runs too: block, chunk,
and sub-block `min_len` and max-bucket against the encoded bounds. Once per
cache entry, not on every lookup. The cache charges the trailer. A mismatch
rejects the open: a CRC-32/ISO-HDLC failure, a `field_total` that is not the
checked sum of its column, `STNF` present when the envelope
`field_count` is 1, `STNF.field_count` different from the envelope,
malformed norms dimensions or field totals, a length the positions do not
produce, recomputed bounds that disagree, an invalid `df_agg` entry,
`STNF` version other than 1, or any unpaired `~` in a stored fielded
payload. `shortest()`
is a sound floor only if that recomputation passed. A valid envelope beside an LSG1–LSG4 segment is
mixed-format corruption, not a migration error. `STNF.field_count` must
equal the envelope's `field_count` (§8). The scorer reads each segment's section and
sums across segments (§5.1). Names, weights, and the analysis stamp are not
here. They live in the meta envelope.

Writers: index build, the insert buffer (lengths exist before any immutable
section; flush has nothing to write otherwise), and merge, including
meta-lock-free merges. Owned segment files: `segment/src/segment.rs`
(`Reader::new`), merge, verify, and the mutable index.

Single-column indexes omit both sections. Stock STN3 document length is
`len*`, and stock term df is `df_agg` — they coincide. Field 0 is not
duplicated beside the stock norm.

**Phrases, operators, highlights.** An unscoped phrase expands to the OR of
per-field phrase bindings: the phrase must occur inside one field, then the
fields are disjoined. That is not cross-field adjacency. One token from
`title` and the next from `body` does not match. `==>` does not use the
any-field expansion; `search()` does, for retrieval only. Scoring of that
union is still one fused key. The rest of the field contract is Appendix A,
and it is in the oracle.

### 5.2 POC gate

Semantic smoke first, on the machine that will run the latency numbers,
PostgreSQL 17. Fail closed. A pass is the same order as the 0.4.0 recording,
and `f32::to_bits` equality of each score against that recording. Rounded
decimals are not the comparator. Latency is not consulted if this fails.

These five are arithmetic. They do not prove WAND soundness. That lands in
WS3 and the contract suite.

1. Unweighted `(title, body)`: `(needle, pad)`, `(pad, needle)`,
   `(needle needle, pad)`. This is
   `search_scores_every_field_of_a_multi_column_index`. Aggregate df of
   `needle` is 3, so ids 1 and 2 tie and id 3 ranks first. A field-local-idf
   implementation breaks this tie — that break **fails** the smoke.
   Aggregate idf preserves the tie. Tie identity is `f32::to_bits` equality,
   as in that test.
2. `field_weights = 'title:3,body:1'`: one short title hit against one
   high-tf body hit. Order must match the 0.4.0 recording. Weight sits
   inside `tf*` and `len*` before one saturation, so this is not a rescaling
   of per-field scores.
3. A single-column index, weight 1.0, no fielded key: order matches the
   0.4.0 recording. Bit-equality with `TermScorer` is a codec/scorer property
   test, not this SQL smoke. The product never writes a one-field fielded
   segment, so that LSG4 fixture is not a contract row.
4. A document that contains the term in **both** fields, beside field-only
   hits. This is the end-to-end fused-key case: one scoring key, both
   streams contribute `tf*`, and `df_agg` equals the cardinality of the
   union of the field postings. `max(df_f)` is also less than the sum and
   is not a pass. Case 1's field sets are disjoint, so it does not catch
   this.
5. Scoped `title:(needle)` on two docs with equal title tf and unequal body
   lengths. Order must change. Mask-only `len*` ties them. This is the
   `weighted_length` versus `weighted_tf` split.

If the smoke fails because the implementation is wrong, fix the
implementation. The formula does not move. If it fails because the
representation cannot supply `df_agg`, exact lengths, or a sound fused
bound, stop. The fallback is STN4 with the **same** formula (per-field
payload inside one term entry, so aggregate df can live on the entry again
and §5.5's layout returns), or, if STN4 cannot either, a
`contract/divergences` entry, a `contract_version` bump, and a plain
statement that pg-agent rank order changes. Do not proceed to latency on a
red smoke.

Latency, only after a green smoke. Paired ratios, one machine, PostgreSQL
17, concurrency 1, default GUCs. Corpus: a fixed Stack Exchange `title+body`
extract (row count and checksum in the results) and, once WS2 is green, a
fixed Chinese corpus under the pinned jieba snapshot. Query list is
`docs/benchmarks/stn3-fielded-poc-queries.txt`, committed with the results.
Baselines: single-field STN3 on the concatenated text, and 0.4.0 on the same
rows. One warmup pass discarded, then 20 timed runs. p50 and p99 are
nearest-rank on those 20. The mandatory dictionary gate is two builds; a
ratio over 1.8× fails, and there is no variance waiver on it. Cold is a
separate labeled run, not mixed in. Write `docs/benchmarks/stn3-fielded-poc.json`.

- **Mandatory.** Dictionary bytes and build time ≤ 1.8× the single-field
  STN3 index. Dictionary bytes include the aggregate-df section, not only
  postings. A miss stops the fielded-terms path.
- **Advisory, waiver in the results file.** Ranked p50 ≤ 1.3× that baseline.
  The waiver names the workload, the ratio, and why staying is cheaper than
  STN4. No waiver, no pass.
- **Labeled, not a gate.** p99 of 20 runs is an extreme sample. Report it.
  Do not fail the POC on it. "No worse than 0.4.0" is not a gate; noise has
  no tolerance written here, so it is not one.

A mandatory miss with a green smoke is a representation choice — stay is not
available; STN4 under the same formula, or a recorded divergence. An advisory
miss with a waiver may stay. Neither is a formula change.

### 5.3 What we inherit for free

Single-column indexes, the majority of pg-agent's, stay stock STN3 for
postings and statistics: no fielded terms, no sidecars. They inherit
chunk-fold counts, dead-doc bitmaps, meta-lock-free merges, hot standby,
the #88 engine checks that ship in `d57ef58` (present, not optional; SQL
error text that differs from 0.4.0 is a divergence, §4.2), tokenizer
heap-string elimination on the non-jieba path, and the TIN conformance
suite. The saturation is still `TermScorer`. §5.2 case 3 is the
single-column order check. "Inherit the score" is not the claim.

Multi-field is the critical path. It does not inherit the score, the bound,
or the norms. §10 weights it that way.

## 6. jieba port

The tokenizer crate is upstream-shared. Upstream #92 reworked the
tokenization loop. Port jieba onto that API as `tokenizer = 'jieba'`,
preserving the snapshot contract. "One parse per backend" is not that
contract.

- `CompiledPipelineKind::Jieba` captures `Arc<Jieba>`, generation, and
  fingerprint. `compile_with_snapshot` is how the cache compiles, so the
  cache key and the pipeline cannot observe different snapshots.
  Generation is **not** the cache identity (`compiled.rs`: "not a cache
  identity"). The fingerprint is. `compiled_pipeline_keeps_its_captured_dictionary_generation`
  locks the pipeline. `holder_swap_preserves_an_in_flight_iteration` locks
  the iterator, not the pipeline.
- Both constructors take that snapshot. `JiebaIter::new` (owned cuts,
  `compiled.rs`) and `JiebaIter::new_borrowed` (`source_spans.rs`). Highlight
  uses `pipeline.source_spans()`. A port that updates only the owning
  iterator, or that reads the global holder from the span path, breaks
  highlight/index symmetry after `jieba_add_word`. `source_spans` is a port
  site.
- Both paths materialize `cut()` into a `Vec` up front. That buffer is an
  accepted exception to #92's heap-string claim. It is also an exception to
  #92's cancellation: `cut()` is not polled, so cancellation is observed
  between documents, not inside one cut. Accepting the allocation does not
  make the cut interruptible, and the port must not claim it is.
- Governance is the state machine in `docs/compatibility.md` (Dictionary
  governance), which is normative for WS2, not a stamp stuffed into
  `SPEC_BYTES` (already 8). The stamp is an `AnalysisStamp` in the §8
  envelope: `jieba_rs_version`, `dict_fingerprint`. Fingerprint is
  SipHash-1-3 v1 (`stannum.jieba.dict.v1`); `0` is the embedded sentinel and
  is never a table identity; the empty table is `6855a0736155f3dd`.
  `strict_analysis` errors only on stamped drift; a missing stamp warns.
  The compatibility rules that WS2 must keep, by that document: fingerprint-
  keyed cache reuse and eviction of stale identities; `jieba_reload_dict`
  reinstalls even when the fingerprint is unchanged; abort and savepoint
  discard uncommitted dictionary changes on next use; a failed or cancelled
  reload keeps the previous dictionary and leaves refresh pending; committed
  changes invalidate other backends; cached plans depend on `jieba_words` so
  mutations re-evaluate worker eligibility. Standbys load WAL-visible rows.
  Parallel workers skip SPI. A nonempty custom dictionary makes
  `parallel_safe` false, and the leader declines the parallel path. The
  query does not error. A worker that segments with the empty-table
  dictionary while the leader used custom words is a bug.
- `tokenize` and the `jieba_words` API in §4.1 stay byte-for-byte.

WS2 acceptance is that list, with fixtures. The Chinese half of the §5.2
smoke waits on it.

## 7. Contract suite

Adapt upstream's `conformance/` runner — engine-agnostic, imports nothing
from the repo — into `contract/`.

Producer, pinned: commit `24f5c02`, PostgreSQL 17 (the floor of
`.github/workflows/ci.yml`; 18 replays and must not rewrite the JSON),
extension built from that commit. Empty cluster, `CREATE EXTENSION stannum`.
"Embedded dictionary" in a case means the empty-table fingerprint
`6855a0736155f3dd` after that create, not sentinel `0`. A case that loads
`jieba_words` says so.

Replay is not one comparator. Five gates. Producer database is `UTF8`,
`--no-locale`, the same as `extension_upgrade.py`.
PostgreSQL minor is the 17.x the recording job prints into the JSON; a later
17.x replays and must not rewrite. 18 replays and must not rewrite either.
Score identity is `f32::to_bits`. Tie order in a recording is ascending
`ctid`. A case that stores a decimal score rounds half away from zero to 6
decimal places, and that is secondary to bit-equality of ties. The float-tail
caveat is not permission for a different model.

1. **Recordings.** `contract/expected/stannum-0.4.0/*.json`, committed. CI
   replays and refuses rewrite. Covers pg-agent
   v13 call patterns, the §5.2 arithmetic rows, unscoped and field-scoped
   phrases, one unscoped `NEAR` or `THEN` that must not match across fields
   (lsg4-rfc §5.11: intervals are never compared across fields), highlight
   spans and snippet selection, fielded wildcard / regex / fuzzy / range
   (decoded token, per field; field copies do not consume `max_expansion`),
   tokenizer symmetry.
2. **Normalized diagnostics.** `version()` must change; it is not a 0.4.0
   recording. Page counts, segment counts, and root blocks normalize or are
   excluded, with the rule in the case. A raw page-count diff is not a
   failure.
3. **0.5.0-only.** `capabilities()` has no 0.4.0 recording. Its gate is the
   §4.2 shape, not a replay.
4. **Internal tests.** HOT underfill is not plain SQL. A normal HOT update
   leaves only the root indexed. Enforcement is the existing pg_test
   `search_acceptance_rejects_hot_root_and_member_underfill`, which injects
   `PrunedCandidates` holding both root and member.
   `accepted_pruned_rows` rejects when `visible.len() != pruned.rows.len()`.
   The contract cites that test; it does not pretend a SQL fixture builds
   the pair.
5. **Parallel jieba.** A nonempty `jieba_words` query returns the serial
   result and does not error. `parallel_safe = false` on the SQL functions
   does not stop a parallel bitmap or heap path. The mechanism is the
   planner hook already installed from `customscan.rs`: when a relevant
   stannum index uses a nonempty custom dictionary, it clears that
   relation's parallel eligibility. The hook depends on `jieba_words`, so a
   dictionary change invalidates the plan. An empty custom dictionary keeps
   parallelism. The two `EXPLAIN`s test that hook, under gather-capable
   settings: the chosen plan has no parallel worker, and a second plan with
   `enable_custom_scan` off is also serial, including bitmap and heap paths.
   Declining only the custom-scan node is not a pass.

Also a recording: the same token posted in two segments, plus a dead doc
that remains in both that segment's `df_agg` and in `N` until rewrite.
Query-time idf uses the sum of the two `df_agg` values. A scorer that reads
one segment, or that drops the dead doc early, fails.

Coverage is not functions only. Every function in `stannum--0.4.0.sql`,
every reloption, every 0.4.0 GUC, both `==>` operators, the opclass, every
`SUPPORT` attachment, the `jieba_words` ACL, and `index_health`'s
`security_invoker` has a case or a line in `contract/exclusions.yaml`. A
miss fails CI. `contract/divergences/stannum.yaml` holds intentional
differences only, including #88 error texts and limits that differ from
0.4.0's `max_expansion` of 1024, with expected-error fixtures. The oracle
for an entry there is not `stannum-0.4.0`. Rank order is not an entry unless
`contract_version` bumped (§4.3).

Round-trip fixtures for the key: leading, trailing, repeated, and adjacent
tildes, every ordinal 0..15. Field 0's `~0~foo` must encode `~0~~~0~~foo`.

WS3, not this arithmetic smoke, owns the soundness cases: the min-floor
witness, a bucket boundary, misaligned blocks, empty interval intersections,
and pruned-versus-exhaustive top-k rows and ordering across more than one
block and one chunk. The invariant is §5.1:
`exact_score(candidate) ≤ fused_bound(interval)`.

CI: build, replay the suite, green required for any merge to `main` / `stn3`,
including an `upstream/main` dry-run.

## 8. Versioning and migration

0.5.0 is a new lineage. `stannum--0.4.0--0.5.0.sql` **will** be written. It
is not in the tree; present scripts stop at `stannum--0.4.0.sql` and
`stannum--0.3.0--0.4.0.sql`. The script carries catalog objects only. It
does not convert segments. Definitions, volatility, parallel safety, and
ACLs match `stannum--0.5.0.sql`, including `capabilities()` and every new
`#[pg_extern]`. `SECURITY DEFINER` is forbidden. Released scripts are not
edited in place.

One cluster has one `stannum.so`, one access method, and one extension name.
Installing 0.5.0 "alongside" in that cluster is not implementable, and it is
not the plan.

**Supported cutover: downtime, then a process boundary.** Replacing
`stannum.so` on a running postmaster does not switch loaded backends. The
migration error does not apply "from the moment" of the file replace.
Running backends keep the code they loaded. The procedure is per database,
and it applies to replicas: stop them before the primary moves, and rebuild
them from the upgraded primary. Do not let a replica replay across the
library swap.

The guarantee that old WAL is not replayed is a shutdown checkpoint, not
rmgr compatibility. 0.4.0's custom rmgr (`stannum.wal_rmgr_id`, RECLAIM
records) is not promised to redo under 0.5.0. A clean shutdown checkpoint
leaves nothing to replay. Skipping it can refuse recovery.

1. Backup. Rollback is restore, or fail back to a cluster still on 0.4.0.
   There is no downgrade. A 0.4.0 binary cannot read STN3, and 0.5.0 cannot
   read LSG1–LSG4. Once non-concurrent REINDEX has swapped in an STN3 index,
   reinstalling 0.4.0 does not bring the old segments back.
2. Quiesce writers. Drain sessions and background workers that have loaded
   `stannum.so`.
3. `CHECKPOINT`, then a clean `pg_ctl stop`. This is the replay boundary.
4. Stop PostgreSQL if step 3 did not. Install the 0.5.0 package only while
   the server is stopped. Restart with the same `shared_preload_libraries`
   the cluster requires. The 0.5.0 binary keeps exporting the 0.4.0 SQL
   entry points, so the pre-ALTER catalog resolves symbols. `capabilities()`
   does not exist until step 5.
5. `ALTER EXTENSION stannum UPDATE TO '0.5.0'`, in each database. Then
   non-concurrent `REINDEX` of each stannum index. Each rebuild takes
   `AccessExclusiveLock`; that index's relfilenode swap is visible at its
   own commit. This is transactional replacement of one index, not an atomic
   swap of every index on a table or in the cluster. `REINDEX CONCURRENTLY`
   is not this procedure. REINDEX is rebuild-from-heap. `ambuild` /
   `build_empty` replace meta and read the heap; they do not run the
   migration check, or step 5 cannot clear the error. Traffic stays off until every index has been rebuilt and verified on its
   own. The fence is the guarded-callback table, not a blanket over every AM
   entry point and not a role check. Every relation-specific callback listed
   as guarded returns the migration error for `PreStn3` before any
   index-page allocation, WAL emission, or metadata mutation.
   `ambuild` and `ambuildempty` are exempt. `amvalidate`, `amoptions`, and
   `ambuildphasename` are outside the fence: they have no index relation.
   Call order: `aminsert` guards before the buffer write; vacuum callbacks
   guard before `ambulkdelete` or `amvacuumcleanup` dirties a page; merge
   guards before it reads a run; scan callbacks guard in `ambeginscan`
   before the first pin. From the moment of restart,
   `storage::open_index` classifies the relation `Current | PreStn3 | Corrupt`
   before any page is dirtied. Every path that can resolve an index relation
   calls it: the guarded callbacks below, and `storage::view` (custom scan,
   `verify_index`, `search`, `search_count`, `score_bound_indexed`).
   `PreStn3` is the migration error and performs no write. Autovacuum is
   covered because it calls `ambulkdelete` / `amvacuumcleanup`, which call
   the opener. Merge workers call it too.

   | Callback | Class |
   |---|---|
   | `aminsert`, `ambulkdelete`, `amvacuumcleanup`, `amgetbitmap`, `ambeginscan`, `amrescan`, `amendscan`, `amcostestimate` | guarded |
   | `ambuild`, `ambuildempty` | exempt, so `REINDEX` can build |
   | `amvalidate` | not relation-specific: it receives only an opclass OID (`am.rs`) and cannot see a predecessor |
   | `amoptions`, `ambuildphasename` | not relation-specific |

   WS6 includes an insert after restart and before `REINDEX`: it must error
   and must not write. `amgetbitmap` applies the same implicit field scope
   as `scope_scan_query`, and it sets `tbm_add_tuples`'s recheck bit to
   `!planned.exact`. The custom-scan `exec.recheck` sites do not run on
   this path. With `enable_custom_scan` off, a body hit must still not
   satisfy `title ==> …`, and an inexact plan must still be rechecked.
6. `stannum.verify_index` on each new index, resume traffic, re-run the
   pg-agent suite. pg-agent's SQL is unchanged. Replicas are rebuilt after
   this, not upgraded in place by replay.

**Migration predicate.** Page images do not have a relation.
`classify_meta_page(bytes) -> Current(meta) | PreStn3 | Corrupt(error)` is
the pure parser. `storage::open_index` is the live-relation wrapper and
calls it. WAL redo, page verification, and recovery call the parser, not
the wrapper. After the clean-checkpoint cutover, `PreStn3` during WAL redo
cannot occur: the checkpoint precedes the swap, so there is nothing to
replay. If it occurs anyway, recovery aborts. It does not skip the record
as corruption. The wrapper is not called by `amvalidate`. `ambuild` /
`ambuildempty` do not call it.

0.4.0 meta starts with an `identity` `u64`. A body prefix cannot separate
that from a magic, so the page-special kind is the first cut. Page-special
`version` stays 2. Do not bump it. Kind 5 is free in `d57ef58` (kinds 1–4
only, `postgres/src/storage/layout.rs`). Every 0.5.0 index writes
`KIND_ENVELOPE = 5`, including single-column, so "not an envelope" is
legacy or corruption. A single-column index has `field_count` 1 and no
`STNF` trailer.

Kind 5 is the upstream meta image — `identity`, `spec` (8 bytes), buffer and
generation state, and the segment directory — plus one framed `STNM` record
appended after those fields. It is not a closed page whose body is only
field names. Dropping `spec` or the directory writes an index `open_index`
cannot read, and it breaks `ALTER INDEX … SET (tokenizer=…)`, which 0.4.0
keeps stable by storing `spec` on the meta page.

```
record   := magic "STNM", version u8 = 1, body_len u32le, body
body     := field_count u8,
            repeat field_count {
              name_len u16le, name bytes, weight f32le
            },
            analysis_flag u8,
            [stamp if flag = 1]
stamp    := jieba_rs_version u32le, dict_fingerprint u64le
body_len := length of body only, stamp included when flag = 1
name_len := UTF-8 byte length, 1..=63
weight   := finite and > 0
flag     := 0 or 1; 1 is followed by jieba_rs_version u32le,
            dict_fingerprint u64le
```

`name[i]` and `weight[i]` are key-column ordinal `i`, the same `i` as the
`~{:x}~` header. A different order would resolve `title:(…)` onto another
column's stream. Exact consumption constrains the record, not the page. The 12-byte stamp
(`jieba_rs_version` `u32le`, `dict_fingerprint` `u64le`) sits inside `body`
when `flag = 1`. Any byte after `flag = 0` is corruption. The upstream
image ends at its own length prefix. Exactly one `STNM` record follows.
Any byte after that record is corruption. The upstream "page must end at
the image" check applies only to the image prefix.
`field_count` is `1..=16`. The all-fields mask at 16 is `u16::MAX`. A
single-column expression key has no `attname`; the slot stores the fixed
name `expr` and weight `1.0`, and scoring still uses `TermScorer` and never
reads that weight. Duplicate key attributes (`(title, title)`) are rejected
at build, before the record is written. A non-jieba index has `flag = 0`;
a stamp on it is corruption. A jieba index with `flag = 0` is the 0.4.0
missing-stamp warning, not a migration error. A stamp whose
`dict_fingerprint` is 0 is corruption. `classify_meta_page` checks only
this grammar. `storage::open_index`, after a valid kind-5 image, checks
the live relation: `field_count == indnkeyatts`; `field_count == 1` means
one key, no `STNF`, and weight exactly `1.0`; `field_count >= 2` means a
multi-key index and an `STNF` trailer; the recorded-name comparison runs only when `field_count >= 2`. DDL
rejects multi-column expression keys and any `INCLUDE` list. A
single-column expression index stays legal: `field_count` 1, name `expr`,
weight `1.0`, no `STNF`, `TermScorer`, snippet `'none'`. Open does not
compare `["expr"]` with an empty `attname` list. Segment-magic rules 1 and 3 run here, not inside the page parser. `field_count = 1` means no `STNF` trailer, and that one weight is `1.0`.
The stored name is inert. `fields_meta` returns no queryable field plan for
any single-column index, including one whose real column is named `expr`.
Scoring stays `TermScorer`. The expansion adapter is not called.
`scope_scan_query` is not called. Field syntax still raises
`field syntax requires a multi-column index`. The rename check runs only
when `field_count >= 2`. Snippet and highlight inference stay on `attnum`.
An expression key (`attnum <= 0`) forces snippet `'none'`, and
`column_field_name` returns nothing. Column drift tells an ordinary key
from an expression key by `indkey`, so `[]` is never compared with
`["expr"]`.

Error matrix, applied at `open_index` and at sidecar open. Kind `KIND_META`
with a parseable 0.4.0 body, including empty and buffer-only indexes, is
the migration error. Kind 5 whose `STNM` record has a bad magic, a version
other than 1, a `body_len` that does not match `body`, a name length outside
`1..=63` or an interior NUL, bad UTF-8, a duplicate name, a weight that is
not finite and positive, `field_count` outside `1..=16`, or an analysis flag
outside `{0, 1}` is corruption. A stamp on a non-jieba index is corruption.
A stamped `dict_fingerprint` of 0 is corruption. So is `STNF` version other than 1, `STNF`
present when `field_count` is 1, or `STNF.field_count` different from the
envelope. A valid kind-5 envelope beside an LSG1–LSG4 segment is
mixed-format corruption, not migration. An immutable segment magic in
LSG1–LSG4, with a kind-1 meta, is the migration error. TAG `0x02`
corroborates. It is not the trigger. Any other segment magic, or a meta
page whose kind is neither 1 nor 5, is corruption.

Precedence:

1. Kind 5, upstream meta image intact, a valid `STNM` record, and every
   existing immutable segment magic `STN3` → current. Never legacy.
2. Kind 1, the recognized 0.4.0 layout, including a kind-1 meta beside an
   LSG segment → migration error. The meta page classifies empty and
   buffer-only indexes. Segment magic alone does not.
3. Kind 5 beside an LSG1–LSG4 segment → mixed-format corruption, not
   migration.
4. Anything else → corruption, not the migration message.

The message is `stannum: index requires REINDEX to 0.5.0 (pre-STN3 segment)`.
Tests: a valid kind-5 image is never classified legacy; every recognized
0.4.0 shape yields that message, never generic corruption; empty,
buffer-only, immutable, recovery, and WAL-redo of a kind-5 page round-trip
`spec` and the directory. A column rename still errors until REINDEX when `field_count >= 2`, the
0.4.0 rule: `stannum index {name}: the indexed columns changed since the index was built; REINDEX required`.

One loader, `storage::open_index`. Callers do not classify on their own.

| Caller | Kind 1 | Kind 5, valid record | Kind 5, bad record | Other kind |
|---|---|---|---|---|
| `present`, `read_meta`, `analysis_meta`, `index_spec`, `index_tokenizer`, `tokenizer_by_oid`, `spec_by_oid`, `fields_meta` | `PreStn3` | `Current` only if no immutable segment exists or every one is `STN3`; an LSG segment is `Corrupt` and no write. `fields_meta` returns no queryable plan when `field_count` is 1; the stored name stays in the record | `Corrupt` | `Corrupt` |
| `view` construction, custom scan, `verify_index`, `search`, `search_count`, `score_bound_indexed` | same, before any page is dirtied | same | same | same |
| recovery, WAL redo of the meta page | `PreStn3`; abort recovery, do not skip | page parser only: valid image is not corruption; the segment-magic check runs later in `open_index` | `Corrupt` | `Corrupt` |
| page verify, `kind_name` | name `meta`, then the migration finding | name `envelope` | corruption finding | kinds 2/3/4 keep buffer/run/free; `unknown kind` is only a kind outside 1–5 |
| builder init, insert init, merge | not called; build writes kind 5 | read `Current` | `Corrupt` | `Corrupt` |

**Fingerprint.** `extension_upgrade.py`'s `CASE` covers functions
(`pg_get_functiondef` plus `proacl`), types, operators, access methods, and
opclasses. Relation rows fall through to `ELSE ''`. It does **not** cover
`jieba_words` `REVOKE ALL`, and it does **not** cover the `index_health`
view definition or `security_invoker`. Function `REVOKE` on `score_bound`
and `score_bound_indexed` is in the CASE. WS6 extends the fingerprint with
relation ACLs, the view definition, and `pg_class.reloptions` for
`index_health` (`pg_get_viewdef` omits `security_invoker`), and keeps the
behavioral tests:
`require_index_select` and `postgres/tests/search_srf.py` (SELECT and RLS).
Catalog equality alone does not establish those. `SECURITY DEFINER` stays
forbidden.

**Zero downtime, if required, is a second cluster** — dump/restore or
logical replication onto 0.5.0, build indexes there, verify, cut over. Two
clusters, not two extensions in one database. No second access-method name:
that would change the SQL pg-agent issues.

`extension_upgrade.py`'s single-binary loop cannot produce a genuine old
segment: it builds the "old" index with whatever `.so` is installed. The
LSG-still-answers assertion is retired for every path whose target is 0.5.0.
Replacement is a two-artifact CI job, not a changed assertion in that loop:

1. Isolated install of the retained 0.4.0 binary and its SQL (`24f5c02`).
2. Create old indexes: single-column, multi-column, jieba, empty, and
   buffer-only (no immutable segment, so no magic).
3. Record their segment-magic and meta-page bytes. The job fails if those
   bytes are already STN3 or `KIND_ENVELOPE`.
4. Clean shutdown of that server.
5. Install 0.5.0 into a separate prefix. Restart the same data directory
   on 0.5.0.
6. `ALTER EXTENSION`. Each old index, on scan or `verify_index`, returns
   the migration message, not `segment magic`.
7. Non-concurrent `REINDEX` of each index. Then `==>` and `search_count`
   answer. A failure on one index does not roll back the others; the job
   stays red until every index has been rebuilt and verified.

The catalog fingerprint job stays, and it is not historical-format evidence.
The 0.3.0 → 0.4.0 "still answers" check is a 0.4.0-line fact and is not
inherited. WS6's three suites are `contract/`, `extension_upgrade.py`, and
`postgres/tests/search_srf.py`.

`main` stays the production branch for pg-agent until the contract suite is
green on 0.5.0. Afterwards it is maintenance-only.

## 9. Repository strategy

- Long-lived branch `stn3`, forked from `upstream/main`. Our work lives in
  `postgres/src/tool/`, `postgres/src/fields/`, the shims in §3 (including
  `tinql/src`, `scope_scan_query`, and `storage/layout.rs`), `tokenizer/`
  (jieba only, §6), `contract/`, and the segment reader. The reader edit is
  `Reader::new` in `segment/src/segment.rs`: stock end-of-blob tests use
  `pages_end`; only the trailer API reads past it. Writers: build, insert
  buffer, merge, verify. Upstream merges land on our cadence, gated by the
  contract suite. Conflicts in `operator.rs`, `score.rs`,
  `highlight_udfs.rs`, `customscan.rs`, `am.rs`, `options.rs`,
  `storage/layout.rs`, `segment/src/segment.rs`, and `tinql/src` are
  expected. A later upstream snapshot that assigns page kind 5 is one of
  those conflicts. §1's dry-run already hit `tinql/src`.
- `origin/main` keeps 0.4.0 until parity. Both branches push to origin.
- Upstream is pre-1.0. We merge tagged snapshots, never chase
  `upstream/main` heads, and record the pinned SHA.

## 10. Workstreams and gates

| WS | Scope | Gate |
|----|-------|------|
| WS0 | Snapshot plus generated manifest; `capabilities()`; runner; 0.4.0 answers | producer in §7; five assertion kinds; catalog coverage beyond functions; CI refuses rewrite |
| WS1 | `stn3` branch, module layout, CI with upstream-merge dry-run | CI green on untouched upstream |
| WS2 | jieba onto #92; compatibility.md lifecycle is normative | both iterators; pipeline lock vs iterator lock; fingerprint cache key; abort/reload/eviction; cut() uninterruptible |
| WS3 | Fielded streams, expansion adapter, `Reader::new` trailer, df_agg reader/merge, envelope grammar, kind-5 matrix, WAND bound | §5.2 arithmetic smoke first; then the document-ordinal bound invariant, including a conjunction; identical top-k rows and order |
| WS4 | Phrases, spans, highlights, and the widened `==>` matcher. Starts after WS3 | Appendix A fixtures; `title ==>` and `body ==>` under a custom scan and with `enable_custom_scan` off |
| WS5 | Wheel on PostgreSQL 18 (`BUILT_FOR_POSTGRES_MAJOR = 18`) | §4.1 call sites, including `check_health` in `_index.py`, green on 18 |
| WS6 | Two-artifact harness, upgrade script, fingerprint extension, insert-before-REINDEX test. Starts after WS4 | relation ACLs and view options; migration error and no write; `contract/`, `extension_upgrade.py`, and `search_srf.py` green |

WS0 and WS1 are prerequisites. The English half of the §5.2 arithmetic smoke
starts as soon as the fused scorer exists; the Chinese half waits on WS2.
That smoke is not the WAND proof — WS3's bound cases are. WS4 is not a free
mapping on top of fielded terms — the shims, including tinql, are the work.
STN4, if the smoke forces it, is a separate branch (§11), not a row in this
table's remaining time.

## 11. Risks

- **Formula versus representation.** The formula is settled (§5.1). The
  residual risk is that fielded streams cannot supply `df_agg`, exact
  lengths, or a sound fused bound. That fails the §5.2 smoke and stops the
  fielded-terms schedule. It does not silently change rank order.
- **STN4 is a schedule break, not a mitigation inside this one.** The P0
  plan called LSG4-shaped format work the quarter-scale project
  (`docs/designs/p0-agent-features.md`). STN4 is that project on STN3
  machinery. Those ranges are engineer-weeks. The addends are WS0–WS1 2 + WS2 2 + WS3 5 + WS4 2 + WS6 3 = 14 on the
  critical path, plus WS5 1 = 15 if that wheel week is counted. Elapsed is 10–14: WS5 is off
  that path, and WS0/WS1 overlap WS2's prep. STN4 is not inside those weeks. If it is chosen, re-plan it; do not keep the old
  5–7 week figure. 0.4.0 keeps serving pg-agent either way, so the risk is
  slip, not a forced cutover date.
- **Dictionary growth and pruning.** Fielded terms are the bloat §5.5
  refused. The 1.8× dictionary gate is mandatory and counts the aggregate-df
  section. An advisory p50 miss needs the waiver in §5.2. p99 of 20 runs is
  labeled, not a gate.
- **Upstream format churn pre-1.0.** Pinned-snapshot merges. Our segment
  footprint is the STNF trailer, the length check that admits it, and the
  meta envelope, sized to be re-done. Merge, verify, and the mutable index are the files that absorb
  that redo. The key type stays `&str`.
- **Migration window.** After restart, every guarded callback returns the
  migration error for `PreStn3` and does not write. `amvalidate` is outside
  the fence. Autovacuum is covered through the vacuum callbacks. Replicas
  are rebuilt, not replayed across the swap.
  Rollback is a backup, not a downgrade script.
- **Oracle drift.** Recorded answers are the behavioral truth. HOT underfill,
  parallel jieba refusal, fingerprint equality, and the §5.1 tie are in the
  suite so they cannot drift quietly. A wanted change is a recorded
  divergence and, if it moves rank order, a contract bump.
- **Shared-file conflicts.** The §3 shims will conflict on upstream merges.
  That is priced in. Pretending `fields/` absorbs them is how the seam lie
  starts.

## Appendix A. Field semantics (normative)

These are in the 0.4.0 oracle. A literal reading of "fielded terms" that
drops one of them changes SQL behavior. Fixtures live in the contract suite.

- **`==>` implicit scope.** `title ==> 'foo'` restricts unscoped terms to
  `title`. `Private.field` carries the scan key's field; `scope_scan_query`
  applies it. A field group naming another column errors
  `stannum: this ==> clause answers '<column>'; use stannum.search() for '<name>'`
  (`check_clause_field_scope`). Unknown field:
  `stannum: unknown field '<name>'`. Field syntax on a single-column index:
  `stannum: field syntax requires a multi-column index`. The any-field union
  is the `search()` retrieval rule, not the `==>` rule. A body hit must not
  satisfy a title clause.
- **Scoring keys.** `(text, field_mask)`. The same text under two masks is
  two terms; boosts add only within a mask. An unscoped term is one key with
  the all-fields mask and one fused saturation (§5.1), not an OR of
  per-field saturations. `tf*` sums `mask ∩ present` only. `len*` and
  `avgdl*` sum every index field, mask or not. `term_add` and `term_replace`
  cannot both be non-NULL. They are analyzed by the index tokenizer,
  idempotent, unscoped, pin an existing query term without changing its
  weight, and enter at 1.0.
- **Phrases and spans.** An unscoped phrase matches when any one field
  contains it (the Lucene rule): OR of per-field bindings, each binding a
  phrase over that field's position stream. Never cross-field adjacency. The
  same rule covers unscoped `NEAR` and `THEN`: intervals are not compared
  across fields. A field-scoped phrase or span matches only in that field.
- **Highlights.** `project_to_field`: a wrapper for another field contributes
  no marks; a wrapper for this field contributes its inner marks; unscoped
  parts still mark; a NULL field is single-column behavior (the query is
  unchanged). `search()` returns one scalar snippet: the wrapper field, else
  the first field with a mark, else the first non-NULL column. NULL columns
  are skipped. All-NULL yields no snippet. The 5-arg `highlight` and the
  bound overloads are the SQL shape of this rule; unknown field on the bound
  form errors as in §4.1.
- **Stop words.** The filter is `compile_scoring_terms` only, on analyzed
  token text, before the `(text, mask)` key is formed. It is not an
  index-time omit. `==>` and `search_count` still match stop words.
  `full_score` ignores the list. Stripping `的` after it has been encoded
  as a fielded key changes which terms `score` drops, and it is wrong.
- **Encoding.** Internal keys are the `~{:x}~` quoting in §5.1, not
  `0xFF || field_ordinal || token`. Every payload tilde doubles. Field 0's
  `~0~foo` is `~0~~~0~~foo`. `field_ordinal` is the 0-based key-column
  index. Errors and `score_inspect` show the surface token.
