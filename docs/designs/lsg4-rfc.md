# LSG4: Multi-Field Segment Format for BM25F — RFC

- **Status:** proposed — freeze candidate
- **Date:** 2026-09-22
- **Basis:** `main@6d02ec7`
- **Delivers:** P0-2 of `docs/plans/stannum-p0-agent-features-2026-09-22.md`
  (work item WI-6, implementation-order step 10)
- **Gates:** implementation-order steps 11-13 start only after this RFC is
  accepted (resolved question 1). `Format::CURRENT` stays `Lsg3` until then;
  no persisted LSG4 bytes ship before the freeze; no released intermediate
  LSG4 interpretation.

## 1. Summary

This RFC freezes the on-disk format, scoring semantics, query-language
behavior, and golden-vector schema for **LSG4**: a field-aware segment format
that stores per-field term frequencies and per-field document lengths so a
multi-column Stannum index can score BM25F (`title:3.0,body:1.0` style
weighted fields).

LSG4 is a *superset* of LSG3 discipline, not a redesign: the dictionary entry
layout, the postings body layout, the payload skip-table framing, and the
segment area order are all unchanged. What changes is that three streams gain
a field dimension (length table, payload entries, score bounds), the forward
record gains a per-term-group field id, and the meta page gains a fields
trailer that names the fields and their weights.

Everything below is normative. Each item maps to a gate in the plan's RFC-gate
list; §11 is the acceptance checklist.

## 2. Non-goals

- No code changes ship with this RFC. The RFC is the entire P0-2 deliverable
  this cycle.
- No per-field document frequency in the dictionary (design decision: bloat;
  see §5.5).
- No field axis inside `boldi-vigna::Interval` (§5.11).
- No planner work: field-qualified terms are evaluated by existing scan
  machinery plus a payload-filter cursor (§5.11).
- No change to LSG1/LSG2/LSG3 bytes, ever (§5.9).

## 3. Definitions

