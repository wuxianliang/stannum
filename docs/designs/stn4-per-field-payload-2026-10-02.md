# STN4: Per-Field Payload in One Term Entry

Status: proposal, oracle-repair-4 · Basis: `stn3@1e1b20b` after the 4.6 mandatory miss · Normative
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
channel-set ∩ mask is nonempty). Two expansion consumers: capped candidates
vs the complete scoring set (parent §5.1 `expand_in`).
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
30 bytes of directory across the two areas. `n = 0` is corruption (the entry
would not exist). `record.len = 0` is corruption: a listed channel has a
stream. **`child.df == 0` is also corruption**, even when `record.len > 0`:
the stock codecs accept a zero-document stream (payload grammar: `count = 0`),
but a listed channel that decodes to no documents would report the field as
present. **Omitted = no posting.** A field listed in the envelope but with no
postings for this token is absent from **both** directories. Verify enforces
the omit; `channels()` enforces `child.df > 0` on every record.

`ordinals` records describe stock STN3 ordinal streams (block bounds, 65,536-doc
chunk folds, sub-block bounds, tf buckets, field-local document lengths).
`payload` records describe stock STN3 payload streams (position lists, skip
table).

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

**`field_count` is validated unconditionally inside the method**, not only by
courtesy of the caller. `field_count < 2` or `field_count > 16` is
`Error::Corrupt` even if a single-column path should never call it. Envelope
`field_count == 1` still must **not** call `channels()`: stock `ordinals()` /
`payload()` / `df()` are the term. Single-column verify uses **only** those
stock decoders. It does **not** sniff a `FCH1` prefix (§1.3.1). Envelope
`field_count` in `2..=16`: `channels(field_count)` **must** succeed before any
score or bound.

**Exact framing.** Both extents are parsed independently, then cross-checked.
Any failure is `Error::Corrupt`. Arithmetic is **checked** (`checked_add` /
`checked_mul`); overflow is corruption, not wrap.

1. **Header + records fit.** `extent_len >= 5`. `n` is the byte at offset 4.
   Directory bytes `dir = 5 + 5n` must not overflow and must satisfy
   `dir <= extent_len`. Magic is `FCH1`. `n == 0` is corruption.
   `n > field_count` is corruption.
2. **Records parse inside `dir`.** Each record is `field: u8` + `len: u32le`
   at `5 + 5i`. Fields are strictly ascending, unique, and `< field_count`.
3. **Stream bytes tile the remainder.** `sum = Σ record.len` (checked).
   `sum == extent_len - dir`. A remainder, a shortfall, or `record.len == 0`
   is corruption.
4. **Child decoder consumes the slice exactly.** Absolute child extent is
   `parent.offset + dir + Σ_{j<i} record[j].len`, length `record[i].len`.
   The stock ordinals decoder (ordinals extent) or payload decoder (payload
   extent) must parse that slice and leave **no trailing bytes**. Trailing
   garbage, a short parse, or a decoder error is corruption of this entry.
5. **Directories agree.** After both extents parse, their `(field, …)` lists
   have the same `n`, the same field ordinals in the same order. A present
   in one extent and absent in the other is corruption.
6. **Magic absent** on a `field_count` in `2..=16` is corruption — not a
   silent stock fallback. Treating a missing directory as one mixed-length
   stream would feed the fused bound the wrong `shortest()` and would fail
   the 0..1000 / 500 witness. Absence of magic is decided by `channels()`
   failing the directory parse, not by a prefix sniff on single-column data.
7. **Listed channels are nonempty.** After step 4, each child's ordinals
   count varint (`child.df`) is `> 0`, and the payload stream's count equals
   that df. `record.len > 0` is not enough: a stock empty stream is a few
   bytes of `count = 0`. Zero-document listed channels are corruption.
   Verify walks `0..field_count` and checks that every zero-posting field is
   omitted from **both** directories.

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
count, `child.df > 0`, and `child.max_tf_bucket ==` recomputed stream max.
A mismatch is corruption.

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
maxima (dictionary nibble only), extents the two `FCH1` blobs. A field with
no occurrences for that token is **omitted** from both directories — never a
`record.len > 0` empty stock stream. Merge merges each field's stock streams
independently, then **recounts** the union into
`TermEntry.df`. Adding channel dfs is forbidden: field sets overlap, and a
sum would keep dead ordinals the merge dropped. Across segments, ordinals are
disjoint; query time sums parent `TermEntry.df` the way `build_index_scorer`
summed sidecar `df_agg`.

`fields/` is the only query caller of `channels()` on a multi-column index.
It does not fork the key type. After unpack, a channel is a `FieldTerm` and
the rest of the 4.1–4.4 types (`LogicalTerm`, `LogicalPostingCursor`,
`FieldHit`) are unchanged. Every multi-column bound or score path goes through
that unpack; see §3.

### 1.3.1 Unambiguous representation (choice a)

