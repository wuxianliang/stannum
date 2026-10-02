# STN4: Per-Field Payload in One Term Entry

Status: proposal, oracle-repair · Basis: `stn3@1e1b20b` after the 4.6 mandatory miss · Normative
parent: `docs/designs/stn3-tool-layer-2026-09-29.md` (formula, SQL surface,
migration fence) · 2026-10-02

STN4 is the representation re-plan the 4.6 gate required. The fused BM25F
formula in parent §5.1 does not move. The fielded-terms dictionary — one
`Index::term` key per `(field, token)` plus a `df_agg` sidecar — is withdrawn.
Each surface token is one `TermEntry` again. Aggregate df lives on that entry.
Per-field tf, positions, and block bounds ride inside the entry's two STN3
extents as **stock posting channels**, not as extra dictionary keys.

This returns to the dictionary decision lsg4-rfc §5.5 stated (one `TermEntry`
per term text, aggregate df on the entry, no per-field df) and keeps STN3's
posting machinery (ordinal streams, block bounds, chunk folds, dead-doc
bitmaps). The RFC file is not in this tree; parent §5.1, §5.2, §5.10, and
§5.11 are the citations this document treats as the RFC's content.

## Executive summary

**Why.** Parent §5.2's semantic smoke is green at bit level (`3e113925` tie,
weighted order, single-column R-BIT). The 4.6 latency gate is red:
dictionary **3.34×**, build **1.95×** against the 1.8× mandatory ceilings;
advisory p50 **5.61×** against 1.3×, no waiver. Evidence:
`docs/benchmarks/stn3-fielded-poc.json` on the 20,000-row corpus (seed
`20260930`, sha256 `52775c8a17077f09f028f5758eb53df65f94dc37c5a1f3e800990f5a39f0bbeb`,
columns `id, title, body`). The formula is not the miss. Multiplying dictionary
entries by the field count, then storing the union df a second time in STNF, is.

**What changes.** Multi-column dictionary keys are the analyzed token text,
identical to single-column STN3. `Index::term(&str)` is called once per logical
token. The entry's `df` is the per-segment union cardinality. The entry's
`ordinals` and `payload` extents begin with a channel directory and then F
**unmodified** STN3 ordinal/payload streams, one per field that posted the
token. Query-time mask selects channels; it is not encoded in the key. The
STNF `df_agg` section disappears (version 2, norms only). Fielded-terms codec,
per-field dictionary lookup, and the df sidecar reader become dead code.

**What stays.** Parent §5.1 tf*/len*/avgdl*/idf/score/cast-order. Parent §4.1
SQL surface. Kind-5 STNM envelope (names, weights, analysis stamp). The 0.4.0
→ 0.5.0 catalog-only upgrade. `REINDEX` still rebuilds from the heap.
`fields/score.rs` and `fields/bound.rs` arithmetic. Expansion grouping
semantics (one logical token, scope mask, `max_expansion` counted only after
channel-set ∩ mask is nonempty).
Query/verify error split. Guarded-callback fence. Single-column indexes remain
stock STN3: no directory, no trailer.

**Cost claim.** Fielded dictionary bytes were `50772` keys + `39947` STNF df
= `90719` against single-field `27168`. STN4's **hypothesis** is that the key
set matches single-field and the df sidecar is gone, so dictionary pages land
near 1.0×. That is not a proof: extent varints and prefix-compression can
still move the ratio. **C.2 measures it.** Channel directories live in the
posting areas (tens to hundreds of KiB at POC scale), not in dictionary
pages. p50 stays the parent's advisory gate: a named waiver in the results
file, or it does not pass.

---

## 1. The representation

### 1.1 Dictionary

Pinned upstream `d57ef58` (`segment/src/index.rs`) exposes `Index::term(&str)`,
`Window::{Prefix, Range, All}` of `&str`, `expand`'s `Fn(&str)` filter, and
`Expanded::Terms(Vec<(String, Term)>)`. `TermEntry` (`segment/src/dictionary.rs`)
is unchanged:

```
TermEntry { df: u32, max_tf_bucket: u8, ordinals: Extent, payload: Extent }
```

On a multi-column index the key is the **decoded surface token**, UTF-8, the
same bytes a single-column index would store for that analyzed text. There is
no `~{h}~` prefix. Tildes in the token are ordinary payload; they are not
escaped. Errors and `score_inspect` already show the surface token; they stop
having an encoded form to hide.

`df` is the index-time union cardinality for that token in this segment:
distinct document ordinals that contain the token in any field, dead ordinals
included until rewrite. It is the idf input parent §5.1 named `df_agg`. It is
not a field-local postings length. `max_tf_bucket` is packed into the stock
`df_bucket` nibble because `TermEntry` is unchanged; on a multi-column entry
it is the max of the channel maxima and is **not a fused bound**. No consumer
may score or bound a multi-column term from the parent nibble alone (§3).

A token absent from every field is absent from the dictionary. A token present
in a subset of fields is present as **one** entry; missing fields are missing
channels, not missing keys.

Single-column indexes do not write a directory. Their `TermEntry` is stock
STN3: `df` is the score df, `payload` is one position stream, `ordinals` is
one ordinal stream. Field 0 is not duplicated beside that entry.

### 1.2 Channel directory (wire format)

`TermEntry` still has only two extents. Per-field data multiplexes **inside**
those extents so `fields/bound.rs` can keep reading stock `Ordinals` /
`ChunkBound` / `BlockBound` per field.

Each multi-column extent starts with the same directory, then concatenated
stock streams:

```
channel_extent :=
    magic "FCH1"               # 4 bytes; Field CHannels, version 1
    n        u8                # present fields, 1..=field_count
    record*  n                 # sorted by field ordinal, unique
    streams                    # concatenation, same order as records

record :=
    field    u8                # 0-based key-column index, < field_count
    len      u32le             # byte length of that field's stock stream
                               # NOT a df. df is the ordinals stream count.
```

