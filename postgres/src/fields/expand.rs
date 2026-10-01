// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Multi-column caller of `Index::term` / `Window` / `expand`.

use std::collections::BTreeMap;

use segment::index::{Expanded, Index, Window};

use super::codec::{decode, fielded_key, header, upper_fence};
use super::df::union_df_agg_from_streams;
use super::error::{AdapterError, KeyDefect, query_defect};
use super::types::{FieldTerm, LogicalTerm, Lookup, fields_in_mask};

/// Surface (decoded) window. The adapter escapes it per field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceWindow<'q> {
    Prefix(&'q str),
    /// Inclusive decoded bounds; `None` is open inside the field fence.
    Range(Option<&'q str>, Option<&'q str>),
    All,
}

/// Direct lookup: `term` once per field in the mask. Absent from every
/// selected field is [`Lookup::Term`] with empty streams, not an error.
pub(crate) fn lookup<'a>(
    index: &'a dyn Index,
    text: &str,
    mask: u16,
    field_count: u8,
) -> Result<Lookup<'a>, AdapterError> {
    if text.is_empty() {
        return Err(query_defect(KeyDefect::EmptyToken));
    }
    let mut streams = Vec::new();
    for field in fields_in_mask(mask, field_count) {
        let key = fielded_key(field, text, field_count).map_err(query_defect)?;
        if let Some(term) = index.term(&key)? {
            streams.push(FieldTerm { field, term });
        }
    }
    finish_term(text.to_owned(), mask, streams).map(Lookup::Term)
}

fn finish_term(
    text: String,
    mask: u16,
    streams: Vec<FieldTerm<'_>>,
) -> Result<LogicalTerm<'_>, AdapterError> {
    Ok(LogicalTerm {
        df_agg: union_df_agg_from_streams(&streams)?,
        text,
        mask,
        streams,
    })
}

/// Two-stage expansion: per-field `expand` with `max_expansion`, then merge
/// by decoded text. The collapsed mask is the scope, not the hit-set.
/// Per-field [`Expanded::Overflow`] is global overflow. `max_expansion`
/// counts logical tokens once.
pub(crate) fn expand<'a>(
    index: &'a dyn Index,
    window: SurfaceWindow<'_>,
    mask: u16,
    field_count: u8,
    max_expansion: usize,
) -> Result<Lookup<'a>, AdapterError> {
    let mut grouped: BTreeMap<String, Vec<FieldTerm<'a>>> = BTreeMap::new();
    for field in fields_in_mask(mask, field_count) {
        match expand_field(index, field, window, field_count, max_expansion)? {
            Expanded::Overflow => return Ok(Lookup::Overflow),
            Expanded::Terms(pairs) => {
                for (key, term) in pairs {
                    let (ordinal, text) = decode(&key, field_count).map_err(query_defect)?;
                    if ordinal != field {
                        return Err(query_defect(KeyDefect::OrdinalOutOfRange {
                            ordinal,
                            field_count,
                        }));
                    }
                    grouped
                        .entry(text)
                        .or_default()
                        .push(FieldTerm { field, term });
                }
            }
        }
    }
    if grouped.len() > max_expansion {
        return Ok(Lookup::Overflow);
    }
    let terms = grouped
        .into_iter()
        .map(|(text, streams)| finish_term(text, mask, streams))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Lookup::Terms(terms))
}

fn expand_field<'a>(
    index: &'a dyn Index,
    field: u8,
    window: SurfaceWindow<'_>,
    field_count: u8,
    limit: usize,
) -> Result<Expanded<'a>, AdapterError> {
    match window {
        SurfaceWindow::Prefix(prefix) => {
            let encoded = if prefix.is_empty() {
                header(field)
            } else {
                fielded_key(field, prefix, field_count).map_err(query_defect)?
            };
            Ok(index.expand(Window::Prefix(&encoded), &|_| true, limit)?)
        }
        SurfaceWindow::Range(lower, upper) => {
            let field_header = header(field);
            let lower_key = match lower {
                Some(text) => fielded_key(field, text, field_count).map_err(query_defect)?,
                None => field_header.clone(),
            };
            match upper {
                Some(text) => {
                    let upper_key = fielded_key(field, text, field_count).map_err(query_defect)?;
                    Ok(index.expand(
                        Window::Range(Some(&lower_key), Some(&upper_key)),
                        &|_| true,
                        limit,
                    )?)
                }
                None => {
                    let fence = upper_fence(field, field_count);
                    Ok(index.expand(
                        Window::Range(Some(&lower_key), fence.as_deref()),
                        &|key| key.starts_with(&field_header),
                        limit,
                    )?)
                }
            }
        }
        SurfaceWindow::All => {
            let field_header = header(field);
            Ok(index.expand(Window::All, &|key| key.starts_with(&field_header), limit)?)
        }
    }
}

#[cfg(test)]
mod tests {
    use segment::Tid;
    use segment::forward::ForwardRecord;
    use segment::index::MutableIndex;

    use super::{Lookup, SurfaceWindow, expand, fielded_key, lookup};
    use crate::fields::df::union_df_agg_from_streams;
    use crate::fields::score::all_fields_mask;

