// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Multi-column caller of `Index::term` / `Window` / `scan_window`.
//!
//! `field_count == 1` keeps the parent split (finite stock expand for
//! candidates, scoring expand at `usize::MAX`). `field_count` in `2..=16`
//! walks `scan_window` only: (a) capped candidates, (b) uncapped scoring.

use segment::index::{Expanded, Index, Window};

use super::error::AdapterError;
use super::types::LogicalTerm;
use super::types::Lookup;

/// Surface (decoded) window. On STN4 the dictionary key is this text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceWindow<'q> {
    Prefix(&'q str),
    /// Inclusive decoded bounds; `None` is open.
    Range(Option<&'q str>, Option<&'q str>),
    All,
}

impl<'q> From<SurfaceWindow<'q>> for Window<'q> {
    fn from(window: SurfaceWindow<'q>) -> Self {
        match window {
            SurfaceWindow::Prefix(prefix) => Window::Prefix(prefix),
            SurfaceWindow::Range(lower, upper) => Window::Range(lower, upper),
            SurfaceWindow::All => Window::All,
        }
    }
}

fn field_count_ok(field_count: u8) -> Result<(), AdapterError> {
    if field_count == 1 || (2..=16).contains(&field_count) {
        Ok(())
    } else {
        Err(segment::Error::Corrupt("field_count").into())
    }
}

/// Direct lookup: one `Index::term` on the surface token. Absent from the
/// dictionary is [`Lookup::Term`] with empty streams, not an error.
/// `df_agg` is the parent `Term::df()` (0 if absent). The scope mask does
/// not change it.
pub(crate) fn lookup<'a>(
    index: &'a dyn Index,
    text: &str,
    mask: u16,
    field_count: u8,
) -> Result<Lookup<'a>, AdapterError> {
    let span = super::profile::Span::begin();
    let result = lookup_inner(index, text, mask, field_count);
    super::profile::add_lookup(std::time::Duration::from_nanos(span.ns()));
    result
}

fn lookup_inner<'a>(
    index: &'a dyn Index,
    text: &str,
    mask: u16,
    field_count: u8,
) -> Result<Lookup<'a>, AdapterError> {
    if text.is_empty() {
        return Err(AdapterError::EmptyToken);
    }
    field_count_ok(field_count)?;
    match index.term(text)? {
        None => Ok(Lookup::Term(LogicalTerm::absent(text.to_owned(), mask))),
        Some(term) => Ok(Lookup::Term(LogicalTerm::from_entry(
            text.to_owned(),
            mask,
            field_count,
            term,
        )?)),
    }
}

fn stock_logical<'a>(
    pairs: Vec<(String, segment::segment::Term<'a>)>,
    mask: u16,
) -> Result<Vec<LogicalTerm<'a>>, AdapterError> {
    let mut terms = Vec::with_capacity(pairs.len());
    for (text, term) in pairs {
        let logical = LogicalTerm::from_entry(text, mask, 1, term)?;
        if logical.streams.is_empty() {
            continue;
        }
        terms.push(logical);
    }
    Ok(terms)
}

fn expand_single<'a>(
    index: &'a dyn Index,
    window: SurfaceWindow<'_>,
    filter: &'a dyn Fn(&str) -> bool,
    mask: u16,
    max_expansion: usize,
) -> Result<Lookup<'a>, AdapterError> {
    match index.expand(window.into(), filter, max_expansion)? {
        Expanded::Overflow => Ok(Lookup::Overflow),
        Expanded::Terms(pairs) => Ok(Lookup::Terms(stock_logical(pairs, mask)?)),
    }
}

fn expand_in_single<'a>(
    index: &'a dyn Index,
    window: SurfaceWindow<'_>,
    filter: &'a dyn Fn(&str) -> bool,
    mask: u16,
) -> Result<Vec<LogicalTerm<'a>>, AdapterError> {
    match index.expand(window.into(), filter, usize::MAX)? {
        Expanded::Overflow => Err(segment::Error::Corrupt("single-column scoring overflow").into()),
        Expanded::Terms(pairs) => stock_logical(pairs, mask),
    }
}