Directory size is `5 + 5n` bytes per extent (`"FCH1"` + `n` + `n × (1+4)`),
not `8 + 5n`. Both extents carry the same directory. A two-channel token is
30 bytes of directory across the two areas.

`ordinals` records describe stock STN3 ordinal streams (block bounds, 65,536-doc
chunk folds, sub-block bounds, tf buckets, field-local document lengths).
`payload` records describe stock STN3 payload streams (position lists, skip
table). The two directories name the **same** present set in the same order; a
mismatch is corruption. `n = 0` is corruption (the entry would not exist).
A field listed in the envelope but absent from this token is simply omitted.

Each channel is indexed with **that field's raw `u32` length** as STN3's
document length for the stream — the same rule fielded-terms used, now inside
the entry rather than across F dictionary keys. `shortest()` on a channel is
therefore `min_doc_length(f)`.

The directory is **not** a dictionary row. Prefix compression, block terms
(`BLOCK_TERMS = 64`), and `df_bucket` packing stay stock. Concatenated stream
**bytes** live in the ordinals/payload areas. The only dictionary-block effect
is that `ordinals_len` / `payload_len` varints in `TermEntry` get larger
(§5 bounds that).

### 1.3 Opening channels without forking `Index::term`

Stock `Term::ordinals()` / `Term::payload()` parse the whole extent as one
stream. A `FCH1` prefix would look like truncation or a bad varint to them.
The seam is a narrow owned method on `Term` in `segment/src/segment.rs` (already
an STNF-owned file per parent §3):

```
Term::channels(&self, field_count: u8) -> Result<Vec<(u8, Term<'_>)>>
```

`field_count` is the envelope's count (`2..=16`). The method validates it:
`n > field_count`, any `field >= field_count`, duplicate/unsorted fields, or
`n == 0` is `Error::Corrupt`. Callers branch on the envelope, not on the magic:

- `field_count == 1`: do **not** call `channels()`. Stock `ordinals()` /
  `payload()` / `df()` are the term. A `FCH1` prefix on a single-column
  segment is corruption. Scoring and bounding use the stock term.
- `field_count >= 2`: `channels(field_count)` **must** succeed before any
  score or bound. Magic absent, or any directory defect, is `Error::Corrupt`
  — not a silent stock fallback. Treating a missing directory as one
  mixed-length stream would feed the fused bound the wrong `shortest()` and
  would fail the 0..1000 / 500 witness.

**Child `TermEntry` fields come from stock stream headers, not from `len`.**
Unpack caches them on the child so later `Term::df()` / `max_tf_bucket` do
not walk postings:

| Child field | Source | Not the source |
|---|---|---|
| `ordinals` / `payload` extents | parent offset + directory bytes + sum of earlier `len`s; `len` is the **byte** size of that stock stream | — |
| `df` | the ordinals substream's leading **count varint** (document count in that channel, dead included). Same quantity stock `encode_term` stores as `data.tids.len()`. | directory `len` (bytes); parent union `df`; payload skip count |
| `max_tf_bucket` | `BlockBound::max_tf_bucket` over that ordinals substream's stock bounds (list bound, or max of chunk bounds). Same derivation as `encode_term`'s `occurrence.bucket` max. | parent nibble; a payload scan |

The parent `Term::df()` remains the **union**, and is the only idf input.
The once-per-cache-entry verify pass checks `child.df ==` ordinals membership
count and `child.max_tf_bucket ==` recomputed stream max. A mismatch is
corruption.

**Build / insert caller.** The mutable index does not infer field identity
from column order at flush time. Every posting names the field and that
field's raw document length:

```
add_occurrence(token: &str, field: u8, positions: &[u32], field_length: u32)
```

`field` is the 0-based key-column index (`< field_count`). `field_length` is
that column's raw token count for this document — the STN3 `doc_len` written
into that channel's bounds — not the concatenated length and not another
field's length. Heap `CREATE INDEX` and subsequent `INSERT` both call this.
Flush writes one `TermEntry`: union `df` counted across the token's field
ordinals (dead included), parent `max_tf_bucket` the max of the channel
maxima (dictionary nibble only), extents the two `FCH1` blobs. Merge merges
each field's stock streams independently, then **recounts** the union into
`TermEntry.df`. Adding channel dfs is forbidden: field sets overlap, and a
sum would keep dead ordinals the merge dropped. Across segments, ordinals are
disjoint; query time sums parent `TermEntry.df` the way `build_index_scorer`
summed sidecar `df_agg`.

`fields/` is the only query caller of `channels()` on a multi-column index.
It does not fork the key type. After unpack, a channel is a `FieldTerm` and
the rest of the 4.1–4.4 types (`LogicalTerm`, `LogicalPostingCursor`,
`FieldHit`) are unchanged. Every multi-column bound or score path goes through
that unpack; see §3.

### 1.4 What does not live in the entry

Per-document field lengths and `field_total[f]` are document-table data.
Putting a norms row inside every posting would multiply 160 KiB of POC norms
by the term count. They stay in STNF, doc-major, parent §5.2 layout:
ordinal `o`, field `f` at `(o * field_count + f) * 4` from the start of the
rows. `len*` and `avgdl*` keep reading that table. See §6 for the trailer
version bump.

---

## 2. The scoring path

The formula is parent §5.1, copied so this document is executable without a
cross-read for the arithmetic. Cast order, dequantize, and combination order
are part of the formula.

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

`fields/score.rs` (`fused_tf`, `fused_len`, `fused_avgdl`, `fused_idf`,
`saturate`, `fused_score`, `raw_tf_from_hits`) does not change its math.
`raw_tf_from_hits` still takes `FieldHit.positions.len()`, never the stored
bucket.

