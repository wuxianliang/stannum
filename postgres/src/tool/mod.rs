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
//! needs the field model, on the `fields` layer (L1). `fields` is declared
//! but has no items yet, and this module does not import it, so L2 stays
//! self-contained. Later steps (2.2-2.6) land UDF families here one at a
//! time.

mod capabilities;
