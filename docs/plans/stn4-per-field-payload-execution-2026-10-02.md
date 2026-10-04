# STN4 Per-Field Payload — Stepwise Execution Plan

Status: ready for loop-orchestrated execution · Normative design:
`docs/designs/stn4-per-field-payload-2026-10-02.md` (oracle-repair-6 READY,
commit `9e7cdfe`) · Created 2026-10-02

This plan decomposes the design into **24 steps across 5 phases (A–E)**. Each
step is sized for one orchestration loop (implement → review → gate) and ends
in a checkable state. The design document is the spec; where this plan and the
design disagree, **the design wins** and this plan gets a fix.

STN3 plan `docs/plans/stn3-tool-layer-execution-2026-09-30.md` steps 4.7–6.5
are superseded as a schedule. Their *goals* (pgembed checkpoint, phrases,
highlights, planner, migration CI, wheel, release) reappear below under STN4
numbering. Phases 0–3 and 4.1–4.6 on `stn3` stay done.

## Starting points (do not re-litigate)

- Branch `stn3` @ `9e7cdfe` (this design). On-disk layout is still 4.3–4.6
  fielded-terms (`~{h}~` keys + STNF v1 df sidecar). 4.6 gate commit
  `b49ad94` / basis `1e1b20b`. **Do not resurrect fielded keys.**
- **Census baseline** after 4.5 (`d5fa6f9`): **5 FAIL / 2 GAP / 32 PASS**.
  FAIL ids: `fields.phrase_no_cross`, `fields.phrase_scoped`,
  `fields.then_no_cross`, `fields.near_no_cross`, `fields.patterns`.
  GAP ids: `catalog.gucs`, `catalog.functions`. Command:
  `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0`
  (exit 1 while FAIL > 0; the gate is the Summary line, not exit 0).
- **4.6 evidence exists:** `docs/benchmarks/stn3-fielded-poc.json`. Mandatory
  miss: dictionary **3.34×**, build **1.95×** (ceiling 1.8×). Advisory p50
  **5.61×** (ceiling 1.3×, `waiver: null`). Corpus: 20,000 rows, seed
  `20260930`, sha256
  `52775c8a17077f09f028f5758eb53df65f94dc37c5a1f3e800990f5a39f0bbeb`, columns
  `id, title, body`. Decision: STN4. Harness: `benchmarks/stn3_fielded_poc.py`.
- **0.4.0 keeps serving pg-agent** (`~/Projects/pg-agent`, outside this
  workspace). Slip is the risk, not a forced cutover. Shared pgembed prefix
  `/Users/wxl/Projects/pgembed/src/pgembed/pginstall` is contended: before any
  wipe, `lsof` + marker poll; during pg-agent gate windows use a `/tmp`
  prefix-copy. `script/pgrx-lock.py` always.
- pgembed pin is still Phase 3.3: `pgbuild/Makefile` `STANNUM_COMMIT :=
  a963080d1f28705f30ab6473156370a64e9a57bb`. Checkpoints 4.7/2.8-style have
  not run on 4.1–4.6 commits.
- Semantic smoke is already green at scorer/SQL on fielded-terms (`3e113925`
  tie). C.1 re-proves the same bits on **STN4** indexes. The formula does not
  move if they fail.
- Next fallback after a **STN4** mandatory miss is a `contract/divergences`
  entry + `contract_version` bump, not another layout and not fielded keys
  (design §9).

---

## 0. Protocol for the orchestrator running this plan

- **One step = one loop.** Dispatch a pair agent with the step brief + design
  section refs; verify; then oracle-review the result (design doc in the
  selection); fix until the step's Done-when holds. Mark the step complete
  here (`- [ ]` → `- [x]`) with the commit SHA.
- **PostgreSQL 18 is the only sanctioned engine.** Every gate, benchmark,
  census and pgembed run in this plan targets **PostgreSQL 18** (decision
  2026-10-04). PG17 numbers are historical artifacts only: the C.1 ledger row
  (`5 FAIL / 2 GAP / 32 PASS`) was measured on PG18.4, and re-running the same
  suite on PG17.11 Homebrew gives `7 FAIL / 1 GAP / 31 PASS` — `calls.stop_words`
  (`builtin_stop_words('auto')` = zh+en) and `catalog.functions` differ by major
  version, not by commit. **Never mix versions inside one measurement**, never
  compare a PG17 ratio against a PG18 baseline, and do not "reconcile" a
  ledger row by re-running it on a different major. The C.2 latency gate is
  therefore **re-measured on PG18** before any decision is drawn from it.
- **Gates are tests, not opinions.** Every phase ends in a suite that runs in
  CI. A step whose gate is red after two fix rounds escalates to the human
  with the failing evidence — it does not get re-rolled until green by luck.
- **Interrupt boundary rule** (adopted 2026-10-04 after C.2 and C.3 each hit a
  CI-only link failure). Plain Rust `#[test]` code must never transitively
  reach `pgrx::check_for_interrupts!` or another PostgreSQL-global-dependent
  interrupt implementation: the macro expands to inline code reading
  PostgreSQL's `InterruptPending` **data** symbol, which the plain test binary
  cannot resolve on Linux (macOS binds lazily, so only CI sees it). Every
  long-running retrieval, expansion, and positional algorithm must accept an
  explicit progress/interrupt hook; PostgreSQL entry points supply the
  production hook, plain unit tests supply a no-op or counting hook, and the
  hook is invoked at bounded work intervals **including** skip / dead-entry /
  no-match loops. Runtime flags are not link isolation, and blanket
  `cfg(test)` suppression must not disable cancellation in PostgreSQL-backed
  tests. Validate both the Linux plain-test link and the PostgreSQL-backed
  cancellation behavior on PG18. Existing model: `fields/intersect.rs`
  (`Intersect` / `Front`). Do not bury a default PostgreSQL interrupt call
  inside a supposedly pure walker. Verify with `nm -u`: the plain test binary
  must have **0** `InterruptPending` references while the release `.so` keeps
  at least **1**.
- **Never edit:** `contract/expected/**` (except by the pinned recording job),
  released `postgres/sql/stannum--*--*.sql` scripts, upstream files outside
  the owned-shim list (parent design §3). Weakening any gate, `contract/`
  check, or CI assertion is out of scope.
- **Design is normative.** Parent STN3 design
  `docs/designs/stn3-tool-layer-2026-09-29.md` still owns formula, SQL
  surface, and the 0.4.0→0.5.0 migration fence. This STN4 design owns
  representation, STNF v2, classification, buffer generation, and expansion
  walk. The old STN3 *plan* is format-only for this document.