| Term | Meaning |
|---|---|
| field | One indexed key attribute of a multi-column index, identified by `field_id` = its ordinal in index attribute order (0-based). |
| `MAX_FIELDS` | 16. Frozen for `layout_revision` 1 (resolved decision #8). |
| packed entry byte | `field_id << 4 | tf_bucket` — one byte carrying both, because both fit in four bits. |
| dequantize(bucket) | `TfBucket::representative_count() as f32` (tf_bucket.rs:48-52). |
| selected fields | The fields a scoring term applies to: all fields for an unscoped term; the operator's field (or explicit field group) otherwise. |
| `FieldMask` | A `u16` bitmask over field ids, the query-side widening of the scoring key (§5.11). |
| fields trailer | Meta-page trailer record `FIELDS_TAG = 0x02` carrying field names and weights (§5.7). |

## 4. DDL and field plan

```sql
CREATE INDEX docs_search ON docs USING stannum (title, body)
    WITH (field_weights = 'title:3.0,body:1.0');
```

- `field_weights` is a string reloption, registered like `score_stop_words`.
- `amoptions` checks **syntax only**: comma-separated `name:float`, unique
  names, finite weights `> 0`, no `,` or `:` inside names.
- `ambuild`/AM-validate checks **relation-aware rules**: names are a
  permutation of the index column `attnames`, `indnatts >= 2`, and
  single-column + weights is an ERROR (`field_weights applies to
  multi-column stannum indexes`).
- Omitted weights default to `1.0` per column.
- `FieldPlan { names, weights }` is held in **index attribute order**;
  `MAX_FIELDS = 16`.
- Multi-column indexes reject expression keys and INCLUDE columns in the
  initial release.
- Field **names live only in the meta trailer**, never in segment bytes; a
  segment is weight- and name-agnostic (pinned by golden vector
  `three_fields_weights`, §7).

## 5. Frozen format specification

### 5.1 Segment header

LSG3 today, for contrast (segment.rs module doc, :8-15):

```text
blob := magic "LSG3", doc_count varint, total_length varint,
        dictionary_len varint, postings_len varint, payload_len varint,
        docs_len varint, dictionary, postings_area, payload_area, docs, lengths
```

`Reader::new` reads only `min(total, 64)` bytes (segment.rs:416) and enforces
`lengths_at + doc_count * 4 == total`.

LSG4:

```text
blob := magic "LSG4", layout_revision varint, doc_count varint,
        total_length varint, field_count varint,
        field_total u64le × field_count,
        dictionary_len varint, postings_len varint, payload_len varint,
        docs_len varint,
        dictionary, postings_area, payload_area, docs, lengths
```

Field by field:

| Field | Encoding | Notes |
|---|---|---|
| `magic` | `b"LSG4"` | 4 bytes; `Format::from_magic` accepts all four magics. |
| `layout_revision` | varint | **MUST be 1.** Any other value is rejected (fail closed). This is the widening path: 32 fields or any header change ships as revision 2. |
| `doc_count` | varint u32 | Documents in the segment. |
| `total_length` | varint u64 | Unweighted Σ over all fields and documents of field lengths. |
| `field_count` | varint | `1..=16` in blobs (see §5.8 for why 1 is valid on disk and §5.7 for the trailer's `2..=16`). |
| `field_total` | `field_count × u64le` | Per-field unweighted totals, in field order. Fixed width (not varint) so the header size is bounded and the head probe is provable. |
| area lens | varint u32 × 4 | As LSG3. |
| areas | bytes | Dictionary, postings, payload, docs — layouts per §5.3-§5.5. |
| lengths | doc-major rows | §5.2. |

Invariants, all checked at open (`Error::Corrupt` on violation):

1. `lengths_at + doc_count * field_count * 4 == total`
2. `Σ_{f<field_count} field_total_f == total_length` (checked `u64` adds)
3. `1 <= field_count <= 16`
4. `doc_count == 0` is legal (empty segment; all field totals are 0).

The LSG3 invariant stays in the LSG3 parser; the two are never mixed.

**Head probe (post-critique resolution #14).** `Reader::new` keeps the
64-byte probe exactly as today (segment.rs:416). On `LSG4` magic it re-reads
an extended head of `min(total, 256)` bytes *before* parsing varints. The
worst-case 16-field header is 169 bytes — 4 magic + 1 revision + 5 doc_count
+ 10 total_length + 1 field_count + 128 field totals + 20 area lens — which
overruns 64; 256 covers it with 87 bytes of margin. LSG1-3 parse from the
64-byte probe with no re-read and no behavior change.

`Sections::header` (= `dictionary_at`) now includes the field-totals block;
size accounting follows automatically.

### 5.2 Length table

```text
lengths := row × doc_count, in document-table (TID) order
row     := field_count × u32le     (document ordinal major, field minor)
```

The length of document ordinal `o`, field `f` is the `u32le` at
`lengths_at + (o * field_count + f) * 4`.

Frozen accessor rules:

- `Index::field_length(ordinal, field)` and `AreaFetch::field_length(...)`
  are the general accessors; `Index::field_count()` (default `1`) and
  `Index::field_total(field)` join them on `trait Index` (index.rs:41).
- `length(ordinal)` / `length_at(ordinal)` keep their LSG3 meaning and, on an
  LSG4 source, return **field 0's** length. LSG3 callers are unaffected.
- A document's unweighted total is `Σ_f field_length(ordinal, f)`; this is
  what `ForwardRecord::doc_len` carries on LSG4 (§5.6), consistent with
  `total_length`'s definition.
- **Chunk straddle:** `Lengths::Bytes(&[u8])` has no field count in the type
  today and indexes `ordinal * 4` (segment.rs:722-742). It becomes
  `Lengths::Fields { bytes, field_count }` (the `Lazy` variant gains the
  count too). `LENGTH_CHUNK = 4096` is byte-granular and the reader caches
  one chunk (segment.rs:402), so a document's field row — up to
  `16 × 4 = 64` bytes — can straddle a chunk boundary. `field_length`
  therefore fetches the **whole document row** (or a two-chunk window), never
  a single `u32`.

### 5.3 Payload entries

LSG3 today, for contrast (payload.rs:12-29):

```text
stream := count varint, skip u32le * slots, data
slots  := ceil(count / SKIP_INTERVAL) - 1            (SKIP_INTERVAL = 32, payload.rs:34)
entry  := tf_bucket u8, n varint, position varint * n
          positions: first absolute, then (delta - 1)
```

LSG4 entry (one per posting, i.e. per document, per term):

```text
entry  := field_hit_count varint, group+
group  := packed u8, n varint, position varint * n
packed := field_id << 4 | tf_bucket
          positions: first absolute, then (delta - 1), within one field
```

- `field_hit_count` groups follow, in **ascending field id** order.
- The skip table stays **LSG3-shaped**: ordinal `i` = posting `i`, slot `i`
  holds the byte offset of entry `(i + 1) * 32`, entry 0 has no slot, fixed
  `u32le`. Entries are self-delimiting, so the framing needs no change.
- The decoded form is `Entry { fields: Vec<FieldHit> }` with
  `FieldHit { field: u8, tf_bucket: u8, positions: Vec<u32> }`.

**Validation set (complete — every rule is a reader-side check that fails
with `Error::Corrupt`):**

1. `1 <= field_hit_count <= field_count` — a term hits at least one field and
   cannot hit more fields than exist.
2. Field ids **strictly increasing** across the entry's groups.
3. `field_id < field_count` for every group (implied by 1+2; checked at the
   boundary group).
4. The bucket nibble is `0..=15` by construction (four bits); its binding
   check is rule 8.
5. `position_count > 0` per group — a field hit carries at least one
   position (the existing `n == 0` rejection, payload.rs:126-129 and
   :142-145).
6. Positions **strictly increasing within each field group**, with the
   existing cumulative-overflow check (payload.rs:141-160). Positions are
   independent per field: two fields may both start at 0.
7. The `field_hit_count` groups consume the entry's bytes exactly, and the
   whole data area is consumed exactly across `count` entries.
8. **Bucket-quantization cross-check:** for every group,
   `TfBucket::from_count(n).value() == (packed & 0x0F)`, where `n` is the
   group's decoded position count. This is the corrupted-bucket cross-check:
   a flipped bucket byte is detected even when the position list decodes
   cleanly. The maintenance validator already performs exactly this check
   for LSG3 payloads (maintenance.rs:245-248,
   `Error::Corrupt("payload frequency bucket")`); LSG4 extends it to every
   field group of every entry.

LSG1-3 payload bytes are unchanged.

### 5.4 Postings score bounds

LSG3 today, for contrast: `term_bound := buckets varint, min_len varint per
set bit` (postings.rs:193-201, decode at :663); per-block table entries add
`last_block` delta and `last_offset` varints (sparse streams also a block
start delta); `FORM_TERM_BOUND` (bit 4, postings.rs:70) replaces the table
for single-block streams; `BlockBound { min_len: [u32; 16], last: Tid }`
(postings.rs:78-84).

LSG4 term-bound payload:

```text
term_bound := field_mask varint,
              (bucket_mask varint, min_len varint × set_bits)+     per set field bit, ascending
field_mask := bit i set: field i occurs in the block
bucket_mask:= bit b set: bucket b occurs in field i within the block
min_len    := shortest field-i length among the block's postings
              that carry field i at bucket b
```

The envelope is unchanged: `form u8, count varint, [bounds_len varint, table
| term_bound], body`; `BLOCK_POSTINGS = 128` (postings.rs:58); a single-block
scored stream stores the term bound with no last location (resolved by
walking the stream, as today).

**Validation set (complete):**

1. `field_mask != 0` — a scored block has at least one field.
2. `field_mask >> field_count == 0` — no field id at or beyond `field_count`.
3. Per set field: `bucket_mask != 0` and `bucket_mask >> 16 == 0`
   (`BUCKET_COUNT = 16`).
4. `min_len < u32::MAX` for every `(field, bucket)` pair — mirroring
   `decode_term_bound`'s rejection of the absent marker (postings.rs:672-674).
5. Table framing unchanged: `last_block` delta with checked add, last TIDs
   strictly increasing across entries, sparse block starts increasing and
   `< body_len`.
6. Term-bound form: `1 <= count <= BLOCK_POSTINGS`.

**Types (frozen sketch):**

```rust
pub struct FieldBlockBound {
    pub field_count: u8,            // 1..=16
    pub present_fields: u16,        // bit i: field i occurs in the block
    pub max_tf_bucket: [u8; 16],    // per field; highest occurring bucket (0 when absent)
    pub min_doc_length: [u32; 16],  // per field; shortest occurring field length (u32::MAX when absent)
    pub last: Tid,
}

pub enum ScoreBound { Single(BlockBound), Fields(FieldBlockBound) }
```

Semantics:

- `FieldBlockBound::over(entries: &[(field: u8, bucket: u8, field_len: u32)],
  last: Tid, field_count: u8)` folds per-`(field, bucket)` minima and derives
  the per-field aggregates (`max_tf_bucket[f]` = highest set bit;
  `min_doc_length[f]` = min over set buckets).
- `FieldBlockBound::merge` (sibling of `BlockBound::merge`, postings.rs:101):
  `present_fields` = union, `max_tf_bucket` = per-field max,
  `min_doc_length` = per-field min, `last` = max. LSG3's `merge`/`over`
  (postings.rs:101, :118) are untouched.
- `PostingsBuilder::push_scored_fields(tid, &[(field, bucket)], field_lens)`
  is the LSG4 score entry point; `push_scored` (postings.rs:152) is unchanged
  for LSG3. `field_lens` is the document's full per-field length row.

**Bound evaluation (frozen):**

```text
max_tf*  = Σ_{f ∈ selected} w_f · dequantize(max_tf_bucket[f])
min_len* = Σ_{f ∈ selected} w_f · min_doc_length[f]
bound    = saturate(max_tf*, min_len*)        // the §5.10 f32 expression
```

- `selected` is the query term's field mask — all fields for an unscoped
  term, the scoped field(s) otherwise.
- Safe in both directions: for every posting `p` in the block,
  `tf*(p) = Σ_f w_f · dequant(b_f(p)) ≤ Σ_f w_f · dequant(max_tf_bucket[f])`
  and `len*(p) ≥ Σ_f w_f · min_doc_length[f]`. The per-field maxima and
  minima may come from different postings, so the bound can be loose — never
  wrong. Underestimating length overestimates score, which is the safe
  direction for pruning.
- Summing over **all present fields** instead of `selected` is also safe but
  looser exactly on field-scoped queries — the case the feature exists for.
- Expected consequence: **fewer prunes than LSG3** (weaker WAND recall).
  Measured through the explain prune identity; not a bug.

### 5.5 Dictionary (unchanged)

`TermEntry { df, max_tf_bucket, postings, payload }` (dictionary.rs:53-60),
`df_bucket := df << 4 | max_tf_bucket`, zigzag gap extents — LSG4 uses the
LSG3 entry layout verbatim. Frozen semantics:

- `df` = number of documents containing the term **in any field**.
- `max_tf_bucket` = max across fields.
- **No per-field df** (design decision: dictionary bloat). Field-restricted
  estimates use aggregate df as a conservative upper bound (§5.11).

### 5.6 Forward records

LSG3 today, for contrast (forward.rs:12-17):

```text
record := len varint, block varint, offset varint, doc_len varint,
          term_count varint, term*
term   := shared varint, suffix_len varint, suffix, n varint, position varint * n
          terms sorted, unique
```

LSG4 record:

```text
record := len varint, block varint, offset varint, doc_len varint,
          term_count varint, term*
term   := field_id varint, shared varint, suffix_len varint, suffix,
          n varint, position varint * n
```

- One term group per `(field, term)` pair. Groups are ordered by
  **(term bytes ascending, field_id ascending)**; the same term may appear in
  several groups (one per field it occurs in); `(term, field)` pairs are
  unique within a record. Prefix compression applies to the term bytes
  relative to the previous group, as today.
- Positions are strictly increasing **within one field group**; sequences
  are independent across fields (both may start at 0).
- `doc_len` = unweighted token total across fields.
- The in-memory `ForwardRecord` gains `field_lengths: Vec<u32>` — empty for
  the fieldless codec, `field_count` entries for LSG4. Decoders recompute it
  from the term groups (per-field position counts); encoders do not write it
  (it is derivable), so the bytes carry field ids only. This gives the
  mutable-index evaluator per-field lengths without a second byte-layout
  axis.
- **Codec discriminator: the meta fields trailer, not the byte layout.**
  Records in a buffer belonging to an index without a fields trailer decode
  with the old codec and every term is field 0. There is no in-record flag
  and no layout probe.
- `ForwardRecord::peek` / `RecordHeader` are unchanged (the field id is per
  group, below the fixed header).

### 5.7 Meta fields trailer (TAG 0x02)

Framing is the P0-3 generic trailer: `record := tag u8, payload_len u32le,
payload`; `FIELDS_TAG = 0x02`.

```text
payload := record_version u8 = 1,
           field_count u8 (2..=16),
           reserved u16 = 0,
           per field, in index attribute order:
               name_len u16le, name_utf8, weight f32le
```

Validation (all `Err` on violation):

1. `record_version == 1`.
2. `field_count` in `2..=16` — a one-field trailer is invalid; single-column
   indexes have no trailer and stay LSG3 (§5.8).
3. `reserved == 0`.
4. `name_len > 0`; name bytes valid UTF-8; names unique within the record.
5. `weight` finite and `> 0` (mirrors the reloption rule).
6. `payload_len` exactly consumed by the parsed content.
7. Duplicate `FIELDS_TAG` records and unknown tags are rejected by the
   generic trailer rules (fail closed).

Write and round-trip rules:

- Written **only** by the meta-initialization path used by multi-column
  CREATE INDEX / REINDEX. Insert, fold, merge, and VACUUM round-trip the
  decoded trailer unchanged — an upgraded binary never rewrites it on first
  insert.
- **Open:** recorded names are compared with the current heap attributes;
  a mismatch (column rename) is an ERROR requiring REINDEX.
- **`ALTER INDEX … SET (field_weights = …)` is a fail-closed ERROR
  (`REINDEX to change field_weights`)** until an alter hook exists and is
  covered by a test. Never a silent reloption/meta split.
- The insert-path meta recheck compares the fields-tag bytes next to
  `(identity, spec)`, so a concurrent REINDEX changing field count cannot
  publish a mismatched forward record.
- The insert path never re-stamps `analysis` either; it round-trips what it
  decoded, so a retry cannot publish a stamp the rebuilt index does not have.

### 5.8 Single-column-stays-LSG3 rule

1. `Format::CURRENT` **stays `Lsg3`** (segment.rs:66) — permanently, not just
   until acceptance. LSG4 is written only by the field-aware builder entry
   point; no caller's default changes. This structurally removes the
   "partial land flips CURRENT" risk from the plan's risk list.
2. A single-column index writes LSG3 indefinitely. Postgres passes `Lsg4`
   into finish/merge only when `field_count >= 2`.
3. An index never mixes LSG3 and LSG4 segments; **LSG3 × LSG4 merges are
   refused**.
4. LSG4 blobs with `field_count == 1` are **valid on disk** (the parser
   accepts `1..=16`). They exist so the bit-equality fixture (§5.10) can
   compare a one-field LSG4 blob against the LSG3 blob of the same tokens.
   The product write path never produces them; the meta trailer's
   `2..=16` rule is the product-level gate.
5. Multi-column indexes reject expression keys and INCLUDE columns in the
   initial release.

### 5.9 Old-format read behavior

- `Format::from_magic` accepts `LSG1`, `LSG2`, `LSG3`, `LSG4`. LSG1-3 byte
  layouts are untouched by this RFC; their fixtures stay byte-identical.
- An LSG4 blob opened by a pre-LSG4 binary fails with
  `Error::Corrupt("segment magic")` — the same rejection today's code gives
  an unknown signature. (segment.rs's `empty_segment_and_corruption` test
  currently flips a blob to `LSG4` to assert exactly that; it is updated
  when LSG4 lands.)
- An index carrying a fields trailer does not open on a pre-P0-2 binary
  (the meta decoder rejects trailing bytes). Accepted rollback limit, same
  shape as the analysis stamp; documented, no on-disk downgrade.
- Old forward bytes decode as field 0 under the old codec, selected by the
  meta trailer (§5.6).
- LSG3 segments remain readable forever; upgrades never rewrite them.
- Postgres branches on `Format::has_fields()` for LSG4 (today it never
  branches on `Format`); the branch is confined to the write path
  (§5.10 note) and the reader's field accessors.

### 5.10 BM25F scoring

Per-term contribution on an LSG4 source:

```text
tf*         = Σ_f w_f · dequantize(tf_bucket_f)     one dequantize per field; never re-quantized
len*        = Σ_f w_f · length_f
saturation  = the TermScorer f32 expression with tf := tf*, length := len*
contribution = idf · boost · saturation             idf stays aggregate-corpus
```

The saturation expression is exactly today's (bm25.rs:291-416, `score_bucket`
at :348):

```text
multiplier = idf * boost
numerator  = multiplier * tf * (k1 + 1.0)
score      = numerator / ((tf + k1 * (1.0 - b)) + (k1 * b / avgdl) * length)
```

- `dequantize(bucket)` = `TfBucket::representative_count() as f32`
  (tf_bucket.rs:48-52). `tf*` is used directly; it is never passed back
  through `TfBucket::from_count`.
- `idf` = `bm25_idf(total_docs, df)` (bm25.rs:283) with aggregate `df`
  (§5.5) — scoped idf is not a thing.
- **Weighted average length** = `Σ_f w_f · field_total_f / document_count`,
  computed once in `build_index_scorer`: per-field totals are aggregated in
  `u64` across sources (exactly as `total_length` is today, score.rs:1285),
  then one left-to-right `f32` fold in field order; `document_count == 0`
  yields `1.0` (mirroring score.rs:1286-1288).
- Terms still combine through `sum_scores_in_order` (bm25.rs:409) in
  canonical `(term bytes, field mask)` order.
- `TermScoreModel::{Bm25, Bm25f}` in the score readers. `Bm25f` holds the
  weights and evaluates the expression per posting from its field hits; the
  §5.4 bound is evaluated by the same expression at `(max_tf*, min_len*)`.

**Bit-equality requirement (R-BIT).** A single-field weight-`1.0` fixture
must be **bit-equal** (`f32::to_bits`) to the LSG3 scorer on the same tokens:

- Arithmetic level: for every bucket `b ∈ 0..15` and length `L` over a
  covering grid, `Bm25f` with fields `[(w = 1.0)]`, hits `[(field 0, b)]`,
  length `[L]`, must equal `TermScorer::score_bucket(TfBucket(b), L)`
  bit-for-bit. This pins: `tf* = 0.0 + 1.0 · rep(b)` and
  `len* = 0.0 + 1.0 · L` are exact; the multiply order
  `multiplier * tf * k1_plus_one` is preserved; `avgdl*` reduces to
  `total_length as f32 / total_docs as f32` in the one-field case (the
  `Σ field_total == total_length` invariant makes the `u64` aggregates
  identical).
- End-to-end level: the one-field LSG4 golden vector (§7) scores bit-equal
  through the full scorer against the LSG3 blob of the same tokens.

### 5.11 Field-scope semantics and query language

**Grammar (frozen):**

- `field_head = ${ field_name ~ "(" }` — compound-atomic (a space in
  `title: (foo)` is not field syntax); first alternative of `base`, before
  `word_primary`.
- `field_name = bare_ident | quoted_ident`;
  `bare_ident = @{ ASCII_ALPHA ~ (ASCII_ALPHANUM | "_")* }`, matched with
  PostgreSQL ASCII case-fold; quoted identifiers use the phrase escape rule
  and match bytes exactly.
- `title:foo` is unchanged — one word (`:` is a word char today).
- **The grammar change ships only together with the executor-side
  unknown-field error.** Shipped alone, `title:(foo)` changes meaning today
  (word `title:` AND a group) and then fails inside eval.

**AST and resolution:**

- `Expr::Field { name, inner }`; every `match` on `Expr` is updated (no
  wildcard arms): subtokenize (rewrite inner, keep the wrapper; no name
  prefix on terms — the dictionary stays one entry per term string),
  simplify, display/quote, estimate, retrieval, plan/eval, span_expr,
  position filters.
- Name resolution at scorer build: unknown field → ERROR
  `stannum: unknown field '<name>'`; field syntax on a fieldless index →
  ERROR requiring a multi-column index.

**Scoring keys:**

- The scoring key widens to `(text, FieldMask)`. An unscoped term = all
  fields. Duplicate `(text, mask)` entries combine boosts; identical text
  with different masks stays separate; scoped idf remains aggregate.

**Retrieval:**

- Field-qualified non-positional terms need a **payload-filter cursor**:
  postings alone cannot field-restrict because `df` is aggregate (§5.5), so
  each candidate's payload entry is decoded and its field set checked.
  Estimates use aggregate df as a conservative upper bound.
- Phrases/spans: partition positions by field, run the boldi-vigna solver
  **per field**, union matches. Intervals are never compared across fields;
  no field axis is added to `boldi-vigna::Interval`.

**Operator semantics (multi-column `==>`):**

- Precondition (phase-1 dependency): today a multi-column stannum index
  cannot reach the operator path at all — suitability is
  `is_stannum && indisvalid && indisready && indnkeyatts == 1`
  (score.rs:1946-1947) and the clause matcher reads `indkey.values[0]` as
  *the* key attribute (:1949/:1955). Widening that gate and carrying the
  per-clause attribute is part of P0-2 phase 1 (score.rs:1943-1960 joins the
  file list).
- The SQL operator still has one text left operand. `title ==> 'foo'`
  restricts unscoped terms to `title`: **the scan key's attribute defines an
  implicit outer field scope**.
- An explicit field group naming another field in that operator context is
  rejected; all-fields queries use `stannum.search()`.
- The scan-key attribute number is carried through custom-scan private state
  and bitmap keys — a `body` match must never satisfy `title ==> …`.
- Heap fallback and recheck evaluate only the left operand's field; the
  concrete path is `passes_clause` inside `search_access`
  (customscan.rs:1416) and the count path (:1514); `search_recheck`
  (:1468-1474) stays a `true` stub.

**Phrase scope rule (frozen):** an unscoped phrase matches when **any ONE
field** contains it (the Lucene rule) — never across fields. A field-scoped
phrase matches only within that field.

**Highlights (phase 3):** internal highlight positions gain a field id;
`stannum.highlight` gains an optional `field text` overload (NULL =
single-column behavior; multi-column without a field highlights each field
separately, joined by one newline, marks confined to the matching field);
`search()` on a multi-column index highlights the field named by a single
top-level `Field` wrapper, else the first index column (first matching
field, else first non-NULL field — preserving the scalar return type).

## 6. Compatibility and migration

- Each implementation phase is an LSG4 REINDEX boundary; LSG3 indexes are
  never rewritten.
- Old binaries reject LSG4 blobs (magic) and fields trailers (meta length)
  — fail closed, no on-disk downgrade. Documented rollback limit.
- `layout_revision` is the widening path: `MAX_FIELDS = 16` with the packed
  entry byte is frozen for revision 1 (resolved decision #8); widening to 32
  later stays expressible as revision 2, with readers rejecting unknown
  revisions.
- The postgres write-path change is confined to `storage/mod.rs` (loop
  `0..indnkeyatts`, skip NULLs, tokenize each datum, tag with the attribute
  ordinal; all-NULL rows index nothing). The datum plumbing is already
  sufficient — `build_callback` (am.rs:90-104) and `aminsert` (:117-129)
  forward the whole `values`/`isnull` arrays, and the take-first-datum
  happens inside `storage::Builder::add` / `storage::insert`. `am.rs` keeps
  only name/column validation and scan-key propagation.

## 7. Golden vectors

**Purpose.** Pin the frozen bytes with a decoder that shares no code with
the reader, so a writer/reader pair that agree on a wrong layout cannot
both pass.

**Layout:**

- `segment/tests/fixtures/lsg4/<case>.segment` — the frozen blob.
- `segment/tests/fixtures/lsg4/<case>.json` — the manifest (inputs +
  expected decode, or the expected error).
- `segment/tests/lsg4_golden.rs` — a self-contained decoder: `std` only, no
  `segment` crate imports, written against this RFC's text. It decodes each
  blob and asserts equality with the manifest; `invalid` cases must fail
  with the manifest's error class.

**Manifest schema (`"schema": "stannum.lsg4-golden/1"`):**

```json
{
  "schema": "stannum.lsg4-golden/1",
  "case": "two_fields_basic",
  "kind": "valid",
  "generator": { "git": "6d02ec7", "tool": "SegmentBuilder::finish_fields" },
  "format": "LSG4",
  "layout_revision": 1,
  "fields": [{"name": "title", "weight": 3.0}, {"name": "body", "weight": 1.0}],
  "documents": [
    {"tid": [0, 1], "tokens": [
      {"field": 0, "term": "数据库", "position": 0},
      {"field": 1, "term": "数据库", "position": 0},
      {"field": 1, "term": "索引", "position": 1}
    ]}
  ],
  "expected": {
    "doc_count": 1,
    "total_length": 3,
    "field_total": [1, 2],
    "documents": [{"tid": [0, 1], "lengths": [1, 2]}],
    "terms": [
      {"term": "数据库", "df": 1, "max_tf_bucket": 1,
       "postings": [{"tid": [0, 1],
                     "fields": [{"field": 0, "bucket": 0, "positions": [0]},
                                {"field": 1, "bucket": 1, "positions": [0]}]}]},
      {"term": "索引", "df": 1, "max_tf_bucket": 0,
       "postings": [{"tid": [0, 1],
                     "fields": [{"field": 1, "bucket": 0, "positions": [1]}]}]}
    ]
  }
}
```

`"kind": "invalid"` cases carry `"expected_error"` naming the validation
rule (by the §5.3/§5.4 rule number and short text, e.g.
`"payload rule 8: bucket disagrees with position count"`).

**Decoder requirements** — each maps to a frozen check:

- Header: magic, `layout_revision == 1`, varints, field totals, area lens;
  invariants 1-4 of §5.1.
- Lengths: doc-major rows; expected per-document per-field lengths.
- Dictionary: LSG3 entry layout; `df`, `max_tf_bucket`, extents within
  areas.
- Postings: form, count, bounds envelope; LSG4 term-bound/table bytes with
  the full §5.4 validation set; body decodes to the expected TIDs.
- Payload: LSG3-shaped skip framing; LSG4 entries with the full §5.3
  validation set, **including the quantization cross-check**.
- `invalid` cases: the decoder must fail with the manifest's error class.

**Minimum vector set:**

| Case | Pins |
|---|---|
| `two_fields_basic` | Term in both fields of one document; cross-field positions both starting at 0; per-field lengths. |
| `three_fields_weights` | Three fields, distinct weights; segment bytes are weight-agnostic (same blob re-validated under a second manifest with different weights). |
| `sixteen_fields` | `MAX_FIELDS` boundary; a term hitting all 16 fields; the 169-byte worst-case header inside the 256-byte head. |
| `bounds_table` | A term with more than `BLOCK_POSTINGS` (128) postings, so the bounds **table** (not the term bound) is written, across several fields and buckets. |
| `skip_table` | A term with more than `SKIP_INTERVAL` (32) postings, so the payload skip table exists. |
| `empty_fields_omitted` | A document with every field empty is not recorded (`doc_count`). |
| `one_field_bit_equality` | A one-field LSG4 blob beside the LSG3 blob of the same tokens (R-BIT, end-to-end). |
| `single_column_lsg3` | A one-field build writes `LSG3` magic (product rule at the byte level). |
| corruption set | Bad magic; `layout_revision = 2`; `field_count = 17`; lengths invariant violation; `Σ field_total ≠ total_length`; `field_hit_count = 0`; `field_hit_count > field_count`; non-increasing field ids; `position_count = 0`; bucket ≠ `from_count(n)`; `field_mask = 0`; `field_mask ≥ field_count`; `bucket_mask = 0`; `min_len = u32::MAX`; truncated blob. |

**Generation and freeze discipline:**

- Blobs are generated once by a blessed generator (an `#[ignore]`d test or
  example writing both files), reviewed, and committed. CI never regenerates
  them. Any byte change is a deliberate RFC amendment (new
  `layout_revision`), never a refresh.
- Manifest expected values are hand-authored for the small vectors and
  hand-reviewed for generated ones; the decoder is written against this
  RFC's text, not against the reader's code.
- `verify_segment` (verify.rs:200) additionally runs over every valid blob
  as a second gate — but it is *not* the independent decoder (it shares the
  reader). Its LSG4 additions assert per-field length sums, per-field
  position counts, field-id range, and per-field (not global)
  strictly-increasing positions.

## 8. Implementation phases and gates

Each phase is an LSG4 REINDEX boundary; LSG3 indexes are never rewritten;
each phase is atomic across the bytes it introduces (never merge a writer
without its reader).

1. **Format + field-scoped terms.** LSG4 read/write, forward records,
   builder/insert, meta trailer, `field:(…)` parse/eval, weights-applied
   scoring with the R-BIT test. Gates: LSG3 fixtures byte-unchanged; golden
   vectors green; upgrade test reads an old LSG3 index; `search()` keeps
   rejecting `indnatts != 1` until parity is green.
2. **BM25F bounds.** Per-field `FieldBlockBound`, WAND on LSG4, prune rate
   recorded through the explain identity; relax `search()`'s single-column
   check.
3. **Phrases + highlights.** Same-field spans, highlight overload,
   `extension_upgrade.py` field fixture, oracle/fuzz field dimension.

## 9. Risks

- **Blast radius.** Merges, direct merge, maintenance, verify, the mutable
  buffer, and every TINQL `Expr` match move together. Mitigation: `CURRENT`
  stays `Lsg3` (§5.8), so a partial land cannot rewrite single-column
  indexes into an unreadable format.
- **WAND recall on LSG4 is weaker than LSG3** (looser field-scoped bounds).
  Expected; measured, not treated as a bug.
- **Grammar timing.** `title:(foo)` parses today as word `title:` AND a
  group; after P0-2 it is a field query. Ship the grammar only with the
  executor-side unknown-field error ready (phase 1).
- **Forward-record compatibility.** The old codec must stay for metas
  without a fields trailer, or existing write buffers become undecodable.
- **Golden-vector circularity.** Encoder and decoder written from one
  mental model can both be wrong. Mitigations: hand-authored manifests, the
  independent decoder, and the quantization-cross-check corruption vector.
- **WAL opacity.** Confirm `storage/wal.rs` treats run/buffer pages as
  opaque before phase 1 lands — stop if any WAL record parses postings.

## 10. Deferred to implementation (not part of the freeze)

- Whether the per-`(field, bucket)` minima later tighten the WAND bound.
  They are retained in the bytes; the frozen formula uses the per-field
  aggregates.
- The boldi-vigna same-field span work is needed only if the solver assumes
  one position space (spot-check at phase 3).
- Exact `Expr::Field` match-arm mechanics, tinql error types, and the
  payload-filter cursor's API shape.

## 11. Acceptance checklist

| RFC-gate item (plan §P0-2) | Section |
|---|---|
| Max field count (16, packed entry byte) | §3, §5.3 |
| Exact segment header (incl. head-probe rule) | §5.1 |
| Length-table order | §5.2 |
| Payload entry bytes, complete validation set | §5.3 |
| Postings-bound bytes (`FieldBlockBound`/`ScoreBound`) | §5.4 |
| Forward-record bytes | §5.6 |
| Field-name/weight meta record (TAG 0x02) | §5.7 |
| Single-column-stays-LSG3 rule | §5.8 |
| Old-format read behavior | §5.9 |
| Scoring formula + bit-equality | §5.10 |
| Field-scope semantics | §5.11 |
| Golden vectors decoded by independent test code | §7 |

Acceptance means: this checklist is complete, every "frozen" statement has a
corresponding golden vector or named test, and steps 11-13 may begin.

## 12. References

- Plan: `docs/plans/stannum-p0-agent-features-2026-09-22.md` §P0-2
  (authoritative for phases, gates, and the work-item mapping).
- Design: `docs/designs/p0-agent-features.md` §P0-2.
- Critique: `docs/reviews/p0-plan-critique-2026-09-22.md` (post-critique
  resolutions #8 `MAX_FIELDS`/packed byte and #14 `layout_revision` + head
  probe are folded into §3/§5.1).
- Current code cited above: `segment/src/{segment,payload,postings,
  dictionary,tf_bucket,varint,forward,index,verify,error,format_tests}.rs`;
  `postgres/src/{bm25,score}.rs`; `postgres/src/storage/layout.rs`.
