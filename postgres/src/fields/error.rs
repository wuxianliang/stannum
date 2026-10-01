// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Fielded-key defects and the two ways callers report them.
//!
//! The decoder returns one [`KeyDefect`]. Query-time mapping is an error;
//! verify-time mapping of a stored dictionary key is corruption. They are
//! distinct kinds. STNF sidecar errors belong to plan 4.3, not here.

use thiserror::Error;

/// Why a fielded dictionary key is not `~` + one lowercase hex nibble + `~`
/// + escaped token.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum KeyDefect {
    #[error("header is not '~' plus one lowercase hex nibble plus '~'")]
    BadHeader,
    #[error("trailing incomplete tilde escape")]
    UnbalancedEscape,
    #[error("decoded token is empty")]
    EmptyToken,
    #[error("field ordinal {ordinal} is outside 0..{field_count}")]
    OrdinalOutOfRange { ordinal: u8, field_count: u8 },
}

/// Which reporting kind a caller wants for a [`KeyDefect`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReportMode {
    Query,
    Verify,
}

/// Query-time malformed key versus verify-time stored corruption. Same
/// defect, two kinds: tests assert they are not equal.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum FieldKeyError {
    #[error("malformed fielded term key: {0}")]
    Query(KeyDefect),
    #[error("corrupt stored fielded term key: {0}")]
    Verify(KeyDefect),
}

impl FieldKeyError {
    #[must_use]
    pub(crate) fn report(defect: KeyDefect, mode: ReportMode) -> Self {
        match mode {
            ReportMode::Query => Self::Query(defect),
            ReportMode::Verify => Self::Verify(defect),
        }
    }

    #[must_use]
    pub(crate) fn defect(&self) -> &KeyDefect {
        match self {
            Self::Query(defect) | Self::Verify(defect) => defect,
        }
    }
}

/// Lookup and expansion failures: a fielded-key defect, or the index.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub(crate) enum AdapterError {
    #[error(transparent)]
    Key(#[from] FieldKeyError),
    #[error(transparent)]
    Index(#[from] segment::Error),
}

pub(crate) fn query_defect(defect: KeyDefect) -> AdapterError {
    FieldKeyError::report(defect, ReportMode::Query).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_verify_are_distinct_kinds() {
        let defects = [
            KeyDefect::BadHeader,
            KeyDefect::UnbalancedEscape,
            KeyDefect::EmptyToken,
            KeyDefect::OrdinalOutOfRange {
                ordinal: 2,
                field_count: 2,
            },
        ];
        for defect in defects {
            let query = FieldKeyError::report(defect.clone(), ReportMode::Query);
            let verify = FieldKeyError::report(defect.clone(), ReportMode::Verify);
            assert_ne!(query, verify, "{defect}");
            assert_ne!(
                std::mem::discriminant(&query),
                std::mem::discriminant(&verify),
                "{defect}"
            );
            assert_ne!(query.to_string(), verify.to_string(), "{defect}");
            assert_eq!(query.defect(), &defect);
            assert_eq!(verify.defect(), &defect);
            assert!(query.to_string().starts_with("malformed fielded term key"));
            assert!(
                verify
                    .to_string()
                    .starts_with("corrupt stored fielded term key")
            );
        }
    }
}
