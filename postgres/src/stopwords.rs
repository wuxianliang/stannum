// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Original Stannum function-word presets, frozen as stannum-stopwords-v1
//! (2026-09-23). Curated for this repository, not copied from an external list.
//! These are source words; scorer construction analyzes and single-token filters
//! them with the index's own pipeline. See source-provenance.json.
use pgrx::prelude::*;

pub(crate) const ZH: &[&str] = &[
    "的", "了", "着", "过", "地", "得", "是", "有", "在", "和", "与", "或", "而", "但", "也", "都",
    "就", "又", "还", "很", "更", "最", "太", "不", "没", "无", "别", "再", "才", "只", "已", "曾",
    "将", "正", "要", "会", "能", "可", "应", "该", "被", "把", "让", "给", "对", "向", "从", "到",
    "由", "为", "于", "以", "按", "比", "跟", "同", "及", "并", "且", "因", "如", "若", "则", "即",
    "我", "你", "他", "她", "它", "谁", "这", "那", "哪", "此", "其", "各", "每", "某", "另", "们",
    "个", "些", "种", "位", "次", "所", "之", "内", "外", "上", "下", "中", "前", "后", "里", "间",
    "吗", "呢", "吧", "啊", "呀", "哦", "嗯", "嘛", "啦", "哇", "我们", "你们", "他们", "她们",
    "它们", "自己", "大家", "别人", "人家", "这个", "那个", "这些", "那些", "这里", "那里", "哪里",
    "这样", "那样", "怎样", "这么", "那么", "什么", "怎么", "如何", "为何", "因为", "所以", "如果",
    "虽然", "但是", "而且", "或者", "以及", "并且", "不过", "可是", "然而", "于是", "因此", "为了",
    "关于", "对于", "由于", "按照", "通过", "作为", "除了", "只有", "只要", "无论", "不管", "即使",
    "尽管", "否则", "然后", "已经", "正在", "曾经", "仍然", "还是", "仍旧", "总是", "一直", "一切",
    "所有", "任何", "各自", "彼此", "一起", "其中", "其他", "其它", "一些", "一样", "一般", "本来",
    "原来", "确实", "实在", "其实", "当然", "也许", "大概", "必须", "应该", "可以", "能够", "不能",
    "不会", "没有", "不是", "不要", "不用", "不必",
];
pub(crate) const EN: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "then", "else", "as", "at", "by", "for", "from",
    "in", "into", "of", "on", "to", "with", "is", "am", "are", "was", "were", "be", "been",
    "being", "it", "its", "this", "that", "these", "those", "i", "you", "he", "she", "we", "they",
    "me", "him", "her", "us", "them", "my", "your", "our", "their", "not", "no", "do", "does",
    "did", "have", "has", "had", "can", "will",
];

pub(crate) fn words(preset: &str) -> impl Iterator<Item = &'static str> + use<> {
    let (zh, en) = match preset.to_ascii_lowercase().as_str() {
        "zh" => (ZH, &[][..]),
        "en" => (&[][..], EN),
        "auto" => (ZH, EN),
        _ => pgrx::error!("unknown stop-word preset; expected zh, en, or auto"),
    };
    zh.iter().chain(en).copied()
}

/// Resolve bare auto using the actual index tokenizer, not a token-count guess.
pub(crate) fn reloption(csv: &str, jieba: bool) -> String {
    csv.split(',')
        .map(|word| {
            if word.trim().eq_ignore_ascii_case("auto") && !jieba {
                "auto:en"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[pg_extern(immutable, parallel_safe)]
fn builtin_stop_words(preset: &str) -> SetOfIterator<'static, &'static str> {
    SetOfIterator::new(words(preset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokenizer::{Tokenizer, TokenizerPipelineSpec, TokenizerSpec};
    #[test]
    fn presets_are_unique_single_tokens() {
        assert!((150..=200).contains(&ZH.len()), "{} zh entries", ZH.len());
        for (words, kind) in [(ZH, TokenizerSpec::Jieba), (EN, TokenizerSpec::Unicode)] {
            let pipeline = TokenizerPipelineSpec {
                tokenizer: kind,
                ..Default::default()
            }
            .compile()
            .unwrap();
            let mut unique = std::collections::HashSet::new();
            for word in words {
                assert!(unique.insert(word), "duplicate {word}");
                assert_eq!(pipeline.tokenize(word).count(), 1, "preset {word}");
            }
        }
    }
}