`"FCH1"` is the bytes `46 43 48 31`. Those bytes are **not** disjoint from
stock stream prefixes. A four-byte sniff is therefore not a decoder. Option (c)
(structural disjointness of stock streams and FCH1 directories) is false.
Option (b) (a new discriminator that cannot be a valid stock prefix) is
unnecessary once `field_count` selects the codec.

**Proof that prefixes collide.** Stock streams begin with a count varint
(`ordinals.rs`: `stream := count varint, …`; `payload.rs`: `stream := count
varint, skip u32le * slots, data`). A one-byte varint stores values `0..=127`
with the high bit clear, so `count = 70` encodes as `0x46` — the first byte
of `FCH1`. Payload skip slots are `ceil(count / 32) - 1`; for `count = 70`
that is two `u32le`. The first skip may be `0x..314843`, whose leading three
bytes are `43 48 31`. Skip slots are opaque `u32le`; any value whose first
three bytes are `43 48 31` is in-grammar. Therefore `46 43 48 31` **is a legal
prefix of a valid stock payload**. A verifier that rejects single-column
extents starting with `FCH1` will reject legal STN3 payloads. Payload
collision is sufficient; the proof does not claim ordinals prefixes are
disjoint. Do not read a bound `buckets` of `0x43` as failing
`buckets >> BUCKET_COUNT` with `BUCKET_COUNT = 16`: `0x43` is 67,
`67 >> 16 == 0`, and 67 is a legal 16-bit bucket mask.

**Choice (a).** Envelope `field_count` — known from kind-5 STNM before any
`TermEntry` is opened — selects the decoder. The two codecs are never both
applied to the same extent.

| `field_count` | Decoder | Forbidden |
|---|---|---|
| `1` | stock `ordinals()` / `payload()` / `df()` only | `channels()`; `memeq`/`starts_with(b"FCH1")` on the extent |
| `2..=16` | `channels(field_count)` (directory + child stock slices) | stock-parsing the **parent** extent; sniffing as a substitute for `channels()` |

Single-column write still never emits a directory. Single-column verify does
not care whether the first four bytes happen to be `FCH1`: if the stock
decoder accepts the extent, it is valid stock; if it rejects, it is stock
corruption. Multi-column missing-magic is `channels()` failing step 1/6, not
a prefix test that single-column also runs. Options (b) (new discriminator
byte that cannot start a varint) and (c) (prove byte-level disjointness) are
rejected: (c) is false, and (b) forks both extents to dodge a collision that
`field_count` already prevents.

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

Expansion still uses the stock `Window` and a text-only `Fn(&str) -> bool`.
The adapter owns the count **and** the walk. For every key the cursor yields:

1. **Decode token.** The dictionary key *is* the analyzed surface token.
   The `Fn(&str)` text filter (identity for prefix/range; regex/fuzzy
   predicate for those shapes) runs on that string. It does not encode a
   field and it does not see channels.
