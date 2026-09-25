# TIN index options and highlighting compatibility

Audited 2026-09-18 against Stannum `45d2696` plus this change. The live
[PlanetScale index reference](https://planetscale.com/docs/postgres/search/reference/indexes)
was retrieved on that date; the supplied `/docs/postgres/tin` URL is not the
current reference. Historical observations dated 2026-09-17 were recovered from
`33e4ea8^:docs/archive/tin-configuration-research.md`,
`33e4ea8^:docs/archive/tin-observed-shape.md`, and
`33e4ea8^:docs/archive/tin-research.md` (deleted from the current tree).
The historical live observations used TIN 1.0.2 on PostgreSQL 18.6. Current web
pages do not identify a TIN release, so this is an audit of the documented
surface, not a claim to enumerate undocumented options.

## Complete documented index-option surface

All 15 options on the public index reference are listed below. Stannum's
registration is in [`options.rs`](../postgres/src/options.rs); tokenization
is represented by [`TokenizerPipelineSpec`](../tokenizer/src/spec.rs).
An equal option surface does not establish identical behavior on every Unicode
version or script; the exact Stannum behavior and remaining uncertainty follow.

| Option | TIN accepted values; default | Meaning | Stannum values; default | Status / decision |
| --- | --- | --- | --- | --- |
| `tokenizer` | `unicode`, `whitespace`; `unicode` | Word boundaries or whitespace fields | Same, plus Stannum-specific `jieba` | Matches documented modes; test punctuation, URLs, numerics, apostrophes, mixed scripts. `jieba` is a Stannum addition beyond the TIN surface: dictionary word segmentation for Chinese via the embedded jieba dictionary (see below) |
| `case_folding` | `fold`, `preserve`; `fold` | Case-insensitive or original-case terms | Same | Surface matches; Stannum lowercases Unicode scalars, not full Unicode case folding (`ß` stays `ß`); TIN docs do not specify an algorithm |
| `accent_folding` | `fold`, `preserve`; `fold` | Remove or retain accents | Same | Surface matches; Stannum uses canonical decomposition, removes combining marks, recomposes; TIN algorithm unspecified |
| `long_tokens` | `split`, `truncate`, `discard`; `split` | Handle terms beyond byte ceiling after folding | Same | Matches documented modes; chunks prefer grapheme boundaries, oversized clusters fall back to bounded scalar chunks |
| `max_token_bytes` | integer `4..2692`; `256` | UTF-8 analyzed-term byte ceiling | Same | Matches; tested with accented source text that shrinks after folding |
| `graphemes` | `emoji`, `retain`, `discard`; `emoji` | Standalone emoji/symbol clusters | Same | Matches documented modes; tests ZWJ emoji, flags, symbols |
| `position_gaps` | `preserve`, `collapse`; `preserve` | Positions consumed by removed analyzed tokens | Same | Matches documented modes; discarded long tokens leave gaps only in preserve mode; base-tokenizer discarded punctuation/graphemes do not consume positions |
| `k1` | real `0..10000`; `1.2` | BM25 saturation | Same | Matches; query-time setting, no rebuild |
| `b` | real `0..1`; `0.75` | BM25 length normalization | Same | Matches; query-time setting, no rebuild |
| `score_stop_words` | comma-separated text; unset | Exact analyzed terms omitted from default scoring | Same, plus `auto`, `auto:zh`, `auto:en` | Explicit entries remain literal; presets are analyzed and single-token filtered; neither removes indexed tokens or changes matching; full scoring ignores this list |
| `initial_segment_count` | integer `1..4096`; available parallelism | Build partitions; default target count | `1..4096`; stored default `1` | Accepted **and ignored, with warning**; expanded previous `1..1024` domain |
| `target_segment_count` | integer `1..4096`; initial count | Background maintenance target | Same range; inert stored default `1` | Newly accepted **and ignored, with warning** |
| `max_mutable_segment_size` | integer `>=131072`; `4194304` bytes | Promotion trigger; also 16,384 docs | Signed 32-bit integer `131072..2147483647`; `4194304` | Newly accepted **and ignored, with warning**; public docs give no explicit upper bound beyond `int` |
| `max_merged_segment_size` | integer `>=100`; `2000` MB | Segment merge-size ceiling | Signed 32-bit integer `100..2147483647`; `2000` | Newly accepted **and ignored, with warning**; public docs give no explicit upper bound beyond `int` |
| `dead_percent_threshold` | real `0..1`; `0.5` | Dead-entry rewrite threshold | Same | Newly accepted **and ignored, with warning** |

The five storage options are accepted for portable DDL, with a warning whenever
explicitly validated by CREATE/ALTER INDEX. Stannum still uses its own documented
[storage and maintenance settings](architecture/segmented-storage.md). In
particular, setting a compatibility option does not enforce a memory limit,
create workers, or change the rewrite threshold. Default values in the catalog
are inert; no background maintenance is introduced. The new fields are only
PostgreSQL reloptions data, not serialized segment or meta-page fields. There
is no new SQL function, upgrade script, or on-disk format change.

No documented tokenizer behavior is missing from the pipeline. The reference
lists neither a stemming option nor a language selector nor indexing-time stop
words. This change adds none: inventing a Snowball or other stemmer would change
matching without a TIN contract. `score_stop_words` remains scoring-only. The
[settings reference](https://planetscale.com/docs/postgres/search/reference/settings)
contains server/session GUCs, not additional `WITH` index options; they are
outside this table and are not accepted as reloptions.

## Stannum-specific `jieba` tokenizer

`tokenizer = 'jieba'` is a Stannum extension with no TIN counterpart, for
corpora mixing Chinese and English. It segments text with the jieba
dictionary segmenter (maximum-probability path over the embedded dictionary,
HMM fallback for out-of-dictionary Han runs), so `开源数据库` analyzes as the
two terms `开源` and `数据库` instead of four per-character tokens, and a
query term that spans several words rewrites to an adjacent phrase exactly as
the `unicode` tokenizer's multi-token terms do. Non-Han runs analyze as whole
jieba segments, which differs from UAX#29 for punctuation-bridged ASCII
(`can't` splits into `can` and `t`); both sides of every query use the same
pipeline, so matching remains consistent. The `graphemes` option has no
effect under `jieba` (the segmenter emits no standalone emoji/symbol tokens).
Determinism follows from the pinned jieba-rs version in `Cargo.lock`; a
version bump can change segmentation for some inputs, so reindex after
upgrading it.

Tokenization changes require REINDEX for stored rows, as the public reference
states. Stannum binds matching and highlighting to the index's persisted
pipeline, including sequential/bitmap rechecks. This is more permissive than
the archived TIN 1.0.2 observation that non-default tokenization could reject a
query lacking a usable custom scan. The unit tests pin all folding, boundary,
grapheme, long-token and gap modes with concrete edge inputs; pg_tests create
indexes with each mode and compare plans, matches and highlighting. These
checks establish Stannum's contract and Lead compatibility, not exhaustive
binary equivalence to hosted TIN.

## Reference highlighting oracle

`script/reference-oracle` records the actual Lead revision and `\df
tin.*highlight*` output alongside its report. Lead exposes
`tin.highlight(text, begin_tag text DEFAULT '<b>', end_tag text DEFAULT '</b>',
query text DEFAULT NULL)` and `tin.highlight_ansi(text, wrap_to integer DEFAULT
NULL, query text DEFAULT NULL)`; Stannum has the equivalent schema-qualified
functions plus its internal bound-query overloads.

Each query observes the one-argument HTML and ANSI functions for every matching
document, ordered by ID, exercising implicit query binding. Rendered strings,
including escape codes, are compared exactly in both score-bit and rank-order
modes. Highlight errors are retained independently of matching/scoring evidence.
Score exclusions for Lead expansions do not exclude their highlights. A
separate `REFERENCE_UNHIGHLIGHTED` reason map controls only proven reference
highlight defects in order mode; raw observations and exclusions are always
recorded in `oracle.json`.

The fixture now contains numeric decimals, apostrophes, hyphens, URL hosts,
accented text, and ZWJ emoji. Six new ordered shapes bring the suite to 47
queries across five mutation states (235 query/state pairs).

Local result: **235/235 agree, zero differences**, against Lead revision
`0e29dbe5177bb64d027d6afeaa20eb0b46536be6` (extension `tin` 1.0.3),
PostgreSQL 18, 5,000 generated fixture rows plus the edge rows and later inserts.
HTML and ANSI observations contain no errors. **Highlight exclusion list:
empty.** No Stannum renderer bug was found, and no renderer change was needed.
The six pre-existing score-order exclusions remain restricted to scoring.

The initial harness attempted an aggregate over a flattenable subquery. Both
engines lost implicit query binding in that shape. The final harness uses a
direct ordered projection, `SELECT json_build_array(id, engine.highlight(body))
FROM oracle_docs WHERE body ==> query ORDER BY id`, and parses each JSON row.
This exercises the supported implicit-binding form without suppressing any
query shape or using explicit-query arguments to bypass binding.

Validation: PG18 suite 76 tests passed; all non-extension workspace tests
passed, including four new tokenizer audit tests; PG18 and PG17 clippy with
pg_test and warnings denied passed; 44 Python benchmark tests passed. Full
reference report and catalog evidence were saved locally under
`/tmp/stannum-ab7-oracle-final/`; the large raw report is not checked in.


## Dictionary governance (0.2.0)

`stannum.jieba_words` is the database-local custom dictionary. Use
`stannum.jieba_add_word(word, freq DEFAULT 0, tag DEFAULT NULL)` and
`stannum.jieba_delete_word(word)` rather than direct table DML. Add upserts;
delete is idempotent. Words must be nonempty, at most 256 UTF-8 bytes, and
contain no Unicode whitespace. Frequencies must be nonnegative; zero lets
jieba choose its default frequency. Only superusers and members of
`pg_database_owner` may mutate or force-reload the dictionary. The functions
are invoker-run, with an explicit Rust authorization check; the private,
OID-resolved table is accessed internally under its owner's identity with
error-safe restoration. No direct DML privileges are granted to callers.

Committed changes invalidate other backends' caches. The next jieba use
refreshes from that transaction's visible rows. Aborting a transaction or
savepoint discards its uncommitted dictionary on next use. Reload failure or
cancellation propagates an ERROR, keeps the previous dictionary installed,
and leaves refresh pending. Compiled pipelines retain their dictionary
snapshot for their lifetime. `jieba_reload_dict()` always reinstalls the
embedded dictionary plus custom words and increments the backend generation,
even when `jieba_dict_version()` is unchanged. Unchanged content reuses the
fingerprint-keyed pipeline cache; stale identities are evicted.

`jieba_dict_version()` is a deterministic SipHash-1-3 content fingerprint,
returned as a bit-preserving signed bigint. For a hexadecimal display, use
`SELECT lpad(to_hex(stannum.jieba_dict_version()), 16, '0')`. Zero is reserved
for the not-yet-loaded embedded-only identity; even the empty table has a
nonzero content fingerprint (`6855a0736155f3dd` for the frozen v1 empty table). Row ordering is raw UTF-8, not database collation.

CREATE INDEX and REINDEX stamp jieba indexes with the runtime jieba version
and dictionary fingerprint. `stannum.index_analysis(index)` reports recorded
and runtime identities, a nullable `matches`, and a status. Non-jieba indexes
have NULL identities and matches, with status `not applicable`. Drift emits
one WARNING per index per statement with REINDEX advice (planner-only selectivity reads are silent). Setting
`stannum.strict_analysis = on` upgrades **stamped-but-drifted** indexes to
ERROR; unstamped legacy jieba indexes continue to WARNING and remain usable.
Inserts use the current runtime dictionary without changing an existing
stamp; REINDEX is required to make the entire index consistent again.

On a hot standby the dictionary is read from WAL-visible table rows, not
silently replaced with the embedded dictionary. Drift WARNINGs repeat on
each query. REINDEX cannot run there until promotion (or rebuild on the
primary and replay it). The v1 custom scan declines parallel-worker execution
for jieba indexes with nonempty custom dictionaries (including competing
bitmap/heap paths for the same relation); dictionary snapshot
transfer through DSM is not implemented. Non-jieba indexes and empty custom
dictionaries retain the existing parallel policy. Empty-dictionary workers use
the defined empty-table fingerprint, and cached custom-scan plans depend on
the dictionary relation so mutations re-evaluate worker eligibility.

`score_stop_words` accepts case-insensitive selectors `auto`, `auto:zh`, and
`auto:en`, mixed with literal CSV entries. Bare `auto` means Chinese plus
English on jieba, English otherwise. Only preset words producing exactly
one index-analyzer token are kept; explicit CSV entries are not analyzed.
The frozen, repository-authored source lists are exposed through
`stannum.builtin_stop_words('zh'|'en'|'auto')`. `score()` honors the reloption;
`full_score()` always ignores it. `score_inspect` uses the persisted analyzer
for segmented indexes and a fresh reloption pipeline for its diagnostic
non-segmented fallback (no tokenizer-cache lookup).

### Upgrade and rollback limits

Upgrade with `ALTER EXTENSION stannum UPDATE TO '0.2.0'`. No existing index
is automatically stamped. Inserts, folds, merges and VACUUM preserve the
original optional stamp. Indexes never rebuilt retain their old-readable
meta layout. Indexes created or reindexed with a jieba stamp on 0.2.0 cannot
be opened by 0.1.0, whose decoder rejects the trailer. There is no on-disk
stamp downgrade or reverse SQL upgrade script: restoring the old binary
alone is not a rollback for rebuilt indexes; restore an appropriate backup
or rebuild under the old binary. Page-special version remains 2 and unknown
meta trailer tags fail closed. `tokenize` and `ql_parse` are now STABLE and
PARALLEL UNSAFE because jieba analysis can consult the mutable dictionary.

## Multi-column field indexes (0.4.0)

Stannum 0.4.0 accepts more than one key column and adds the
Stannum-specific `field_weights` reloption — a sixteenth option with no TIN
counterpart, so it appears in no row of the surface table above. TIN and
every documented TIN option remain single-column; a multi-column index is a
Stannum extension.

`field_weights = 'title:3.0,body:1.0'` names a permutation of the index's
key columns with finite positive weights (omitted columns default to `1.0`).
Setting it on a single-column index is an error, `ALTER INDEX … SET
(field_weights = …)` fails closed until a REINDEX, and a column rename
requires a REINDEX because the recorded names no longer match the relation.
Multi-column indexes reject expression keys and INCLUDE columns.

Scoring is BM25F over the recorded weights; the query language gains
[field groups](query-language/fields.md) (`title:(beer OR ale)`, quoted
identifiers for other spellings) with Lucene's same-field rule for
positional queries, and the `==>` operator scopes its clause to its left
operand's column. `stannum.search()` works on multi-column indexes; its
snippet renders one field per row. `stannum.highlight` gains a field-aware
five-argument overload (no defaults on the trailing arguments, so existing
one-to-four-argument calls are unaffected and unambiguous).

### Format and rollback limits (0.4.0)

A multi-column index writes `LSG4` segments (see
[segmented storage](architecture/segmented-storage.md)); single-column
indexes keep writing `LSG3` indefinitely, and `LSG3` indexes — including
any built by 0.3.0 — read unchanged after the upgrade. An `LSG4` index does
not open on a pre-0.4.0 binary: the segment magic and the meta fields
trailer both fail closed. There is no on-disk downgrade; restore a backup or
REINDEX under the old binary after dropping the extension version. Upgrade
with `ALTER EXTENSION stannum UPDATE TO '0.4.0'`; the upgrade path adds the
two `stannum.highlight` overloads and nothing else.
