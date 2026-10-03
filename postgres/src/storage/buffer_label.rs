// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! The durable write-buffer generation (design §6.3.1).
//!
//! A multi-column write buffer's generation is the persisted
//! [`STN4_BUFFER_TAG`](segment::forward::STN4_BUFFER_TAG) — not any in-memory
//! map, not the STNF version of the immutable segments, and not the spelling
//! of a surface token. The label is derived from persisted state alone, so a
//! backend that reopens, recovers, or replays WAL — the tag is part of the
//! KIND_BUFFER bytes that buffer-page WAL already logs — emits the same label
//! as the live relation that wrote the stream.
//!
//! The classifier is a pure function: it never calls into PostgreSQL, never
//! converts a stream into [`segment::index`] builders, and consults
//! [`legacy_fielded_key_shape`] only on the untagged arm. Loading a legacy
//! stream into STN4 maps and then asking the map shape is forbidden by the
//! design; this module cannot do it.

use segment::forward;

/// The generation class of a KIND_BUFFER stream (design §6.3.1 decoding
/// order). `BufferEmpty` and `BufferNoTerms` are generation-neutral: they
/// contribute to neither `G_v1` nor `G_v2` (design §6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferLabel {
    /// `docs == 0 ∧ bytes == 0`. No tag on disk; the next multi-column
    /// insert writes the tag and the record as one append.
    Empty,
    /// Documents but no postings: a tagged STN4 stream (arm 6.1) or a
    /// single-column stock stream (arm 4) that parsed with zero terms.
    NoTerms,
    /// The live generation: a tagged STN4 stream with postings (arm 6.1) or
    /// a single-column stock stream (arm 4). A surface token shaped
    /// `~0~foo` does not change this.
    Current,
    /// An untagged well-formed legacy stream (arm 6.2): a 4.3–4.6
    /// fielded-terms buffer. The missing tag **is** the generation. Never
    /// written again: the relation is `StaleFielded` or `MixedFielded`.
    Stale,
    /// Counts disagree, the field count is out of range, or the stream fits
    /// no grammar. The relation is `Corrupt`.
    Malformed(&'static str),
}

/// Classifies a write buffer by the §6.3.1 decoding order.
///
/// `field_count` comes from the STNM envelope, `docs` / `bytes` from
/// [`BufferState`](crate::storage::layout::BufferState), and `stream` is
/// the concatenated KIND_BUFFER payload (`stream.len() == bytes`). No term
/// text is visited until the branch is chosen.
///
/// Every record-walking arm cross-checks the decoded record count against
/// `docs`: the stream and the meta counts are written as one append, so a
/// disagreement names corruption, not a generation.
pub fn classify_buffer(field_count: usize, docs: u32, bytes: u32, stream: &[u8]) -> BufferLabel {
    // Arms 1–3: the counts decide before any stream byte is read.
    match (docs, bytes) {
        (0, 0) => return BufferLabel::Empty,
        (0, _) => return BufferLabel::Malformed("write buffer holds bytes but no documents"),
        (_, 0) => return BufferLabel::Malformed("write buffer holds documents but no bytes"),
        _ => {}
    }
    // Arm 5: a field count outside 1..=16 cannot name a grammar.
    if !(1..=16).contains(&field_count) {
        return BufferLabel::Malformed("write buffer field count out of range");
    }
    if field_count == 1 {
        // Arm 4: single-column stock records, no tag, never Stale. A stock
        // key `~0~foo` is Current: this arm has no fielded-key validator.
        let (terms, records) = match stock_terms(stream) {
            Ok(counts) => counts,
            Err(malformed) => return malformed,
        };
        if records != docs {
            return BufferLabel::Malformed("write buffer document count disagrees with the stream");
        }
        return if terms > 0 {
            BufferLabel::Current
        } else {
            BufferLabel::NoTerms
        };
    }
    if stream.starts_with(&forward::STN4_BUFFER_TAG) {
        // Arm 6.1: the tag selects the STN4 grammar for the remainder.
        // Surface tokens are never inspected for the `~{h}~` shape here —
        // `legacy_fielded_key_shape` is not consulted on this arm.
        let body = &stream[forward::STN4_BUFFER_TAG.len()..];
        let (terms, records) = match fielded_terms(body) {
            Ok(counts) => counts,
            Err(malformed) => return malformed,
        };
        if records != docs {
            return BufferLabel::Malformed("write buffer document count disagrees with the stream");
        }
        return if terms > 0 {
            BufferLabel::Current
        } else {
            BufferLabel::NoTerms
        };
    }
    if stream.first() == Some(&0x00) {
        // A well-formed legacy stream's first byte is never 0x00 (the
        // ForwardRecord length varint is at least 4), so any other 0x00-led
        // stream names an unknown STN4 format version.
        return BufferLabel::Malformed("write buffer starts with an unknown STN4 format");
    }
    // Arm 6.2: untagged legacy. Do not call `add_occurrence`; never load
    // into STN4 builders. With terms, every key must satisfy the legacy
    // fielded-key grammar; anything else is malformed, not stale.
    let mut records = 0u32;
    for record in forward::records(stream) {
        match record {
            Ok(record) => {
                for term in &record.terms {
                    if !legacy_fielded_key_shape(&term.term, field_count as u8) {
                        return BufferLabel::Malformed(
                            "untagged write buffer holds a non-legacy fielded key",
                        );
                    }
                }
                records += 1;
            }
            Err(_) => {
                return BufferLabel::Malformed("untagged write buffer fails the legacy grammar");
            }
        }
    }
    if records != docs {
        return BufferLabel::Malformed("write buffer document count disagrees with the stream");
    }
    // Stale even with zero terms: choice 3 classifies the untagged
    // zero-term buffer as stale, the design's chosen transition.
    BufferLabel::Stale
}

