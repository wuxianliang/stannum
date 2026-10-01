# STN3 Tool Layer — Stepwise Execution Plan

Status: ready for loop-orchestrated execution · Normative design:
`docs/designs/stn3-tool-layer-2026-09-29.md` (the P0/P1-clean reviewed revision,
commit `f4bc1e3`) · Created 2026-09-30

This plan decomposes the design into **24 steps across 7 phases**. Each step is
sized for one orchestration loop (implement → review → gate) and ends in a
checkable state. The design document is the spec; where this plan and the
design disagree, **the design wins** and this plan gets a fix.

## 0. Protocol for the orchestrator running this plan

- **One step = one loop.** Dispatch a pair agent with the step brief + design
  section refs; verify; then oracle-review the result (design doc in the
  selection); fix until the step's Done-when holds. Mark the step complete
  here (`- [ ]` → `- [x]`) with the commit SHA.
- **Gates are tests, not opinions.** Every phase ends in a suite that runs in
  CI. A step whose gate is red after two fix rounds escalates to the human
  with the failing evidence — it does not get re-rolled until green by luck.
- **Never edit:** `contract/expected/**` (except by the pinned recording job),
  released `postgres/sql/stannum--*--*.sql` scripts, upstream files outside
  the owned-shim list (design §3).
- **pgembed checkpoints.** pgembed is a separate repo (loaded workspace root
  `pgembed/`). Its build pins `STANNUM_COMMIT` in `pgbuild/Makefile`. At every
  checkpoint: bump the pin to the step's commit, build the wheel
  (`tools/build_standalone_extension_wheel.py` / `pgbuild/Makefile`), run the
  named pgembed tests, record pass/fail here. **The 0.5.0 lineage must stay
  installable into pgembed from Phase 2 onward** — this is the product's
  delivery vehicle, not a final-phase afterthought.
- **Upstream pin.** Base is `upstream/main@d57ef58`. Record any re-pin here.
  Monthly upstream-merge dry-runs start after Phase 2 (design §9).
- **External dependency.** pg-agent v13 lives at `~/Projects/pg-agent`
  (outside this workspace). Steps that need it are marked **[pg-agent]** —
  they require the user to run or expose that repo; skip without blocking
  other steps.

---

## Phase 0 — Vehicle (WS1): the `stn3` branch

