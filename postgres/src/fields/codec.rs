// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Fielded dictionary keys: one lowercase hex nibble and doubled tildes.

use super::error::KeyDefect;

/// `~` + one lowercase hex nibble + `~`. Never decimal, never `{:02x}`.
/// Only `0..=15`: `header(16)` is `~10~`, which sorts *before* `~f~`.
#[must_use]
pub(crate) fn header(ordinal: u8) -> String {
    assert!(
        ordinal <= 15,
        "header is one lowercase hex nibble; field 15's exclusive bound is upper_fence, not header(16)"
    );
    format!("~{ordinal:x}~")
}

/// Exclusive dictionary fence past `ordinal`'s keys.
///
/// For a non-last field this is [`header`] of the next ordinal, used as the
/// inclusive `Window::Range` upper bound (`~1~` is not a payload key). Field
/// 15 — and any last field — returns `None`: the prefix end, **not**
/// `header(16)` (`~10~` sorts before `~f~`).
#[must_use]
pub(crate) fn upper_fence(ordinal: u8, field_count: u8) -> Option<String> {
    debug_assert!(ordinal <= 15 && ordinal < field_count);
    let next = ordinal.checked_add(1)?;
    if next > 15 || next >= field_count {
        None
    } else {
        Some(header(next))
    }
}

fn check_ordinal(ordinal: u8, field_count: u8) -> Result<(), KeyDefect> {
    if ordinal > 15 || ordinal >= field_count {
        Err(KeyDefect::OrdinalOutOfRange {
            ordinal,
            field_count,
        })
    } else {
        Ok(())
    }
}