What changes is **where the inputs come from**:

| Input | Fielded-terms (withdrawn) | STN4 |
|---|---|---|
| streams | `Index::term(fielded_key(f, text))` once per field in the mask | `Index::term(text)` once; `Term::channels()`; keep channels whose `f` is in the mask |
| `df_agg` | STNF df section, else `union_df_agg_from_streams` | parent `Term::df() as u64`; query-time total is the sum across segments (`query_total_df` rewritten to sum entry dfs) |
| `tf*` | `FieldHit` from those streams | same `FieldHit`, from the unpacked channels |
| `len*` / `avgdl*` | STNF norms rows / `field_total` | **same STNF norms** (v2, df section gone) |
| scoped query | open only masked field keys | open the one entry, drop channels outside the mask; `df_agg` stays the unscoped union |

The lookup table in parent §5.1 still holds:

| Where the token occurs | Scope | Channels opened | `df_agg` |
|---|---|---|---|
| title only | unscoped | title | entry.df |
| body only | unscoped | body | same |
| both | unscoped | title and body, one key | same |
| either | `title:(…)` | title only | same; `tf*` is title only |

A field with no posting at ordinal `o` does not remove `o` from the candidate
union and does not contribute `tf*`. Direct lookup absent from every selected
channel is `Lookup::Term` with empty streams: no candidates, not an error.
Direct lookup is still one `Index::term(text)` plus drop channels outside the
mask. It does not use `max_expansion`.

**Scoped expansion must not count out-of-scope tokens against the cap.**
Fielded-terms expanded *per field* inside that field's `~{h}~` fence, so
`title:(pre*)` never saw body-only keys. STN4 has one dictionary and one
window on decoded text. A global `Index::expand(window, filter, max_expansion)`
would let body-only `pre*` matches consume the title-scoped budget and return
`Overflow` when the in-scope set fits. That is a behavior change and is
forbidden.

Expansion still uses the stock seam (`Window`, `Fn(&str) -> bool`, `limit`).
The adapter owns the count. For every key the window yields:

1. **Decode token.** The dictionary key *is* the analyzed surface token.
   The `Fn(&str)` text filter (identity for prefix/range; regex/fuzzy
   predicate for those shapes) runs on that string. It does not encode a
   field and it does not see channels.
2. **Channel-set ∩ query mask.** The adapter then opens that key's `Term`
   (`Index::term`, or the `Term` stock expand already loaded) and reads
   `channels(field_count)`. Keep the token only if at least one channel's
   field bit is set in the query mask. A body-only token under `title:(…)`
   fails this step and is discarded. Opening a missing term is not a hit.
3. **Then count against `max_expansion`.** Only tokens that passed (1) and
   (2) increment the cap. If that in-scope count exceeds `max_expansion`,
   the result is `Lookup::Overflow` and the lists are discarded. Tokens
   that failed (2) never increment the cap and never appear in `Terms`.

Implementation may fold (2) into the stock `Fn(&str)` by capturing `&Index`
and calling `term` + `channels` inside the predicate, then passing
`limit: max_expansion` so stock overflow matches the in-scope cap. It may
also walk with `limit: usize::MAX` / a scan cap and apply (2)–(3) after, as
long as (3) does not count tokens that failed (2). It must not pass
`limit: max_expansion` to stock expand with a text-only filter on a partial
mask.

Unscoped queries use the all-fields mask; a stored token always has ≥1
channel, so (2) is true for every existing key and the cap counts the window
the same way a single-column expand would. Tokens remain unique in the
dictionary, so the BTree group-by-decoded-text step is identity. Order of
`Lookup::Terms` is still `(decoded text, mask)`.

`Lookup::Overflow` is still not an error and not an empty result: `Plan {
cursor, exact: false }` and the three SQL outcomes in parent §5.1 are
unchanged.

Regex and fuzzy still use `Window::All` plus a text predicate in step (1),
then (2) and (3). They do not keep a prefix of the matches. Fixtures lock
the cap (B.1): for each of prefix/wildcard, range, regex, and fuzzy, a
`title:(…)` query whose **body-only** hits exceed `max_expansion` while
**title** hits do not must return `Lookup::Terms` of the title tokens, not
Overflow. A second case where in-scope hits exceed the cap still Overflows.

A malformed **channel directory** at query time is an error; at verify time
it is corruption. The fielded `KeyDefect` variants (`BadHeader`,
`UnbalancedEscape`, …) stop firing on the lookup path because there is no
encoded key. Empty token remains `EmptyToken`.

Acceptance for this section is parent §5.2 cases 1–5, bit-identical to the
0.4.0 recordings and to the 4.2/4.5 fielded-terms smoke. The bits do not
move because the inputs (`tf*` from position counts, `len*` from norms,
`df_agg` the same union) do not move — only their storage does.

---

## 3. The WAND bound

Parent §5.1 bound, unchanged:

```
max_tf*  = Σ_{f ∈ mask ∩ present} w_f · representative_count(max_tf_bucket_f)
min_len* = min over mask ∩ present of w_f · shortest_f
bound    = saturate(max_tf*, min_len*)
```

`fields/bound.rs` already consumes a `Vec<FieldTerm>`: per-field `Ordinals`,
`field_envelope` over intersecting blocks, fused interval truncation at the
earlier of covering-block end and the next block/chunk/sub-block of any
mask-internal stream (the 0..1000 / 500 witness). After `Term::channels()`,
those `FieldTerm`s exist again. The bound functions do not grow a second
formula.

The alternative — one union ordinal stream with field tags in the payload —
would collapse per-field `ChunkBound` into a single stock bound whose
`shortest()` mixed field lengths and whose `max_tf_bucket` could not feed
`max_tf*`. Reconstructing per-field envelopes from payload tags would fork
`bound.rs` and risk the intersecting-blocks invariant. It is rejected.