    const FIELDS: u8 = 2;

    fn add(index: &MutableIndex, id: u32, parts: &[(u8, &str)]) {
        let keys: Vec<String> = parts
            .iter()
            .map(|&(field, text)| fielded_key(field, text, FIELDS).unwrap())
            .collect();
        let tokens: Vec<(&str, u32)> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| (key.as_str(), i as u32 + 1))
            .collect();
        index
            .add_record(ForwardRecord::from_tokens(Tid::new(id, 1).unwrap(), tokens).unwrap())
            .unwrap();
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
        let index = MutableIndex::default();
        add(&index, 0, &[(0, "foo"), (1, "foo")]);
        add(&index, 1, &[(0, "food")]);
        add(&index, 2, &[(1, "fool")]);
        add(&index, 3, &[(1, "bar")]);
        index
    }

    #[test]
    fn groups_by_decoded_text_with_scope_mask_and_text_order() {
        let index = fixture();
        let mask = all_fields_mask(FIELDS);
        let Lookup::Terms(terms) =
            expand(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 16).unwrap()
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
            expand(&index, SurfaceWindow::Prefix("fo"), scoped, FIELDS, 16).unwrap()
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
            expand(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 2).unwrap(),
            Lookup::Overflow
        ));
        assert!(matches!(
            expand(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 1).unwrap(),
            Lookup::Overflow
        ));
        let Lookup::Terms(terms) =
            expand(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 3).unwrap()
        else {
            panic!("limit 3 fits foo/food/fool");
        };
        assert_eq!(terms.len(), 3);

        let copies = MutableIndex::default();
        add(&copies, 0, &[(0, "aa"), (1, "aa")]);
        add(&copies, 1, &[(0, "bb"), (1, "bb")]);
        let Lookup::Terms(terms) = expand(&copies, SurfaceWindow::All, mask, FIELDS, 2).unwrap()
        else {
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
    fn range_stays_inside_the_field_fence() {
        let index = fixture();
        let mask = all_fields_mask(FIELDS);
        let Lookup::Terms(terms) = expand(
            &index,
            SurfaceWindow::Range(Some("foo"), Some("food")),
            mask,
            FIELDS,
            16,
        )
        .unwrap() else {
            panic!("range");
        };
        assert_eq!(
            terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["foo", "food"]
        );
    }

    #[test]
    fn lookup_and_expand_df_agg_comes_from_union_not_zero() {
        let index = fixture();
        let mask = all_fields_mask(FIELDS);
        let Lookup::Term(foo) = lookup(&index, "foo", mask, FIELDS).unwrap() else {
            panic!("foo");
        };
        assert!(
            !foo.streams.is_empty(),
            "nonempty streams must not leave df_agg at the silent 0"
        );
        let union = union_df_agg_from_streams(&foo.streams).unwrap();
        assert!(union > 0);
        assert_eq!(foo.df_agg, union);
        assert_eq!(
            foo.streams.iter().map(|s| s.field).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            foo.df_agg, 1,
            "foo is only in doc 0; both-field postings count once"
        );

        let Lookup::Term(bar) = lookup(&index, "bar", mask, FIELDS).unwrap() else {
            panic!("bar");
        };
        assert_eq!(bar.df_agg, union_df_agg_from_streams(&bar.streams).unwrap());
        assert_eq!(bar.df_agg, 1);

        let Lookup::Term(absent) = lookup(&index, "zzz", mask, FIELDS).unwrap() else {
            panic!("absent");
        };
        assert!(absent.streams.is_empty());
        assert_eq!(absent.df_agg, 0);
        assert_eq!(
            absent.df_agg,
            union_df_agg_from_streams(&absent.streams).unwrap()
        );

        let Lookup::Terms(terms) =
            expand(&index, SurfaceWindow::Prefix("fo"), mask, FIELDS, 16).unwrap()
        else {
            panic!("prefix");
        };
        for term in &terms {
            assert!(!term.streams.is_empty(), "{} must have streams", term.text);
            let union = union_df_agg_from_streams(&term.streams).unwrap();
            assert!(union > 0, "{} df_agg", term.text);
            assert_eq!(term.df_agg, union, "{} df_agg vs union", term.text);
        }
    }

    #[test]
    fn open_upper_range_on_field_fifteen_is_prefix_end_not_header_sixteen() {
        assert!("~10~" < "~f~");
        let index = MutableIndex::default();
        let key = fielded_key(15, "zzz", 16).unwrap();
        index
            .add_record(
                ForwardRecord::from_tokens(Tid::new(1, 1).unwrap(), [(key.as_str(), 0)]).unwrap(),
            )
            .unwrap();
        let mask = 1u16 << 15;
        let Lookup::Terms(terms) =
            expand(&index, SurfaceWindow::Range(None, None), mask, 16, 16).unwrap()
        else {
            panic!("open-upper field 15");
        };
        assert_eq!(
            terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["zzz"]
        );
        assert_eq!(terms[0].mask, mask);
        assert_eq!(terms[0].df_agg, 1);
    }
}