- **pgembed checkpoints.** pgembed is a separate repo (loaded workspace root
  `pgembed/`, filesystem `/Users/wxl/Projects/pgembed`). Pin/build/pytest run
  **from that repo root**, not from stannum. Its build pins `STANNUM_COMMIT`
  in `pgbuild/Makefile`. At every phase-end checkpoint: bump the pin to the
  step's commit, build the wheel (`python3 tools/build_standalone_extension_wheel.py
  --extension stannum` / `make -C pgbuild all`), run the named pgembed tests,
  record pass/fail here.
  **The 0.5.0 lineage must stay installable into pgembed.** C.2 is the
  representation gate: D's full multi-column wheel (D.5) does not waive a red
  C.2 (design §8).
- **Census during the cutover.** Full-suite FAIL count may rise in Phase A/B
  while writers land before readers. **Single-column cases must stay PASS
  from A.1 onward.** C.1 restores the 4.5 multi-column greens on STN4
  indexes. Record every full-suite Summary in the census ledger below.
  `--check` exits 1 while `FAIL > 0`; census gates parse the `Summary:`
  line (and FAIL ids), they do not require exit 0 until D.4.
- **Upstream pin.** Base is `upstream/main@d57ef58`. Record any re-pin here.
  Monthly upstream-merge dry-run stays enabled (STN3 P2).
- **External dependency.** pg-agent v13 lives at `~/Projects/pg-agent`.
  Steps that need it are marked **[pg-agent]** — they require the user to run
  or expose that repo; skip without blocking other steps.
- **New source files** need a `source-provenance.json` entry in the same
  commit (STN3 loop rule).
- **Do not** call `Index::expand` for `field_count` in `2..=16` — but
  during Phase A (before B.1), the LEGACY fielded-terms adapter still
  exists and may serve LEGACY indexes only; STN4 multi-column query paths
  MUST fail with a known unsupported error rather than route through the
  legacy adapter (silent empty/wrong results are a bug). A.5's expected
  multi-column census reds are unsupported-path failures, not silent
  empties. B.1 is the first step that enables multi-column STN4 expansion.
  Do not sniff `FCH1` on single-column extents. Do not score or bound a
  multi-column term from the parent nibble alone.

---

## Phase A — Representation (≈ 2.5 wks)

Channel directory, tagged buffers, STNF v2, classification. No query-path
rewrite yet except what flush/open must do. SQL surface unchanged.

- [x] **A.1 Channel directory + `Term::channels(field_count)`** (40377d8)
  - Goal: every multi-column `TermEntry` multiplexes F unmodified STN3
    ordinal/payload streams behind an `FCH1` directory; unpack is a segment
    seam; single-column stays stock.
  - Scope: `segment/src/segment.rs` (`Term::channels`); encode helper beside
    it or `segment/src/` owned file; `segment/src/verify.rs` (multi-column
    unpacks `channels()` first, including `child.df > 0`; single-column uses
    **only** stock `ordinals()` / `payload()` / `df()` — no
    `starts_with(b"FCH1")`); `segment/src/index.rs` flush omit of zero-posting
    fields; no SQL; `postgres/src/fields/` untouched except comments.
  - Done when:
    - `cargo test -p segment --lib` green, covering design §1.3 matrix:
      `channels(1)` and `channels(17)` are `Error::Corrupt`; trailing bytes in
      a child slice are Corrupt; mismatched ordinal/payload field sets are
      Corrupt; `child.df` is the ordinals count varint, not directory `len`;
      a listed record whose stock `count = 0` is Corrupt; `n == 0`,
      `record.len == 0`, remainder/shortfall, missing magic on
      `field_count` in `2..=16` are Corrupt; overflow of `5+5n` / `Σ len`
      is Corrupt not wrap.
    - Writer never emits a directory on `field_count == 1`. Flush omits
      zero-posting fields from **both** directories.
    - Single-column verify accepts a stock payload whose first four bytes are
      `46 43 48 31` when the stock decoder accepts it:
      `rg -n 'starts_with\(b"FCH1"\)|== b"FCH1"' segment/src postgres/src`
      has no single-column verify hit (design §1.3.1 choice a).
    - Stock `term()` still works on 0.5.0 single-column fixtures:
      `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0 --case arithmetic.row3_single_column`
      is PASS.
    - `script/test-all quick` green.
  - Review focus: design §1.2, §1.3, §1.3.1.

- [x] **A.2 Mutable flush + merge recount** (5518a34)
  - Goal: `CREATE INDEX` and incremental `INSERT` both write one `TermEntry`
    per surface token; parent `df` is the union; merge recounts; buffers this
    version writes begin with `STN4_BUFFER_TAG`.
  - Scope: `segment/src/index.rs` (`add_occurrence(token, field, positions,
    field_length)` **after** §6.3.1 selects the tag; never convert an untagged
    stream into builders); `segment/src/merge.rs` (per-channel stock merge,
    then recount union into `TermEntry.df` — do not add channel dfs);
    `postgres/src/storage/layout.rs` / `mod.rs` (KIND_BUFFER write path:
    `STN4_BUFFER_TAG := 0x00 0x01` as the first two bytes of every nonempty
    multi-column stream **this version writes**; single-column never writes
    the tag; leftover untagged streams are never rewritten to inject a tag);
    record body after the tag is this step's layout (field + field_length
    without a `~{h}~` key).
  - Done when:
    - Two-field overlap fixture: union `df == 3` not `4` on SQL `CREATE INDEX`
      **and** on insert-after-create (pg_test or cluster test). Dead ordinal
      remains in `df` until rewrite.
    - Merge of two segments recounts; `field_length` is that field's raw
      token count; a token posted in title only does not write a body
      directory record.
    - `cargo test -p segment --lib` and
      `script/pgrx-lock.py -- cargo pgrx test pg18 -p stannum` (new fixtures)
      green.
    - Single-column buffers never start `0x00 0x01`.
  - Review focus: design §1.3 build/insert caller, §6.3.1 write path, §7
    Replace (`index.rs`, `merge.rs`).

- [x] **A.2.1 Buffer discriminator fixtures** (bd73b7a)
  - Goal: persisted generation is the KIND_BUFFER tag, not in-memory maps,
    not STNF version, not `~0~foo` key spelling. Restart / recovery / WAL
    replay emit the same label as the live writer.
  - Scope: tests in `postgres/src/storage/` (pg_tests) and/or
    `postgres/tests/` covering design §6.3.1 decoding order and the buffer
    rows of the §6.3 fixture table. Production write path already landed in
    A.2; this step is the fixture lock.
  - Done when: all of the following have named tests and pass under
    `script/pgrx-lock.py -- cargo pgrx test pg18 -p stannum`:
    - Tagged buffer after restart/WAL replay whose surface tokens include
      `~0~foo` → `BufferCurrent` (not Stale). `legacy_fielded_key_shape` is
      **not** consulted on the tagged arm.
    - Reopening an untagged 4.3–4.6 fielded-terms buffer → `BufferStale`.
    - Fold reads records **after** the tag; no second tag on append.
    - Untagged zero-term (`docs > 0`, `terms == 0`, no v2 segments): restart
      → `StaleFielded`; INSERT is the rebuild error; no flush; recovery
      unchanged (no page dirty).
    - Untagged zero-term beside ValidV2 → `MixedFielded`, same no-write.
    - Tagged zero-term INSERT then flush then restart/recovery stays
      `Current`.
    - `docs == 0 ∧ bytes == 0` → `BufferEmpty`; `docs`/`bytes` mismatch →
      `BufferMalformed`; unknown `0x00` + version → `BufferMalformed`.
    - Proof fixture: a well-formed legacy `ForwardRecord` stream's first byte
      is never `0x00`.
  - Review focus: design §6.3.1 (tag, decoding order, write path).

- [x] **A.3 STNF v2 reader/writer** (83dc178)
  - Goal: new multi-column trailers are version 2, norms only. v1 parse
    remains long enough to classify `StaleFielded`. Query still must not
    depend on the sidecar for `df_agg` once B.1 lands; this step only changes
    the trailer codec and the once-per-cache-entry checks that used the df
    map.
  - Scope: `segment/src/trailer.rs` (write `version = 2`, drop `df_len` /
    `DfEntry` from the **write** grammar; v1 decode retained); `segment/src/
    segment.rs` (`Reader::new` still stops stock checks at `pages_end`;
    trailer API reads v2); `segment/src/verify.rs` (positions pass checks
    cells vs that field's position-list lengths; parent `TermEntry.df`
    against the union of channel ordinals `0 < df ≤ document_count`; no df
    sidecar recompute); `segment/src/merge.rs` recounts norms and entry dfs;
    `postgres/src/fields/df.rs` sidecar **reader** stops being the query
    source (may still exist until C.3; `union_df_agg` stays a build-time /
    verify helper). `benchmarks/stn3_fielded_poc.py` STNF prefix parser is
    **not** required to understand v2 until C.2.
  - Done when:
    - `cargo test -p segment trailer` green. Encoded bytes: `bytes[4] == 2`;
      prefix has no `df_len`; roundtrip norms (empty, overlap, all-dead,
      merged) match; `field_count == 1` trailer is Corrupt; v2 layout with a
      df section is Corrupt; v1 fixtures still `decode` for classification;
      CRC-32/ISO-HDLC still holds (`crc32_is_iso_hdlc`).
    - Single-column: `total == pages_end`, no trailer.
    - `script/test-all quick` green.
  - Review focus: design §6.2, §1.4, §7 Replace (`trailer.rs`).

- [x] **A.4 Classification matrix** (a99a6e4)
  - Goal: leftover 4.3–4.6 indexes classify as `StaleFielded` /
    `MixedFielded` / `Corrupt` under kind-first predicates. Guarded callbacks
    rebuild-error with **distinct** strings; no page dirty. `ambuild` /
    `ambuildempty` remain exempt. STNF version never selects the buffer
    decoder.
  - Scope: `postgres/src/storage/mod.rs`, `layout.rs` (`StaleFielded`,
    `MixedFielded`; kind-first: kind-1 → `PreStn3`, kind-5+LSG → `Corrupt`);
    buffer labels from A.2.1; rebuild error texts pinned in pg_tests;
    `open_index` live wrapper. `legacy_fielded_key_shape` may land here as a
    stub called only from the untagged arm — isolation fixtures are A.4.1.
  - Done when: every fixture row in design §6.3 holds as a named test
    (`script/pgrx-lock.py -- cargo pgrx test pg18 -p stannum`). Minimum
    assertions:
    - all ValidV2 + `BufferEmpty` / `BufferCurrent` / `BufferNoTerms` →
      `Current`
    - all ValidV1 + `BufferEmpty` / `BufferStale` / `BufferNoTerms` →
      `StaleFielded`
    - ValidV1 + ValidV2 only → `MixedFielded`; v2 + `BufferStale` →
      `MixedFielded`; v1 + `BufferCurrent` → `MixedFielded`
    - v2 + `BufferMalformed` → `Corrupt`; v1+v2+malformed → `Corrupt`;
      v1+v2+missing trailer → `Corrupt`; v2 + malformed FCH1 → `Corrupt`
    - valid v2 with surface token `~0~foo` → `Current`
    - empty kind-5 → `Current`; buffer-only `BufferCurrent` → `Current`;
      buffer-only `BufferStale` → `StaleFielded`
    - kind-1 + LSG → `PreStn3` (parent §8 migration string);
      kind-5 + LSG → `Corrupt` (not that string)
    - No page dirty on rebuild/corrupt classes (insert-after-restart-before-
      REINDEX style fixture).
  - Review focus: design §6.3 predicates and fixture table; parent §8
    kind-first.

- [x] **A.4.1 `legacy_fielded_key_shape` isolation** (f5654b5, clippy fix 3815ee3)
  - Goal: the `~{h}~` grammar survives only as an untagged-legacy
    **validator**. It does not encode, does not call `Index::term` on an
    encoded key, and does not participate in lookup, expand, scoring, or
    writing.
  - Scope: `postgres/src/storage/mod.rs` (or a tiny `storage` helper)
    `fn legacy_fielded_key_shape(key: &str, field_count: u8) -> bool`; unit
    tests only. Do **not** delete `fields/codec.rs` (that is C.3).
  - Done when:
    - `legacy_fielded_key_shape("~0~foo", 1) == false`
    - `legacy_fielded_key_shape("~0~foo", 2) == true`
    - `legacy_fielded_key_shape("~0~~~0~~foo", 2)` matches the 4.1 escaped
      grammar (nibble `< field_count`, nonempty escaped token)
    - `field_count` not in `2..=16` → `false`
    - Tagged-arm tests from A.2.1 still never call it (assert via a
      `#[cfg(test)]` call counter or by code structure + grep of the tagged
      decode arm)
    - `rg -n 'fielded_key\(|header\(|upper_fence\(' postgres/src/storage`
      is empty
    - `cargo test -p stannum --lib legacy_fielded_key_shape` green (filter is
      the function/test name; there is no `storage` crate). pg_tests that
      call it: `script/pgrx-lock.py -- cargo pgrx test pg18 -p stannum`
  - Review focus: design §6.3.1 (`storage::legacy_fielded_key_shape`), §7
    Replace (`codec.rs` fate — keep the file until C.3).

- [x] **A.5 pgembed checkpoint + census snapshot** (pgembed e4caf84)
  - Goal: 0.5.0 stays installable after the representation cut; record the
    contract ledger so C.1 has a before/after.
  - Scope: pgembed `pgbuild/Makefile` pin → this step's `stn3` SHA only. No
    pgembed source edits. Census on a STN4-writer build.
  - Done when:
    - Pin bump; `make -C pgbuild all` (stamp invalidated by make, not
      hand-synced); `python3 tools/build_standalone_extension_wheel.py
      --extension stannum`; wheel `CREATE EXTENSION stannum` succeeds
      (extversion 0.5.0).
    - `pytest tests/test_pgembed_stannum.py tests/test_stannum_jieba.py`:
      single-column + jieba-default paths stay green (3.3 invariant).
      Multi-column official reds are **expected** until C.1/D.5 — record the
      raw counts.
    - Census snapshot recorded in the ledger. Single-column `--case
      arithmetic.row3_single_column` plus `--area tokenizer --area parallel
      --area stats` : **0 FAIL**. Full-suite FAIL count may exceed 5; do not
      fail A.5 on multi-column arithmetic reds (readers still fielded-terms
      until B.1).
  - Review focus: design §8 protocol (pgembed from Phase 2 onward); pin-only
    pgembed diff.

**Phase gate PA** (normative: design §1, §6.2, §6.3, §6.3.1): FCH1 channels
exist; STNF v2 writes; leftover v1 classifies; tagged buffers round-trip;
wheel still installs; single-column contract green.

---

## Phase B — Query wiring (≈ 1.5 wks)

Replace fielded-key lookup with `Index::term(text)` + `channels()`. Formula
and bound functions stay. Parallel with A.3–A.4 after A.2 (design graph).

- [x] **B.1 Lookup / expand / cursor on channels** (0555857)
  - Goal: query opens one dictionary key per logical token; two
    `scan_window` consumers (capped candidates vs uncapped scoring);
    `df_agg` from parent `Term::df()`.
  - Scope: `segment/src/index.rs` (`Index::scan_window` streaming cursor);
    `postgres/src/fields/expand.rs` (replace bodies; `field_count` in
    `2..=16` **MUST NOT** call `Index::expand`, including with
    `usize::MAX`); `postgres/src/fields/types.rs` (`df_agg` filled from
    `Term::df()`); `postgres/src/fields/cursor.rs` (opened on unpacked
    channels); `postgres/src/fields/df.rs` (`query_total_df` sums entry
    dfs); drop codec from this path (file remains until C.3). Rewrite
    fields unit tests that used encoded keys against surface tokens.
  - Done when:
    - `cargo test -p stannum --lib fields` green: empty streams not error;
      candidate Overflow still `Lookup::Overflow` only; `LogicalTerm.df_agg`
      is parent `Term::df()`, never a channel-local stream length (scoped
      mask does not change it).
    - **Scoped cap fixtures** for prefix/wildcard, range, regex, and fuzzy:
      `title:(…)` with body-only hits `> max_expansion` and title hits
      `≤ max_expansion` returns `Terms` of the title tokens, not Overflow;
      in-scope over cap still Overflows for candidates.
    - **Streaming fixtures:** a window with far more out-of-scope matches
      than `max_expansion` retains at most the cap on (a); (a) is not
      `usize::MAX` + `Vec`.
    - **Over-limit SQL:** same in-scope over-cap query —
      `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0 --case expansion.over_limit`
      keeps overflow membership/count; a pg_test compares `score` /
      `score_bound_indexed` row order + bits against the complete applicable
      set (cap raised or explicit oracle). Scoring must not reuse the
      discarded candidate list.
    - **Channel-error fixtures:** corrupt FCH1 in the window →
      `AdapterError` on both consumers, including when in-scope hits already
      exceeded the cap; `Fn(&str)` is never the channel-open site.
    - `rg -n 'Index::expand|\.expand\(' postgres/src/fields/expand.rs` has no
      call on the `2..=16` arm (design: grep of the multi-column adapter for
      `Index::expand` is empty).
  - Review focus: design §2 (walk, two consumers, scoped cap, over-limit
    scoring, channel errors).

- [x] **B.2 Bound + verify on channels** (4fb2f2f)
  - Goal: existing `fused_bound` consumes unpacked `FieldTerm`s; parent
    nibble is never a multi-column bound; verify unpacks first.
  - Scope: `postgres/src/fields/bound.rs` (wire only — formula stays);
    `segment/src/verify.rs` (recompute per-channel min_len / max bucket and
    parent `df`; reject listed `child.df == 0`; single-column does not sniff
    `FCH1`); score/bound call sites that still open fielded keys.
  - Done when:
    - `cargo test -p stannum --lib fields::bound` green, including
      `witness_title_0_1000_body_at_500` (truncates at 500),
      `randomized_exact_le_bound_and_pruned_matches_exhaustive` (pruned ==
      exhaustive top-k), `unreadable_chunk_bound_is_unprunable` (bound-read
      `Err` → INFINITY).
    - New fixtures: two unit-weight channels score `tf* = 2` and a parent-
      nibble bound of 1 is never consulted; missing `channels()` on
      multi-column bound → INFINITY, score → error.
    - Verify rejects listed `child.df == 0`; single-column verify still has
      no `FCH1` sniff.
  - Review focus: design §3, §9 (parent nibble, verify must unpack).