/// (a) Capped candidate expansion. O(`max_expansion`) retention. Skip
/// `ch ∩ mask == ∅` before counting. Keep iterating after overflow so a
/// later channel error still wins. Precedence: error > Overflow > Terms.
fn expand_capped<'a>(
    index: &'a dyn Index,
    window: SurfaceWindow<'_>,
    filter: &'a dyn Fn(&str) -> bool,
    mask: u16,
    field_count: u8,
    max_expansion: usize,
) -> Result<Lookup<'a>, AdapterError> {
    let mut kept = Vec::with_capacity(max_expansion);
    let mut overflow = false;
    let mut count = 0usize;
    for item in index.scan_window(window.into(), filter)? {
        let (text, term) = item?;
        let logical = LogicalTerm::from_entry(text, mask, field_count, term)?;
        if logical.streams.is_empty() {
            continue;
        }
        count += 1;
        if count <= max_expansion {
            kept.push(logical);
        } else {
            overflow = true;
        }
    }
    if overflow {
        Ok(Lookup::Overflow)
    } else {
        Ok(Lookup::Terms(kept))
    }
}

/// (b) Uncapped scoring expansion. Same text filter and mask; no cap; no
/// [`Lookup::Overflow`]. Memory is O(in-scope hits).
fn expand_uncapped<'a>(
    index: &'a dyn Index,
    window: SurfaceWindow<'_>,
    filter: &'a dyn Fn(&str) -> bool,
    mask: u16,
    field_count: u8,
) -> Result<Vec<LogicalTerm<'a>>, AdapterError> {
    let mut scored = Vec::new();
    for item in index.scan_window(window.into(), filter)? {
        let (text, term) = item?;
        let logical = LogicalTerm::from_entry(text, mask, field_count, term)?;
        if logical.streams.is_empty() {
            continue;
        }
        scored.push(logical);
    }
    Ok(scored)
}

/// Capped candidate expansion (`search` / `search_count` / custom scan).
pub(crate) fn expand<'a>(
    index: &'a dyn Index,
    window: SurfaceWindow<'_>,
    filter: &'a dyn Fn(&str) -> bool,
    mask: u16,
    field_count: u8,
    max_expansion: usize,
) -> Result<Lookup<'a>, AdapterError> {
    field_count_ok(field_count)?;
    if field_count == 1 {
        return expand_single(index, window, filter, mask, max_expansion);
    }
    expand_capped(index, window, filter, mask, field_count, max_expansion)
}