- [x] **0.1 Create the branch and prove CI on untouched upstream** (d57ef58)
  - Goal: `stn3` exists at `d57ef58`, pushed to origin, CI green.
  - Scope: branch + `.github/workflows/ci.yml` if the fork's CI needs porting.
  - Done when: `git rev-parse stn3` = `d57ef58…`; CI run on the pushed branch
    is green (build + upstream's own tests, PG 17 and 18).
  - Review focus: no source changes ride along with the branch creation.

- [x] **0.2 `contract/` skeleton** (1a0a7b0)
  - Goal: the adapted engine-agnostic runner compiles and runs empty.
  - Scope: port `conformance/{run.py,cases,expected,divergences}` structure
    from upstream into `contract/`; empty case list; `--help` works.
  - Done when: `contract/run.py` executes against a live extension and reports
    0 cases, 0 failures.
  - Review focus: the runner imports nothing from the repo (design §7).

- [x] **0.3 pgembed early smoke [checkpoint]** (pgembed 3385c3b)
  - PASS-with-caveat: built during a GitHub outage via a local-repo fetch
    override and a hand-synced (byte-verified) bundle stamp; the
    from-scratch wipe+rebuild is re-proven at checkpoint 2.8. Measured
    ledger for upstream d57ef58: 15/16 reloptions registered (all but
    field_weights — score_stop_words and the five TIN options ARE
    upstream), zero stannum.* GUCs, tokenize/ql_parse IMMUTABLE
    PARALLEL SAFE.
  - Goal: prove the L3 install path on day one, scoped to what stock upstream
    provides.
  - Scope: pgembed `pgbuild/Makefile` pin → `stn3` HEAD; build wheel; install.
  - Done when: wheel builds; `CREATE EXTENSION stannum` succeeds; a
    single-column index builds and the queries upstream supports answer.
    Record which §4.1 calls are absent (expected — they land in Phase 2).
  - Review focus: pin bump is the only pgembed change; no pgembed source edits.

---

## Phase 1 — Contract freeze (WS0): record the 0.4.0 oracle

Runs on the **`main` lineage** (`24f5c02`), not on `stn3`.

- [x] **1.1 Generate `docs/tool-contract.md` + manifest** (1f60ba5)
  - Goal: the authoritative surface, generated not hand-written.
  - Scope: generator (tools script) reading `postgres/sql/stannum--0.4.0.sql`,
    `options.rs` reloptions, GUC registrations; emits the manifest with exact
    signatures, volatility, parallel safety, ACLs, defaults, domains.
  - Done when: manifest covers every object in design §4.1; generator is
    idempotent; diff against hand-audit of §4.1 is empty.
  - Review focus: §4.1 says the snapshot+manifest are authoritative and the
    doc is a summary — no drift between them.

- [x] **1.2 Record 0.4.0 answers** (646e249)
  - Goal: `contract/expected/stannum-0.4.0/*.json` from the pinned producer.
  - Scope: recording job (commit `24f5c02`, PG 17, UTF8/`--no-locale`, PG minor
    written into the JSON, jieba = empty-table fingerprint after
    `CREATE EXTENSION`). Cases: every §4.1 function per the five assertion
    kinds (design §7) incl. BM25F orderings (§5.2 rows), scoped-mask and
    both-fields rows, unscoped NEAR/THEN no-cross-field, HOT underfill (keep
    the injected pg_test as enforcement, cite it), parallel custom-dict
    (serial-correct + EXPLAIN no-worker both plans), over-limit fixture
    (locks the superset recording), diagnostics with the version()-changes
    normalization rule, `index_stats.average_length = 0.0`.
  - Done when: recording job runs once, JSON committed, replay is green, CI
    job replays and **fails if expected files change**.
  - Review focus: coverage check — every function/reloption/GUC/operator/
    opclass/ACL/view-option has a case or an `exclusions.yaml` line.

- [x] **1.3 `capabilities()` on both lineages** (stn3 9305ebd / main b195ab0)
  - Goal: the function exists on 0.5.0 (stn3 branch) and its absence on 0.4.0
    is a recorded fact.
  - Scope: `postgres/src/tool/capabilities.rs`; `IMMUTABLE STRICT PARALLEL
    SAFE`; closed JSON schema; byte-identical in fresh-install and upgrade
    scripts; acceptance compares parsed key sets + array order (jsonb!).
  - Done when: contract case for capabilities runs on stn3; the 0.4.0 replay
    documents it under new-objects assertion kind.
  - Review focus: design §4.2 — `limits` reports what 0.5.0 enforces; #88
    differences are divergence entries.

**Phase gate P1:** suite green against 0.4.0 recordings; CI replay locked. — **MET**: 39 cases (1 engine-scoped exclusion, 38 recorded), replay green on PG17+PG18 CI jobs (runs 36672116737 / 36672015529); capabilities() live on stn3 with the §4.2 shape, type-strict gate, max_expansion=1024 measured against tinql.

---

## Phase 2 — L2 skeleton on `stn3`: the frozen SQL surface, single-column

All stock-engine (single-column) behavior; no fields yet. The `postgres/src/tool/`
module lands here. This is the largest phase; steps are independently landable.

- [x] **2.1 `postgres/src/tool/` module + module layout** (a65ab44)
  - Goal: the L2 home exists; UDFs move in step by step; `postgres/src/fields/`
    created empty with the design §3 seam docs.
  - Done when: crate builds; module docs state the seam rules; no behavior
    change yet.
  - Review focus: L2 imports nothing from L0 internals (design §3).

- [x] **2.2 Operator, opclass, custom scan port** (4078674)
  - Scope: `==>` both forms + `stannum_text_ops` + `bind_query` +
    `indexed_query` + SUPPORT + RESTRICT; custom scan with `Private.field`
    plumbing; **the planner widening** (design §3: keep `matching_stannum_indexes`
    walking every key, carry matched ordinal through scan/bitmap/cost/
    heap-fallback/recheck — tests for `title ==>`, `body ==>`,
    `enable_custom_scan = off` even though multi-column arrives in Phase 5).
  - Done when: `==>` single-column contract cases green; bitmap recheck bit
    (`recheck = !planned.exact`) test present.
  - Review focus: Appendix A implicit scope; overflow = `inexact_universe`
    with recheck only on scan/bitmap paths.

- [x] **2.3 Score family on the stock engine** (1d7ebd0)
  - Scope: `score`, `full_score`, `max_score`, `score_inspect`,
    `score_bound(_indexed)` incl. `REVOKE`s; `dense_ratio`/`term_add`/
    `term_replace`; `TermScorer` (R-BIT) parity including the f64→f32 idf cast.
  - Done when: single-column score cases match 0.4.0 recordings (`f32::to_bits`).
  - Review focus: `(text, mask)` keys; stop words in `compile_scoring_terms`
    only; `full_score` ignores the list.

- [x] **2.4 `search` / `search_count` SRFs** (15d90b3)
  - Scope: full signature (begin_tag/end_tag, snippet modes, k1/b);
    `matching_tids` semantics preserved (no recheck on these paths);
    `accepted_pruned_rows` HOT-underfill logic; multi-column form stubbed to
    error until Phase 5.
  - Done when: search SRF cases + permissions/RLS suite (`search_srf.py`
    ported) green single-column.
  - Review focus: design §5.1's three overflow outcomes; MVCC/visibility.

- [x] **2.5 Highlight family** (d90a80e)
  - Scope: all §4.1 overloads incl. 5-arg and `indexed_query` forms,
    `highlight_support` on the right forms only, unknown-field error text.
  - Done when: highlight cases green single-column.
  - Review focus: `column_field_name` returns nothing for attnum ≤ 0.

- [x] **2.6 Diagnostics, GUCs, `jieba_words` table (empty)** (c90703d)
  - Scope: `verify_index`, `segment_info`, `index_stats` (average_length 0.0),
    `index_health` view (security_invoker), `index_analysis`, `version`,
    `wal_rmgr_id` fn+GUC, `index_reads_allowed`, `logs_removal_horizons`,
    `builtin_stop_words`, `ql_parse`, `maybe_quote`, `tokenize` (unicode/
    whitespace now, jieba in Phase 3); 16 reloptions with the five TIN
    ignores + warning; GUC set from §4.1.
  - Done when: diagnostics assertion-kind cases green (normalization rules).
  - Review focus: option/GUC defaults/domains byte-match the manifest.

- [x] **2.7 Upgrade script + fingerprint gate** (d46c2ce)
  - Scope: `stannum--0.4.0--0.5.0.sql` (catalog objects only; every new
    `#[pg_extern]` byte-identical; `SECURITY DEFINER` forbidden); port
    `extension_upgrade.py` with the LSG-still-answers assertion retired for
    0.5.0 targets + relation-ACL/`pg_class.reloptions` assertions added.
  - Done when: fingerprint(fresh 0.5.0) == fingerprint(upgraded); migration
    error (not `segment magic`) when opening a 0.4.0 index after update.
  - Review focus: design §8 step list; ACLs on `jieba_words`/`score_bound*`.

**Phase gate P2:** full contract suite green on `stn3` for every
single-column case; monthly upstream dry-run job enabled. — **MET** @ bfdf466:
single-column set all green (18 PASS incl. tokenizer.unicode_whitespace after the
tinql field_expr port 57c86f3; 2 GAP = documented divergences: 12 upstream GUCs +
capabilities() per §7.3); 19 remaining FAILs are all multi-column/jieba (P3–P5);
upstream-dry-run job live (monthly + dispatch; dispatch vs real upstream bd95c7e
reports MERGE CONFLICT informatively — run 36816452727); stn3 CI 6/6, main CI 8/8.

- [x] **2.8 pgembed checkpoint — wheel green (single-column)** (pgembed ebc2bbe)
  - Pin bump; wheel builds on PG18; `test_pgembed_stannum.py` green for
    single-column paths (create/drop/search/search_count/analysis/
    check_health/hybrid single-col); record the multi-column failures as
    expected-until-Phase-5.
  - **From here on, every phase's checkpoint must keep the wheel green.**

---

## Phase 3 — jieba (WS2): tokenizer parity

- [x] **3.1 Port onto the #92 API** (ed2783c)
  - Scope: both `JiebaIter` constructors + `source_spans` port site; snapshot
    via `compile_with_snapshot`; the `cut()` buffer exception to #92 explicit;
    cancellation expectation documented (not interruptible mid-`cut`).
  - Done when: tokenize symmetry fixtures match 0.4.0 byte-for-byte
    (`test_stannum_jieba.py` ported + contract cases).
  - Review focus: design §6 snapshot contract; generation vs fingerprint
    cache identity.

- [x] **3.2 Governance** (18a3892)
  - Scope: `jieba_words` DDL lifecycle (add/delete/reload/version),
    `AnalysisStamp` on the STNM record (arrives fully in Phase 4.5 — here the
    stamp lives in the current meta mechanism on stn3), `strict_analysis`,
    standby reads, the parallel-eligibility planner hook (clears relation
    parallel safety when a relevant index has nonempty custom dict;
    jieba_words invalidation dependency), `compatibility.md` rules normative.
  - Done when: parallel custom-dict contract case green (both EXPLAIN plans
    serial, serial-correct rows); drift warning/error cases green.
  - Review focus: design §6 state machine; `parallel_safe` = false only on
    nonempty dict.

- [x] **3.3 pgembed checkpoint** (pgembed 68386ec)
  - Pin bump; `test_stannum_jieba.py` green; Chinese-corpus wheel smoke.

---

## Phase 4 — Fielded terms (WS3): the pivotal risk

**Stop condition:** a red semantic smoke halts this phase (and only it).
Fix the implementation; the formula never moves. If the representation
cannot supply df_agg/exact norms/sound bounds → escalate for the STN4
decision (design §5.2) — do not silently proceed.

- [x] **4.1 Codec + fused scorer skeleton (`postgres/src/fields/`)** (be40629)
  - Scope: `~{hex}~` key codec (one lowercase nibble header, `~~` escape,
    round-trip fixtures incl. all 16 ordinals); `FieldTerm`/`LogicalTerm`/
    `Lookup`/`LogicalPostingCursor` per design §5.1 (union candidates,
    equal-ordinal coalescing, `next_bound_interval`); df_agg union-count
    rule; fused formula with 0.4.0's exact arithmetic order.
  - Done when: codec unit tests + cursor fixtures (both-fields / one-field /
    absent) green in Rust tests.
  - Review focus: encoding example `~0~~~0~~foo`; malformed key = query error,
    verify = corruption.

- [x] **4.2 Semantic smoke — GATE** (c8c3b06) — **GREEN**: row1 tie bits 3e113925 both
  ids + id3-first + ctid order; row2 weighted order/bits; row3 single-column
  bits via the real SQL path (2.3 pg_test). The fused scorer reproduces the
  0.4.0 arithmetic bit-for-bit; no STN4 escalation.
  - Scope: the three §5.2 cases (needle tie with `f32::to_bits` + ascending
    ctid; weighted title-vs-body; single-column R-BIT bit-equality).
  - Done when: all three green against 0.4.0 recordings. **Red → stop.**
  - Review focus: a broken tie is a formula bug, never a float tail.

- [ ] **4.3 STNF trailer + reader delta**
  - Scope: segment writers (build, insert buffer, merge incl. meta-lock-free)
    emit `norms` (exact u32 rows, u64 totals, CRC-32/ISO-HDLC u32le inside
    norms_len) + `df_agg` section; `Reader::new` `pages_end` rule (stock
    checks stop there; only the trailer API reads past); trailer bytes in the
    reader-cache budget; once-per-cache-entry validation (token-count cells
    from payload positions — not `max(pos)+1`; checked u64 field totals;
    union df recompute `0 < df ≤ doc_count`).
  - Done when: sidecar fixtures green (empty, one-field, overlap, all-dead,
    merged); every corruption class rejects at open.
  - Review focus: design §5.1 wire grammar; verifier vs open-time split.

- [ ] **4.4 Fused WAND bound + property tests**
  - Scope: bound = `saturate(max_tf*, min_len*)` with min-floor; interval
    truncation at the earlier of covering-block ends and any mask-internal
    stream's next block/chunk/sub-block start; per-field stream indexed with
    that field's raw length so stock `BlockBound` supplies inputs; bucket
    representative translation.
  - Done when: property test holds `exact_score ≤ fused_bound` over generated
    streams/masks/weights (incl. the title-0..1000/body-at-500 case and a
    conjunction); pruned vs exhaustive top-k identical.
  - Review focus: invariant, not prune-count.

- [ ] **4.5 STNM kind-5 envelope + classification**
  - Scope: kind 5 = upstream meta image + one framed STNM record (exact
    tuple grammar, conditional stamp inside body, every matrix row);
    `classify_meta_page` pure parser + `storage::open_index` live wrapper
    (indnkeyatts equality, single-column weight 1.0/no-STNF, names vs
    attnum>0 keys for field_count ≥ 2, segment-magic rules: kind5+LSG =
    corruption, kind1+LSG = migration); every loader-table caller routed;
    guarded-callback fence (insert/vacuum/merge/bitmap before any page
    allocation or WAL).
  - Done when: loader matrix tests green (empty/buffer-only/immutable/
    recovery/WAL-redo; valid v1 never legacy; every 0.4.0 shape → migration
    error); insert-after-restart-before-REINDEX test errors without writing.
  - Review focus: design §8 grammar/matrix; `amvalidate`/`amoptions`/
    `ambuildphasename` outside the fence.

- [ ] **4.6 Latency benchmark + decision — GATE**
  - Scope: §5.2 protocol (paired ratios, PG17, concurrency 1, 20 runs,
    nearest-rank quantiles, corpus checksummed, results to
    `docs/benchmarks/stn3-fielded-poc.json`); Chinese half gated on Phase 3.
  - Done when: dictionary+build ≤ 1.8× (mandatory, includes df_agg section);
    p50 ≤ 1.3× (advisory, waiver needs a named approver); count ≤ 0.4.0;
    decision recorded (fielded terms | STN4 | escalate).
  - Review focus: numbers reproducible from the recorded corpus + protocol.

**Phase gate P4:** smoke green → latency gate → multi-column contract cases
(BM25F orderings, df union two-segment + dead-doc, scoped-mask row) green.

- [ ] **4.7 pgembed checkpoint**
  - Pin bump; multi-column `CREATE INDEX` via `_format_field_weights`; wheel
    multi-column search/search_count smoke green.

---

## Phase 5 — Phrases, field highlights, planner shims (WS4)

- [ ] **5.1 Field scope in tinql + operator**
  - Scope: `Expr::Field` grammar/AST on the merged engine, `project_to_field`,
    `scope_scan_query` application, `check_clause_field_scope` error texts,
    single-column "field syntax requires a multi-column index" rule.
  - Done when: Appendix A operator cases green.

- [ ] **5.2 Same-field phrases + spans**
  - Scope: unscoped phrase = OR of per-field bindings (never cross-field
    adjacency); unscoped NEAR/THEN must not cross fields; field-scoped phrase
    confined; positions stay per-field through the cursor.
  - Done when: phrase/span contract cases match 0.4.0 recordings.

- [ ] **5.3 Field-aware highlights + snippets**
  - Scope: 5-arg/bound forms with field; wrapper-field/no-mark/first-mark/
    first-non-NULL/NULL-skip snippet selection; `search()` multi-column form
    unstubbed.
  - Done when: highlight + snippet cases green; multi-column `search` cases
    (from Phase 2.4 stubs) green.

- [ ] **5.4 Full planner coverage**
  - Scope: bitmap scope + recheck bits end-to-end; heap fallback + recheck;
    cost model; `enable_custom_scan = off` multi-column plans.
  - Done when: WS4 gate — every Appendix A fixture green on both plan modes.

- [ ] **5.5 pgembed checkpoint**
  - Pin bump; **full `test_pgembed_stannum.py` + `test_stannum_jieba.py`
    green on multi-column + jieba**; langchain/llama retriever smoke.

---

## Phase 6 — Packaging + migration + release (WS5/WS6)

- [ ] **6.1 Two-artifact migration CI job**
  - Scope: isolated 0.4.0 server builds old indexes (single/multi/jieba/
    empty/buffer-only) → byte-evidence of magic/meta → clean shutdown →
    install 0.5.0 → restart → ALTER EXTENSION → migration-error assertions →
    per-index REINDEX → answers. Per-index commits; traffic-off until all
    verified.
  - Done when: job green in CI.
  - Review focus: design §8 procedure steps 1–6 exactly.

- [ ] **6.2 Migration + runbook docs**
  - Scope: operator doc (downtime window, replicas rebuilt not replayed,
    rollback = restore, no downgrade); REINDEX runbook; divergence ledger
    (`contract/divergences/stannum.yaml` final review).

- [ ] **6.3 Wheel release alignment**
  - Scope: pgembed 0.5.0-aligned wheel (PG18, `BUILT_FOR_POSTGRES_MAJOR`
    audit), standalone binaries, release evidence per pgembed's existing
    release process.
  - Done when: wheel installs the 0.5.0 extension and the full pgembed suite
    is green against it.

- [ ] **6.4 Conformance + parity sweep — GATE**
  - Scope: upstream TIN conformance suite; full contract suite; pgembed
    suite; **[pg-agent]** pg-agent v13 suite replay.
  - Done when: three suites green (pg-agent optional-but-recommended; if
    unavailable, record it and ask the user to run it).
  - Review focus: §4.3 — any recorded-answer change without a contract bump
    is a bug.

- [ ] **6.5 Release 0.5.0**
  - Scope: `stannum--0.5.0.sql` final, CHANGELOG, tag, `stn3` → merged per
    §9 strategy; `main` → maintenance-only note.
  - Done when: tag exists; origin `stn3` and release artifacts pushed;
    tracker closed.

---

## Dependency graph

```
0.1 ── 0.2 ── 0.3
1.1 ── 1.2 ── 1.3        (on main; parallel to Phase 0)
0.* + 1.* ── 2.1 ── 2.2 ── 2.3 ── 2.4 ── 2.5 ── 2.6 ── 2.7 ── 2.8
                              │
              3.1 ── 3.2 ── 3.3   (after 2.6; parallel to 4.x)
                              │
         4.1 ── 4.2(GATE) ── 4.3 ── 4.4 ── 4.5 ── 4.6(GATE) ── 4.7
                                                              │
         5.1 ── 5.2 ── 5.3 ── 5.4 ── 5.5                       │
              └────────────────────────────────────────────────┘
                              │
         6.1 ── 6.2 ── 6.3 ── 6.4(GATE) ── 6.5
```

## Estimate (from design §11)

10–14 elapsed weeks; critical path 2 + 2 + 5 + 2 + 3 = 14 engineer-weeks
(WS0–WS1, WS2, WS3, WS4, WS6), WS5 off-path. STN4 fallback, if triggered at
4.2/4.6, is a separate re-plan — never folded into this figure.

## Upstream re-pins

| date | from → to | reason | contract suite |
|------|-----------|--------|----------------|
| 2026-09-30 | — → `d57ef58` | initial base | n/a yet |