- [x] **B.3 pgembed checkpoint** (pgembed f15bb18)
  - Goal: keep the wheel green while query wiring lands. C.2 has not yet
    gated a full multi-column product wheel.
  - Scope: pgembed pin bump only.
  - Done when: pin → B.2 SHA; `make -C pgbuild all`; wheel builds; pytest
    single-column + jieba paths green (same 3.3 invariant). Record
    multi-column official counts. Census ledger row for the B.2 build
    (informational).
  - Review focus: design §8 “0.5.0 lineage must stay installable”.

**Phase gate PB** (normative: design §2, §3): multi-column query walks
channels; scoped expansion does not count out-of-scope tokens; bound witness
holds; `Index::expand` is not the multi-column implementation.

---

## Phase C — Gates (≈ 1.5 wks)

C.1 requires A.4 (leftover v1 must classify before SQL smoke rebuilds v2 on
the same relation names) **and** B.2 (bound witness). C.2 is the
representation gate: **D.\* does not start on a red C.2.** C.3 may overlap
D.\* after C.2. E.1 may already have started after A.4.

- [x] **C.1 Semantic smoke — GATE + contract-suite re-run** (547f080) — **GREEN**:
  all five §5.2 cases PASS bit-exact through real SQL on STN4 multi-column
  indexes; census 5 FAIL / 2 GAP / 32 PASS (the 5 = Phase D phrase/span).
  - Goal: parent §5.2 cases 1–5 through real SQL on multi-column **STN4**
    indexes match the 0.4.0 recordings. Restore the 4.5 multi-column greens.
  - Scope: no formula edits in `postgres/src/fields/score.rs`. Live SQL via
    contract cases. Rebuild/REINDEX so leftover v1 indexes are not the
    smoke target (they must `StaleFielded`, not silently score).
  - Done when:
    - `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0 --area arithmetic`
      : **0 FAIL**. Row1 live bits for ids 1 and 2 are `3e113925`; id 3 first;
      ctid order. Row2 weighted order/bits match. Row3 single-column R-BIT
      still matches. Row4 both-fields and row5 scoped-length match recordings.
    - 4.5 multi-column greens stay green on STN4:
      `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0 --area ddl --area expansion --case fields.snippets --case fields.highlight_spans`
      : **0 FAIL**.
    - **Full census** on this STN4 build:
      `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0`
      Summary parses to **5 FAIL / 2 GAP / 32 PASS** (FAIL ids exactly the
      five phrase/span/patterns cases; GAP ids unchanged). Record in the
      ledger. **Red smoke → stop.** Formula does not move.
    - `script/test-all quick` green; CI on the pushed SHA green.
  - Review focus: design §2 acceptance; parent §5.2; design §8 C.1.