Interval truncation still sees a body channel that starts at 500 inside a
title channel covering 0..1000, because both channels are real ordinal
streams. Bound-read `Err` remains unprunable `INFINITY` (4.4 repair). Identical
top-k rows and ordering, not the same prune count, is still the gate.

**Parent `max_tf_bucket` is not a bound.** Two unit-weight channels each with
max bucket 1 give fused `max_tf* = 1+1 = 2`, while the parent nibble is
`max(1,1) = 1`. Using the parent as a "conservative" stock bound **under-**
estimates and can prune a document whose exact fused score exceeds it. Every
multi-column bound or scoring consumer **must** unpack via
`channels(field_count)` and run the fused formula on those `FieldTerm`s.
Stock `Term::ordinals()` / `payload()` / the parent nibble on a `FCH1` term
are not a fallback. If channels are unavailable (error opening the directory,
wrong `field_count`, a caller that has not unpacked), the bound is
`INFINITY` (unprunable) and scoring is an error — the same class as 4.4's
bound-read failure, not a silent parent-nibble bound. Single-column continues
to use stock `Term` bounds.

---

## 4. Phrases, spans, highlights, planner

Parent Appendix A is still the SQL contract. Representation consequences:

**Phrases and spans.** An unscoped phrase is the OR of per-field bindings:
each binding walks **that field's** payload channel, never concatenating
positions across fields. Unscoped `NEAR` / `THEN` do not compare intervals
across fields (lsg4-rfc §5.11 / parent §7). A field-scoped phrase opens only
that channel. `LogicalPostingCursor::field_hits` already returns per-field
`positions`; Phase 5 consumes that, not the encoded key.

**Field-scoped queries.** The mask is a property of `LogicalTerm`, not of the
dictionary key. `title:(needle)` is `Index::term("needle")` plus keep-channel-0.
`==>` still applies `scope_scan_query` via `Private.field` (owned shim,
parent §3). The any-field union remains the `search()` retrieval rule.

**Highlights.** `project_to_field` is unchanged: wrapper-for-other contributes
no marks; wrapper-for-this contributes inner marks; unscoped parts still mark.
Snippet selection (wrapper field, else first field with a mark, else first
non-NULL) does not read the dictionary key. The 5-arg / bound SQL forms stay
parent §4.1.

**Planner widening.** `matching_stannum_indexes` already walks every key and
carries the 0-based ordinal. STN4 does not reopen `amcanmulticol`, kind-5,
or the guarded-callback table. Phase 5 still has to apply scope on bitmap /
heap / cost paths that 2.2 stubbed; that work is independent of the key codec.

**Stop words.** Filtered on analyzed token text in `compile_scoring_terms`
before the `(text, mask)` key is formed. There is no longer an encoded key to
strip `的` out of; the fielded-terms footgun in Appendix A goes away with the
codec.

---

## 5. The dictionary cost argument

Gate (parent §5.2, no waiver): dictionary bytes **and** build time ≤ 1.8× the
single-field STN3 index on the same corpus and machine. Dictionary bytes
include the aggregate-df section. Norms are not in that sum (the POC did not
charge `stnf_norms_len` to `dict_bytes`).

Measured, PG 17.11, corpus above, `stn3_fielded` vs `stn3_single`:

| | fielded | single | ratio |
|---|---:|---:|---:|
| `sections_dictionary` | 50,772 | 27,168 | 1.87× |
| `stnf_df_len` | 39,947 | 0 | — |
| **dict_bytes (gate)** | **90,719** | **27,168** | **3.34×** |
| `stnf_norms_len` (not in gate) | 160,021 | 0 | — |
| `segment_bytes` | 11,925,011 | 8,874,951 | 1.34× |
| `build_s` | 1.355 | 0.694 | 1.95× |
| ranked p50 (advisory) | — | — | 5.61× |

Fielded keys cost 1.87× because a token posted in both fields is two dictionary
entries (`~0~…`, `~1~…`), and even a one-field token pays the `~{h}~` prefix
and tilde escaping, which prefix-compression cannot share across fields. The
df sidecar then copies every unique token again (`token_len` + utf-8 + `df u64`)
into STNF — 39,947 bytes, 1.47× the entire single-field dictionary — because
field-local `TermEntry.df` was not the union.

STN4 **hypothesis**, same corpus — C.2 is the decision, not this table:

| Component | Hypothesis | Why |
|---|---|---|
| dictionary keys | near single-field `27168` | Same unique analyzed tokens as concatenated single-field; no `~{h}~`; one entry per token |
| STNF df | **0** | `TermEntry.df` is the union; this term is no longer in `dict_bytes` |
| **dict_bytes** | **hypothesis ≈ 1.0×, not a proof** | Gate ceiling 1.8× = `27168 × 1.8 = 48902` bytes; growth headroom **21734** bytes |
| channel directory | posting areas, not dictionary pages | `5 + 5n` bytes per extent; two extents ⇒ `10 + 10n` bytes/term |
| posting streams | ≈ fielded postings | F stock streams still exist; they were never the dict miss |
| norms | 160,021, still not in the gate | Document-table `len*` / `avgdl*` |

Directory size worked example: 4,000 two-channel terms, `n = 2` → 15 bytes per
extent × 2 extents = **30 bytes/term = 120 KiB in the ordinals/payload areas**.
That is not "a few KB" and it is not dictionary pages. It does not enter the
1.8× dictionary ratio except insofar as larger extents widen the `ordinals_len`
/ `payload_len` varints in each `TermEntry`.

