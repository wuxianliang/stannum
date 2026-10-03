// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! L1 field model (design §3, §5.1).
//!
//! This module owns the fielded-term codec, `LogicalPostingCursor`, the
//! expansion adapter, fused BM25F arithmetic, df_agg union-count, and the
//! fused WAND bound. L2 (`tool`) calls this layer. It does not reach into
//! segment internals except through `Index::term` / `Window` / `expand` /
//! `scan_window`, the `Term` streams the adapter wraps, and `Ordinals` /
//! `ChunkBound` the bound reads. STNF trailer writers landed in plan 4.3;
//! fused bound is plan 4.4.
//!
//! It does not absorb the owned shims design §3 lists: `operator.rs`,
//! `score.rs`, `highlight_udfs.rs`, `customscan.rs`, `am.rs`, `options.rs`,
//! `storage/layout.rs`, and `tinql/`. Those stay shared surface. A merge
//! conflict in them is expected and reviewed, not evidence this seam slipped.

#![allow(dead_code)]
#![allow(unused_imports)]

mod bound;
mod codec;
mod cursor;
mod df;
mod error;
mod expand;
mod score;
mod types;

pub(crate) use bound::{fused_bound, fused_interval_bound, next_interval_end};
pub(crate) use codec::{decode, fielded_key, header, upper_fence};
pub(crate) use cursor::{FieldHit, LogicalPostingCursor};
pub(crate) use df::{query_total_df, union_df_agg, union_df_agg_from_streams};
pub(crate) use error::{AdapterError, FieldKeyError, KeyDefect, ReportMode};
pub(crate) use expand::{SurfaceWindow, expand, expand_in, lookup};
pub(crate) use score::{
    all_fields_mask, fused_avgdl, fused_idf, fused_len, fused_score, fused_tf, raw_tf_from_hits,
    saturate,
};
pub(crate) use types::{FieldTerm, LogicalTerm, Lookup, fields_in_mask};
