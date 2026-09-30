// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! L1 field model (design §3, §5).
//!
//! Contract, not yet implemented: this module owns the fielded-term codec,
//! the fused scorer, and the norms and aggregate-df readers. L2 (`tool`)
//! calls this layer. It does not reach into segment internals except through
//! the sidecar API this module owns.
//!
//! It does not absorb the owned shims design §3 lists: `operator.rs`,
//! `score.rs`, `highlight_udfs.rs`, `customscan.rs`, `am.rs`, `options.rs`,
//! `storage/layout.rs`, and `tinql/`. Those stay shared surface. A merge
//! conflict in them is expected and reviewed, not evidence this seam slipped.
//!
//! Empty on purpose. No items until the fielded-term work lands.