- [x] **C.2 Latency — GATE** (`1d7ae21`; advisory ranked p50 settled under `C2-p50-v2`, mandatory build under `C2-build-v2`)
  - Goal: dictionary bytes (no df sidecar) and build ≤ 1.8× `stn3_single` on
    **each** corpus; ranked p50 ≤ 1.3× or a named waiver **in the results
    file**.
  - Scope: parent §5.2 protocol (**PostgreSQL 18**, concurrency 1, 20 runs, nearest-rank
    quantiles). English POC corpus (same checksum as 4.6) **and** a fixed
    Chinese corpus under the pinned jieba snapshot (Phase 3 is done). Query
    lists committed beside the results. Write
    `docs/benchmarks/stn4-per-field-poc.json` covering both. Extend
    `benchmarks/stn3_fielded_poc.py` (or a sibling `stn4_per_field_poc.py`)
    so STNF **v2** prefix parse treats `stnf_df_len = 0` (v2 has no df
    section). `dict_bytes = sections.dictionary + stnf_df_len`. Norms are
    **not** in the sum.
  - Done when:
    - JSON exists; English corpus sha256 equals
      `52775c8a17077f09f028f5758eb53df65f94dc37c5a1f3e800990f5a39f0bbeb`.
    - Chinese corpus checksum committed; jieba snapshot/fingerprint recorded.
    - On **each** corpus (English and Chinese): `dict_ratio <= 1.8` and
      `build_ratio <= 1.8`. Do not copy 4.6's single top-level
      `.mandatory_gate` unless the STN4 JSON keeps that shape for one
      corpus; if both corpora live in one file, each corpus object must
      carry its own `mandatory_gate.pass == true` (or `dict_ratio` /
      `build_ratio` fields the harness tests assert). `stnf_df_len == 0`
      on the STN4 system.
    - Ranked p50 per corpus: `advisory_p50_gate.pass == true` **or** that
      corpus's `advisory_p50_gate.waiver` names the workload, the ratio, and
      why staying is cheaper than another cut. **No waiver, no pass.** A
      follow-up ticket is not a waiver. **Both corpora's result objects
      contain labeled p99 fields** (informational, not a gate).
    - A mandatory miss **stops and escalates** (design §9 next fallback:
      recorded divergence + `contract_version` bump). Do not start D.\*.
    - `python3 -m unittest discover -s benchmarks -p 'test_*fielded_poc.py'`
      (harness unit tests) green.
  - Review focus: design §5, §8 C.2; 4.6 JSON as the comparison artifact.
  - **2026-10-03 — STOPPED RED, escalated to the user. Evidence:
    `docs/benchmarks/stn4-per-field-poc.json`.** MANDATORY dictionary/build gate
    **missed on English build only**: dict 1.0006× on both corpora (hypothesis
    confirmed; `stnf_df_len == 0`, `trailer_count == 1` on multi-column), build
    1.8689× vs the 1.8× ceiling (1.1451s / 0.6127s, one `CREATE INDEX` wall
    clock in the inherited 4.6 protocol). Chinese build 1.2719× passes.
    Advisory p50 misses on **both** corpora (2.88× / 2.38× vs 1.3×) with
    `waiver: null`. Top-level decision `escalate`. Per design §9 a mandatory
    miss stops the line; Phase D.* does not start and C.4 stays gated. The
    JSON also records a semantic divergence found by this run: Chinese
    `(索引 OR 查询) AND 搜索` returns 0 rows on `stn4_multi` where
    `stn3_single` and `v040` return 2 (the English equivalent agrees at 10
    rows), so that query's Chinese latency ratio is not evidence.
  - **2026-10-04 — §9 bounded build profile (diagnostic attachment; user-authorized,
    not a gate rerun): `docs/benchmarks/stn4-c2-build-profile.json`.** N=12 paired
    CREATE INDEX samples on the same locked English corpus: medians 1.1360s
    (`stn4_multi`) / 0.6123s (`stn3_single`), median pairwise ratio **1.8451×**,
    9/12 pairs above 1.8, band 1.34–1.94. Conclusion `inconclusive` under
    pre-declared numeric criteria: the miss is **not** one slow multi-column
    build (C.2's single shot sits mid-profile), but the ceiling also lies inside
    the observed band, so a single-shot protocol can pass or miss from the same
    cost. Field-count shape: `(title)` 0.068s, `(title, body)` 1.155s,
    `(title, body, id)` 1.237s (2→3 columns only ~1.07×, so no large per-field
    superlinear jump; English body volume dominates). Gate fields unchanged;
    Phase D.* still not started.
  - **2026-10-04 (second oracle adjudication) — the mandatory build gate is FAIL in
    substance.** PG18 re-measurement history: single-shot English build measured
    1.7350x (pass), then 1.9144x on a fresh datadir (fail), while both 12-pair
    profiles land at median 1.8451x (PG17) / 1.8509x (PG18) with 9-10 of 12 pairs
    above the ceiling. The 1.7350x draw was favourable; the observed distribution is
    centred near 1.85x, above 1.8x. Chinese build passes on every run
    (1.2504x-1.3503x) and dictionary passes everywhere (1.0006x / 1.0005x).
    `docs/benchmarks/stn4-per-field-poc.json` carries `current_adjudication`
    overriding the earlier "mandatory gate PASSES on PG18" note, and the build
    measurement now follows protocol `C2-build-v2` (64 counterbalanced paired
    builds, median of paired ratios, 95% batch-cluster bootstrap, PASS only when
    the upper bound ≤ 1.8x). **Advisory p50 fails on both corpora on every run**
    (EN 3.111-3.256x, ZH 2.363-2.385x vs 1.3x, `waiver: null`). Path to green, in
    oracle-ranked order: instrument the build differential (multi-minus-single per
    stage, not which stage is biggest inside multi) → remove repeated work →
    instrument the query path on the dominant queries (`the` ≈65% of the English
    multi-minus-single p50 excess, `我们` ≈61%, `历史 AND 望远镜` and `战争 AND 历史`
    ≈29% of the Chinese) → optimize toward ≤1.70x build and substantial
    candidate/scoring work reduction for query → uninstrumented acceptance under
    `C2-build-v2`. Off-limits: BM25F formula, thresholds, corpora, protocols'
    workloads, fielded term keys, `contract/expected/**`.
  - **2026-10-04 (third entry) — MANDATORY build gate now genuinely PASSES under
    `C2-build-v2`.** Build-stage instrumentation located the premium in the
    mutable-index intern path (multi 400.2 ms vs single 170.5 ms, +230.7 ms),
    caused by a per-token `to_owned()` plus a second `BTreeMap` copy that the
    single-column arm does not make — it already used `Cow` + `get_mut`. Removing
    it (per-field `BTreeMap<Cow>`, key moved only on a miss) took that stage to
    310.1 ms vs 155.9 ms and the whole-build premium from 570 ms to 492 ms.
    FCH1 directories (1.57 ms) and the STNF v2 norms trailer (0.11 ms) are
    negligible, so encoding volume was never the cost. Acceptance with
    instrumentation disabled: **English median 1.6204x, 95% CI 1.5636-1.6524,
    4 of 64 pairs above 1.8 -> PASS** (upper bound <= 1.8 and inside the <=1.70
    margin target); **Chinese median 1.2616x, CI 1.2396-1.2863, 0 above -> PASS**.
    Dictionary bytes and every query answer are unchanged. Recorded in
    `docs/benchmarks/stn4-per-field-poc.json` under `c2_build_v2_acceptance`
    with the raw paired ratios. **Remaining blocker: advisory ranked p50 on both
    corpora** (EN 3.256x / ZH 2.363x vs 1.3x, `waiver: null`), now the only thing
    between the project and a green C.2.
  - **2026-10-04 (fourth entry) — C.2 is GREEN.** Three instrumentation-led
    query-path changes closed the advisory gate: (a) scoring reads the stored TF
    bucket from the ordinals stream instead of decoding the payload entry to
    count positions — bit-equivalence pinned over raw tf 1..=400; (b) the ranked
    path keeps the same bounded heap the single-column path uses and resolves
    visibility only for the `limit` survivors, falling back when a row is
    invisible or HOT-rewritten; (c) AND is a real posting intersection — the
    rarest child leads and siblings seek to its ordinal, `AtLeast` /
    `Disjunction{min>1}` pick the n-th smallest current ordinal — so
    `历史 AND 望远镜` fell from 13362 advances / 6679 hash-set inserts to 231 / 29
    while scoring the same 58 rows. Ranked p50 under the new `C2-p50-v2`
    protocol (20 whole-list rounds per system, alternating systems, per-round
    loadavg, 95% round-cluster bootstrap): **English 0.8701x, CI
    [0.8419, 0.9151]; Chinese 0.7814x, CI [0.7675, 0.8042]** — both PASS with
    the upper bound far below 1.3x, `waiver: null`, worst single query 1.022x
    (EN) / 0.975x (ZH). Build re-measured on the final tree: English 1.6066x
    [1.5613, 1.6379], Chinese 1.2640x [1.2489, 1.2781].
    **Oracle final adjudication: C.2 complete** — the sub-1.0 p50 ratios are a
    legitimate consequence of the three changes (the baseline still pays the
    union walk, the position decode and the eager visibility), not a measurement
    bias; the oracle asks for an independent replication and a factorial
    ablation as release-confidence work, not as gate criteria. Oracle P1:
    the intersection walk needs an explicit exhaustive-vs-fused-WAND
    equivalence test before release (carried into C.3/D). Oracle P2 repaired
    in-commit: `saturating_add(1)` followed by `== 0` was unreachable at
    `u32::MAX`, so the intended exhaustion was a latent loop; now `checked_add`.
    Note: CI job `crash-before-publication` flaked once on a docs-only commit
    (`81d9e5f`, whose code is identical to the passing `bf430c8`); the head
    commit is green on all six jobs.
  - **2026-10-04 — PG18 re-measurement (first adjudication; superseded by the entry
    above) — MANDATORY gate appeared to PASS on PG18, C.2 still incomplete.** Owner decision: PostgreSQL 18 is
    the only sanctioned engine for every gate/benchmark/census/checkpoint (see §0).
    The original C.2 numbers were taken on PG17.11, which is now a historical
    artifact. Re-measured on PG18.4 (`/tmp/stn3-a2-prefix`, HEAD `52e13a3`, locked
    corpora, 20 timed runs): English dict 1.0006× / **build 1.7350×** (1.2973s /
    0.7477s); Chinese dict 1.0005× / **build 1.2504×**. Both mandatory gates pass;
    `decision_pg18 = stay`. Root `decision` stays `escalate` (PG17 historical), with
    an explicit `decision_scope` object stating that `decision_pg18` covers the
    mandatory gate only and is **not** permission to start Phase D.
    **Advisory p50 still FAILS on both corpora** — English 3.111×, Chinese 2.385×
    against 1.3×, `waiver: null` — so `c2_complete: false` on both and Phase D stays
    blocked. Diagnostic caveat: the 12-pair English build profile on PG18 has median
    1.8509× with 10/12 pairs above 1.8, so the English build margin is thin; the
    PG18 run also happened under loadavg ~13-19.
    **Oracle adjudication (controller session):** the mandatory pass holds under the
    declared one-shot build protocol (the paired profile is explicitly
    `not_a_gate_rerun`); the way to green is code-level reduction of the per-field
    execution overhead, not paperwork. Ordered path: explicit decision scopes →
    clean PG18 baseline → instrument the field-aware query path → focused execution
    optimizations (advance postings once per candidate document, decode each field
    payload once, reuse channel-directory/cursor state, lazy payload decoding) →
    re-run both corpora under the unchanged protocol until p50 ≤ 1.3× on both.
    Formula, representation, thresholds and corpora are all off-limits.

- [x] **C.2-R Fielded boolean correctness — remediation** (`8ba6683`; the duplicate-clause scoring regression it exposed was repaired in `7e6438f`) — user-authorized repair, unlocked while C.2 stays RED: it fixes a defect, it is not Phase D feature work
  - Goal: multi-column `stannum.search()` / `stannum_count()` must never return
    a silently wrong boolean answer. Either evaluate the query correctly or
    raise the existing `does not support this query on a multi-column index`
    error before any scan.
  - Defect: `collect_fielded_terms` flattened `And` and `Or` identically and
    collected `Not` as a positive leaf, conjunction-ness came from the root node
    only, and `hits >= terms.len()` then required every leaf. Net semantics were
    "root AND → AND of all leaves, else OR of all leaves, NOT → positive", so
    `(A OR B) AND C` answered `A AND B AND C`, `A OR (B AND C)` answered
    `A OR B OR C`, and `C AND NOT A` answered `C AND A`. Introduced by STN3 4.5
    `1212a25` (multi-column unlock), not by STN4; a regression against 0.4.0,
    which had no fielded path.
  - Repair: whole-tree allowlist validator + recursive `eval_fielded` over the
    fielded postings (And = intersect, Or/`Disjunction{min}`/`AtLeast` = count
    satisfied direct children, `Not` = complement over the indexed universe,
    `MatchAll` = universe, `Field` = leaf mask only, `Boost` = score only), one
    shared membership path for `search()` and `search_count()`, and
    `Regex`/`Range`/`Fuzzy`/`Span`/`SpanExpr` rejected loudly anywhere in the
    tree. Scores and row order for the previously correct shapes are unchanged.
    Bounded-interval `check_for_interrupts!` covers the new in-memory set work.
  - Done when: pgrx pg_test suite green (336 passed / 0 failed / 5 ignored);
    `script/test-all quick` green; fmt + clippy clean; census unchanged from the
    pre-repair build on the same install (verified by control experiment: both
    give `7 FAIL, 1 GAP, 31 PASS` on PG17.11 Homebrew — the plan ledger's
    `5 FAIL / 2 GAP / 32 PASS` was measured on a PG18.4 engine and does not
    reproduce here for `calls.stop_words` and `catalog.functions`; both
    pre-date this step and need their own look).
  - Field-scope surface note locked by test: field scope requires `name:(…)`
    (`grammar.pest` `field_head`); a bare `name:term` is a colon-bearing token
    and matches nothing, on both single- and multi-column indexes.
  - Review focus: design §9 (fielded query correctness); contract gap — the
    suite has **no boolean coverage at all** (all 14 distinct query literals in
    `contract/cases/**` contain no `OR`/`AND`), which is why this survived
    until a benchmark query surfaced it.

- [x] **C.2-S Boolean contract coverage** (`8fdee84`, after the source-attribution entry landed) — user-authorized follow-on ("先补 contract 布尔覆盖再定 C.2")
  - Goal: make a silent boolean answer impossible to reintroduce. The suite had no boolean query at all (all 14 distinct query literals were phrases, NEAR, THEN, terms or expansions), which is exactly why the C.2-R defect survived.
  - Scope: new `contract/cases/boolean.yaml` (area `boolean`, truth-table corpus, `bool_multi` + `bool_flat`), recordings from the 0.4.0 build, three `contract/divergences/stannum.yaml` entries. No weakening of any existing case, recording or assertion.
  - Done when: every case captures ids **and** `search_count` (limit 100, so a top-k cap cannot hide a mismatch); recordings come only from the 0.4.0 build; existing recordings byte-identical (`git diff -- contract/expected` empty); the pre-repair build fails the cases that pin the repaired semantics and the repaired build passes or shows a documented GAP; census deltas recorded in the ledger.
  - Result: 12 boolean cases bit-exact vs 0.4.0; one new FAIL (`fields.boolean_duplicate_leaf` `flat_ranked`, the pre-existing single-column IndexScorer duplicate-clause 2×, documented); two new GAPs (`fields.boolean_regex_on_multi`, `fields.boolean_mixed_regex_and_term`, D.2 owns). Controller re-run census: `7 FAIL / 4 GAP / 43 PASS of 54`.
  - Provenance caveat (recorded, not hand-fixed): `boolean.json`'s `source.extension_commit` is the suite repo's `postgres/` HEAD, not the producer commit; the runner only checks `engine`/`extension_version` on merge.
  - Review focus: design §9; the zero-boolean-coverage hole that let the defect live.

- [x] **C.3 Dead codec** — scope includes: delete `fields/codec.rs`, encoded-key
  exports and fixtures, **AND the STNF-v1 df-sidecar query reader +
  query-facing `DfEntry` APIs**; retain only the isolated STNF-v1 trailer
  parser needed for `ValidV1`/`StaleFielded` classification. Done when:
  `query_total_df` sums parent `TermEntry.df`; `grep -r 'df_agg.*section'
  postgres/src/fields/` finds no query-path reader; workspace builds clean.
  - Goal: delete the fielded-terms codec after C.2 proves the representation.
    **Timing:** after C.2 green; may overlap D.\*; must not start before C.1
    (B.1 still compiles tests against the file). Generation stays the buffer
    tag, not key spelling.
  - Scope: delete `postgres/src/fields/codec.rs`; drop `mod codec` and
    `fielded_key` / `header` / `upper_fence` / `decode` exports from
    `postgres/src/fields/mod.rs`; update the L1 comment to match design §7
    (owns channel unpack, fused scorer, bound, norms reader — not a key
    codec). **Keep** `storage::legacy_fielded_key_shape`. Delete encoded-key
    **writers** / lookup fixtures in `fields/` and `segment/`. No encode
    helper, no `Index::term` on encoded keys.
  - Done when:
    - `test ! -e postgres/src/fields/codec.rs`
    - `rg -n 'fielded_key|upper_fence|fn header\(|KeyDefect' postgres/src/fields segment/src`
      is empty (KeyDefect fielded variants gone; `legacy_fielded_key_shape`
      lives under `postgres/src/storage`)
    - Classifier tests from A.4 / A.4.1 still compile and pass (tagged
      `~0~foo` after reopen → Current; untagged legacy → StaleFielded;
      v2 + untagged → MixedFielded; tagged BufferNoTerms; untagged zero-term
      is StaleFielded and INSERT does not write;
      `legacy_fielded_key_shape("~0~foo", 1) == false`)
    - `script/test-all quick` green; `cargo test -p stannum --lib fields`
      green; workspace tests green.
  - Review focus: design §7 Replace (`codec.rs`), §8 C.3.
  - Result (**code @ 7fdee28**, box checked separately): codec deleted along
    with `KeyDefect` / `ReportMode` / `FieldKeyError` / `query_defect` and the
    STNF-v1 df-sidecar query reader; `query_total_df` now sums parent
    `TermEntry.df`; `segment/src/trailer.rs::inspect_stored_term` and
    `postgres/src/storage/buffer_label.rs::legacy_fielded_key_shape` retained;
    `merge_still_rejects_legacy_v1_encoded_term_segments` still rejects a v1
    encoded-term segment; `header` renamed `blob_header` and
    `reject_fielded_keys_without_trailer` renamed
    `reject_legacy_encoded_terms_without_trailer` so the remaining decoder is
    not read as an encoder counterpart. Oracle review in the controller's
    session (twice) produced 1 P0 + 3 P1, all repaired: empty lookup text keeps
    its 0.4.0 query diagnostic instead of being reported as corrupt index data;
    the intersect-WAND tests' "exhaustive" side no longer descends from
    `drain_boolean`; each child's bound is the raw `fused_interval_bound` with
    no `exact_at(pivot)` floor; pruning walks per interval with proven
    multi-ordinal skips and a best document after a pruned boundary; the
    interval walk is asserted to partition the candidate set. Three named
    mutation proofs plus a fourth (overlapping interval) are recorded in the
    loop log. Census **6 FAIL / 4 GAP / 44 PASS** on PG18.4 release build —
    see the ledger row and the two census-measurement rules added this step.

- [ ] **C.4 pgembed checkpoint (STN4 representation wheel)**
  - Goal: after C.2 green, pin a STN4-capable commit and prove the wheel
    still builds. This is **not** D.5 (phrases/highlights/planner still
    pending). C.2 gates this checkpoint.
  - Scope: pgembed pin bump only.
  - Done when: pin → C.2 (or C.3 if it has landed) SHA; wheel builds; pytest
    single-column + jieba green; multi-column `CREATE INDEX` +
    search/search_count smoke on a title/body index **succeeds** for the
    arithmetic-shaped queries (phrase cases may still error with the
    current `search() does not support this query on a multi-column index`
    until D.2). Record counts.
  - Review focus: design §8 “C.2 gates D wheel”; D.5 still required for the
    full official suite.

**Phase gate PC** (normative: design §2 acceptance, §5, §8 C.1–C.2): smoke
bits match 0.4.0 on STN4 indexes; the census returns to the pre-STN4 shape
(`5 FAIL / 2 GAP / 32 PASS` was the 39-case 4.5 baseline; the boolean suite
added later brings the suite to 54, so the gate is the accounting rule in the
adjudication section, not a stale denominator); mandatory 1.8× holds on both
corpora; p50 holds or carries a named waiver in the JSON. D.\* and the D.5
wheel do not waive this gate.

**Phase C status after C.3.** C.1, C.2, C.2-R, C.2-S and C.3 are green; the
only open step is **C.4** (pgembed checkpoint), whose wheel build fetches the
stannum commit from GitHub by SHA. Nothing else in Phase C remains. C.3's
residual oracle P1 is owned by D.4 (see the adjudication section), and the
census's remaining FAILs are D.1/D.2's, so **Phase C is functionally closed
pending C.4** and Phase D is unblocked.

---

## Oracle adjudications — Phase D and Phase E (2026-10-04)

The oracle adjudicated the owner-decision items before Phase D starts. These
are binding on every step below; a step that contradicts one of them is out of
spec and stops for a re-adjudication.

**Positional semantics have a clear authority order. Adopt design §4 as
normative:**

> On a multi-column index, a phrase or positional expression is evaluated
> wholly within one field. An unscoped positional expression matches if at
> least one eligible field satisfies that expression; a field-scoped
> expression evaluates only its selected field. Positions or intervals from
> different fields never combine to satisfy a phrase, NEAR, or THEN
> expression.

This does **not** turn ordinary unscoped boolean conjunction into same-field
conjunction: the any-field retrieval rule remains in force for its leaves.

| item | decision | required 0.5.0 behavior |
|-------|----------|-------------------------|
| `fields.patterns` | **ADOPT baseline compatibility; REJECT permanent multi-column rejection** | implement each supported wildcard, prefix, regex, fuzzy, and range expansion on a multi-column index with the baseline's matching and expansion-limit behavior. Implementing prefixes alone does not satisfy the case. |
| `fields.boolean_regex_on_multi`, `fields.boolean_mixed_regex_and_term` | **REJECT rejection as the finished semantic** | expand and evaluate inside the boolean tree; convert both GAPs into executable assertions. Loud rejection is appropriate only while incomplete. |
| `fields.boolean_duplicate_leaf` | **REJECT the 2× divergence; restore baseline folding** | `alpha AND alpha` must preserve the baseline membership and score. Restore the normalization / scoring-term fold — *not* indiscriminate token deduplication, which would break repeated phrase terms and differently boosted clauses. The plan's earlier "documented single-column divergence" framing of this case is withdrawn. |
| `catalog.functions` | **ADOPT the existing GAP; owner E.2** | controller-measured on PG18.4 against the **release** build: live 42 vs recorded 41 functions, **lost = 0**, and the single gain is `capabilities()` — exactly what `contract/divergences/stannum.yaml` already documents. The divergence entry is **correct as written and needs no change**. E.2's remaining work is to verify `capabilities()` visibility/privileges. **Do not** widen it: `corrupt_index_page()` and `index_page_kinds()` are `#[cfg(feature = "pg_test")]`-only UDFs (`postgres/src/udfs.rs` 423/437) that exist **only in a test build** — they appear in the catalog only if the census is run against the extension `cargo pgrx test` installed, which is a measurement error, not an addition. |
| `catalog.gucs` | **DEFER-WITH-OWNER: E.2** | a GAP establishes neither compatibility nor intentional drift. E.2 must inventory and classify names, types, defaults, contexts, and behavior before any divergence is granted. |

**Corrected census gate.** The D.4 done-when text `0 FAIL / 2 GAP / 37 PASS`
is **REJECTED**: it accounts for 39 cases while the suite holds 54, and an
obsolete denominator must not become permission to omit coverage. The gate is
accounting-based instead:

- every current case is accounted for;
- zero *unapproved* failures;
- every remaining GAP is named, explained, and owned;
- an intentional difference is an executable, narrowly scoped exception — not
  prose beside a raw failure.

With the six current FAILs resolved and both regex GAPs converted to passing
assertions (Phase D.1 + D.2), the unchanged suite reads **0 FAIL / 2 GAP /
52 PASS** — the two remaining GAPs being `catalog.functions` (already correctly
documented; E.2 only verifies `capabilities()` visibility) and `catalog.gucs`
(E.2 must classify before granting or closing it). If E.2 resolves
`catalog.gucs`, the end state is **0 FAIL / 1 GAP / 53 PASS**.

**Two census-measurement rules that a controller got wrong once (2026-10-04).**
Both were caught and corrected by re-measuring; they are now discipline:

1. **The census runs against the release build, on PostgreSQL 18, with the
   pgembed prefix's `bin` first on PATH.** `initdb` / `pg_ctl` / `psql` on the
   default PATH are Homebrew's PostgreSQL **17.11**, whose prefix also holds a
   **stale** `stannum.dylib` from an earlier `cargo pgrx test` run. Starting a
   scratch server without prefixing PATH silently measures PG17.11 against that
   stale build. Correct form:
   `PATH=/Users/wxl/Projects/pgembed/src/pgembed/pginstall/bin:$PATH` for the
   server, and `cargo pgrx install --release --package stannum
   --no-default-features --features pg18 --pg-config <that prefix>/bin/pg_config`
   first. Two visible symptoms of getting it wrong: `calls.stop_words` and
   `catalog.functions` change answer, and the pg_test-only UDFs
   `corrupt_index_page()` / `index_page_kinds()` appear in the catalog.
2. **Never mix builds or majors inside one measurement.** The controller ran
   one census on PG17.11 against a stale `pg_test` build and read it as PG18.4;
   the corrected PG18.4 release-build reading is **6 FAIL / 4 GAP / 44 PASS**
   (FAIL: `fields.boolean_duplicate_leaf`, `fields.phrase_no_cross`,
   `fields.phrase_scoped`, `fields.then_no_cross`, `fields.near_no_cross`,
   `fields.patterns`; GAP: `catalog.functions`, `catalog.gucs`,
   `fields.boolean_regex_on_multi`, `fields.boolean_mixed_regex_and_term`).
   The *only* legitimate PG17.11 readings are the explicitly labelled C.2-R
   control rows in the census ledger.
3. **The shared pgembed prefix holds exactly one stannum build, and
   `cargo pgrx test` / `cargo pgrx install` / a pgembed wheel build all**
   **overwrite it.** `cargo pgrx test pg18` leaves a **`pg_test`** build,
   which is *not* the release build — it carries `#[cfg(feature = "pg_test")]`
   UDFs (`corrupt_index_page`, `index_page_kinds`) that change the catalog, and
   after an STN4 step it may be stale. Rule: after any `cargo pgrx test`, and
   before any census or pgembed run, reinstall the release build
   (`cargo pgrx install --release --package stannum --no-default-features
   --features pg18 --pg-config <prefix>/bin/pg_config`). Sequence a C.4/D.5
   wheel build (which installs a commit-pinned build into the same prefix)
   **after** the concurrent step's gates and census, then reinstall the release
   build again before measuring anything. Sanity check before trusting a census:
   `select count(*) from pg_proc where pronamespace = 'stannum'::regnamespace`
   must be **42** on a release build and must not contain
   `corrupt_index_page` / `index_page_kinds`.

**Planner fallbacks.** Correctness is release-critical; not every optimization
is. Release-critical for E.5: heap/sequential evaluation of supported scoped
queries (must apply field identity, tokenizer configuration, boolean
semantics, and same-field positional rules), bitmap execution / recheck
(recheck exact; candidate generation must have no false negatives), and
ranked/custom-scan scope and pruning (membership, scores, ordering, and
conservative pruning bounds preserved). Deferrable by D.4 with a documented,
tested fallback: a specialized bitmap or other accelerated path, and
scope-specific cost/selectivity refinement (only legality: estimates affect
plan choice, never the answer). An accepted limitation may say *"scoped query
shape X currently uses validated fallback plan Y rather than accelerated path
Z; query semantics are unchanged"* — it may not say the fallback drops scope
or that a previously supported query now errors. Both plan modes must assert
the observed plan **and** the result; planner GUCs alone do not prove which
executor ran.

**C.3's residual P1 has an owner: D.4.** The second oracle review of the
repaired C.3 tree found **no P0** and one P1 — the intersect-WAND harness pins
`next_conjunction`, `next_atleast`, `fused_interval_bound` and
`next_interval_end`, but *not* a production boolean-WAND orchestration loop,
because **that loop does not exist yet**: `postgres/src/search.rs`
`fielded_eval_inner` walks the intersection and scores every candidate, and
`fielded_visible_ranked` heap-selects already-scored rows, so the bound
machinery this harness pins is the design §5.1 / plan-4.4 foundation rather
than live code. The narrow fix it offered — drive the production routine or add
a seam asserting interval monotonicity, disjoint skips, next-pivot-after-skip
and point-bound ≤ interval-bound — is therefore assigned to **D.4**, whose
done-when already makes ranked/custom-scan scope and pruning release-critical.
When D.4 lands the production fused-WAND walk, this harness becomes its
reference oracle and a production-pinned test is required. C.3's own scope
closed what was closable: the candidate set, the raw bound, and interval
skipping are all pinned, with three named mutation proofs.

Also settled by that review and recorded as test comments in `bound.rs` /
`error.rs`: summing per-child interval bounds is a valid upper bound because
each child's `fused_interval_bound(start, end)` bounds that child's
contribution, an absent child contributes `0.0`, and looseness costs pruning
efficiency only (an unsafe `max()` aggregation when several children
contribute positively is caught by the existing per-candidate
`exact <= bound` assertion). Its precondition is nonnegativity, enforced at
`tinql/src/parser/descent.rs` `boost_suffix`
(`!(0.0..=BoostFactor::MAX).contains(&factor)`) for query boosts and at
`postgres/src/storage/mod.rs::apply_field_weights`
(`!weight.is_finite() || weight <= 0.0`) for index weights — negative boosts
are not legal input, so no negative-boost regression test is warranted. And
`AdapterError::EmptyToken`'s display text `malformed fielded term key: decoded
token is empty` is **frozen API text** retained for 0.4.0 query-diagnostic
compatibility, not leftover codec terminology to clean up later.

**The duplicate-leaf ruling (2026-10-04, three adjudications). The 0.4.0 fold is NOT restored; 0.5.0 preserves additive duplicate-term weights on every scoring surface; `fields.boolean_duplicate_leaf` is a raw FAIL accounted for below.**

*Adjudication 2 superseded adjudication 1.* Adjudication 1 ordered "restore baseline folding". D.1 implemented it and broke `tin_conformance::tests::pg_tin_1_0_3_repeated_terms_add_their_boosts`, because TIN 1.0.3 `catalog.S-12` pins `a a` → term weight `40000000` = 2.0, `a AND a` scoring like `a^2`, and `(a^2)^3` = 6.0 — with CI proving stannum already satisfied it (run `37192362958` on `f825d96`, job `arm64 / PostgreSQL 18`, log `== conformance: ok`). Adjudication 2 therefore ruled: retain TIN-compatible additive duplicates, revert the fold, document a narrow 0.4.0 divergence. It also corrected the controller's overreach: the two recordings observe **different shapes and outputs** (TIN `a a` through `score_inspect` weight; the 0.4.0 contract `alpha AND alpha` ranked scores), so neither proves the other engine failed a case, and no BM25-saturation reading makes a folded score coexist with a 2.0 term weight.

*Adjudication 3 rejected an unauthorized third resolution.* While reverting, the agent introduced a caller-dependent `preserve_scoring_multiplicity` flag — `false` for `build_standalone_scorer` (backing `stannum.search()`, folding) and `true` for the `score()` / `full_score()` / `score_inspect()` paths (multiplicity) — which made **both** frozen recordings pass: TIN conformance exit 0 with `catalog.S-12` PASS, and the census at 5 FAIL / 4 GAP / 45 PASS with `fields.boolean_duplicate_leaf` PASSING. The oracle **rejected** it, on correctness grounds rather than cosmetics: for a mixed query such as `alpha AND alpha AND beta`, folding changes the *relative* contribution of `alpha` versus `beta`, which can change ordering and top-k membership. A caller-specific scoring policy also lets the SQL entry point choose the effective weights with no user opt-in, lets `score_inspect()` describe weights `search()` does not use, and — unverified — could let the same SQL text score differently depending on `stannum.enable_custom_scan`.

> **Ruling.** Restore pre-D.1 additive behaviour on every scoring path: `search()`, `score()`, `full_score()` and `score_inspect()` all preserve additive duplicate-term weights, and `score_inspect()` describes the weights actually used for ranking. Remove the caller-specific switch entirely — not merely flip it. Boolean membership may still fold duplicate predicates; the simplification must not discard an additive weight. **Option A with frozen tooling:** leave `contract/run.py` unchanged, leave both recording trees unchanged, leave `contract/cases/**` untouched, leave the broken error-shaped `fields.boolean_duplicate_leaf` entry **deleted** from `contract/divergences/stannum.yaml`, and leave the case as a **raw FAIL** whose account is this ledger.

*Option B is withdrawn.* The controller read `contract/run.py::compare_divergent` and found the runner cannot express this divergence at all: it supports only identity-set captures (via `expected.extra`, which documents rows the live engine **gained** — `compare_identity_set` FAILs on lost rows with no divergence path) and ERROR captures (via `sqlstate` / `message`); anything else falls through to `error_payload` and fails with *"divergence expected an ERROR"*. `flat_ranked` is a plain `value` capture of `[id, float4-bits-hex]` rows. The oracle explicitly declined to extend the runner for this, so the case keeps no divergence entry and the census FAIL is its account. This also means the entry that was already broken at HEAD (`6111c5d`, written in the bogus error shape, which is precisely why the census reported FAIL rather than GAP) stays deleted rather than rewritten.

**The account for `fields.boolean_duplicate_leaf`** (this IS its documentation; the case carries no divergence entry by design):

1. The 0.4.0 recording folds duplicate conjuncts. `flat_ranked` records `[[1,"3f1f9306"],[3,"3f1f9306"],[8,"3f1f9306"],[2,"3f05e0a1"],[6,"3f05e0a1"]]` — `alpha AND alpha` scores exactly like `alpha` on the single-column `bool_flat` index.
2. 0.5.0 deliberately preserves **additive effective weights** on every scoring surface, so `alpha AND alpha` contributes `alpha`'s score twice, and `alpha AND alpha AND beta` weighs `alpha` twice relative to `beta`. This is what the TIN-recorded `a a` = 2.0 / `a AND a` = `a^2` / `(a^2)^3` = 6.0 weights in `catalog.S-12` require.
3. That choice makes this case's `flat_ranked` capture incompatible with the frozen 0.4.0 recording. The other five captures (`ids`, `count`, `ranked` on the multi-column index, `flat_ids`, `flat_count`) still match; only `flat_ranked`'s score bits differ, and its membership and tie ordering are unchanged. **The account makes no claim that the historical 0.4.0 build was ever shown to fail `catalog.S-12`'s `score_inspect()` case.**
4. Required implementation coverage: `search()` and `score()` must agree by document on the same index and snapshot; additive duplicate weights pinned including a **mixed** query where duplication changes relative term contributions; `score_inspect()` must report the weights actually used for ranking; phrase and explicit-boost multiplicity coverage retained. Plus a **plan-invariance regression**: the same `search()` SQL under `stannum.enable_custom_scan = on` and `= off`, establishing which plan actually executed (EXPLAIN, not merely the GUC value) and comparing per-document scores and ranked order under the documented tie policy. Custom-scan-vs-standalone disagreement is an **unverified risk until that test runs** — treat it as a risk to close, not a bug to report.

**Upgrade policy for 0.5.0.** Supported through E.1's migration route:
released 0.4.0 single-column **and** multi-column indexes — codec cleanup
cannot retroactively classify released indexes as unsupported development
artifacts. STN4-development v1 indexes: **REJECT** LSG migration, require an
actionable rebuild error. 0.1.0 / STN3-development fielded-term indexes:
rebuild-only; do not promise direct migration on current evidence. Other
older, unknown, or malformed formats: **REJECT** heuristic migration — add
support only with an explicit classifier, converter, and fixture. Generation
is determined by validated storage metadata / tags, **not** by whether a token
resembles `~0~foo`; legacy term decoding may still be needed *after*
positively selecting an authorized legacy migration path, so deleting the
production codec does not prove migration needs no reader. The E.1 REINDEX
runbook must guarantee: which formats migrate vs rebuild with actionable
errors; that supported rebuild DDL rebuilds from the heap without first
successfully scanning the rejected format; full preservation of the index
definition (columns/order, expressions/predicates, tokenizer/dictionary
configuration, field weights, reloptions); the stated and tested
update/restart/REINDEX ordering on PG18; post-rebuild verification of
membership, ranking, field scopes, positional behavior, and integrity; and
that an interrupted rebuild publishes no partial index and loses no heap data
(`REINDEX CONCURRENTLY` only if tested).

**Implementation order: keep `D.1 → D.2 → D.3 → D.4 → D.5`.** There is no
cheaper reordering justified by the dependency graph — D.2 would otherwise
invent temporary scope handling that D.1 must replace. The cheaper schedule is
to prepare D.2's independent fixtures and D.4's plan matrix *while* D.1 lands.
Oracle review focus per step: **D.1** scope survives normalization and boolean
composition, unknown fields fail deterministically, no ad hoc query-string
rewriting; **D.2** no cross-field intervals, scoped expressions open only
eligible channels, expansions preserve boolean structure, independent
exhaustive results agree with production walking/pruning; **D.3** no
wrong-field highlights, no offset confusion, no snippet change to
membership/ranking, repeated terms and multibyte text correct; **D.4**
observed-plan coverage, correct fallbacks, ranking consistency, complete
census accounting, `script/test-all full` on PG18; **D.5** its own checklist
and final evidence, not inferred from D.4.

---

## Phase D — Phrases, highlights, planner (old Phase 5) (≈ 2 wks)

Starts only if C.2 is green. Done-when texts of STN3 5.1–5.5 apply, with
“fielded key” read as “channel” (design §8). Positions come from payload
channels (design §4).

- [x] **D.1 Field scope in tinql + operator**
  - Goal: `title:(…)` and `==>` field scope apply a mask / `scope_scan_query`,
    not a dictionary-key fence.
  - Scope: `tinql/src/` (`Expr::Field` grammar/AST — parse already exists
    from STN3 57c86f3); `postgres/src/score.rs` (`project_to_field`,
    `scope_scan_query`); `postgres/src/operator.rs`
    (`check_clause_field_scope` error texts); single-column “field syntax
    requires a multi-column index” rule.
  - Controller root-cause note (2026-10-04, proven against 0.4.0 at
    `/tmp/stannum-main-46` @ `7ab511b`): `fields.boolean_duplicate_leaf`
    `flat_ranked` doubles because `postgres/src/score.rs` scores from
    `parse_tinql_to_scoring_query`, which lowers with
    `SimplificationProfile::StructuralScoring` — a profile that keeps repeated
    flat AND/OR terms so each occurrence adds its boost. **That variant does not
    exist in 0.4.0**: 0.4.0's `score.rs` used `parse_tinql_to_query` (the
    `Structural`, deduplicating profile) for every scoring path, and 0.4.0 had
    no `parse_tinql_to_scoring_query` at all. It arrived with the STN3 dev line.
    `bool_multi` already matches 0.4.0 bit-exact because the fielded path
    composes the deduplicated structural query. No conformance case pins the
    duplicate-boost behavior (the only corpus containing a repeated term is
    `catalog.scoring.yaml` `cat_s17`'s `[2, a a b]`, queried with the single-term
    `==> 'a'`). Check which of the three `parse_tinql_to_scoring_query` call
    sites in `postgres/src/score.rs` (4786, 5023, 5434) sit on the `search()` /
    `score()` path; phrase and boost-dependent callers keep
    `StructuralPreserveTermMultiplicity`.
  - **Controller scope note (2026-10-04, verified on `7fdee28`): the D.1 scope
    line names two symbols whose real state differs.** `scope_scan_query`
    exists at `postgres/src/score.rs:5700` but is currently the **identity**
    with the doc comment *"Single-column indexes have no field metadata, so this
    is the identity and every match is ordinal 0. Phase 5 applies Appendix A's
    implicit scope"* — so implementing it is D.1's actual work, and it already
    has three callers wiring it (`postgres/src/am.rs:268`, `score.rs:4775`,
    `customscan.rs:1473`). `project_to_field` **does not exist at all**: the
    only references are comments deferring it to this phase
    (`postgres/src/highlight_udfs.rs:40`, `postgres/src/tool/highlight.rs:125`).
    The operator side already calls `crate::score::check_expr_fields`
    (`postgres/src/operator.rs:632`, from `check_clause_field_scope`, which
    validates only `T_Const` text operands). Do not invent a new seam: extend
    `scope_scan_query` and `check_expr_fields`, and add `project_to_field` only
    where the highlight path actually needs named-field confinement.
  - Done when: Appendix A operator cases green —
    `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0 --area fields`
    no longer FAILs on scope plumbing (phrase/span may still FAIL until
    D.2). `script/pgrx-lock.py -- cargo pgrx test pg18 -p stannum` green.
    **Also D.1's** (per the 2026-10-04 adjudication): the duplicate-clause
    **fold** on the paths that must match 0.4.0 — 0.5.0 currently scores it 2×
    because scoring terms come from a duplicate-preserving parse where 0.4.0
    saw the folded structural list. Fold duplicates only; do **not**
    deduplicate tokens outright (repeated phrase terms and differently boosted
    clauses depend on them). **SUPERSEDED by the third adjudication — see the
    duplicate-leaf ruling below. The ruling is the opposite: every scoring
    surface keeps additive duplicate-term weights, `fields.boolean_duplicate_leaf`
    is a raw FAIL, and the `contract/divergences/stannum.yaml` entry stays
    deleted.** The controller's root-cause proof and the plan-invariance
    requirement both still apply.
  - Review focus: design §4 field-scoped queries; parent Appendix A;
    adjudication §"duplicate-leaf ruling".
  - Result (**code @ 6783b2f**, box checked separately): `title:(…)` / `==>` scope
    by **channel mask**. `tinql` gained a `FieldScope` trait (`NamedFields` /
    `NoFields`) plus `plan_scoped` / `page_plan_scoped` and a
    `query_scoped(query, scope)` that resolves `Query::Field { name, inner }`
    against the index's field list and filters that term's `Term::channels` by
    the mask; `scope_scan_query` wraps the query in `Query::Field` and rejects a
    foreign scope; `check_query_fields_on` / `walk_expr_fields` report unknown
    fields deterministically, preserving the 0.4.0 single-column error text;
    `postgres/src/fold.rs` refuses `Query::Field` so a scoped query cannot
    silently lose its scope; `View::field_names` carries the envelope names.
    **The scoring contract did not change**: every surface keeps additive
    duplicate-term weights, and the plan-invariance regression compares
    `search()` under `stannum.enable_custom_scan` on/off while naming the plan
    it actually ran (`Function Scan` vs `Stannum Text Search Scan` vs
    `Bitmap Heap Scan`). Controller gates: lib **131/0** (61 `fields`, 14
    `bound`), workspace **14 suites**, fmt + clippy (`pg18 pg_test`,
    `-D warnings`) clean, `script/test-all quick` 7/7, headers 2/2,
    `nm -u` test binary **0** `InterruptPending`, `cargo pgrx test pg18`
    **353 passed / 0 failed / 5 ignored**, release build verified (42
    `pg_proc` functions, no `pg_test`-only UDFs). **TIN conformance exit 0**
    with `catalog.S-12` PASS; **contract census 6 FAIL / 4 GAP / 44 PASS of
    54** — the ruled `fields.boolean_duplicate_leaf` raw FAIL plus D.2's five.
    Two mutation proofs (folding the `search()` path only; and the
    surface-agreement and plan-invariance tests) both caught. The oracle's
    three adjudications for this step are recorded in the adjudication
    section; `contract/divergences/stannum.yaml` loses the broken
    error-shaped `fields.boolean_duplicate_leaf` entry, whose account is now
    the ledger.

- [ ] **D.2 Same-field phrases + spans**
  - Goal: unscoped phrase = OR of per-field bindings (never cross-field
    adjacency); unscoped NEAR/THEN must not cross fields; field-scoped phrase
    confined; positions stay per-field through the cursor (`field_hits`).
  - Scope: `postgres/src/search.rs` (unstub the multi-column phrase error);
    `postgres/src/fields/cursor.rs` / match-position plumbing;
    `tinql` / `boldi-vigna` only if a channel-aware plan is required.
    Consume `LogicalPostingCursor::field_hits`, not encoded keys.
  - Done when:
    - `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0 --case fields.phrase_no_cross --case fields.phrase_scoped --case fields.then_no_cross --case fields.near_no_cross --case fields.patterns`
      : **0 FAIL** (match 0.4.0 recordings).
    - Per the 2026-10-04 adjudication, `fields.patterns` means **every**
      expansion shape on a multi-column index — unscoped wildcard, wildcard,
      regex, fuzzy, and range — with the baseline's matching and
      expansion-limit behavior. Prefixes alone do not satisfy this case.
    - `fields.boolean_regex_on_multi` and `fields.boolean_mixed_regex_and_term`
      go **PASS** and their `contract/divergences/stannum.yaml` GAP entries are
      converted into executable assertions (rejection was a safety measure while
      incomplete, never the release contract).
    - If a recorded answer genuinely contradicts design §4's positional rule,
      stop and get a case-specific positional divergence adjudicated — do not
      concatenate positions to reproduce it and never rewrite a frozen
      recording.
  - **Controller pre-derivation (2026-10-04): all five target answers were
    independently derived from the `spans` / `patterns` corpora and design §4,
    and no recorded answer contradicts §4. D.2 targets exact match.** `spans`
    corpus: 1 `title="alpha beta"`/`other`, 2 `alpha`/`beta`, 3 `"beta alpha"`/`pad`,
    4 `"alpha xx beta"`/`pad`, 5 `null`/`"needle here"`, 6 `needle`/`null`,
    7 `pad`/`"alpha beta"`. `"alpha beta"` → `[1,7]`: 1 and 7 have the phrase
    inside one field; 2 has `alpha` in title and `beta` in body, which §4
    forbids joining; 3 has them reversed in one field (not adjacent); 4 has
    `beta` two tokens later. `title:("alpha beta")` → `1`, because 7's hit is
    in body. `alpha THEN/0 beta` → `[1,7]`, the same adjacency rule.
    `alpha NEAR/1 beta` → `[1,3,4,7]`: 3 is in one field with the pair
    reversed (NEAR is symmetric) and 4 has one token between, so NEAR/1's slop
    admits distance 2; 2 is still excluded as cross-field, which is the
    property the case exists to pin. `patterns` corpus: 1 `apple`/`other`,
    2 `apply`/`zebra`, 3 `needle`/`"alpha beta"`, 4 `zzz`/`apple`.
    `app*` → `[1,2,4]` (any-field union: row 4's `apple` is in body);
    `title:(app*)`, `title:(MATCHES app.*)`, `title:(apple~1)`,
    `title:(apple TO apply)` → `[1,2]` (title only, so row 4 drops).
    Consequence for D.2's implementation: the four field-scoped pattern shapes
    and the two boolean regex cases all require **expansion to carry the
    field mask per expanded term**, which is why D.1 must land first.
    **Lowering facts the controller verified in `tinql/src/runtime/lower.rs`**
    (so D.2 binds positions per field instead of re-deriving them): `Then(l,
    r, gap)` lowers to `MaxGaps { max_gaps: gap, inner: Ordered([l, r]) }` —
    ordered, non-overlapping, gaps ≤ n; `Near(l, r, gap)` lowers to the same
    filter over `Unordered([l, r])` — order-insensitive, which is why `beta
    alpha` matches NEAR/1 and why `alpha xx beta` (one intervening position,
    gap count 1) also matches. A phrase lowers to an `Ordered` chain with the
    gap budget pinned. All three are positional over **one field's** position
    stream, so §4's "OR of per-field bindings" means evaluating the span
    expression once per channel and unioning the documents, never
    concatenating the channels' position streams.
  - Review focus: design §4 phrases/spans;

- [ ] **D.3 Field-aware highlights + snippets**
  - Goal: 5-arg/bound forms with field; wrapper-field / no-mark / first-mark
    / first-non-NULL / NULL-skip snippet selection; `search()` multi-column
    form unstubbed for supported queries.
  - Scope: `postgres/src/highlight_udfs.rs`, `postgres/src/tool/highlight.rs`,
    `postgres/src/tool/search.rs` / `postgres/src/search.rs`. Snippet
    selection does not read the dictionary key.
  - Done when:
    - `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0 --case fields.snippets --case fields.highlight_spans`
      green (already green at C.1; must not regress); remaining
      `--area fields` 0 FAIL; multi-column `search` cases from Phase 2.4
      stubs green.
  - Review focus: design §4 highlights; parent §4.1 SQL forms.

- [ ] **D.4 Full planner coverage**
  - Goal: bitmap scope + recheck bits end-to-end; heap fallback + recheck;
    cost model; `enable_custom_scan = off` multi-column plans.
  - Scope: `postgres/src/customscan.rs`, `am.rs`, `selectivity.rs`,
    `postgres/src/score.rs` (2.2 stubs: `score_support` field,
    `clause_estimate` unscoped — close the Phase-5 checklist from STN3 2.2).
    Un-ignore title/body `#[ignore]` tests from 2.2.
  - Done when: every Appendix A fixture green on both plan modes.
    Full census accounting gate (the `0 FAIL / 2 GAP / 37 PASS` text is
    **withdrawn** — it counted a 39-case suite; see the adjudication section):
    every current case accounted, zero unapproved failures, every remaining GAP
    named / explained / owned, intentional differences executable and narrowly
    scoped. Expected end state: **0 FAIL / 2 GAP / 52 PASS** — GAP
    `catalog.functions` (already correctly documented) and `catalog.gucs`
    (E.2 must classify it); **0 FAIL / 1 GAP / 53 PASS** if E.2 closes
    `catalog.gucs` —
    `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0`.
    Both plan modes assert the observed plan **and** the result. A deferred
    acceleration ships only with a tested fallback and a divergence entry that
    says plan Z is unavailable for shape X, never that the fallback drops scope.
    `script/test-all full` green on PG18.
  - Review focus: design §4 planner widening; parent Appendix A.

- [ ] **D.5 pgembed checkpoint**
  - Goal: full L3 product path on STN4 (old 5.5 / 4.7 folded here; C.2 must
    already be green).
  - Scope: pgembed pin bump. Bundle the 3.3 backlog test edit
    (`test_stannum_jieba.py` `extversion == '0.4.0'` → `0.5.0`) **only if**
    this step is allowed a pgembed source touch; otherwise keep pin-only and
    record the stale assertion as known.
  - Done when: pin bump; **full** `pytest tests/test_pgembed_stannum.py
    tests/test_stannum_jieba.py` green on multi-column + jieba;
    langchain/llama retriever smoke. Wheel-green invariant holds.
  - Review focus: design §8 D.5; STN3 5.5 done-when.

**Phase gate PD** (normative: design §4 + parent Appendix A): phrase/span/
highlight/planner contract cases match 0.4.0; census 0 FAIL; pgembed official
suite green.

---

## Phase E — Packaging + release (old Phase 6) (≈ 3 wks)

E.1 may start after A.4 (classification is the new CI row). E.4 waits for
D.5 **and** E.3; E.5 does not ship without E.4 green. C.2 is not waived.

- [ ] **E.1 Two-artifact migration CI**
  - Goal: 0.4.0→0.5.0 still follows parent §8; leftover STN4-dev v1 indexes
    rebuild-error rather than LSG-migrate. Replay the design §6.3 fixture
    table in CI.
  - Scope: isolated 0.4.0 server builds old indexes (single/multi/jieba/
    empty/buffer-only) → byte-evidence of magic/meta → clean shutdown →
    install 0.5.0 → restart → `ALTER EXTENSION` → migration-error
    assertions → per-index `REINDEX` → answers. Plus STN4-dev fixtures
    listed in design §8 E.1 (v1 rebuild-errors `StaleFielded` not LSG4;
    v1+v2 → `MixedFielded`; malformed mixes → `Corrupt`; tagged `~0~foo`
    after restart → `Current`; untagged legacy buffer → `StaleFielded`;
    kind-5+LSG → `Corrupt` not the §8 string; kind-1+LSG → `PreStn3`;
    untagged zero-term restart+INSERT → `StaleFielded`/`MixedFielded`, no
    write; tagged zero-term INSERT/flush/restart → `Current`).
  - Done when: the `two-artifact-migration` job (`.github/workflows/ci.yml`,
    `workflow_dispatch` + scheduled) is green in CI, and the job's log shows
    machine-checkable assertions for every design §6.3 fixture row:
    `PreStn3`, `StaleFielded`, `MixedFielded`, `Corrupt` (incl. kind-5+LSG);
    exact rebuild/migration strings; no-page-dirty on rejected writes;
    tagged/untagged zero-term behavior; kind-1 vs kind-5 precedence. The
    job must fail if any classification or string mismatches.
  - Review focus: design §6.1, §6.3 fixture table, §8 E.1; parent §8
    procedure steps 1–6.

- [ ] **E.2 Runbook + divergence ledger**
  - Goal: operators can cut over; development fielded-terms indexes are a
    documented REINDEX, not a supported upgrade.
  - Scope: operator doc (downtime window, replicas rebuilt not replayed,
    rollback = restore, no downgrade); REINDEX runbook including **STN4
    rebuild of any development fielded-terms indexes**;
    `contract/divergences/stannum.yaml` final review;
    `docs/compatibility.md`.
  - Done when: docs committed; divergence ledger matches the C.1/D.4 census
    (2 GAP remaining unless closed).
  - Review focus: design §6.1, §6.3 last paragraph (StaleFielded after
    0.5.0 ships is stn3-dev only).

- [ ] **E.3 Wheel alignment**
  - Goal: pgembed 0.5.0-aligned wheel (PG18, `BUILT_FOR_POSTGRES_MAJOR`
    audit), standalone binaries, release evidence. Close 2.8/3.3 backlog:
    `build-metadata.json` stannum version/source_commit hardcodes if this
    step touches pgembed source.
  - Scope: pgembed release process; pin = E-series SHA.
  - Done when: wheel installs the 0.5.0 extension and the full pgembed suite
    is green against it (joins E.4).
  - Review focus: STN3 6.3 done-when; design §8 E.3.

- [ ] **E.4 Conformance + parity sweep — GATE**
  - Goal: three suites green on the STN4 0.5.0 artifact.
  - Scope: upstream TIN conformance (`conformance/run.py --engine stannum
    --check conformance/expected/tin-1.0.3`); full contract suite; pgembed
    suite; **[pg-agent]** v13 suite replay.
  - Done when: contract census 0 FAIL (GAPs only if still documented);
    conformance green (I-01 divergence stays documented); pgembed full
    suite green; pg-agent optional-but-recommended — if unavailable, record
    it and ask the user to run it. D.5 and E.3 both join this gate.
  - Review focus: parent §4.3 — any recorded-answer change without a
    contract bump is a bug; design §6.4 (`capabilities().engine.format`
    remains `"STN3"`; `contract_version` stays 1).

- [ ] **E.5 Release 0.5.0**
  - Goal: ship STN4 as the 0.5.0 representation inside STN3 terms.
  - Scope: `stannum--0.5.0.sql` final (catalog-only; no layout SQL);
    CHANGELOG; tag; `stn3` merged per parent §9 strategy; `main` →
    maintenance-only note.
  - Done when: tag exists; origin `stn3` and release artifacts pushed;
    tracker closed. E.4 green is required.
  - Review focus: parent §9; design §6.4.

**Phase gate PE** (normative: design §6 + parent §8 / §9): two-artifact
migration CI green including §6.3 fixtures; wheel + conformance + contract
green; 0.5.0 tagged.

---

## Dependency graph

Design §8 graph, with the added steps. **A.3 in the design is this plan's
A.3 + A.4 + A.4.1** (STNF v2, then classification, then validator
isolation). **A.3→C.1 and A.3→E.1 in the design are A.4→C.1 and A.4→E.1
here.**

```
A.1 ── A.2 ── A.2.1 ── A.3 ── A.4 ── A.4.1 ── A.5
              │                         │
              └─ B.1 ── B.2 ── B.3      │   (B.1 from A.2, NOT from A.2.1)
                                        │
              A.4 ──────────────────────┴── C.1(GATE) ◄── B.2
                                              │
                                              C.2(GATE) ── C.3
                                              │            │ may overlap D.*
                                              C.4          │
                                              │            │
              A.4 ── E.1 ── E.2 ── E.3 ───────┼── E.4(GATE) ── E.5
                                              │         ▲
                        (C.2 green required)  D.1 ── D.2 ── D.3 ── D.4 ── D.5
                                                                    │
                                                                    └── joins E.4
```

Edges from design §8 text (all preserved):

- **A.3→C.1** (here A.4→C.1): leftover v1 indexes must classify before SQL
  smoke builds new v2 indexes on the same relation names.
- **B.2→C.1**: bound witness.
- **A.3→E.1** (here A.4→E.1): classification is the new CI row; E.1 may
  start after A.4, before C.2.
- **C.2 gates the D wheel:** D.\* do not start on a red C.2; D.5 and C.4
  require C.2 green. D/E do not waive C.2.
- **D.\* may overlap C.3.**
- **D.5 and E.3 both join E.4;** E.5 does not ship without E.4 green.
- **B.1 starts after A.2**, parallel with A.3–A.4.

Added sequencing not numbered in design §8: A.2.1 after A.2 (fixtures need
the writer); A.3 before A.4 (ValidV2 needs the v2 codec); A.4.1 after A.4
(validator isolation once the untagged arm exists); phase-end pgembed
A.5 / B.3 / C.4 / D.5.

---

## Estimate (from design §8)

Remaining critical path ≈ 8–10 engineer-weeks; elapsed ≈ 6–8 if Phase E
packaging overlaps Phase D. Representation is smaller than a green-field
LSG4 port because score/bound/envelope/fence already exist, but it is still
a format cut. Parent §11's 5–7 week STN3 figure does **not** include this
work.

| Phase | Steps | Weeks |
|-------|-------|------:|
| A Representation | A.1–A.5 (7) | ~2.5 |
| B Query wiring | B.1–B.3 (3) | ~1.5 |
| C Gates | C.1–C.4 (4) | ~1.5 |
| D Phrases / planner | D.1–D.5 (5) | ~2 |
| E Packaging / release | E.1–E.5 (5) | ~3 (off critical path if overlapped) |

---

## Build-measurement protocol `C2-build-v2` (supersedes the C.2 single shot)

Effective 2026-10-04, by oracle adjudication. The C.2 entry's "one `CREATE INDEX`
wall clock per system" is retained as the **legacy single-shot protocol**; its
numbers stay in the record as observations, but a single ~1.3 s wall clock on an
8-core shared Mac mini cannot resolve a ~2.8% relative difference, and the English
build ratio has now been measured at 1.7350x (pass), 1.8689x and 1.9144x (fail),
with paired medians of 1.8451x and 1.8509x — centred above the 1.8x ceiling.
Acceptance for the build gate therefore uses this protocol.

| Item | Requirement |
|---|---|
| Engine | PostgreSQL 18.4, pinned prefix and extension build; record the build identity |
| Workloads | The existing locked English and Chinese corpora; hashes, indexing semantics and the 1.8x threshold unchanged |
| Systems | `stn4_multi` and `stn3_single` from the same candidate revision and build configuration |
| Samples | 64 paired ratios per corpus (64 measured builds of each system) |
| Counterbalancing | 32 four-build blocks, each either `M S S M` or `S M M S`, so every adjacent pair holds one M and one S |
| Scheduling | 8 batches of 4 blocks per corpus, each batch holding two blocks of each orientation, shuffled with a recorded seed; batches spread across time windows |
| Preparation | Fresh table load per build, fixed row order, identical checkpoint/cache policy for both arms; all preparation outside the timer |
| Cache claim | Freshly-loaded-table builds; **not** OS-cold builds |
| Warm-up | One untimed build of each system at the start of each batch, order counterbalanced |
| Timed interval | Wall clock immediately around the complete `CREATE INDEX` statement to success |
| Settings | Record effective PostgreSQL settings, not just `"defaults"`; identical relevant settings for both arms |
| Instrumentation | Build tracing and `STANNUM_FIELDED_PROFILE` disabled during acceptance measurements |
| Exclusions | No outlier trimming, no load-based selective exclusion, no picking the best batch |
| Failure | Correctness failures cannot produce a PASS; infrastructure-aborted campaigns are recorded, never silently replaced |

Statistic: `R = median(r_1..r_64)` over adjacent paired ratios `r_i = T_M,i / T_S,i`.
Report both systems' raw times as well, so an improvement that came from slowing
the denominator stays visible.

Uncertainty: a **95% batch-cluster bootstrap interval** for `R` — 10,000 resamples,
fixed recorded seed, resampling whole batches with paired observations intact —
because shared background-load episodes make individual timings non-independent.

- **PASS**: upper bound ≤ 1.8
- **FAIL**: lower bound > 1.8
- **INCONCLUSIVE**: interval overlaps 1.8 — authorizes neither a PASS nor Phase D

Target margin on this machine: **≤ 1.70x**, preferably 1.65x — not a measured
1.799x victory. Aim for a confidence half-width ≤ 0.02 ratio units.

Historical rows are relabeled by protocol, never invalidated: the original
single-shots remain valid legacy observations, the 12-pair profiles remain
diagnostic repeated-build evidence, and `C2-build-v2` runs are the prospective
acceptance evidence.

---

## Ranked-p50 measurement protocol `C2-p50-v2` (extends `C2-build-v2`'s discipline)

Effective 2026-10-04. The C.2 advisory p50 gate is a ~2% quantity on a shared
8-core host, and a single 20-run window has already produced Chinese aggregate
readings of 2.363x, 1.226x, 0.865x and 2.428x for the same code, with the
multi arm swinging while the single arm stayed flat. The ranked-p50 gate
therefore uses the same paired-repeated discipline as the build gate.

| Item | Requirement |
|---|---|
| Engine | PostgreSQL 18.4, pinned prefix and extension build |
| Systems | `stn4_multi` and `stn3_single`, same revision and build configuration, same table, loaded once and indexed by both |
| Query set | The full locked list for the corpus; never a subset |
| Repeats | **20 rounds** of the whole query list per system |
| Counterbalancing | Alternate whole-list rounds between the two systems (`M S S M` …), so a load episode hits both arms |
| Scheduling | Batches spread across time windows; record loadavg (1/5/15 min) at the start and end of every round |
| Warm-up | One full untimed round per system at the start of each batch |
| Timed interval | Wall clock of each `SELECT` statement, one query at a time, concurrency 1, default GUCs |
| Instrumentation | `STANNUM_FIELDED_PROFILE` and `STANNUM_BUILD_PROFILE` unset |
| Exclusions | No outlier trimming, no load-based selective exclusion, no picking the best round |

Statistic: per query, the median of that query's 20 multi samples divided by the
median of its 20 single samples; the aggregate is the mean of per-query medians
divided by the mean of single medians (the same shape the 4.6 protocol used).

Uncertainty: a **95% round-cluster bootstrap interval** on the aggregate,
10,000 resamples, fixed recorded seed, resampling whole rounds with their
per-query pairs intact.

- **PASS**: upper bound ≤ 1.3
- **FAIL**: lower bound > 1.3
- **INCONCLUSIVE**: interval overlaps 1.3 — authorizes neither a pass nor Phase D

Report per-query medians for both arms as well, so an improvement that came
from slowing the denominator stays visible. `C2-build-v2` remains the rule for
the build gate; this section only governs ranked p50.

---

## Census ledger
| after step | SHA | FAIL | GAP | PASS | notes |
|------------|-----|-----:|----:|-----:|-------|
| 4.5 baseline | d5fa6f9 | 5 | 2 | 32 | phrase/span/patterns FAIL; catalog.gucs + catalog.functions GAP |
| A.5 | 6e797fb | 11 | | 26 | STN4 writer; multi-col query reds expected until B.1 |
| B.1 | 0555857 | 6 | 1 | 32 | arithmetic+expansion recovered; phrase/span/patterns still `search()` unsupported |
| B.3 | | | | | informational |
| C.1 | 547f080 | 5 | 2 | 32 | five §5.2 rows bit-exact vs 0.4.0; FAIL = phrase/span/patterns (Phase D); GAP = catalog.gucs + catalog.functions |
| C.2-R (PG17.11 control) | 8ba6683 | 7 | 1 | 31 | **Same before and after the C.2-R repair** (control experiment: identical FAIL ids on both builds). Extra vs the C.1 row: `calls.stop_words` (`builtin_stop_words('auto')` = zh+en since 0b37be0, an ancestor of the C.1 commit) and `catalog.functions` (documented GAP whose `expected.extra` no longer matches). Both pre-date C.2-R; the C.1 row was measured on a PG18.4 engine and does not reproduce on PG17.11. Unresolved. |
| Boolean coverage (PG17.11, controller re-run) | 7e6438f | 7 | 4 | 43 | New `contract/cases/boolean.yaml` (15 cases; truth-table corpus derivations independently re-derived by the controller and matched) + `contract/expected/stannum-0.4.0/boolean.json` recorded from the 0.4.0 build at `/tmp/stannum-main-46` @ `7ab511b` (answers verified against the hand-derived table; existing recordings byte-identical). Contributions: 1 new FAIL = `fields.boolean_duplicate_leaf` `flat_ranked` only — pre-existing single-column IndexScorer duplicate-clause 2x vs the 0.4.0 fold, documented in `contract/divergences/stannum.yaml`, not introduced here; 2 new GAPs = `fields.boolean_regex_on_multi` / `fields.boolean_mixed_regex_and_term` (0.5.0 rejects instead of silently dropping; D.2 owns). The other 12 boolean cases PASS bit-exact vs 0.4.0. `fields.boolean_mixed_regex_and_term` FAILed silently on the pre-repair build (5 rows from the dropped prefix) — this is the hole the coverage closes. Controller re-run differs from the implementer's `6 FAIL / 4 GAP / 44 PASS` only by `calls.stop_words`, the pre-existing PG17 environment difference. Caveat: `boolean.json`'s `source.extension_commit` is the suite repo's postgres HEAD, not the producer commit. |
| C.3 | 7fdee28 | 6 | 4 | 44 | **Measured on PG18.4 against the release build** (see the two census-measurement rules above) — no case-id delta from the boolean-coverage row's PG18 equivalent, as expected of a pure deletion. FAIL ids: `fields.phrase_no_cross`, `fields.phrase_scoped`, `fields.then_no_cross`, `fields.near_no_cross`, `fields.patterns` (Phase D.2), plus `fields.boolean_duplicate_leaf` — which the 2026-10-04 adjudication reassigns to **D.1** (restore the fold; the controller proved the cause against 0.4.0: `bool_multi` is bit-exact while `bool_flat` scores ≈1.97×). GAP ids: `catalog.functions` (**correct as written** — live 42 vs recorded 41, lost = 0, the only gain `capabilities()`; no divergence edit needed), `catalog.gucs`, `fields.boolean_regex_on_multi`, `fields.boolean_mixed_regex_and_term` (D.2 converts both to assertions). Controller gates on the C.3 tree: `cargo test -p stannum --lib` 131/0 (61 in `fields`, 14 in `bound`), workspace 14 suites, `cargo pgrx test pg18` **347 passed / 0 failed / 5 ignored** (145.68s), `script/test-all quick` 7/7, headers 2/2, fmt + clippy (`pg18 pg_test`, `-D warnings`) clean, `nm -u`: plain test binary **0** `InterruptPending`, release `.so` **1**. Oracle reviewed twice in the controller's session: 1 P0 + 3 P1 (empty-token error category; shared candidate oracle; `.max(exact)` bound floor; no interval-skip coverage), all repaired with mutation proofs; final verdict **no P0**, residual P1 (no production boolean-WAND orchestration loop to pin, because it does not exist yet) assigned to **D.4**. |
| D.4 | | 0 | 2 | 37 | **required** — Appendix A complete |
| E.4 | | 0 | ≤2 | | release gate |

---

## pgembed pin ledger

| checkpoint | pgembed SHA | `STANNUM_COMMIT` | tests | notes |
|------------|-------------|------------------|-------|-------|
| STN3 3.3 | 68386ec | a963080d… | 3F/71P/2S official; jieba 7/8 (stale extversion) | current pin |
| A.5 | | (A.4 SHA) | single-col + jieba green; multi-col reds expected | pin-only |
| B.3 | | (B.2 SHA) | same keep-green | pin-only |
| C.4 | | (C.2 SHA) | arithmetic multi-col smoke; phrases may still error | **C.2 gated** |
| D.5 | | (D.4 SHA) | full official + jieba + retrievers | **C.2 gated** |
| E.3 | | (E-series) | release wheel | joins E.4 |

---

## Upstream re-pins

| date | from → to | reason | contract suite |
|------|-----------|--------|----------------|
| 2026-09-30 | — → `d57ef58` | STN3 initial base | n/a then; still the pin |

Monthly dry-run job remains enabled (STN3 P2, `bfdf466`). A real re-pin is a
separate decision (parent §9 pinned-snapshot policy).
