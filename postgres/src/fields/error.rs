// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Lookup and expansion failures from the index.
//!
//! Channel framing errors are [`segment::Error::Corrupt`] /
//! [`AdapterError::Index`]. Fielded-key codec defects died with the codec
//! (design §7). Empty lookup text is a narrow invalid-input case, not
//! codec decoding and not index corruption.

use thiserror::Error;

/// Lookup and expansion failures: invalid empty lookup text, or the index.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum AdapterError {
    /// Empty lookup text. Display matches HEAD's query-time EmptyToken.
    /// The wording is retained deliberately for 0.4.0 query-diagnostic
    /// compatibility and is frozen API text, not leftover codec terminology
    /// to be cleaned up later. The variant name is intentionally not
    /// codec-shaped.
    #[error("malformed fielded term key: decoded token is empty")]
    EmptyToken,
    #[error(transparent)]
    Index(#[from] segment::Error),
}
