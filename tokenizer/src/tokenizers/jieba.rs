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
//! there but split here) — either way the same pipeline runs on both sides
//! of a query, so matching stays consistent.
//!
//! Segments with no alphanumeric character (whitespace, punctuation, symbols)
//! are skipped without consuming a position, mirroring how the UAX#29 word
//! source filters non-word segments.

use crate::{Classification, Token};
use jieba_rs::Jieba;
use std::sync::LazyLock;

/// The embedded-dictionary segmenter, parsed once per process on first use.
/// The dictionary is several megabytes and its parse takes on the order of a
/// hundred milliseconds in release builds, so the first jieba-preset
/// operation in a backend process pays a one-time initialization cost.
static JIEBA: LazyLock<Jieba> = LazyLock::new(Jieba::new);

pub(crate) struct JiebaIter<'a> {
    words: std::vec::IntoIter<&'a str>,
    pos: u32,
}

impl<'a> JiebaIter<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        Self {
            // Precise mode with HMM: dictionary words stay whole, and an
            // out-of-dictionary Han run is segmented into characters by the
            // HMM instead of becoming one giant token.
            words: JIEBA.cut(text, true).into_iter(),
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
            let token = Token::with_classification(word, self.pos, Classification::Word);
            self.pos = self.pos.checked_add(1).expect("token position overflow");
            return Some(token);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(text: &str) -> Vec<(String, u32)> {
        JiebaIter::new(text)
            .map(|token| (token.text.into_owned(), token.pos))
            .collect()
    }

    #[test]
    fn chinese_dictionary_words_stay_whole() {
        // Canonical jieba example; stable across dictionary versions.
        assert_eq!(
            collect("我来到北京清华大学"),
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
        assert_eq!(
            collect("PostgreSQL 是开源数据库"),
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
        assert_eq!(
            collect(" 你好，世界！hello, world! "),
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
        // The HMM pieces unrecognized runs into plausible word fragments
        // instead of one long token.
        let tokens = collect("他来到了网易杭研大厦");
        let words: Vec<&str> = tokens.iter().map(|(word, _)| word.as_str()).collect();
        assert_eq!(words, vec!["他", "来到", "了", "网易", "杭研", "大厦"]);
    }
}
