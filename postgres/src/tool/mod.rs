// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! L2 tool contract (design §3).
//!
//! This layer owns the SQL entry points, including `capabilities()`. It
//! imports nothing from engine internals: not `IndexScorer`, not
//! `storage::View`, and not segment internals. L2 may later call the field
//! sidecar API; it does not reach past that API. `capabilities` needs no
//! internals and must stay that way.

mod capabilities;
