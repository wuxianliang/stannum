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
- **Gates are tests, not opinions.** Every phase ends in a suite that runs in
  CI. A step whose gate is red after two fix rounds escalates to the human
  with the failing evidence — it does not get re-rolled until green by luck.
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

- [ ] **C.2 Latency — GATE**
  - Goal: dictionary bytes (no df sidecar) and build ≤ 1.8× `stn3_single` on
    **each** corpus; ranked p50 ≤ 1.3× or a named waiver **in the results
    file**.
  - Scope: parent §5.2 protocol (PG17, concurrency 1, 20 runs, nearest-rank
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

- [ ] **C.3 Dead codec** — scope includes: delete `fields/codec.rs`, encoded-key
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
bits match 0.4.0 on STN4 indexes; census is again 5 FAIL / 2 GAP / 32 PASS;
mandatory 1.8× holds on both corpora; p50 holds or carries a named waiver in
the JSON. D.\* and the D.5 wheel do not waive this gate.

---

## Phase D — Phrases, highlights, planner (old Phase 5) (≈ 2 wks)

Starts only if C.2 is green. Done-when texts of STN3 5.1–5.5 apply, with
“fielded key” read as “channel” (design §8). Positions come from payload
channels (design §4).

- [ ] **D.1 Field scope in tinql + operator**
  - Goal: `title:(…)` and `==>` field scope apply a mask / `scope_scan_query`,
    not a dictionary-key fence.
  - Scope: `tinql/src/` (`Expr::Field` grammar/AST — parse already exists
    from STN3 57c86f3); `postgres/src/score.rs` (`project_to_field`,
    `scope_scan_query`); `postgres/src/operator.rs`
    (`check_clause_field_scope` error texts); single-column “field syntax
    requires a multi-column index” rule.
  - Done when: Appendix A operator cases green —
    `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0 --area fields`
    no longer FAILs on scope plumbing (phrase/span may still FAIL until
    D.2). `script/pgrx-lock.py -- cargo pgrx test pg18 -p stannum` green.
  - Review focus: design §4 field-scoped queries; parent Appendix A.

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
  - Review focus: design §4 phrases/spans.

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
    Full census: **0 FAIL / 2 GAP / 37 PASS** (the five former FAILs are
    PASS; GAPs unchanged) —
    `python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0`
    exits 0. `script/test-all full` green on PG18.
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

## Census ledger

| after step | SHA | FAIL | GAP | PASS | notes |
|------------|-----|-----:|----:|-----:|-------|
| 4.5 baseline | d5fa6f9 | 5 | 2 | 32 | phrase/span/patterns FAIL; catalog.gucs + catalog.functions GAP |
| A.5 | 6e797fb | 11 | | 26 | STN4 writer; multi-col query reds expected until B.1 |
| B.1 | 0555857 | 6 | 1 | 32 | arithmetic+expansion recovered; phrase/span/patterns still `search()` unsupported |
| B.3 | | | | | informational |
| C.1 | 547f080 | 5 | 2 | 32 | five §5.2 rows bit-exact vs 0.4.0; FAIL = phrase/span/patterns (Phase D); GAP = catalog.gucs + catalog.functions |
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