Those varints are the only dictionary-block effect of concatenation. A u32
length is at most 5 varint bytes; two extents versus a 1-byte single-field
length is **at most +8 bytes/term**. That worst case is not cheap: `4000 × 8
= 32000`; `27168 + 32000 = 59168 = 2.18×`, **above** the 1.8× ceiling. The
21734-byte headroom allows at most `floor(21734 / 8) = 2716` terms at that
worst-case before varints alone miss the gate. Expected growth is smaller (a
length that was 2 bytes becoming 3 is +1 per extent). **None of this proves
≤ 1.8×.** It only shows why the 3.34× miss (extra keys + 40 KiB sidecar) should
not recur as an entry-count problem, and why C.2 must measure dictionary bytes
and build time on the real index.

Build time: fielded inserted F keys and wrote the df sidecar. STN4 inserts one
key and still writes F channels. Tokenization and posting encoding dominate;
dropping the extra dict insert and sidecar sort is the plausible path under
1.8×. Residual risk: directory packing CPU. The gate is a re-run of the parent
§5.2 protocol, not this arithmetic.

**p50 is advisory with a mandatory waiver.** Ranked p50 ≤ 1.3× the single-field
baseline. A miss may stay only with a waiver **in the results file** that names
the workload, the ratio, and why staying is cheaper than another representation
cut. **No waiver, no pass.** A follow-up task is not a waiver. One `term()`
instead of F is the intended lever; WAND still walks F channels. A miss is not
a formula change and not a return to fielded keys.

**Chinese corpus is in C.2.** Jieba is already on this branch (Phase 3). C.2
runs the English POC corpus (checksum above) **and** a fixed Chinese corpus
under the pinned jieba snapshot, same protocol, same 1.8× / 1.3× rules, both
written into `docs/benchmarks/stn4-per-field-poc.json`. The dictionary
hypothesis (entry count = unique tokens) does not depend on script; the
numbers might.

---

## 6. Migration

### 6.1 0.4.0 → 0.5.0 (unchanged)

Parent §8 stands. `stannum--0.4.0--0.5.0.sql` is catalog-only (`capabilities()`
and matching identities). It does not convert segments. Cutover is downtime,
clean shutdown, one `stannum.so`, `ALTER EXTENSION`, then non-concurrent
`REINDEX`. `PreStn3` is still LSG1–LSG4 / kind-1 0.4.0 meta. Kind-5 envelope
stays: field names, weights, analysis stamp, `field_count` 1..=16. Single-column
`field_count` 1 writes no STNF. Guarded callbacks, exempt `ambuild` /
`ambuildempty`, and `amvalidate` outside the fence do not move.

### 6.2 STNF version 2 (norms only)

```
# v1 (fielded-terms, withdrawn)
trailer  := magic "STNF", version u8 = 1, norms_len u32le, df_len u32le, norms, df_agg

# v2 (STN4)
trailer  := magic "STNF", version u8 = 2, norms_len u32le, norms
norms    := field_count u8 (2..=16), field_total u64le × field_count,
            rows u32le × field_count × document_count,
            crc32 u32le
```

CRC-32/ISO-HDLC, doc-major rows, `field_total[f] == Σ_o rows[o][f]`, and the
once-per-cache-entry positions pass stay. The positions pass no longer rebuilds
a df map to compare to a sidecar; it still checks each cell against that
field's position-list lengths, and it checks parent `TermEntry.df` against the
union of channel ordinals (`0 < df ≤ document_count`). Merge recounts both the
norms and the entry dfs.

Single-column: `total == pages_end`, no trailer, no `FCH1`. A trailer on a
single-column immutable segment is corruption.

### 6.3 Classification (mutually exclusive)

Indexes built on this branch between 4.3 and 4.6 wrote kind-5 + STN3 + STNF v1
+ `~{h}~` keys. STN4 cannot score them without the withdrawn codec. They are
not 0.4.0, so the §8 migration sentence about LSG1–LSG4 must not fire.

`storage::open_index` classes. Exactly one applies. **Well-formed** means the
blob parses, lengths sum, CRC matches, and dimensions agree with the envelope.
Malformed, truncated, unknown version, or a v1 layout with `df_len = 0` / a v2
layout with a df section is not "valid v1" and not "valid v2".

| Class | Recognition (first match that is a complete description of the relation) | Guarded callbacks |
|---|---|---|
| `PreStn3` | LSG1–LSG4 segment magic and/or kind-1 0.4.0 meta (existing, including 0.4.0 empty and 0.4.0 buffer-only) | existing migration error, no write |
| `Current` | kind-5 envelope, and every immutable multi-column segment that exists is **well-formed STNF v2**; single-column immutable segments have no trailer; empty and buffer-only as in the rows below | proceed |
| `StaleFielded` | kind-5 envelope, and every immutable multi-column segment that exists is **well-formed STNF v1** (df section present and well-formed). Buffer-only fielded-terms: see below. | **rebuild error**, no write |
| `MixedFielded` | kind-5 envelope, and the immutable multi-column segments include **at least one well-formed v1 and at least one well-formed v2** | **rebuild error**, distinct string from `StaleFielded`, no write |
| `Corrupt` | everything else: missing trailer on a multi-column **immutable** segment; truncated/bad-CRC/unknown-version STNF; v1/v2 structural mismatch; `FCH1` defects on a Current writer; mixed envelope/LSG segment; single-column with a trailer or `FCH1`; buffer whose terms are neither valid FCH1 nor valid fielded-terms keys | existing corruption, no write |

`StaleFielded` and `Corrupt` do not overlap. A withdrawn v1 that **parses** is
`StaleFielded`. A v1-shaped blob that does **not** parse is `Corrupt`. A valid
v2 is `Current`. Missing or unknown-version is `Corrupt`, never `StaleFielded`.