fn escape(token: &str) -> String {
    let extra = token.chars().filter(|&c| c == '~').count();
    let mut out = String::with_capacity(token.len() + extra);
    for c in token.chars() {
        if c == '~' {
            out.push_str("~~");
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape(payload: &str) -> Result<String, KeyDefect> {
    let mut out = String::with_capacity(payload.len());
    let mut chars = payload.chars();
    while let Some(c) = chars.next() {
        if c == '~' {
            match chars.next() {
                Some('~') => out.push('~'),
                _ => return Err(KeyDefect::UnbalancedEscape),
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn nibble(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        'a'..='f' => Some(c as u8 - b'a' + 10),
        _ => None,
    }
}

/// `fielded_key(ordinal, token) = "~" + format!("{:x}", ordinal) + "~" + escape(token)`.
/// Every `~` in the payload doubles. `ordinal` must be `0..=15` and strictly
/// less than `field_count`.
pub(crate) fn fielded_key(ordinal: u8, token: &str, field_count: u8) -> Result<String, KeyDefect> {
    check_ordinal(ordinal, field_count)?;
    if token.is_empty() {
        return Err(KeyDefect::EmptyToken);
    }
    Ok(format!("~{:x}~{}", ordinal, escape(token)))
}

/// Inverse of [`fielded_key`]. `text` is an owned decoded surface token.
pub(crate) fn decode(key: &str, field_count: u8) -> Result<(u8, String), KeyDefect> {
    let mut chars = key.chars();
    if chars.next() != Some('~') {
        return Err(KeyDefect::BadHeader);
    }
    let Some(ordinal) = chars.next().and_then(nibble) else {
        return Err(KeyDefect::BadHeader);
    };
    if chars.next() != Some('~') {
        return Err(KeyDefect::BadHeader);
    }
    let token = unescape(chars.as_str())?;
    if token.is_empty() {
        return Err(KeyDefect::EmptyToken);
    }
    check_ordinal(ordinal, field_count)?;
    Ok((ordinal, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_field_zero_token_with_header_like_tildes() {
        let encoded = fielded_key(0, "~0~foo", 16).unwrap();
        assert_eq!(encoded, "~0~~~0~~foo");
        assert_ne!(encoded, "~0~~0~foo");
        assert_eq!(decode("~0~~~0~~foo", 16).unwrap(), (0, "~0~foo".to_owned()));
        assert_eq!(decode(&encoded, 16).unwrap(), (0, "~0~foo".to_owned()));
        assert_eq!(
            decode("~0~~0~foo", 16),
            Err(KeyDefect::UnbalancedEscape),
            "the wrong doubling must not decode as ~0~foo"
        );
    }

    #[test]
    fn round_trips_leading_trailing_repeated_and_adjacent_tildes() {
        let tokens = [
            "~foo", "foo~", "~~~~", "a~~b", "~", "~~", "a~b~c", "~a~", "a~b", "~~~x~~~",
        ];
        for token in tokens {
            for ordinal in 0..=15_u8 {
                let encoded = fielded_key(ordinal, token, 16).unwrap();
                assert_eq!(decode(&encoded, 16).unwrap(), (ordinal, token.to_owned()));
            }
        }
    }

    #[test]
    fn round_trips_every_ordinal_as_one_lowercase_hex_nibble() {
        for ordinal in 0..=15_u8 {
            let encoded = fielded_key(ordinal, "foo", 16).unwrap();
            let expected = format!("~{ordinal:x}~foo");
            assert_eq!(encoded, expected);
            assert_ne!(encoded, format!("~{ordinal:02x}~foo"));
            let nibble = encoded.as_bytes()[1];
            assert!(nibble.is_ascii_digit() || (b'a'..=b'f').contains(&nibble));
            assert_eq!(decode(&encoded, 16).unwrap(), (ordinal, "foo".to_owned()));
        }
        assert_eq!(fielded_key(10, "foo", 16).unwrap(), "~a~foo");
        assert_ne!(fielded_key(10, "foo", 16).unwrap(), "~10~foo");
        assert_ne!(fielded_key(10, "foo", 16).unwrap(), "~0a~foo");
        assert_eq!(decode("~10~foo", 16), Err(KeyDefect::BadHeader));
        assert_eq!(decode("~A~foo", 16), Err(KeyDefect::BadHeader));
        assert_eq!(decode("~0a~foo", 16), Err(KeyDefect::BadHeader));
    }

    #[test]
    fn rejects_malformed_headers_escapes_empty_tokens_and_ordinals() {
        assert_eq!(decode("", 2), Err(KeyDefect::BadHeader));
        assert_eq!(decode("foo", 2), Err(KeyDefect::BadHeader));
        assert_eq!(decode("~", 2), Err(KeyDefect::BadHeader));
        assert_eq!(decode("~0", 2), Err(KeyDefect::BadHeader));
        assert_eq!(decode("~g~foo", 2), Err(KeyDefect::BadHeader));
        assert_eq!(decode("~~0~foo", 2), Err(KeyDefect::BadHeader));
        assert_eq!(decode("~0~", 2), Err(KeyDefect::EmptyToken));
        assert_eq!(fielded_key(0, "", 2), Err(KeyDefect::EmptyToken));
        assert_eq!(decode("~0~foo~", 2), Err(KeyDefect::UnbalancedEscape));
        assert_eq!(decode("~0~~~~", 2), Err(KeyDefect::UnbalancedEscape));
        assert_eq!(decode("~0~~~", 2).unwrap(), (0, "~".to_owned()));
        assert_eq!(fielded_key(0, "~", 2).unwrap(), "~0~~~");
        assert_eq!(
            fielded_key(16, "foo", 16),
            Err(KeyDefect::OrdinalOutOfRange {
                ordinal: 16,
                field_count: 16
            })
        );
        assert_eq!(
            fielded_key(2, "foo", 2),
            Err(KeyDefect::OrdinalOutOfRange {
                ordinal: 2,
                field_count: 2
            })
        );
        assert_eq!(
            decode("~2~foo", 2),
            Err(KeyDefect::OrdinalOutOfRange {
                ordinal: 2,
                field_count: 2
            })
        );
        assert_eq!(
            decode("~f~foo", 2),
            Err(KeyDefect::OrdinalOutOfRange {
                ordinal: 15,
                field_count: 2
            })
        );
    }

    #[test]
    fn header_sixteen_sorts_before_field_fifteen_so_upper_fence_is_prefix_end() {
        assert_eq!(header(15), "~f~");
        assert!(
            "~10~" < "~f~",
            "header(16) would be ~10~, which is not an exclusive bound past ~f~"
        );
        assert_eq!(upper_fence(15, 16), None);
        assert_eq!(upper_fence(14, 16).as_deref(), Some("~f~"));
        assert_eq!(upper_fence(0, 2).as_deref(), Some("~1~"));
        assert_eq!(upper_fence(1, 2), None);
        assert_eq!(upper_fence(15, 16), upper_fence(1, 2));
    }
}