/// Arm 4's stock term and record counts, or [`BufferLabel::Malformed`] on
/// the first record that fails the stock grammar.
fn stock_terms(stream: &[u8]) -> Result<(usize, u32), BufferLabel> {
    let mut terms = 0usize;
    let mut records = 0u32;
    for record in forward::records(stream) {
        terms += match record {
            Ok(record) => record.terms.len(),
            Err(_) => {
                return Err(BufferLabel::Malformed(
                    "write buffer fails the stock record grammar",
                ));
            }
        };
        records += 1;
    }
    Ok((terms, records))
}

/// Arm 6.1's STN4 term and record counts, or [`BufferLabel::Malformed`] on
/// the first record that fails the fielded grammar.
fn fielded_terms(body: &[u8]) -> Result<(usize, u32), BufferLabel> {
    let mut terms = 0usize;
    let mut records = 0u32;
    for record in forward::fielded_records(body) {
        terms += match record {
            Ok(record) => record
                .groups
                .iter()
                .map(|group| group.terms.len())
                .sum::<usize>(),
            Err(_) => {
                return Err(BufferLabel::Malformed(
                    "tagged write buffer fails the STN4 record grammar",
                ));
            }
        };
        records += 1;
    }
    Ok((terms, records))
}

/// The §6.3.1 write-path decision, read from persisted state at the stock
/// cost profile: the tagged arm (and the single-column stock arm) is a
/// prefix check that appends without re-walking records this version wrote
/// — fold re-validates the grammar on decode, exactly as the stock path
/// always has — and only the untagged arm, which is always an error path,
/// walks the records to separate stale from malformed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferWritePath {
    /// `docs == 0 ∧ bytes == 0`: the first multi-column insert writes the
    /// tag and the record as one WAL-logged append. The only tag birth.
    TagBirth,
    /// A tagged STN4 stream (or a single-column stock stream): append the
    /// record after the existing tag; never write a second tag.
    Append,
    /// An untagged well-formed legacy stream: **no write**. Do not prepend a
    /// tag, do not append, do not convert. The relation is `StaleFielded`
    /// or `MixedFielded` (design §6.3) and INSERT is the rebuild error
    /// before any page is dirtied.
    Stale,
    /// Counts or the tag prefix are impossible: corrupt.
    Malformed(&'static str),
}