2. **Channel-set ∩ query mask.** The adapter then opens that key's `Term`
   (the cursor's current term) and reads `channels(field_count)`. Keep the
   token only if at least one channel's field bit is set in the query mask.
   A body-only token under `title:(…)` fails this step and is discarded.
   Opening a missing term is not a hit.
3. **Then count against `max_expansion`.** Only tokens that passed (1) and
   (2) increment the cap. Tokens that failed (2) never increment the cap and
   never appear in `Terms`.

**Channel opens are fallible and must not live in `Fn(&str) -> bool`.**
Stock `expand`'s filter cannot return `Result`. Opening `channels()` inside
that predicate would turn a corrupt directory into `false` (a non-match) or
into a panic. Both are forbidden. The text filter is **only** step (1).

**Required walk: a streaming dictionary cursor, not `Index::expand`.**
`Index::expand` materializes `Vec<(String, Term)>` and returns `Overflow`
without scanning further. A scan limit of `usize::MAX` therefore holds the
whole window in RAM (a scoped query with millions of out-of-scope matches is
a memory bomb). A finite limit hides a later channel error behind Overflow.
**Neither is a permitted implementation** of channel-aware expansion, not
even as an alternative to a cursor.

The required seam is an `Index` method (owned, like `Term::channels`; the
`term` / `expand` signatures do not change):

```
Index::scan_window(window, text_filter)
    -> iterator of Result<(String, Term<'_>)>
```

It yields dictionary order, one pair at a time. It applies the text filter
and interrupt checks (`INTERRUPT_INTERVAL`). It does not apply
`max_expansion`, does not open `channels()`, and does not collect. Reader
wraps `dictionary.{prefix,range,iter}` plus `resolve` of the current entry
only. MutableIndex walks its sorted keys the same way. Forking `expand` to
return a stream is rejected: the `Overflow` enum cannot continue.

`fields/expand.rs` is the only multi-column expansion implementation. For
`field_count` in `2..=16` it **MUST NOT** call `Index::expand`, including
with `limit: usize::MAX` and including with any finite limit. Single-column
(`field_count == 1`) keeps the parent split: finite `Index::expand` for
candidates, `expand_in(..., usize::MAX)` for scoring; there are no channels
to open.

**Two consumers of `scan_window`.** Parent §5.1: `search` / `search_count` /
the custom-scan cursor use the capped expansion (`Lookup::Overflow` →
`Plan { cursor, exact: false }`, `inexact_universe`). `score` and
`score_bound_indexed` compile through `expand_in`, which **keeps the complete
applicable set**. A scan-cap overflow must not truncate that list. The
discarded candidate `kept` vec is not the scoring set. These are two walks
(or two accumulators on one walk). Scoring **MUST NOT** consume, reuse, or
truncate the candidate list after Overflow.

**(a) Capped candidate expansion** (`search`, `search_count`, bitmap / custom
scan). O(`max_expansion`) retention only lives here.

- `kept: Vec` with capacity `max_expansion`. Memory is O(`max_expansion`)
  plus the current iterator item, never O(window).
- `overflow = false`.
- For each `Result<(token, term)>` in dictionary order:
  - iterator `Err(e)`: drop `kept` and return that error (dictionary itself
    is corrupt).
  - `term.channels(field_count)` `Err(e)`: this is the first channel error.
    Drop `kept` and return it. Do not treat the token as a non-match. Do not
    increment the cap. Do not push. Scanning further is unnecessary: dictionary
    order already selected the first error.
  - `Ok(ch)` with `ch ∩ mask == ∅`: skip; not counted; not stored.
  - `Ok(ch)` with a nonempty intersection: `count += 1`. If `count <=
    max_expansion`, push onto `kept`. If `count > max_expansion`, set
    `overflow`, do **not** push, **do not stop**: keep iterating so a later
    channel error is still observed (error beats Overflow).
- After a full walk with no channel error: if `overflow`, drop `kept` and
  return `Lookup::Overflow`; otherwise return `Terms(kept)`. Outcome
  priority: **error > Overflow > Terms**. An error must never become
  `Lookup::Overflow` and never become a silent drop.

**(b) Uncapped scoring expansion** (`score`, `score_bound_indexed`, and any
path that compiles terms through `expand_in`). Steps (1) and (2) only: text
filter, then `channels()` ∩ mask. There is **no** `max_expansion` and **no**
`Lookup::Overflow`. Every applicable (in-scope) term is processed. Out-of-scope
keys are skipped and not stored — that is what keeps the walk O(applicable)
rather than O(window). Memory is O(in-scope hits), which may exceed
`max_expansion`; that is required. It is not the `usize::MAX` + `Vec` window
bomb, and it is not an alternative implementation of (a).

- For each `Result<(token, term)>` in dictionary order:
  - iterator `Err(e)`: drop `scored` and return that error.
  - `channels()` `Err(e)`: drop `scored` and return that error. First error
    in dictionary order; do not skip the term as a non-match.
  - `Ok(ch)` with `ch ∩ mask == ∅`: skip; not stored.
  - `Ok(ch)` with a nonempty intersection: push onto `scored` with no cap.
- After a full walk with no error: return `scored` as the complete scoring
  set, even when (a) on the same window would have Overflowed. Channel-error
  priority is the same: **error**, never a truncated score list.

`Lookup::Overflow` is still not an error and not an empty result: `Plan {
cursor, exact: false }` and the three SQL outcomes in parent §5.1 are
unchanged for **candidate** plans. Scoring does not read `planned.exact` to
drop terms.

Query-time channel defects surface as `AdapterError` (index/corrupt), the
same query-vs-verify split as parent: query is an error, verify of a stored
directory is corruption.

Unscoped queries use the all-fields mask; a stored token always has ≥1
nonempty channel, so (2) is true for every existing key and the cap counts
the window the same way a single-column expand would. Multi-column unscoped
still walks `scan_window` for both consumers: a later corrupt FCH1 must not hide behind Overflow
or a truncated score list.
Tokens remain unique in the
dictionary, so the BTree group-by-decoded-text step is identity. Order of
`Lookup::Terms` is still `(decoded text, mask)`.

Regex and fuzzy still use `Window::All` plus a text predicate in step (1),
then (2); candidate (a) then applies (3), scoring (b) does not. They do not
keep a prefix of the matches. Fixtures lock the cap (B.1): for each of
prefix/wildcard, range, regex, and fuzzy, a `title:(…)` query whose
**body-only** hits exceed `max_expansion` while **title** hits do not must
return `Lookup::Terms` of the title tokens, not Overflow. A second case where
in-scope hits exceed the cap still Overflows **for candidates**. An **over-limit
scoring fixture** (SQL, B.1) locks parent §5.1: the same in-scope over-cap
query's `search` / `search_count` keep the overflow plan (membership and count
as recorded); `score` and `score_bound_indexed` return the **same scores and
the same row ordering** as compiling the complete applicable set (compare
against the same query with the cap raised above the hit count, or an explicit
complete-set oracle). Membership/count alone does not pass.

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
`REINDEX`. `PreStn3` is recognized **kind-1** 0.4.0 meta only (empty,
buffer-only, or beside LSG). Kind-5 beside an LSG segment is mixed-format
**corruption**, not migration (parent §8). Kind-5 envelope
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