/// Uncapped scoring expansion (`score` / `score_bound_indexed`).
pub(crate) fn expand_in<'a>(
    index: &'a dyn Index,
    window: SurfaceWindow<'_>,
    filter: &'a dyn Fn(&str) -> bool,
    mask: u16,
    field_count: u8,
) -> Result<Vec<LogicalTerm<'a>>, AdapterError> {
    field_count_ok(field_count)?;
    if field_count == 1 {
        return expand_in_single(index, window, filter, mask);
    }
    expand_uncapped(index, window, filter, mask, field_count)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{Lookup, SurfaceWindow, expand, expand_in, lookup};
    use crate::fields::df::union_df_agg_from_streams;
    use crate::fields::error::AdapterError;
    use crate::fields::score::all_fields_mask;
    use segment::Tid;
    use segment::channels;
    use segment::forward::ForwardRecord;
    use segment::index::{Index, MutableIndex};

    const FIELDS: u8 = 2;
    const ALWAYS: fn(&str) -> bool = |_| true;

    fn add_fielded(index: &MutableIndex, id: u32, columns: &[&str]) {
        index
            .begin_fielded_document(Tid::new(id, 1).unwrap())
            .unwrap();
        for (field, text) in columns.iter().enumerate() {
            let mut by_term: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
            let mut len = 0u32;
            for (i, word) in text.split_whitespace().enumerate() {
                len += 1;
                by_term.entry(word).or_default().push(i as u32 + 1);
            }
            if len == 0 {
                continue;
            }
            for (word, positions) in by_term {
                index
                    .add_occurrence(word, field as u8, &positions, len)
                    .unwrap();
            }
        }
    }

    fn names(lookup: &Lookup<'_>) -> Vec<(String, u16, Vec<u8>)> {
        match lookup {
            Lookup::Term(term) => vec![(
                term.text.clone(),
                term.mask,
                term.streams.iter().map(|s| s.field).collect(),
            )],
            Lookup::Terms(terms) => terms
                .iter()
                .map(|term| {
                    (
                        term.text.clone(),
                        term.mask,
                        term.streams.iter().map(|s| s.field).collect(),
                    )
                })
                .collect(),
            Lookup::Overflow => vec![("<overflow>".into(), 0, vec![])],
        }
    }

    fn fixture() -> MutableIndex {
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        add_fielded(&index, 0, &["foo", "foo"]);
        add_fielded(&index, 1, &["food", ""]);
        add_fielded(&index, 2, &["", "fool"]);
        add_fielded(&index, 3, &["", "bar"]);
        index
    }

    fn expand_all<'a>(
        index: &'a MutableIndex,
        window: SurfaceWindow<'_>,
        mask: u16,
        field_count: u8,
        max_expansion: usize,
    ) -> Lookup<'a> {
        expand(index, window, &ALWAYS, mask, field_count, max_expansion).unwrap()
    }

    #[test]
    fn groups_by_decoded_text_with_scope_mask_and_text_order() {
        let index = fixture();
        let mask = all_fields_mask(FIELDS);
        let Lookup::Terms(terms) =
            expand_all(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 16)
        else {
            panic!("prefix expansion returns Terms");
        };
        assert_eq!(
            names(&Lookup::Terms(terms)),
            vec![
                ("foo".into(), mask, vec![0, 1]),
                ("food".into(), mask, vec![0]),
                ("fool".into(), mask, vec![1]),
            ]
        );
    }

    #[test]
    fn collapsed_mask_is_scope_not_the_hit_set() {
        let index = fixture();
        let mask = all_fields_mask(FIELDS);
        let Lookup::Term(bar) = lookup(&index, "bar", mask, FIELDS).unwrap() else {
            panic!("direct lookup");
        };
        assert_eq!(bar.mask, mask);
        assert_eq!(
            bar.streams.iter().map(|s| s.field).collect::<Vec<_>>(),
            vec![1]
        );

        let scoped = 1u16 << 0;
        let Lookup::Terms(terms) =
            expand_all(&index, SurfaceWindow::Prefix("fo"), scoped, FIELDS, 16)
        else {
            panic!("scoped prefix");
        };
        for term in &terms {
            assert_eq!(term.mask, scoped);
            assert!(term.streams.iter().all(|s| s.field == 0));
        }
        assert_eq!(
            terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["foo", "food"]
        );
    }

    #[test]
    fn max_expansion_counts_logical_tokens_once_and_overflows() {
        let index = fixture();
        let mask = all_fields_mask(FIELDS);
        assert!(matches!(
            expand_all(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 2),
            Lookup::Overflow
        ));
        assert!(matches!(
            expand_all(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 1),
            Lookup::Overflow
        ));
        let Lookup::Terms(terms) = expand_all(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 3)
        else {
            panic!("limit 3 fits foo/food/fool");
        };
        assert_eq!(terms.len(), 3);

        let copies = MutableIndex::with_field_count(FIELDS).unwrap();
        add_fielded(&copies, 0, &["aa", "aa"]);
        add_fielded(&copies, 1, &["bb", "bb"]);
        let Lookup::Terms(terms) = expand_all(&copies, SurfaceWindow::All, mask, FIELDS, 2) else {
            panic!("field copies count once");
        };
        assert_eq!(
            names(&Lookup::Terms(terms)),
            vec![
                ("aa".into(), mask, vec![0, 1]),
                ("bb".into(), mask, vec![0, 1]),
            ]
        );
    }

    #[test]
    fn range_stays_inside_the_decoded_window() {
        let index = fixture();
        let mask = all_fields_mask(FIELDS);
        let Lookup::Terms(terms) = expand_all(
            &index,
            SurfaceWindow::Range(Some("foo"), Some("food")),
            mask,
            FIELDS,
            16,
        ) else {
            panic!("range");
        };
        assert_eq!(
            terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["foo", "food"]
        );
    }

    #[test]
    fn lookup_and_expand_df_agg_is_parent_term_df() {
        let index = fixture();
        let mask = all_fields_mask(FIELDS);
        let Lookup::Term(foo) = lookup(&index, "foo", mask, FIELDS).unwrap() else {
            panic!("foo");
        };
        assert!(
            !foo.streams.is_empty(),
            "nonempty streams must not leave df_agg at the silent 0"
        );
        let parent = u64::from(index.term("foo").unwrap().unwrap().df());
        assert_eq!(foo.df_agg, parent);
        assert_eq!(
            foo.df_agg, 1,
            "foo is only in doc 0; both-field postings count once"
        );
        assert_eq!(
            foo.streams.iter().map(|s| s.field).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            foo.df_agg,
            union_df_agg_from_streams(&foo.streams).unwrap(),
            "unscoped streams still match the stored union"
        );

        let Lookup::Term(bar) = lookup(&index, "bar", mask, FIELDS).unwrap() else {
            panic!("bar");
        };
        assert_eq!(
            bar.df_agg,
            u64::from(index.term("bar").unwrap().unwrap().df())
        );
        assert_eq!(bar.df_agg, 1);

        let Lookup::Term(absent) = lookup(&index, "zzz", mask, FIELDS).unwrap() else {
            panic!("absent");
        };
        assert!(absent.streams.is_empty());
        assert_eq!(absent.df_agg, 0);

        let Lookup::Terms(terms) =
            expand_all(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 16)
        else {
            panic!("prefix");
        };
        for term in &terms {
            assert!(!term.streams.is_empty(), "{} must have streams", term.text);
            let parent = u64::from(index.term(&term.text).unwrap().unwrap().df());
            assert_eq!(term.df_agg, parent, "{} df_agg vs parent", term.text);
            assert!(term.df_agg > 0, "{} df_agg", term.text);
        }
    }

    #[test]
    fn empty_lookup_text_is_query_invalid_not_index() {
        let index = MutableIndex::default();
        let err = lookup(&index, "", 1, 1).unwrap_err();
        assert_eq!(
            err.to_string(),
            "malformed fielded term key: decoded token is empty"
        );
        assert!(
            !matches!(err, AdapterError::Index(_)),
            "empty lookup is invalid input, not index corruption: {err:?}"
        );
    }

    #[test]
    fn empty_streams_are_not_an_error() {
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        let Lookup::Term(missing) = lookup(&index, "foo", all_fields_mask(FIELDS), FIELDS).unwrap()
        else {
            panic!("empty index lookup is Term with empty streams");
        };
        assert!(missing.streams.is_empty());
        assert_eq!(missing.df_agg, 0);
        let cursor = missing.cursor().unwrap();
        assert_eq!(cursor.current_ordinal(), None);

        let index = fixture();
        let title = 1u16 << 0;
        let Lookup::Term(bar) = lookup(&index, "bar", title, FIELDS).unwrap() else {
            panic!("body-only token under title mask");
        };
        assert!(bar.streams.is_empty());
        assert_eq!(bar.df_agg, 1, "parent df, not the empty hit-set");
        assert_eq!(bar.cursor().unwrap().current_ordinal(), None);
    }

    #[test]
    fn candidate_overflow_is_still_lookup_overflow() {
        let index = fixture();
        let mask = all_fields_mask(FIELDS);
        assert!(matches!(
            expand_all(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 2),
            Lookup::Overflow
        ));
        let scored = expand_in(&index, SurfaceWindow::Prefix("fo"), &ALWAYS, mask, FIELDS).unwrap();
        assert_eq!(
            scored.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["foo", "food", "fool"]
        );
    }

    #[test]
    fn scoped_mask_does_not_change_df_agg() {
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        add_fielded(&index, 0, &["shared", ""]);
        add_fielded(&index, 1, &["", "shared"]);
        add_fielded(&index, 2, &["shared", "shared"]);
        let parent = u64::from(index.term("shared").unwrap().unwrap().df());
        assert_eq!(parent, 3);
        let title = 1u16 << 0;
        let body = 1u16 << 1;
        let Lookup::Term(unscoped) =
            lookup(&index, "shared", all_fields_mask(FIELDS), FIELDS).unwrap()
        else {
            panic!("unscoped");
        };
        let Lookup::Term(title_only) = lookup(&index, "shared", title, FIELDS).unwrap() else {
            panic!("title");
        };
        let Lookup::Term(body_only) = lookup(&index, "shared", body, FIELDS).unwrap() else {
            panic!("body");
        };
        assert_eq!(unscoped.df_agg, 3);
        assert_eq!(title_only.df_agg, 3);
        assert_eq!(body_only.df_agg, 3);
        assert_eq!(
            title_only
                .streams
                .iter()
                .map(|s| s.field)
                .collect::<Vec<_>>(),
            vec![0]
        );
        assert_eq!(
            body_only
                .streams
                .iter()
                .map(|s| s.field)
                .collect::<Vec<_>>(),
            vec![1]
        );
        let title_union = union_df_agg_from_streams(&title_only.streams).unwrap();
        assert_eq!(title_union, 2);
        assert_ne!(title_only.df_agg, title_union);
    }

    fn scoped_cap_index(title: &[&str], body: &[&str]) -> MutableIndex {
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        let mut id = 0u32;
        for token in title {
            add_fielded(&index, id, &[token, ""]);
            id += 1;
        }
        for token in body {
            add_fielded(&index, id, &["", token]);
            id += 1;
        }
        index
    }

    fn title_texts(lookup: &Lookup<'_>) -> Vec<String> {
        match lookup {
            Lookup::Terms(terms) => terms.iter().map(|t| t.text.clone()).collect(),
            other => panic!("expected Terms, got {other:?}"),
        }
    }

    #[test]
    fn scoped_cap_prefix_ignores_body_only_hits() {
        let title = ["pre0", "pre1"];
        let body: Vec<String> = (0..8).map(|i| format!("preb{i}")).collect();
        let body: Vec<&str> = body.iter().map(String::as_str).collect();
        let index = scoped_cap_index(&title, &body);
        let mask = 1u16 << 0;
        let Lookup::Terms(terms) =
            expand_all(&index, SurfaceWindow::Prefix("pre"), mask, FIELDS, 2)
        else {
            panic!("title prefix fits");
        };
        assert_eq!(title_texts(&Lookup::Terms(terms)), vec!["pre0", "pre1"]);
        assert!(matches!(
            expand_all(&index, SurfaceWindow::Prefix("pre"), mask, FIELDS, 1),
            Lookup::Overflow
        ));
    }

    #[test]
    fn scoped_cap_range_ignores_body_only_hits() {
        let title = ["na", "nb"];
        let body = ["nc", "nd", "ne", "nf", "ng"];
        let index = scoped_cap_index(&title, &body);
        let mask = 1u16 << 0;
        let window = SurfaceWindow::Range(Some("na"), Some("nz"));
        let Lookup::Terms(terms) = expand_all(&index, window, mask, FIELDS, 2) else {
            panic!("title range fits");
        };
        assert_eq!(title_texts(&Lookup::Terms(terms)), vec!["na", "nb"]);
        assert!(matches!(
            expand_all(&index, window, mask, FIELDS, 1),
            Lookup::Overflow
        ));
    }

    fn ends_with_x(token: &str) -> bool {
        token.ends_with('x')
    }

    #[test]
    fn scoped_cap_regex_ignores_body_only_hits() {
        let title = ["ax", "bx"];
        let body = ["cx", "dx", "ex", "fx", "gx"];
        let index = scoped_cap_index(&title, &body);
        let mask = 1u16 << 0;
        let Lookup::Terms(terms) =
            expand(&index, SurfaceWindow::All, &ends_with_x, mask, FIELDS, 2).unwrap()
        else {
            panic!("title regex fits");
        };
        assert_eq!(title_texts(&Lookup::Terms(terms)), vec!["ax", "bx"]);
        assert!(matches!(
            expand(&index, SurfaceWindow::All, &ends_with_x, mask, FIELDS, 1).unwrap(),
            Lookup::Overflow
        ));
    }

    fn edit_distance(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.iter().enumerate() {
            let mut next = vec![i + 1];
            for (j, cb) in b.iter().enumerate() {
                let cost = usize::from(ca != cb);
                next.push((prev[j + 1] + 1).min(next[j] + 1).min(prev[j] + cost));
            }
            prev = next;
        }
        *prev.last().unwrap()
    }

    fn fuzzy_cat(token: &str) -> bool {
        edit_distance(token, "cat") <= 1
    }

    #[test]
    fn scoped_cap_fuzzy_ignores_body_only_hits() {
        let title = ["cat", "cot"];
        let body = ["bat", "cab", "car", "cut", "rat"];
        let index = scoped_cap_index(&title, &body);
        let mask = 1u16 << 0;
        let Lookup::Terms(terms) =
            expand(&index, SurfaceWindow::All, &fuzzy_cat, mask, FIELDS, 2).unwrap()
        else {
            panic!("title fuzzy fits");
        };
        assert_eq!(title_texts(&Lookup::Terms(terms)), vec!["cat", "cot"]);
        assert!(matches!(
            expand(&index, SurfaceWindow::All, &fuzzy_cat, mask, FIELDS, 1).unwrap(),
            Lookup::Overflow
        ));
    }

    #[test]
    fn streaming_out_of_scope_window_retains_at_most_the_cap() {
        let title = ["zz0", "zz1"];
        let body: Vec<String> = (0..2000).map(|i| format!("aa{i:04}")).collect();
        let body: Vec<&str> = body.iter().map(String::as_str).collect();
        let index = scoped_cap_index(&title, &body);
        let mask = 1u16 << 0;
        let Lookup::Terms(terms) = expand_all(&index, SurfaceWindow::All, mask, FIELDS, 2) else {
            panic!("title-only set fits the cap");
        };
        assert_eq!(terms.len(), 2);
        assert_eq!(
            terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["zz0", "zz1"]
        );
        assert!(
            terms.capacity() <= 2 || terms.len() == 2,
            "capped path retains O(max_expansion), not the window"
        );
    }

    #[test]
    fn expand_in_keeps_complete_set_when_candidates_overflow() {
        let title = ["t0", "t1", "t2", "t3", "t4"];
        let index = scoped_cap_index(&title, &[]);
        let mask = 1u16 << 0;
        assert!(matches!(
            expand_all(&index, SurfaceWindow::Prefix("t"), mask, FIELDS, 2),
            Lookup::Overflow
        ));
        let scored = expand_in(&index, SurfaceWindow::Prefix("t"), &ALWAYS, mask, FIELDS).unwrap();
        assert_eq!(
            scored.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            title
        );
        let Lookup::Terms(raised) =
            expand_all(&index, SurfaceWindow::Prefix("t"), mask, FIELDS, 16)
        else {
            panic!("raised cap");
        };
        assert_eq!(
            raised.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            scored.iter().map(|t| t.text.as_str()).collect::<Vec<_>>()
        );
    }

    fn plant_corrupt(index: &MutableIndex, token: &str) {
        let empty = channels::frame(&[]);
        index
            .install_term_extents(token, empty.clone(), empty, 1, 0)
            .unwrap();
    }

    #[test]
    fn corrupt_fch1_is_an_error_on_both_consumers() {
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        add_fielded(&index, 0, &["aaa", ""]);
        add_fielded(&index, 1, &["aab", ""]);
        add_fielded(&index, 2, &["zzz", ""]);
        plant_corrupt(&index, "zzz");
        let mask = 1u16 << 0;
        let capped = expand(&index, SurfaceWindow::All, &ALWAYS, mask, FIELDS, 16);
        assert!(capped.is_err(), "capped consumer surfaces channel error");
        let uncapped = expand_in(&index, SurfaceWindow::All, &ALWAYS, mask, FIELDS);
        assert!(uncapped.is_err(), "scoring consumer surfaces channel error");
    }

    #[test]
    fn corrupt_fch1_wins_after_in_scope_overflow() {
        let index = MutableIndex::with_field_count(FIELDS).unwrap();
        add_fielded(&index, 0, &["aaa", ""]);
        add_fielded(&index, 1, &["aab", ""]);
        add_fielded(&index, 2, &["aac", ""]);
        add_fielded(&index, 3, &["zzz", ""]);
        plant_corrupt(&index, "zzz");
        let mask = 1u16 << 0;
        let capped = expand(&index, SurfaceWindow::All, &ALWAYS, mask, FIELDS, 2);
        assert!(
            capped.is_err(),
            "error beats Overflow after the cap is already exceeded"
        );
        assert!(
            matches!(capped, Err(AdapterError::Index(_))),
            "expected Index error, got {capped:?}"
        );
        assert!(expand_in(&index, SurfaceWindow::All, &ALWAYS, mask, FIELDS).is_err());
    }

    #[test]
    fn open_upper_range_on_field_fifteen_uses_the_surface_token() {
        let index = MutableIndex::with_field_count(16).unwrap();
        index
            .begin_fielded_document(Tid::new(1, 1).unwrap())
            .unwrap();
        index.add_occurrence("zzz", 15, &[1], 1).unwrap();
        let mask = 1u16 << 15;
        let Lookup::Terms(terms) =
            expand_all(&index, SurfaceWindow::Range(None, None), mask, 16, 16)
        else {
            panic!("open-upper field 15");
        };
        assert_eq!(
            terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["zzz"]
        );
        assert_eq!(terms[0].mask, mask);
        assert_eq!(terms[0].df_agg, 1);
        assert_eq!(
            terms[0].streams.iter().map(|s| s.field).collect::<Vec<_>>(),
            vec![15]
        );
    }

    #[test]
    fn single_column_keeps_parent_expand_split() {
        let index = MutableIndex::default();
        index
            .add_record(
                ForwardRecord::from_tokens(Tid::new(1, 1).unwrap(), [("aa", 1), ("ab", 2)])
                    .unwrap(),
            )
            .unwrap();
        let mask = 1u16;
        assert!(matches!(
            expand_all(&index, SurfaceWindow::Prefix("a"), mask, 1, 1),
            Lookup::Overflow
        ));
        let Lookup::Terms(terms) = expand_all(&index, SurfaceWindow::Prefix("a"), mask, 1, 2)
        else {
            panic!("single-column cap 2");
        };
        assert_eq!(
            terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["aa", "ab"]
        );
        let scored = expand_in(&index, SurfaceWindow::Prefix("a"), &ALWAYS, mask, 1).unwrap();
        assert_eq!(
            scored.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["aa", "ab"]
        );
    }
}
