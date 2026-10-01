// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("truncated segment data")]
    Truncated,
    #[error("corrupt segment data: {0}")]
    Corrupt(&'static str),
    /// An `LSG1`–`LSG4` blob from the 0.4.0 lineage. Not readable as STN3.
    #[error("stannum: index requires REINDEX to 0.5.0 (pre-STN3 segment)")]
    PreStn3,
    #[error("invalid tuple location")]
    InvalidTid,
    #[error("input must be strictly increasing")]
    Unordered,
    #[error("term-frequency bucket must be at most 15")]
    InvalidTfBucket,
    #[error("term must be non-empty")]
    EmptyTerm,
    #[error("positions must be non-empty and strictly increasing")]
    InvalidPositions,
}

pub type Result<T> = std::result::Result<T, Error>;