Single-column: `total == pages_end`, no trailer. Writer never emits a
directory. Verify does **not** sniff `FCH1` on single-column extents (§1.3.1).
A trailer on a single-column immutable segment is corruption.

### 6.3 Classification (mutually exclusive predicates)

Indexes built on this branch between 4.3 and 4.6 wrote kind-5 + STN3 + STNF v1
+ `~{h}~` keys. STN4 cannot score them without the withdrawn codec. They are
not 0.4.0, so the §8 migration sentence about LSG1–LSG4 must not fire.

**Per-segment labels** (immutable STN3 blobs only; single-column blobs are
either valid stock — stock decoder, no `FCH1` sniff — or malformed):

- `ValidV2`: well-formed STNF v2 **and** every dictionary term's extents
  satisfy §1.3 exact framing (`channels(field_count)` succeeds, including
  `child.df > 0`).
- `ValidV1`: well-formed STNF v1 (df section present, lengths sum, CRC and
  dimensions agree). Fielded-terms keys are expected; FCH1 is not required.
- `Malformed`: missing trailer on a multi-column immutable segment; truncated
  / bad-CRC / unknown-version STNF; v1 layout with `df_len = 0`; v2 layout
  with a df section; well-formed STNF v2 whose **any** term fails FCH1
  framing (including a listed channel with `child.df == 0`); an LSG1–LSG4
  segment **under a kind-5 envelope** (mixed-format; parent §8); single-column
  with a trailer; single-column extents the **stock** decoder rejects. A
  single-column extent whose first four bytes are `FCH1` is **not** malformed
  on that fact alone.

A blob is never both `ValidV1` and `ValidV2`. `Malformed` is not a version.

**Buffer labels** — computed on **every** kind-5 relation, including when
immutable segments exist. Skipping the buffer when `S` is nonempty is how a
stale or corrupt buffer would classify as `Current`. Exactly one label;
the tree below is the definition (no overlapping vacuous universals).

```
storage::legacy_fielded_key_shape(key: &str, field_count: u8) -> bool
```

**Restricted to `field_count` in `2..=16`.** Outside that range it returns
`false`. Inside: true iff `key` is `~` + one lowercase hex nibble + `~` + a
nonempty escaped token (every payload `~` doubled), and the nibble is
`< field_count`. `field_count == 1` does **not** make `~0~foo` match: `0 < 1`
would have been true, so the range guard is load-bearing. It does not produce
an encoded key, does not call `Index::term` on an encoded key, and does not
participate in lookup, expand, scoring, or writing. C.3 deletes
`fields/codec.rs`; this function stays as an isolated classifier and **runs
even when immutable segments exist**.

Decision tree (`docs` = buffer document count, `terms` = stored term count):

1. `docs == 0` ∧ `terms == 0` → `BufferEmpty` (generation-neutral).
2. `docs == 0` ∧ `terms > 0` → `BufferMalformed` (inconsistent buffer state).
3. `docs > 0` ∧ `terms == 0` → `BufferNoTerms` (generation-neutral: empty or
   discarded text; token-less records). Not `BufferEmpty`, not vacuously
   Current, not vacuously Stale.
4. `terms > 0` ∧ `field_count == 1` → `BufferCurrent` if every term is stock
   `TermData`; else `BufferMalformed`. The legacy recognizer is **not**
   consulted (`false` by the range guard). A stock key whose bytes equal
   `~0~foo` is Current.
5. `terms > 0` ∧ `field_count` in `2..=16`:
   - every term is the STN4 in-memory shape (surface key + per-field
     occurrence map from `add_occurrence`) → `BufferCurrent`. Value shape
     decides; `~0~foo` as analyzed text is still Current.
   - else every term's key satisfies `legacy_fielded_key_shape(key,
     field_count)` **and** the value is the withdrawn single-stream `TermData`
     → `BufferStale`.
   - else → `BufferMalformed` (mix; neither shape; broken in-memory channel).
6. `field_count` outside `1..=16` → `BufferMalformed` (envelope already
   corrupt; this arm is defensive).

`BufferCurrent` and `BufferStale` require `terms > 0`. They are never both
true. Immutable classification **never** uses the recognizer on immutable
dictionary keys: a ValidV2 surface token `~0~foo` stays `ValidV2`.

**Relation predicates** — exactly one. **Kind-first** (parent §8), not
segment-magic-first:

1. Recognized **kind-1** 0.4.0 meta (empty, buffer-only, or beside LSG) →
   `PreStn3`. Segment magic does not reclassify this as Corrupt.
2. **Kind-5** (valid STNM): never `PreStn3`. Let `S` be the multiset of
   immutable multi-column segment labels, `B` the buffer label. An LSG1–LSG4
   segment is `Malformed` and sets `M`.
3. Any other meta kind, or kind-5 with a bad STNM → `Corrupt`.

Generations (kind-5 only):

```
G_v2 = (S contains ValidV2) ∨ (B = BufferCurrent)
G_v1 = (S contains ValidV1) ∨ (B = BufferStale)
M    = (S contains Malformed) ∨ (B = BufferMalformed)
     ∨ (kind-5 ∧ some immutable segment magic is LSG1–LSG4)
     ∨ (single-column trailer or stock-decoder failure)
