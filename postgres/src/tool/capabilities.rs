// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! `stannum.capabilities()` — the closed §4.2 document.
//!
//! `version` is the frozen 0.5.0 contract target (`CONTRACT_VERSION`), not
//! read from `CARGO_PKG_VERSION` (they currently agree).
//! `limits.max_expansion` is the value `Limits::default` enforces in
//! `tinql/src/runtime/plan.rs` (1024 on this tree), not a sketch.

use pgrx::JsonB;
use pgrx::prelude::*;

/// Contract-target version. Not the crate or extension version.
const CONTRACT_VERSION: &str = "0.5.0";

/// Dictionary-expansion cap enforced by tinql `Limits::default`.
const MAX_EXPANSION: i64 = 1024;

#[pg_extern(immutable, strict, parallel_safe)]
fn capabilities() -> JsonB {
    JsonB(serde_json::json!({
        "contract_version": 1,
        "engine": {
            "name": "stannum",
            "format": "STN3",
            "version": CONTRACT_VERSION,
        },
        "features": {
            "bm25f": true,
            "field_phrases": true,
            "field_highlights": true,
            "highlight_field_arity": 5,
            "tokenizers": ["unicode", "whitespace", "jieba"],
            "stop_word_presets": ["auto", "auto:zh", "auto:en"],
            "jieba_ddl": true,
            "standby_reads": true,
        },
        "limits": { "max_expansion": MAX_EXPANSION },
    }))
}

#[cfg(test)]
mod tests {
    use super::MAX_EXPANSION;

    #[test]
    fn max_expansion_matches_tinql_default() {
        let enforced = tinql::runtime::plan::Limits::default().max_expansion;
        assert_eq!(
            usize::try_from(MAX_EXPANSION).expect("cap fits usize"),
            enforced,
            "capabilities().limits.max_expansion must be what tinql enforces"
        );
    }
}
