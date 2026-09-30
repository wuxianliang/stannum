// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Empty `jieba_words` table, frozen empty-dictionary fingerprint, and the
//! unicode/not-applicable `index_analysis` path. Jieba install/load and the
//! `jieba_*` UDFs are Phase 3.

use std::hash::Hasher;

use pgrx::iter::TableIterator;
use pgrx::{PgRelation, name};
use siphasher::sip::SipHasher13;

#[allow(dead_code)] // frozen v1 empty-table identity; unit-tested and Phase 3 load
const EMPTY_FINGERPRINT: u64 = 0x6855_a073_6155_f3dd;

#[allow(dead_code)] // frozen identity; Phase 3 jieba load consumes these
type Word = (String, i32, Option<String>);

/// Frozen v1 wire identity: SipHash-1-3, fixed keys, raw UTF-8 tuple ordering.
/// Each row is independently domain separated and length framed (including tag).
#[allow(dead_code)] // frozen identity; Phase 3 jieba load consumes this
fn fingerprint(rows: &mut [Word]) -> u64 {
    rows.sort();
    let mut hash = SipHasher13::new_with_keys(0x7374616e6e756d31, 0x6a69656261646963);
    hash.write(b"stannum.jieba.dict.v1\0");
    for (word, freq, tag) in rows {
        hash.write(b"row\0");
        hash.write(&(word.len() as u32).to_le_bytes());
        hash.write(word.as_bytes());
        hash.write(&(*freq as u32).to_le_bytes());
        hash.write(&[u8::from(tag.is_some())]);
        if let Some(tag) = tag {
            hash.write(&(tag.len() as u32).to_le_bytes());
            hash.write(tag.as_bytes());
        }
    }
    // Zero is WI-1's embedded-only sentinel, never a table identity.
    match hash.finish() {
        0 => 1,
        value => value,
    }
}

pgrx::extension_sql!(
    r#"
CREATE TABLE @extschema@.jieba_words (
    word text PRIMARY KEY,
    freq integer NOT NULL DEFAULT 0 CHECK (freq >= 0),
    tag text
);
REVOKE ALL ON TABLE @extschema@.jieba_words FROM PUBLIC;
"#,
    name = "jieba_words"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AnalysisStamp {
    jieba_rs_version: u32,
    dict_fingerprint: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AnalysisStatus {
    NotApplicable,
    Match,
    MissingStamp,
    JiebaVersionDrift,
    DictionaryDrift,
    BothDrift,
}

impl AnalysisStatus {
    fn text(&self) -> &'static str {
        match self {
            Self::NotApplicable => "not applicable",
            Self::Match => "matches",
            Self::MissingStamp => "missing analysis stamp; REINDEX required",
            Self::JiebaVersionDrift => "jieba version drift; REINDEX required",
            Self::DictionaryDrift => "dictionary drift; REINDEX required",
            Self::BothDrift => "jieba version and dictionary drift; REINDEX required",
        }
    }
}

fn status(recorded: Option<AnalysisStamp>, runtime: Option<AnalysisStamp>) -> AnalysisStatus {
    let Some(runtime) = runtime else {
        return AnalysisStatus::NotApplicable;
    };
    let Some(recorded) = recorded else {
        return AnalysisStatus::MissingStamp;
    };
    match (
        recorded.jieba_rs_version == runtime.jieba_rs_version,
        recorded.dict_fingerprint == runtime.dict_fingerprint,
    ) {
        (true, true) => AnalysisStatus::Match,
        (true, false) => AnalysisStatus::DictionaryDrift,
        (false, true) => AnalysisStatus::JiebaVersionDrift,
        (false, false) => AnalysisStatus::BothDrift,
    }
}

/// Jieba tokenizer is Phase 3; unicode/whitespace indexes have no analysis stamp.
fn stamp(_spec: &[u8; crate::options::SPEC_BYTES]) -> Option<AnalysisStamp> {
    None
}

#[allow(clippy::type_complexity)] // the SRF row shape is fixed public SQL surface
pub(crate) fn index_analysis(
    index: PgRelation,
) -> TableIterator<
    'static,
    (
        name!(index_name, String),
        name!(recorded_jieba_version, Option<i32>),
        name!(recorded_dict_fingerprint, Option<i64>),
        name!(runtime_jieba_version, Option<i32>),
        name!(runtime_dict_fingerprint, Option<i64>),
        name!(matches, Option<bool>),
        name!(status, String),
    ),
> {
    crate::udfs::require_stannum_index(&index, "index_analysis");
    let spec = unsafe { crate::storage::index_spec(index.as_ptr()) };
    let runtime = stamp(&spec);
    let recorded = None;
    let _ = spec;
    let state = status(recorded, runtime);
    TableIterator::once((
        index.name().to_string(),
        recorded.map(|s| s.jieba_rs_version as i32),
        recorded.map(|s| s.dict_fingerprint as i64),
        runtime.map(|s| s.jieba_rs_version as i32),
        runtime.map(|s| s.dict_fingerprint as i64),
        runtime.map(|_| state == AnalysisStatus::Match),
        state.text().to_owned(),
    ))
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    #[test]
    fn stable_framed_fingerprint() {
        let a = ("词".into(), 0, None);
        let b = ("词语".into(), 42, Some("n".into()));
        assert_eq!(
            fingerprint(&mut [a.clone(), b.clone()]),
            fingerprint(&mut [b.clone(), a.clone()])
        );
        // Frozen v1 vector; changing keys/framing requires an identity version.
        assert_eq!(fingerprint(&mut []), EMPTY_FINGERPRINT);
        assert_ne!(
            fingerprint(&mut [a.clone()]),
            fingerprint(&mut [(a.0, 0, Some("".into()))])
        );
    }
    #[test]
    fn drift_matrix() {
        let s = AnalysisStamp {
            jieba_rs_version: 7,
            dict_fingerprint: 42,
        };
        assert_eq!(status(None, None), AnalysisStatus::NotApplicable);
        assert_eq!(status(Some(s), None), AnalysisStatus::NotApplicable);
        assert_eq!(status(None, Some(s)), AnalysisStatus::MissingStamp);
        assert_eq!(status(Some(s), Some(s)), AnalysisStatus::Match);
        assert_eq!(
            status(
                Some(s),
                Some(AnalysisStamp {
                    dict_fingerprint: 43,
                    ..s
                })
            ),
            AnalysisStatus::DictionaryDrift
        );
        assert_eq!(
            status(
                Some(s),
                Some(AnalysisStamp {
                    jieba_rs_version: 8,
                    ..s
                })
            ),
            AnalysisStatus::JiebaVersionDrift
        );
        assert_eq!(
            status(
                Some(s),
                Some(AnalysisStamp {
                    jieba_rs_version: 8,
                    dict_fingerprint: 43
                })
            ),
            AnalysisStatus::BothDrift
        );
    }
}
