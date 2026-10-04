// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! L1 field model (design §3, §5.1, §7).
//!
//! This module owns channel unpack, the fused scorer, the bound, and the
//! norms reader. It does not own a key codec. Generation is the buffer
//! tag, not key spelling. L2 (`tool`) calls this layer. It does not reach
//! into segment internals except through `Index::term` / `Window` /
//! `expand` / `scan_window`, the `Term` streams the adapter wraps, and
//! `Ordinals` / `ChunkBound` the bound reads.
//!
//! It does not absorb the owned shims design §3 lists: `operator.rs`,
//! `score.rs`, `highlight_udfs.rs`, `customscan.rs`, `am.rs`, `options.rs`,
//! `storage/layout.rs`, and `tinql/`. Those stay shared surface. A merge
//! conflict in them is expected and reviewed, not evidence this seam slipped.

#![allow(dead_code)]
#![allow(unused_imports)]

mod bound;
mod cursor;
mod df;
mod error;
mod expand;
mod intersect;
pub(crate) mod profile;
mod score;
mod types;

pub(crate) use bound::{
    fused_bound, fused_interval_bound, fused_interval_bound_from_term, next_interval_end,
};
pub(crate) use cursor::{FieldHit, FieldTf, LogicalPostingCursor};
pub(crate) use df::{query_total_df, union_df_agg, union_df_agg_from_streams};
pub(crate) use error::AdapterError;
pub(crate) use expand::{SurfaceWindow, expand, expand_in, lookup};
pub(crate) use intersect::{Front, Intersect, next_atleast, next_conjunction, next_union};
pub(crate) use score::{
    all_fields_mask, buckets_from_tfs, fused_avgdl, fused_idf, fused_len, fused_score,
    fused_score_from_buckets, fused_score_from_term, fused_tf, fused_tf_from_buckets,
    raw_tf_from_hits, saturate,
};
pub(crate) use types::{FieldTerm, LogicalTerm, Lookup, fields_in_mask};
