// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! Word-boundary tokenizer for mixed Chinese/English corpora, backed by the
//! jieba dictionary segmenter.
//!
//! jieba finds the maximum-probability path through its embedded dictionary
//! and falls back to an HMM for out-of-dictionary Han runs, so Chinese text
//! is segmented into whole words (`开源数据库` -> `开源`, `数据库`) instead of
//! the per-character tokens the UAX#29 tokenizer produces. The dictionary is
//! embedded in the binary and deterministic for a pinned jieba-rs version,
//! which is what lets index-time and query-time analysis agree by
//! construction. Non-Han runs (English words, digits) are emitted as single
//! jieba segments; note this differs from the `unicode` tokenizer's UAX#29
//! rules for punctuation-bridged ASCII (`can't`, `left:right` stay whole
//! there but split here) — either way the same pipeline runs on both sides of
//! a query, so matching stays consistent.
//!
//! Segments with no alphanumeric character (whitespace, punctuation, symbols)
//! are skipped without consuming a position, mirroring how the UAX#29 word
//! source filters non-word segments.

use crate::{Classification, Token};
use jieba_rs::Jieba;
use std::borrow::Cow;
use std::sync::{Arc, LazyLock, RwLock};

/// The exact jieba-rs version used by this crate, packed as major/minor/patch.
///
/// Keep this in lockstep with the exact dependency pin in `Cargo.toml`; the
/// unit test below also checks the workspace lockfile.
pub const JIEBA_RS_VERSION: u32 = (0 << 16) | (7 << 8) | 4;

/// Identity of the embedded dictionary before PostgreSQL governance loads it.
/// Computed dictionary fingerprints (including the empty table) must never
/// equal this sentinel. Standalone installs use their nonzero generation as
/// a process-local identity, not a persisted content hash.
pub const JIEBA_EMBEDDED_FINGERPRINT: u64 = 0;

/// An immutable dictionary and its atomically captured identity. The opaque
/// wrapper keeps jieba-rs types out of the public API.
#[derive(Clone)]
pub struct JiebaSnapshot {
    pub(crate) dictionary: Arc<Jieba>,
    generation: u64,
    fingerprint: u64,
}

impl JiebaSnapshot {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }
}

/// Readers clone a snapshot under the lock and never lock while iterating.
/// Dictionary construction happens outside the write lock; dictionary,
/// generation and fingerprint are published together in one swap.
pub struct JiebaHolder {
    inner: RwLock<JiebaSnapshot>,
}

impl JiebaHolder {
    pub(crate) fn new() -> Self {
        Self {
            inner: RwLock::new(JiebaSnapshot {
                dictionary: Arc::new(Jieba::new()),
                generation: 0,
                fingerprint: JIEBA_EMBEDDED_FINGERPRINT,
            }),
        }
    }

    pub(crate) fn snapshot(&self) -> JiebaSnapshot {
        self.inner.read().expect("jieba holder poisoned").clone()
    }

    pub(crate) fn install(
        &self,
        words: &[(&str, Option<usize>, Option<&str>)],
        fingerprint: Option<u64>,
    ) -> u64 {
        assert_ne!(
            fingerprint,
            Some(JIEBA_EMBEDDED_FINGERPRINT),
            "computed fingerprints must not use the embedded identity sentinel"
        );
        let mut replacement = Jieba::new();
        for &(word, frequency, tag) in words {
            replacement.add_word(word, frequency.filter(|&freq| freq > 0), tag);
        }
        let dictionary = Arc::new(replacement);
        let mut current = self.inner.write().expect("jieba holder poisoned");
        let generation = current
            .generation
            .checked_add(1)
            .expect("jieba generation overflow");
        *current = JiebaSnapshot {
            dictionary,
            generation,
            fingerprint: fingerprint.unwrap_or(generation),
        };
        generation
    }
}

/// The current process-local dictionary holder.
static JIEBA: LazyLock<JiebaHolder> = LazyLock::new(JiebaHolder::new);

pub fn jieba_snapshot() -> JiebaSnapshot {
    JIEBA.snapshot()
}

pub(crate) fn jieba_install(words: &[(&str, Option<usize>, Option<&str>)]) -> u64 {
    // This convenience API is for non-PostgreSQL callers. PostgreSQL's
    // dictionary loader uses `jieba_install_with_fingerprint` so its
    // extension-defined SipHash identity is passed in explicitly.
    JIEBA.install(words, None)
}

pub(crate) fn jieba_install_with_fingerprint(
    words: &[(&str, Option<usize>, Option<&str>)],
    fingerprint: u64,
) -> u64 {
    JIEBA.install(words, Some(fingerprint))
}

pub(crate) fn jieba_current_generation() -> u64 {
    JIEBA.snapshot().generation()
}

pub(crate) fn jieba_current_fingerprint() -> u64 {
    JIEBA.snapshot().fingerprint()
}

/// An iterator over Jieba segments. Normal tokenization owns each segment so
/// an iterator remains independent of any later dictionary installation.
pub(crate) struct JiebaIter<'a> {
    words: std::vec::IntoIter<Cow<'a, str>>,
    pos: u32,
}

impl<'a> JiebaIter<'a> {
    /// Construct an owned Jieba iterator from a fixed dictionary snapshot.
    pub(crate) fn new(text: &'a str, snapshot: &Arc<Jieba>) -> Self {
        Self {
            words: snapshot
                .cut(text, true)
                .into_iter()
                .map(|word| Cow::Owned(word.to_owned()))
                .collect::<Vec<_>>()
                .into_iter(),
            pos: 0,
        }
    }