```

`BufferEmpty` and `BufferNoTerms` contribute to neither `G_v2` nor `G_v1`.
Single-column immutable blobs are not members of `S`. They are never
`ValidV1` / `ValidV2`. A coincidental `FCH1` prefix on a stock-accepted
single-column extent does not set `M`.

| Predicate | Class | Guarded callbacks |
|---|---|---|
| kind-1, recognized 0.4.0 meta (incl. empty / buffer-only / beside LSG) | `PreStn3` | existing migration error, no write |
| kind-5 ∧ `M` (incl. kind-5 + LSG) | `Corrupt` | existing corruption, no write |
| kind-5 ∧ `¬M` ∧ `G_v1` ∧ `G_v2` | `MixedFielded` | **rebuild error**, distinct string, no write |
| kind-5 ∧ `¬M` ∧ `G_v1` ∧ `¬G_v2` | `StaleFielded` | **rebuild error**, no write |
| kind-5 ∧ `¬M` ∧ `¬G_v1` | `Current` | proceed |
| any other meta kind | `Corrupt` | existing corruption, no write |

`Current` therefore requires a valid current or generation-neutral buffer:
`B ∈ {BufferCurrent, BufferEmpty, BufferNoTerms}` **and** every immutable
multi-column segment `ValidV2` (or `S` empty). A ValidV2 segment set plus
`BufferStale` is `MixedFielded`, not `Current`. A ValidV2 set plus
`BufferMalformed` is `Corrupt`. Kind-5 + LSG is `Corrupt`, **not** `PreStn3`:
it must not emit the parent §8 migration string.

`MixedFielded` requires **every** segment and the buffer well-formed.
`v1 + v2 + malformed` and `v1 + v2 + missing trailer` are `Corrupt`, not mixed.
A well-formed v2 trailer whose postings fail FCH1 is `Malformed` → `Corrupt`,
not `Current`. Empty kind-5 (`S` empty, `BufferEmpty`): `¬M ∧ ¬G_v1` →
`Current`. Kind-5 buffer-only with `docs > 0` and no terms (`BufferNoTerms`):
same, `Current`. `search` / `search_count` still answer 0 for empty; a
zero-term nonempty buffer has documents and no postings. A 0.4.0 empty index
is `PreStn3` (kind-1), unchanged.

**Fixtures (A.3 / E.1):**

| Fixture | Class |
|---|---|
| all immutable well-formed v2 + valid FCH1, `BufferEmpty` / `BufferCurrent` / `BufferNoTerms` | `Current` |
| all immutable well-formed v1, `BufferEmpty` / `BufferStale` / `BufferNoTerms` | `StaleFielded` |
| well-formed v1 + well-formed v2 only, buffer empty, NoTerms, or matching one side | `MixedFielded` |
| well-formed v2 segments + `BufferStale` | `MixedFielded` |
| well-formed v1 segments + `BufferCurrent` | `MixedFielded` |
| well-formed v2 segments + `BufferMalformed` | `Corrupt` |
| v1 + v2 + one malformed trailer | `Corrupt` |
| v1 + v2 + one multi-column segment missing a trailer | `Corrupt` |
| well-formed v2 trailer, one term with malformed FCH1 | `Corrupt` |
| well-formed v2 + valid FCH1, dictionary contains surface token `~0~foo` | `Current` |
| empty kind-5 (`docs == 0`, no terms) | `Current` |
| kind-5, `S` empty, `docs > 0`, zero terms (`BufferNoTerms`) | `Current` |
| buffer-only `BufferCurrent` | `Current` |
| buffer-only `BufferStale` | `StaleFielded` |
| buffer-only `BufferMalformed` / mix | `Corrupt` |
| kind-1 (empty / buffer-only / beside LSG) | `PreStn3` |
| kind-5 envelope + LSG1–LSG4 segment | `Corrupt` |

Rebuild error texts (`StaleFielded` vs `MixedFielded`) are distinct from the
§8 migration string. Exact strings are pinned in pg_tests. `ambuild` /
`ambuildempty` remain exempt so `REINDEX` clears rebuildable classes.
`MixedFielded` is rebuildable: `ambuild` reads the heap. `Corrupt` is not a
migration class.

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
gain STN4 work except the `StaleFielded` / `MixedFielded` classes and
`legacy_fielded_key_shape` in the opener (always, not only buffer-only).

### Carry (math and semantics; tests stay)

| File | What stays |
|---|---|
| `postgres/src/fields/score.rs` | `fused_tf` / `fused_len` / `fused_avgdl` / `fused_idf` / `saturate` / `fused_score` / `raw_tf_from_hits` / `all_fields_mask`. Bit fixtures in this file are the formula lock. |
| `postgres/src/fields/bound.rs` | `fused_bound` / `fused_interval_bound` / `next_interval_end` / envelopes / intersecting-block truncation. Input remains `FieldTerm` streams. |
| `postgres/src/fields/types.rs` | `FieldTerm`, `LogicalTerm { text, mask, df_agg, streams }`, `Lookup`. `df_agg` is filled from `Term::df()`, not from a sidecar. |
| `postgres/src/fields/cursor.rs` | `LogicalPostingCursor`, `FieldHit`, advance atomicity, successor peeks. Opened on unpacked channels. |
| `postgres/src/fields/expand.rs` | `Lookup` outcomes, empty-streams-not-error, overflow plan, scope mask on the logical term, `max_expansion` counted only after channel-set ∩ mask. Two `scan_window` consumers: capped candidates vs uncapped scoring (`expand_in`). Channel opens are a fallible walk, not `Fn(&str)` and not `Index::expand`. |
| `postgres/src/fields/error.rs` | `AdapterError` Index vs key split; query vs verify as distinct kinds. Fielded `KeyDefect` header/escape variants die with the codec. Channel framing errors are `segment::Error::Corrupt` / `AdapterError::Index`. |
| `postgres/src/storage/layout.rs` | `KIND_ENVELOPE = 5`, names/weights/stamp. |
| `postgres/src/storage/mod.rs` | `Current` / `PreStn3` / `Corrupt` fence; add `StaleFielded` and `MixedFielded`; kind-first: kind-1 → `PreStn3`, kind-5+LSG → `Corrupt`; buffer labels `BufferEmpty` / `BufferNoTerms` / `BufferCurrent` / `BufferStale` / `BufferMalformed`; `legacy_fielded_key_shape` only for `field_count` in `2..=16`, on every kind-5 buffer. |
| `segment/src/dictionary.rs` | Layout of `TermEntry` and prefix-compressed blocks. No new fields. |
| `segment/src/payload.rs`, `ordinals.rs`, `bound.rs` | Stock stream codecs. Channels are those codecs. |

### Replace

| File | Fate |
|---|---|
| `postgres/src/fields/codec.rs` | **Dead as a codec.** `fielded_key` / `header` / `upper_fence` / writers go. C.3 deletes this file. The `~{h}~` **grammar** survives only as `storage::legacy_fielded_key_shape` (§6.3), `field_count` in `2..=16`, which never looks up or writes. |
| `postgres/src/fields/expand.rs` lookup/expand bodies | Decoded `Window`; `Index::scan_window`; two consumers (a) capped candidates O(`max_expansion`) continue-past-overflow (b) uncapped scoring of every applicable term, independent of the discarded retrieval list; text `Fn(&str)` only; error > Overflow > Terms for (a); error and never Overflow for (b). Must not call `Index::expand` when `field_count` in `2..=16`. |
| `postgres/src/fields/df.rs` | `query_total_df` reads entry dfs. `union_df_agg` remains a **build-time** helper to compute the stored union (and a verify check). Sidecar reader goes with STNF v1. |
| `segment/src/trailer.rs` | v2: drop `df_len` / `DfEntry`. v1 parse remains long enough to classify `StaleFielded`. |
| `segment/src/segment.rs` | `Term::channels()`. `Reader::new` still stops stock checks at `pages_end`; trailer API reads v2. |
| `segment/src/index.rs` | `Index::scan_window` streaming cursor. Mutable: `add_occurrence(token, field, positions, field_length)`; per-token per-field builders; flush writes `FCH1` and omits zero-posting fields. |
| `segment/src/merge.rs`, `verify.rs` | Per-channel stock merge/verify; recount union into `TermEntry.df`; no df sidecar add. Multi-column verify unpacks `channels()` (including `child.df > 0`). Single-column verify is stock-only: no `FCH1` prefix sniff. |

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
  - Scope: encode/decode `FCH1`; §1.3 exact framing (header+records fit;
    checked `5+5n` and `Σ len`; `Σ len == extent_len - (5+5n)`; child decoder
    consumes the slice exactly; `len == 0` rejected; `child.df > 0` on every
    listed record; ordinal/payload field lists equal and ordered; `field_count`
    not in `2..=16` is Corrupt even on a mistaken call; §1.3.1 choice (a)).
    Child `df` from count varint; `max_tf_bucket` from stream bounds. No SQL.
  - Done when: unit tests cover that matrix; `channels(1)` and `channels(17)`
    are Corrupt; trailing bytes in a child slice are Corrupt; mismatched
    ordinal/payload field sets are Corrupt; child.df is posting count not byte
    `len`; a listed record with stock `count = 0` is Corrupt; flush omits
    zero-posting fields from both directories; writer never emits a directory
    on single-column; **single-column verify does not sniff `FCH1`** — a stock
    payload whose first four bytes are `46 43 48 31` still verifies when the
    stock decoder accepts it; missing magic on multi-column is corruption via
    `channels()`, not a prefix test shared with single-column; stock `term()`
    still works on 0.5.0 single-column fixtures.
- [ ] **A.2 Mutable flush + merge recount**
  - Scope: `add_occurrence(token, field, positions, field_length)`; per-token
    per-field builders; parent `TermEntry.df` = union including dead; merge per
    channel then recount; forbid add of channel dfs. **Both** heap `CREATE
    INDEX` (SQL build) **and** incremental `INSERT` after the index exists.
  - Done when: two-field fixture with overlap counts union 3 not 4 on SQL build
    **and** on insert-after-create; dead ordinal remains in `df` until rewrite;
    merge of two segments recounts; `field_length` is that field's raw count;
    a token posted in title only does not write a body directory record.
- [ ] **A.3 STNF v2 + classification matrix**
  - Scope: trailer without df; §6.3 predicates (`ValidV1` / `ValidV2` /
    `Malformed` plus `BufferEmpty` / `BufferNoTerms` / `BufferCurrent` /
    `BufferStale` / `BufferMalformed` → relation class); kind-first precedence;
    `legacy_fielded_key_shape` only for `field_count` in `2..=16` on every
    kind-5 buffer; rebuild strings; `ambuild` exempt.
  - Done when: every fixture row in §6.3 holds, including v1+v2+malformed →
    `Corrupt`; v1+v2+missing trailer → `Corrupt`; v2 + malformed FCH1 →
    `Corrupt`; valid v2 with surface token `~0~foo` → `Current`; valid v2
    segments + `BufferStale` → `MixedFielded`; valid v2 + `BufferMalformed` →
    `Corrupt`; valid v1 + `BufferCurrent` → `MixedFielded`; buffer-only
    `BufferCurrent` → `Current`; buffer-only `BufferStale` → `StaleFielded`;
    empty kind-5 → `Current`; kind-5 `BufferNoTerms` alone → `Current`; v2 +
    `BufferNoTerms` → `Current`; v1 + `BufferNoTerms` → `StaleFielded`;
    well-formed v1-only → `StaleFielded`; well-formed v1+v2 only →
    `MixedFielded`; kind-1 + LSG → `PreStn3` (the §8 migration string);
    kind-5 + LSG → `Corrupt` (not that string). No page dirty on
    rebuild/corrupt classes.

### Phase B — Query wiring (≈ 1.5 wks)

- [ ] **B.1 Lookup / expand / cursor on channels**
  - Scope: replace expand/lookup bodies; `Index::scan_window`; two consumers
    (capped candidates vs uncapped scoring); `df_agg` from parent `Term::df`;
    expansion order §2 steps (1)(2)(3) on (a) only; drop codec from this path.
    Multi-column must not call `Index::expand`.
  - Done when: fields unit tests that used encoded keys are rewritten against
    surface tokens; empty streams not error; overflow still `Lookup::Overflow`
    on the **candidate** path only; scoped mask does not change `df_agg`
    (`LogicalTerm.df_agg` is the parent `Term::df()`, never a channel-local
    stream length). **Scoped cap fixtures:** for prefix/wildcard, range, regex,
    and fuzzy, `title:(…)` with body-only hits `> max_expansion` and title hits
    `≤ max_expansion` returns `Terms` of the title tokens, not Overflow;
    in-scope over cap still Overflows for candidates. **Streaming fixtures:** a
    window with far more out-of-scope matches than `max_expansion` retains at
    most the cap on (a); grep the multi-column adapter for `Index::expand` is
    empty; (a) is not implemented as `usize::MAX` + `Vec`. **Over-limit SQL
    scoring fixture:** the same in-scope over-cap query: `search` / `search_count`
    keep overflow membership/count; `score` and `score_bound_indexed` match the
    complete applicable set's **scores and row ordering** (not membership/count
    alone). Scoring must not reuse the discarded candidate list. **Channel-error
    fixtures:** a window that also contains a corrupt FCH1 term returns
    `AdapterError` on both consumers, not Overflow and not `Terms`/`scores` that
    skipped it; the same error wins when in-scope hits already exceeded the cap
    (candidate cursor continues past overflow); `Fn(&str)` is never the
    channel-open site.
- [ ] **B.2 Bound + verify on channels**
  - Scope: wire existing `fused_bound` to unpacked `FieldTerm`s; no parent-nibble
    fallback; verify recomputes per-channel min_len/max bucket and parent `df`.
  - Done when: 0..1000/500 witness still truncates; pruned == exhaustive
    top-k rows; bound-read `Err` still INFINITY; two unit-weight channels score
    `tf* = 2` and a parent-nibble bound of 1 is never consulted; missing
    `channels()` on multi-column bound → INFINITY, score → error; verify
    rejects listed `child.df == 0`; single-column verify does not sniff `FCH1`.

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
  - Scope: delete `fields/codec.rs` and encoded-key **writers** / lookup
    fixtures; `fields/mod.rs` comment matches §7. **Keep**
    `storage::legacy_fielded_key_shape` as the only `~{h}~` grammar — buffer
    classification on every kind-5 buffer (including beside immutable
    segments); `field_count` not in `2..=16` returns false; no encode helper,
    no `Index::term` on encoded keys.
  - Done when: no `fielded_key` writer in `postgres/src` or `segment/src`;
    grep for encode/header/upper_fence in `fields/` is empty; classifier tests
    (buffer-only stale vs current; v2 segments + stale buffer → MixedFielded;
    `BufferNoTerms` alone and beside v1/v2; `legacy_fielded_key_shape("~0~foo",
    1) == false`) still compile; workspace tests green.

### Phase D — Phrases, highlights, planner (old Phase 5) (≈ 2 wks)

- [ ] **D.1 Field scope in tinql + operator** (old 5.1)
- [ ] **D.2 Same-field phrases + spans** (old 5.2) — positions from channels
- [ ] **D.3 Field-aware highlights + snippets** (old 5.3)
- [ ] **D.4 Full planner coverage** (old 5.4)
- [ ] **D.5 pgembed checkpoint** (old 5.5 / 4.7 folded here if C.2 is green)

Done-when texts of old 5.1–5.5 apply, with "fielded key" read as "channel".

### Phase E — Packaging + release (old Phase 6) (≈ 3 wks)

- [ ] **E.1** Two-artifact migration CI (old 6.1) — replay the §6.3 fixture
      table: well-formed v1 rebuild-errors (`StaleFielded`), not LSG4;
      well-formed v1+v2 → `MixedFielded`; v1+v2+malformed and v1+v2+missing
      trailer → `Corrupt`; v2+bad FCH1 → `Corrupt`; v2 with token `~0~foo` →
      `Current`; valid v2 + stale buffer → `MixedFielded`; valid v2 +
      malformed buffer → `Corrupt`; kind-5 + LSG → `Corrupt` (not the §8
      migration string); kind-1 + LSG → `PreStn3`; `BufferNoTerms` alone →
      `Current`; v2 + `BufferNoTerms` → `Current`; v1 + `BufferNoTerms` →
      `StaleFielded`.
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
- **`StaleFielded` vs `Corrupt` vs `MixedFielded`.** Kind-first: kind-1 is
  `PreStn3`; kind-5 + LSG is `Corrupt`. Mixed requires every segment **and the
  buffer** well-formed. Buffer labels are an exclusive tree (`BufferNoTerms`
  for `docs > 0` with no terms). The legacy recognizer is `2..=16` only. A.3
  pins the fixture table.
- **Channel-open errors in expand.** `Fn(&str)` cannot carry `Result`. Multi-column
  expansion is `scan_window` only. Candidate (a): O(`max_expansion`) retained
  terms, continue past overflow. Scoring (b): every applicable term, independent
  of the discarded retrieval list. First FCH1 error wins on both. `Index::expand`
  is not an allowed multi-column implementation of either consumer.
- **Capped retrieval vs uncapped scoring.** Parent §5.1 `expand_in` must keep
  the complete scoring set when candidates Overflow. B.1's over-limit SQL
  fixture locks scores and row ordering, not membership/count alone.
- **Channel/parent df confusion.** Channel `Term::df()` is the ordinals count
  varint (field-local). Using it as idf reintroduces the field-local-idf break
  §5.2 case 1 catches. Tests must read the parent entry for `LogicalTerm.df_agg`.
- **Parent nibble is not a bound.** Two unit-weight channels fuse `tf* = 2`
  above parent max 1. Multi-column bound/score without `channels()` is INFINITY
  / error, never a stock fallback.
- **Scoped expansion cap.** Counting out-of-scope tokens against `max_expansion`
  changes 0.4.0 answers. B.1 fixtures are the lock.
- **Verify must not call stock `ordinals()` on a multi-column parent extent.**
  That path is corruption on a valid STN4 term. Multi-column verify always
  unpacks `channels()` first. Single-column verify must not sniff `FCH1`
  (§1.3.1): payload count 70 is `0x46`.
- **Zero-document listed channels.** Stock codecs accept `count = 0`.
  `channels()` requires `child.df > 0`; flush omits empty fields from both
  directories.
- **Upstream merges.** `segment.rs` and merge/verify already conflict on STNF.
  `Term::channels` is more surface in those files. The dictionary layout and
  `Index::term(&str)` stay upstream-shaped; `scan_window` is the owned streaming
  add, not a change to `expand`'s `Vec`/`Overflow` contract.
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