/// Decides the write path by §6.3.1 arms 1–6 at the stock cost profile.
/// `write_path` and [`classify_buffer`] agree on every shape whose body
/// fits its grammar; the two carve-outs — a tagged or single-column
/// stream whose body fails its grammar — are `Append` here and
/// `Malformed` from the full classification. Fold's decode is the named
/// backstop; `write_path_defers_broken_bodies_to_fold` pins the carve-out.
pub fn write_path(field_count: usize, docs: u32, bytes: u32, stream: &[u8]) -> BufferWritePath {
    // Arms 1–3: the counts decide before any stream byte is read.
    match (docs, bytes) {
        (0, 0) => return BufferWritePath::TagBirth,
        (0, _) => return BufferWritePath::Malformed("write buffer holds bytes but no documents"),
        (_, 0) => return BufferWritePath::Malformed("write buffer holds documents but no bytes"),
        _ => {}
    }
    // Arm 5: a field count outside 1..=16 cannot name a grammar.
    if !(1..=16).contains(&field_count) {
        return BufferWritePath::Malformed("write buffer field count out of range");
    }
    if field_count == 1 {
        // Arm 4: single-column stock records, no tag, never stale. The
        // stock decoder owns the body; the writer never sees a tag here.
        return BufferWritePath::Append;
    }
    if stream.starts_with(&forward::STN4_BUFFER_TAG) {
        // Arm 6.1: the tag selects the grammar for the remainder; the
        // append goes after the tag. `legacy_fielded_key_shape` is not
        // consulted on this arm.
        return BufferWritePath::Append;
    }
    if stream.first() == Some(&0x00) {
        // A well-formed legacy stream's first byte is never 0x00, so any
        // other 0x00-led stream names an unknown STN4 format version.
        return BufferWritePath::Malformed("write buffer starts with an unknown STN4 format");
    }
    // Arm 6.2: untagged legacy — always the error path, so the records are
    // walked here. Well-formed fielded keys are stale; anything else is
    // malformed, not stale.
    for record in forward::records(stream) {
        match record {
            Ok(record) => {
                for term in &record.terms {
                    if !legacy_fielded_key_shape(&term.term, field_count as u8) {
                        return BufferWritePath::Malformed(
                            "untagged write buffer holds a non-legacy fielded key",
                        );
                    }
                }
            }
            Err(_) => {
                return BufferWritePath::Malformed(
                    "untagged write buffer fails the legacy grammar",
                );
            }
        }
    }
    BufferWritePath::Stale
}