**Empty.** Kind-5, no immutable segments, empty insert buffer (zero docs): `Current`.
There is no trailer to be v1 or v2. `search` / `search_count` still answer 0.
A 0.4.0 empty index is `PreStn3` (kind-1), unchanged.

**Buffer-only.** Kind-5, no immutable segments, nonempty mutable buffer:

- `field_count == 1`: `Current` (stock terms, no directory).
- `field_count >= 2`, every stored term unpacks as `FCH1` under the envelope
  `field_count`: `Current`.
- `field_count >= 2`, every stored term is a well-formed fielded-terms key
  (`~` + one lowercase nibble + `~` + escaped token, ordinal `< field_count`):
  `StaleFielded`. This is the only place classification looks at dictionary
  keys, because there is no trailer.
- Mix of FCH1 and fielded-terms keys, or keys that are neither: `Corrupt`.

Detection of **immutable** segments prefers the trailer version, not a key
heuristic: a v2 trailer with a stray `~0~` token is a legal token, not
fielded-terms.

Rebuild error texts (`StaleFielded` vs `MixedFielded`) are distinct from the
§8 migration string, so an operator who already `ALTER EXTENSION`d does not
think they are still on 0.4.0. Exact strings are pinned in pg_tests. `ambuild`
/ `ambuildempty` remain exempt so `REINDEX` clears every class except `Corrupt`
that is not rebuildable. `MixedFielded` is rebuildable: `ambuild` reads the heap.

These classes exist because 0.5.0 is still unreleased as a product tag. After
0.5.0 ships STN4, `StaleFielded` / `MixedFielded` are only relevant to
development indexes on `stn3`. They are not a supported upgrade path and need
no SQL script.

### 6.4 Envelope and SQL

Kind-5 is unchanged. `contract_version` stays 1: recorded rank order does not
change. `capabilities().engine.format` remains `"STN3"` — STN4 is a layout
inside STN3 terms, not a new segment magic. A format string change would be a
contract bump without a behavior bump; it is rejected.

---

## 7. What carries forward / what is replaced

Layer diagram is parent §3. L2 still does not import `IndexScorer`. Owned
shims (`operator.rs`, `score.rs` `scope_scan_query`, `highlight_udfs.rs`,
`customscan.rs`, `am.rs`, `options.rs`, `storage/layout.rs`, `tinql/`) do not
gain STN4 work except the `StaleFielded` / `MixedFielded` classes in the opener.

### Carry (math and semantics; tests stay)

| File | What stays |
|---|---|
| `postgres/src/fields/score.rs` | `fused_tf` / `fused_len` / `fused_avgdl` / `fused_idf` / `saturate` / `fused_score` / `raw_tf_from_hits` / `all_fields_mask`. Bit fixtures in this file are the formula lock. |
| `postgres/src/fields/bound.rs` | `fused_bound` / `fused_interval_bound` / `next_interval_end` / envelopes / intersecting-block truncation. Input remains `FieldTerm` streams. |
| `postgres/src/fields/types.rs` | `FieldTerm`, `LogicalTerm { text, mask, df_agg, streams }`, `Lookup`. `df_agg` is filled from `Term::df()`, not from a sidecar. |
| `postgres/src/fields/cursor.rs` | `LogicalPostingCursor`, `FieldHit`, advance atomicity, successor peeks. Opened on unpacked channels. |
| `postgres/src/fields/expand.rs` | `Lookup` outcomes, empty-streams-not-error, overflow plan, scope mask on the logical term, `max_expansion` counted once. Implementation of *how* streams are found is replaced. |
| `postgres/src/fields/error.rs` | `AdapterError` Index vs key split; query vs verify as distinct kinds. `KeyDefect` header/escape variants become unused on the live path; keep until the codec file dies so old tests can compile during the cut. |
| `postgres/src/storage/layout.rs` | `KIND_ENVELOPE = 5`, names/weights/stamp. |
| `postgres/src/storage/mod.rs` | `Current` / `PreStn3` / `Corrupt` fence; add `StaleFielded` and `MixedFielded`. |
| `segment/src/dictionary.rs` | Layout of `TermEntry` and prefix-compressed blocks. No new fields. |
| `segment/src/payload.rs`, `ordinals.rs`, `bound.rs` | Stock stream codecs. Channels are those codecs. |

### Replace

| File | Fate |
|---|---|
| `postgres/src/fields/codec.rs` | **Dead.** `fielded_key` / `header` / `upper_fence` / tilde escape. No writer, no lookup. Delete once expand/lookup no longer import it and the round-trip fixtures are gone. |
| `postgres/src/fields/expand.rs` lookup/expand bodies | Decoded `Window`; text `Fn(&str)` first; channel-set ∩ mask before `max_expansion`; drop codec. |
| `postgres/src/fields/df.rs` | `query_total_df` reads entry dfs. `union_df_agg` remains a **build-time** helper to compute the stored union (and a verify check). Sidecar reader goes with STNF v1. |
| `segment/src/trailer.rs` | v2: drop `df_len` / `DfEntry`. v1 parse remains long enough to classify `StaleFielded`. |
| `segment/src/segment.rs` | `Term::channels()`. `Reader::new` still stops stock checks at `pages_end`; trailer API reads v2. |
| `segment/src/index.rs` mutable | `add_occurrence(token, field, positions, field_length)`; per-token per-field builders; flush writes `FCH1`. |
| `segment/src/merge.rs`, `verify.rs` | Per-channel stock merge/verify; recount union into `TermEntry.df`; no df sidecar add. |

`fields/mod.rs` L1 comment updates: it owns the channel unpack, the fused
scorer, the bound, and the norms reader. It does not own a key codec.

---

## 8. Execution plan