    /// Construct the source-span variant, which must retain slices into the
    /// original text for highlighting while using the same dictionary snapshot.
    pub(crate) fn new_borrowed(text: &'a str, snapshot: &Arc<Jieba>) -> Self {
        Self {
            words: snapshot
                .cut(text, true)
                .into_iter()
                .map(Cow::Borrowed)
                .collect::<Vec<_>>()
                .into_iter(),
            pos: 0,
        }
    }
}

impl<'a> Iterator for JiebaIter<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let word = self.words.next()?;
            if !word.chars().any(char::is_alphanumeric) {
                continue;
            }
            let token = Token {
                text: word,
                pos: self.pos,
                classification: Classification::Word,
                came_from_split: false,
            };
            self.pos = self.pos.checked_add(1).expect("token position overflow");
            return Some(token);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tokenizer;

    fn collect(text: &str, snapshot: &Arc<Jieba>) -> Vec<(String, u32)> {
        JiebaIter::new(text, snapshot)
            .map(|token| (token.text.into_owned(), token.pos))
            .collect()
    }

    #[test]
    fn chinese_dictionary_words_stay_whole() {
        let snapshot = JIEBA.snapshot().dictionary;
        assert_eq!(
            collect("我来到北京清华大学", &snapshot),
            vec![
                ("我".into(), 0),
                ("来到".into(), 1),
                ("北京".into(), 2),
                ("清华大学".into(), 3),
            ]
        );
    }

    #[test]
    fn mixed_chinese_and_english() {
        let snapshot = JIEBA.snapshot().dictionary;
        assert_eq!(
            collect("PostgreSQL 是开源数据库", &snapshot),
            vec![
                ("PostgreSQL".into(), 0),
                ("是".into(), 1),
                ("开源".into(), 2),
                ("数据库".into(), 3),
            ]
        );
    }

    #[test]
    fn punctuation_and_whitespace_consume_no_positions() {
        let snapshot = JIEBA.snapshot().dictionary;
        assert_eq!(
            collect(" 你好，世界！hello, world! ", &snapshot),
            vec![
                ("你好".into(), 0),
                ("世界".into(), 1),
                ("hello".into(), 2),
                ("world".into(), 3),
            ]
        );
    }

    #[test]
    fn hmm_segments_out_of_dictionary_names() {
        let snapshot = JIEBA.snapshot().dictionary;
        let tokens = collect("他来到了网易杭研大厦", &snapshot);
        let words: Vec<&str> = tokens.iter().map(|(word, _)| word.as_str()).collect();
        assert_eq!(words, vec!["他", "来到", "了", "网易", "杭研", "大厦"]);
    }

    #[test]
    fn holder_swap_preserves_an_in_flight_iteration() {
        let holder = JiebaHolder::new();
        let before = holder.snapshot().dictionary;
        let mut iter = JiebaIter::new("深度学习方法", &before);
        let first = iter.next().expect("first token");

        let generation = holder.install(&[("深度学习", Some(1_000_000), None)], Some(42));
        assert_eq!(generation, 1);
        let after = holder.snapshot();
        assert_eq!(after.generation(), generation);
        assert_eq!(after.fingerprint(), 42);

        let remaining: Vec<String> = iter.map(|token| token.text.into_owned()).collect();
        assert_eq!(first.text, "深度");
        assert_eq!(remaining, vec!["学习", "方法"]);
        assert_eq!(
            collect("深度学习方法", &after.dictionary),
            vec![("深度学习".into(), 0), ("方法".into(), 1)]
        );
    }

    #[test]
    fn compiled_pipeline_keeps_its_captured_dictionary_generation() {
        let holder = JiebaHolder::new();
        let spec = crate::TokenizerPipelineSpec {
            tokenizer: crate::TokenizerSpec::Jieba,
            case_folding: crate::Folding::Fold,
            accent_folding: crate::Folding::Preserve,
            long_tokens: crate::LongTokenSpec {
                mode: crate::LongTokenMode::Split,
                max_bytes: 256,
            },
            graphemes: crate::GraphemeMode::Discard,
            position_gaps: crate::PositionGapMode::Preserve,
        };
        let first = spec
            .compile_with_snapshot(holder.snapshot())
            .expect("valid Jieba spec");
        holder.install(&[("深度学习", Some(1_000_000), None)], Some(7));
        let second = spec
            .compile_with_snapshot(holder.snapshot())
            .expect("valid Jieba spec");

        let first_words: Vec<String> = first
            .tokenize("深度学习方法")
            .map(|token| token.text.into_owned())
            .collect();
        let second_words: Vec<String> = second
            .tokenize("深度学习方法")
            .map(|token| token.text.into_owned())
            .collect();
        assert_eq!(first_words, vec!["深度", "学习", "方法"]);
        assert_eq!(second_words, vec!["深度学习", "方法"]);
    }

    #[test]
    fn version_const_matches_the_workspace_lockfile() {
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../Cargo.lock"))
            .expect("workspace Cargo.lock");
        let package = lock
            .split("[[package]]")
            .find(|block| {
                block
                    .lines()
                    .any(|line| line.trim() == "name = \"jieba-rs\"")
            })
            .expect("jieba-rs package in Cargo.lock");
        let version = package
            .lines()
            .find_map(|line| line.trim().strip_prefix("version = \"")?.strip_suffix('"'))
            .expect("jieba-rs version in Cargo.lock");
        assert_eq!(version, "0.7.4");
        let mut parts = version.split('.').map(|part| part.parse::<u32>().unwrap());
        let packed =
            (parts.next().unwrap() << 16) | (parts.next().unwrap() << 8) | parts.next().unwrap();
        assert_eq!(packed, JIEBA_RS_VERSION);
    }
}