/// The 4.3–4.6 `~{h}~` fielded-key grammar, surviving only as the
/// **untagged** legacy-stream validator (design §6.3.1). It encodes nothing,
/// looks nothing up, and never participates in lookup, expand, scoring, or
/// writing; the tagged arm never calls it.
///
/// True iff `field_count` is in `2..=16` and `key` is `~` + one lowercase
/// hex nibble + `~` + a nonempty escaped token (every payload `~` doubled)
/// whose nibble is `< field_count`.
pub fn legacy_fielded_key_shape(key: &str, field_count: u8) -> bool {
    if !(2..=16).contains(&field_count) {
        return false;
    }
    let bytes = key.as_bytes();
    if bytes.len() < 4 || bytes[0] != b'~' || bytes[2] != b'~' {
        return false;
    }
    let nibble = match bytes[1] {
        b'0'..=b'9' => bytes[1] - b'0',
        b'a'..=b'f' => bytes[1] - b'a' + 10,
        _ => return false,
    };
    if nibble >= field_count {
        return false;
    }
    let token = &bytes[3..];
    if token.is_empty() {
        return false;
    }
    let mut at = 0;
    while at < token.len() {
        if token[at] == b'~' {
            // A payload `~` must be doubled; a lone one is unbalanced.
            if token.get(at + 1) != Some(&b'~') {
                return false;
            }
            at += 2;
        } else {
            at += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stock_stream(tokens: &[(&str, u32)]) -> Vec<u8> {
        let record = segment::forward::ForwardRecord::from_tokens(
            segment::Tid::new(0, 1).unwrap(),
            tokens.iter().copied(),
        )
        .unwrap();
        let mut stream = Vec::new();
        record.encode(&mut stream).unwrap();
        stream
    }

    fn fielded_stream(groups: &[Vec<(&str, u32)>]) -> Vec<u8> {
        let record = segment::forward::FieldedRecord {
            tid: segment::Tid::new(0, 1).unwrap(),
            groups: groups
                .iter()
                .enumerate()
                .map(|(field, terms)| segment::forward::FieldedGroup {
                    field: field as u8,
                    field_length: terms.len() as u32,
                    terms: terms
                        .iter()
                        .map(|&(term, _)| segment::forward::FieldedTerm {
                            term: term.to_owned(),
                            positions: vec![1],
                        })
                        .collect(),
                })
                .collect(),
        };
        let mut stream = Vec::new();
        stream.extend_from_slice(&segment::forward::STN4_BUFFER_TAG);
        record.encode(&mut stream).unwrap();
        stream
    }

    #[test]
    fn counts_decide_before_the_stream_is_read() {
        // Arms 1–3: generation-neutral empty, then both count mismatches.
        assert_eq!(
            classify_buffer(2, 0, 0, &[]),
            BufferLabel::Empty,
            "docs == 0 ∧ bytes == 0 is Empty regardless of field count"
        );
        assert_eq!(
            classify_buffer(2, 0, 7, &[9, 9, 9, 9, 9, 9, 9]),
            BufferLabel::Malformed("write buffer holds bytes but no documents")
        );
        assert_eq!(
            classify_buffer(2, 1, 0, &[]),
            BufferLabel::Malformed("write buffer holds documents but no bytes")
        );
    }

    #[test]
    fn field_count_out_of_range_is_malformed() {
        let stream = stock_stream(&[("foo", 1)]);
        assert_eq!(
            classify_buffer(0, 1, stream.len() as u32, &stream),
            BufferLabel::Malformed("write buffer field count out of range")
        );
        assert_eq!(
            classify_buffer(17, 1, stream.len() as u32, &stream),
            BufferLabel::Malformed("write buffer field count out of range")
        );
    }

    #[test]
    fn single_column_stock_is_current_and_never_stale() {
        // Arm 4: a stock key shaped like a fielded key is still Current —
        // the single-column arm has no legacy validator.
        let stream = stock_stream(&[("~0~foo", 1), ("zzz", 2)]);
        assert_eq!(
            classify_buffer(1, 1, stream.len() as u32, &stream),
            BufferLabel::Current
        );
        let zero = stock_stream(&[]);
        assert_eq!(
            classify_buffer(1, 1, zero.len() as u32, &zero),
            BufferLabel::NoTerms
        );
        // A stream that fails the stock grammar is malformed, not stale.
        let mut broken = stream;
        broken.push(0xff);
        assert_eq!(
            classify_buffer(1, 1, broken.len() as u32, &broken),
            BufferLabel::Malformed("write buffer fails the stock record grammar")
        );
    }

    #[test]
    fn tagged_buffer_with_legacy_shaped_tokens_is_current() {
        // Arm 6.1: the tag decides, not the token spelling.
        let stream = fielded_stream(&[vec![("~0~foo", 1), ("~1~bar", 1)]]);
        assert_eq!(
            classify_buffer(2, 1, stream.len() as u32, &stream),
            BufferLabel::Current,
            "a tagged buffer stays Current even with ~h~-shaped surface tokens"
        );
    }

    #[test]
    fn tagged_zero_term_is_noterms_not_stale() {
        // Arm 6.1 with no groups: generation-neutral NoTerms.
        let stream = fielded_stream(&[]);
        assert_eq!(
            classify_buffer(2, 1, stream.len() as u32, &stream),
            BufferLabel::NoTerms
        );
    }

    #[test]
    fn unknown_tag_version_is_malformed_not_legacy() {
        let mut stream = vec![0x00, 0x02];
        stream.extend_from_slice(&stock_stream(&[("~0~foo", 1)]));
        assert_eq!(
            classify_buffer(2, 1, stream.len() as u32, &stream),
            BufferLabel::Malformed("write buffer starts with an unknown STN4 format")
        );
        // `0x00` alone names no version either.
        assert_eq!(
            classify_buffer(2, 1, 1, &[0x00]),
            BufferLabel::Malformed("write buffer starts with an unknown STN4 format")
        );
    }

    #[test]
    fn untagged_legacy_fielded_terms_buffer_is_stale() {
        // Arm 6.2: keys matching the legacy grammar classify Stale.
        let stream = stock_stream(&[("~0~needle", 1), ("~1~pad", 2)]);
        assert_eq!(
            classify_buffer(2, 1, stream.len() as u32, &stream),
            BufferLabel::Stale
        );
    }

    #[test]
    fn untagged_zero_term_is_stale_not_noterms() {
        // Choice 3: the untagged zero-term buffer is Stale — the missing
        // tag is the generation; BufferNoTerms is tagged-only.
        let stream = stock_stream(&[]);
        assert_eq!(
            classify_buffer(2, 1, stream.len() as u32, &stream),
            BufferLabel::Stale
        );
    }

    #[test]
    fn record_count_disagreeing_with_docs_is_malformed() {
        // One well-formed record — each grammar's own — against a `docs`
        // count the stream does not hold: corruption on every arm, never a
        // generation class (the A.3 review's caller-invariant, made a
        // predicate: the stream and the meta counts are written as one
        // append, so they cannot legally disagree).
        let stock = stock_stream(&[("foo", 1)]);
        assert_eq!(
            classify_buffer(1, 2, stock.len() as u32, &stock),
            BufferLabel::Malformed("write buffer document count disagrees with the stream")
        );
        let tagged = fielded_stream(&[vec![("foo", 1)]]);
        assert_eq!(
            classify_buffer(2, 2, tagged.len() as u32, &tagged),
            BufferLabel::Malformed("write buffer document count disagrees with the stream")
        );
        let legacy = stock_stream(&[("~0~foo", 1)]);
        assert_eq!(
            classify_buffer(2, 2, legacy.len() as u32, &legacy),
            BufferLabel::Malformed("write buffer document count disagrees with the stream")
        );
        // A stream that holds more records than `docs` names the same
        // corruption, and a matching count still classifies normally.
        assert_eq!(
            classify_buffer(2, 1, legacy.len() as u32, &legacy),
            BufferLabel::Stale
        );
    }

    #[test]
    fn untagged_non_legacy_keys_are_malformed_not_stale() {
        // A well-formed record stream whose keys are not fielded keys fits
        // no multi-column grammar: malformed, not stale.
        let stream = stock_stream(&[("needle", 1)]);
        assert_eq!(
            classify_buffer(2, 1, stream.len() as u32, &stream),
            BufferLabel::Malformed("untagged write buffer holds a non-legacy fielded key")
        );
    }

    #[test]
    fn untagged_broken_stream_is_malformed() {
        let mut stream = stock_stream(&[("~0~foo", 1)]);
        stream.push(0xff);
        assert_eq!(
            classify_buffer(2, 1, stream.len() as u32, &stream),
            BufferLabel::Malformed("untagged write buffer fails the legacy grammar")
        );
    }

    #[test]
    fn legacy_fielded_key_shape_matches_the_4_3_grammar() {
        // Field 0 and field 15, plain and escaped payloads.
        assert!(legacy_fielded_key_shape("~0~foo", 2));
        assert!(legacy_fielded_key_shape("~1~foo", 2));
        assert!(legacy_fielded_key_shape("~f~foo", 16));
        // The 4.1 canonical escaped encoding of the token `~0~foo`.
        assert!(legacy_fielded_key_shape("~0~~~0~~foo", 2));
        // Nibble must be below the field count and lowercase hex.
        assert!(!legacy_fielded_key_shape("~2~foo", 2));
        assert!(!legacy_fielded_key_shape("~a~foo", 10));
        assert!(legacy_fielded_key_shape("~9~foo", 16));
        // A lone payload tilde is an unbalanced escape.
        assert!(!legacy_fielded_key_shape("~0~foo~bar", 2));
        assert!(legacy_fielded_key_shape("~0~foo~~bar", 2));
        // Out-of-range field counts reject everything (`0 < 1` would
        // otherwise make `~0~foo` match).
        assert!(!legacy_fielded_key_shape("~0~foo", 1));
        assert!(!legacy_fielded_key_shape("~0~foo", 17));
        // Nonempty token required.
        assert!(!legacy_fielded_key_shape("~0~", 2));
        assert!(!legacy_fielded_key_shape("~0", 2));
    }

    #[test]
    fn write_path_agrees_with_classification_on_well_formed_bodies() {
        let shapes: Vec<(usize, u32, Vec<u8>)> = vec![
            (2, 0, Vec::new()),
            (2, 0, vec![9; 7]),
            (2, 1, Vec::new()),
            (17, 1, stock_stream(&[("foo", 1)])),
            (1, 1, stock_stream(&[("~0~foo", 1)])),
            (1, 1, stock_stream(&[])),
            (2, 1, fielded_stream(&[vec![("~0~foo", 1)]])),
            (2, 1, fielded_stream(&[])),
            (2, 1, {
                let mut stream = vec![0x00, 0x02];
                stream.extend_from_slice(&stock_stream(&[("~0~foo", 1)]));
                stream
            }),
            (2, 1, stock_stream(&[("~0~needle", 1), ("~1~pad", 2)])),
            (2, 1, stock_stream(&[])),
            (2, 1, stock_stream(&[("needle", 1)])),
        ];
        for (field_count, docs, stream) in shapes {
            let label = classify_buffer(field_count, docs, stream.len() as u32, &stream);
            let path = write_path(field_count, docs, stream.len() as u32, &stream);
            let expected = match label {
                BufferLabel::Empty => BufferWritePath::TagBirth,
                BufferLabel::Current | BufferLabel::NoTerms => BufferWritePath::Append,
                BufferLabel::Stale => BufferWritePath::Stale,
                BufferLabel::Malformed(reason) => BufferWritePath::Malformed(reason),
            };
            assert_eq!(
                path,
                expected,
                "write path and classification disagree on {label:?} for \
                 field_count {field_count}, docs {docs}, stream {:02x?}",
                &stream[..stream.len().min(12)]
            );
        }
    }

    #[test]
    fn write_path_defers_broken_bodies_to_fold_but_classification_names_them() {
        // The tagged arm's prefix check: a torn record body is `Append` at
        // the fence (fold's decode is the backstop) while the full
        // classification names it Malformed. Pinned so the carve-out is
        // loud, not implied.
        let mut tagged = fielded_stream(&[vec![("~0~foo", 1)]]);
        tagged.truncate(tagged.len() - 3);
        assert_eq!(
            write_path(2, 1, tagged.len() as u32, &tagged),
            BufferWritePath::Append
        );
        assert_eq!(
            classify_buffer(2, 1, tagged.len() as u32, &tagged),
            BufferLabel::Malformed("tagged write buffer fails the STN4 record grammar")
        );
        // The single-column carve-out, arm 4: stock bodies belong to the
        // stock decoder.
        let mut stock = stock_stream(&[("foo", 1)]);
        stock.push(0xff);
        assert_eq!(
            write_path(1, 1, stock.len() as u32, &stock),
            BufferWritePath::Append
        );
        assert_eq!(
            classify_buffer(1, 1, stock.len() as u32, &stock),
            BufferLabel::Malformed("write buffer fails the stock record grammar")
        );
    }
}