This is a **new** plan. `docs/plans/stn3-tool-layer-execution-2026-09-30.md`
steps 4.7–6.5 are superseded as a schedule; their *goals* (pgembed checkpoint,
phrases, highlights, planner, migration CI, wheel, release) reappear below
under STN4 numbering. Phases 0–3 and 4.1–4.6 on `stn3` stay done. Design wins
over this section if they disagree.

Protocol matches the parent loop: one step = one orchestrate run; L1 command
gates; L4 `ask_oracle` mode=review; two failed repair rounds escalate; never
edit `contract/expected/**` except the pinned recording job, never edit
released `postgres/sql/stannum--*--*.sql`; origin push of `stn3` is authorized.

Estimates are engineer-weeks on the remaining critical path. Parent §11 said
STN4 is not inside the old 5–7 week figure. Representation here is smaller
than a green-field LSG4 port because score/bound/envelope/fence already exist,
but it is still a format cut. **Remaining critical path ≈ 8–10 engineer-weeks;
elapsed ≈ 6–8** if Phase E packaging overlaps Phase D. 0.4.0 keeps serving
pg-agent; the risk is slip, not a forced cutover date.

### Phase A — Representation (≈ 2.5 wks)

- [ ] **A.1 Channel directory + `Term::channels(field_count)`**
  - Scope: encode/decode `FCH1` in both extents; slice into stock `Term`s;
    child `df` from ordinals count varint; child `max_tf_bucket` from stream
    bounds; `field_count` validation; property tests (present-set agreement,
    nibble-free keys, empty token rejected, field 0..15, `field >= field_count`
    corrupt, single-field present still writes a directory on multi-column).
    No SQL.
  - Done when: unit tests cover the corruption matrix in §1.2–1.3; child.df is
    posting count not byte `len`; single-column fixtures never write or accept
    `FCH1`; multi-column missing magic is corruption; stock `term()` still
    works on 0.5.0 single-column fixtures.
- [ ] **A.2 Mutable flush + merge recount**
  - Scope: `add_occurrence(token, field, positions, field_length)`; per-token
    per-field builders; parent `TermEntry.df` = union including dead; merge per
    channel then recount; forbid add of channel dfs. **Both** heap `CREATE
    INDEX` (SQL build) **and** incremental `INSERT` after the index exists.
  - Done when: two-field fixture with overlap counts union 3 not 4 on SQL build
    **and** on insert-after-create; dead ordinal remains in `df` until rewrite;
    merge of two segments recounts; `field_length` is that field's raw count.
- [ ] **A.3 STNF v2 + classification matrix**
  - Scope: trailer without df; opener classes in §6.3 (`Current` / `StaleFielded`
    / `MixedFielded` / `Corrupt` / `PreStn3`); empty and buffer-only rows;
    rebuild error strings; `ambuild` exempt.
  - Done when: v2 round-trip; well-formed v1 kind-5 → `StaleFielded` rebuild
    error, no page dirty; mixed v1+v2 → `MixedFielded` distinct string; missing
    / malformed / unknown-version trailer on a multi-column immutable →
    `Corrupt` not `StaleFielded`; empty kind-5 → `Current`; LSG4 still the §8
    migration string.

### Phase B — Query wiring (≈ 1.5 wks)

- [ ] **B.1 Lookup / expand / cursor on channels**
  - Scope: replace expand/lookup bodies; `df_agg` from parent `Term::df`;
    expansion order §2 steps (1)(2)(3); drop codec from this path.
  - Done when: fields unit tests that used encoded keys are rewritten against
    surface tokens; empty streams not error; overflow still `Lookup::Overflow`;
    scoped mask does not change `df_agg` (`LogicalTerm.df_agg` is the parent
    `Term::df()`, never a channel-local stream length). **Scoped cap fixtures:**
    for prefix/wildcard, range, regex, and fuzzy, `title:(…)` with body-only
    hits `> max_expansion` and title hits `≤ max_expansion` returns `Terms`
    of the title tokens, not Overflow; in-scope over cap still Overflows.
- [ ] **B.2 Bound + verify on channels**
  - Scope: wire existing `fused_bound` to unpacked `FieldTerm`s; no parent-nibble
    fallback; verify recomputes per-channel min_len/max bucket and parent `df`.
  - Done when: 0..1000/500 witness still truncates; pruned == exhaustive
    top-k rows; bound-read `Err` still INFINITY; two unit-weight channels score
    `tf* = 2` and a parent-nibble bound of 1 is never consulted; missing
    `channels()` on multi-column bound → INFINITY, score → error.

### Phase C — Gates (≈ 1.5 wks)

- [ ] **C.1 Semantic smoke — GATE**
  - Scope: parent §5.2 cases 1–5 through real SQL on multi-column STN4
    indexes. Bits must match the 0.4.0 recordings (`3e113925` tie, weighted
    order, R-BIT on single-column). Formula does not move if they fail.
  - Done when: those five plus the 4.5 multi-column contract cases that
    already went green on fielded-terms (arithmetic, ddl, snippets-except-
    phrases, expansion) are green on STN4.
- [ ] **C.2 Latency — GATE**
  - Scope: parent §5.2 protocol, PG17, concurrency 1, 20 runs. English POC
    corpus (same checksum) **and** a fixed Chinese corpus under the pinned
    jieba snapshot (Phase 3 is done). Query lists committed beside the results.
    Write `docs/benchmarks/stn4-per-field-poc.json` covering both.
  - Done when: on **each** corpus, dictionary bytes (no df sidecar) and build
    ≤ 1.8× `stn3_single` (mandatory, no variance waiver); ranked p50 ≤ 1.3×
    (advisory: a miss passes only with a waiver **in that results file** that
    names the workload, the ratio, and why staying is cheaper than another
    cut — **No waiver, no pass.** A follow-up ticket is not a waiver). p99
    labeled, not a gate. A mandatory miss stops and escalates (next fallback
    is a recorded divergence + `contract_version` bump).
