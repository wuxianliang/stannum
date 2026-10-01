// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! L2 tool contract (design §3, §4.1).
//!
//! This is the SQL entry-point layer: the frozen §4.1 surface, including
//! `capabilities()`. It imports nothing from L0 engine internals: not
//! `IndexScorer`, not `storage::View`, and not segment-crate internals.
//!
//! It may depend on `pgrx` and `serde_json`, and, only where a UDF genuinely
//! needs the field model, on the `fields` layer (L1). This module does not
//! import `fields`; L2 stays self-contained until a UDF needs that seam.
//! Score-family `#[pg_extern]` bodies remain the owned shim
//! in `crate::score`. `search`/`search_count` `#[pg_extern]` wrappers live
//! here and delegate to `crate::search`. The 5-arg `highlight` wrappers
//! delegate to `crate::highlight_udfs`. Diagnostics (`tokenize`, `ql_parse`,
//! `builtin_stop_words`, `index_stats`, `index_analysis`) wrap `crate::udfs`,
//! `crate::stopwords`, and `crate::dict`. They do not import `IndexScorer`
//! or `storage` internals.

mod capabilities;
mod diagnostics;
mod highlight;
#[cfg(feature = "pg_test")]
mod score;
mod search;