- [ ] **C.3 Dead codec**
  - Scope: delete `fields/codec.rs` and encoded-key fixtures; `KeyDefect`
    header/escape either deleted or confined to a historical comment test;
    `fields/mod.rs` comment matches §7.
  - Done when: no `fielded_key` / `~0~` writer in `postgres/src` or
    `segment/src`; workspace tests green.

### Phase D — Phrases, highlights, planner (old Phase 5) (≈ 2 wks)

- [ ] **D.1 Field scope in tinql + operator** (old 5.1)
- [ ] **D.2 Same-field phrases + spans** (old 5.2) — positions from channels
- [ ] **D.3 Field-aware highlights + snippets** (old 5.3)
- [ ] **D.4 Full planner coverage** (old 5.4)
- [ ] **D.5 pgembed checkpoint** (old 5.5 / 4.7 folded here if C.2 is green)

Done-when texts of old 5.1–5.5 apply, with "fielded key" read as "channel".

### Phase E — Packaging + release (old Phase 6) (≈ 3 wks)

- [ ] **E.1** Two-artifact migration CI (old 6.1) — add `StaleFielded` and
      `MixedFielded` rows: a 4.6-era v1 index must rebuild-error, not
      migrate-as-LSG4; mixed v1+v2 uses the mixed string.
- [ ] **E.2** Runbook + divergence ledger (old 6.2), including STN4 rebuild
      of any development fielded-terms indexes.
- [ ] **E.3** Wheel alignment (old 6.3)
- [ ] **E.4** Conformance + parity sweep — GATE (old 6.4)
- [ ] **E.5** Release 0.5.0 (old 6.5)

### Dependency graph

```
A.1 ── A.2 ── A.3 ────────────────────────── C.1(GATE)
                    │                         ▲
                    └─ B.1 ── B.2 ────────────┘
                                              │
                                              C.2(GATE) ── C.3
                                              │
                    A.3 ── E.1 ── E.2 ── E.3 ─┼── E.4(GATE) ── E.5
                                              │         ▲
                                              D.1 ── D.2 ── D.3 ── D.4 ── D.5
                                                                    │
                                                                    └── joins E.4
```

A.3 → C.1 is required: leftover v1 indexes must classify before SQL smoke
builds new v2 indexes on the same relation names. B.2 → C.1 as well (bound
witness). D.5 (pgembed) and E.3 (wheel) both join **E.4**; E.5 does not ship
without E.4 green. C.2 is the representation gate; D/E do not waive it. D.*
may overlap C.3. E.1 may start after A.3 (classification is the new CI row).

---

## 9. Risks

- **`Term::channels` is a segment-crate seam.** Parent §3 allowed `segment.rs`
  / merge / verify / mutable index as STNF-owned. Unpacking belongs there
  because `Term`'s `areas` handle is private. Growing `TermEntry` with F
  extra extents would fork `dictionary.rs` layout and prefix-compression;
  it is the worse seam. Keep the directory in the two existing extents.
- **Build gate is closer than the dictionary gate.** Dictionary ≈ 1.0× is a
  **hypothesis** (same key set, no sidecar). Worst-case extent varints at 4,000
  terms would be 2.18× (§5). C.2 is the empirical decision. If C.2 dictionary
  or build misses, profile before inventing a second representation.
- **p50: no waiver, no pass.** One lookup should help; WAND still walks F
  channels. An advisory miss stays only with a named waiver in the results
  file. A follow-up task is not a waiver. Not a formula change and not a
  return to fielded keys.
- **`StaleFielded` vs `Corrupt` vs `MixedFielded`.** Well-formed v1 is rebuild,
  not corruption. Missing/malformed/unknown-version is corruption, not stale.
  Mixed v1+v2 is its own rebuild string. Wrong class on a v1 kind-5 index
  either blocks `REINDEX` (if treated as unrestorable `Corrupt`) or lies to
  operators (if treated as LSG4). A.3 pins all three strings plus empty and
  buffer-only.
- **Channel/parent df confusion.** Channel `Term::df()` is the ordinals count
  varint (field-local). Using it as idf reintroduces the field-local-idf break
  §5.2 case 1 catches. Tests must read the parent entry for `LogicalTerm.df_agg`.
- **Parent nibble is not a bound.** Two unit-weight channels fuse `tf* = 2`
  above parent max 1. Multi-column bound/score without `channels()` is INFINITY
  / error, never a stock fallback.
- **Scoped expansion cap.** Counting out-of-scope tokens against `max_expansion`
  changes 0.4.0 answers. B.1 fixtures are the lock.
- **Verify must not call stock `ordinals()` on a `FCH1` blob.** That path is
  corruption on a valid STN4 term. Multi-column verify always unpacks first.
- **Upstream merges.** `segment.rs` and merge/verify already conflict on STNF.
  `Term::channels` is more surface in those files. The dictionary layout and
  `Index::term(&str)` stay upstream-shaped on purpose.
- **Next fallback after a STN4 mandatory miss** is not another layout inside
  this document. It is a `contract/divergences` entry, a `contract_version`
  bump, and an explicit statement that pg-agent rank order or cost envelope
  changes. Do not resurrect fielded keys.

---

## Appendix. Frozen formula (verbatim parent §5.1 arithmetic)

Normative copy of the scoring and bound arithmetic in
`docs/designs/stn3-tool-layer-2026-09-29.md` §5.1. If this copy drifts, the
parent wins and this appendix is fixed. The surrounding fielded-terms storage
prose in that section is withdrawn by this document and is **not** copied.

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

```
max_tf*  = Σ_{f ∈ mask ∩ present} w_f · representative_count(max_tf_bucket_f)
min_len* = min over mask ∩ present of w_f · shortest_f
bound    = saturate(max_tf*, min_len*)
```
