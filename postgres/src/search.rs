// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use crate::bm25::Bm25Overrides;
use crate::fields::{Lookup, all_fields_mask, fused_score, lookup, raw_tf_from_hits};
use crate::highlight::{highlight_text, highlight_text_ansi, positions_from_query};
use crate::score::{
    PRUNE_MAX_K, PrunedCandidates, VisibleTid, build_standalone_scorer, check_query_fields_on,
    visible_tid_pairs,
};
use crate::storage::FieldMeta;
use pgrx::iter::TableIterator;
use pgrx::{FromDatum, PgRelation, name, pg_sys};
use rustc_hash::{FxHashMap, FxHashSet};
use segment::Tid;
use segment::index::Index;
use std::collections::BTreeSet;
use tinql::runtime::{Query, parse_tinql_to_query};
use tokenizer::CompiledTokenizerPipeline;

type SearchRow = (pg_sys::ItemPointerData, f32, Option<String>);

/// The key attributes snippets read from: one column as before, or every key
/// column of a multi-column (field-aware, `LSG4`) index with the field names
/// its plan recorded (RFC §5.11 highlights).
#[derive(Debug)]
enum SnippetKeys {
    Single(i16),
    Fields {
        attnums: Vec<i16>,
        names: Vec<String>,
    },
}

/// Validates the index's shape for the SRF and returns the key attributes
/// snippets read from.
///
/// A single-column index behaves exactly as before. A multi-column index
/// scores through the BM25F path and its snippet renders one field: the one
/// a single top-level field wrapper names, else the first field with a
/// match, else the first non-NULL column (fetch_snippet picks).
///
/// The Fields branch is unreachable on STN3 until Phase 5 (`amcanmulticol`
/// is false).
fn validate_shape(index: &PgRelation, snippets: bool) -> SnippetKeys {
    unsafe {
        let metadata = &*(*index.as_ptr()).rd_index;
        let keys = metadata.indnkeyatts;
        if keys >= 2 {
            if !snippets {
                return SnippetKeys::Fields {
                    attnums: key_attnums(metadata),
                    names: field_names(index),
                };
            }
            // Multi-column indexes carry plain attribute keys; every key
            // column must be a text-compatible value for snippets.
            let heap_oid = pg_sys::IndexGetRelation(index.oid(), false);
            let heap = pg_sys::table_open(heap_oid, pg_sys::AccessShareLock as _);
            for attnum in key_attnums(metadata) {
                if attnum <= 0 {
                    pgrx::error!(
                        "stannum.search() snippets require a positive text key; use snippet => 'none' for an expression index"
                    );
                }
                let typid = attribute_type((*heap).rd_att, attnum);
                if !matches!(
                    typid,
                    pg_sys::TEXTOID | pg_sys::VARCHAROID | pg_sys::BPCHAROID | pg_sys::NAMEOID
                ) {
                    pgrx::error!(
                        "stannum.search() snippets require text-compatible key columns; use snippet => 'none' for degraded mode"
                    );
                }
            }
            pg_sys::table_close(heap, pg_sys::AccessShareLock as _);
            return SnippetKeys::Fields {
                attnums: key_attnums(metadata),
                names: field_names(index),
            };
        }
        let key = *metadata.indkey.values.as_ptr();
        if !snippets {
            return SnippetKeys::Single(key);
        }
        if key <= 0 {
            pgrx::error!(
                "stannum.search() snippets require a positive text key; use snippet => 'none' for an expression index"
            );
        }
        let heap_oid = pg_sys::IndexGetRelation(index.oid(), false);
        let heap = pg_sys::table_open(heap_oid, pg_sys::AccessShareLock as _);
        let typid = attribute_type((*heap).rd_att, key);
        let text_compatible = matches!(
            typid,
            pg_sys::TEXTOID | pg_sys::VARCHAROID | pg_sys::BPCHAROID | pg_sys::NAMEOID
        );
        pg_sys::table_close(heap, pg_sys::AccessShareLock as _);
        if !text_compatible {
            pgrx::error!(
                "stannum.search() snippets require a text-compatible key column; use snippet => 'none' for degraded mode"
            );
        }
        SnippetKeys::Single(key)
    }
}

unsafe fn attribute_type(tupdesc: pg_sys::TupleDesc, attnum: i16) -> pg_sys::Oid {
    let index = i32::from(attnum - 1);
    unsafe {
        #[cfg(not(feature = "pg18"))]
        let attribute = &(*tupdesc).attrs.as_slice((*tupdesc).natts as usize)[index as usize];
        #[cfg(feature = "pg18")]
        let attribute = &*pg_sys::TupleDescAttr(tupdesc, index);
        pg_sys::getBaseType(attribute.atttypid)
    }
}

/// The index's key attribute numbers, in key order.
unsafe fn key_attnums(metadata: &pg_sys::FormData_pg_index) -> Vec<i16> {
    let keys = metadata.indnkeyatts as usize;
    let values = unsafe { metadata.indkey.values.as_slice(keys) };
    (0..keys).map(|position| values[position]).collect()
}

/// The field names a multi-column index's envelope recorded.
unsafe fn field_names(index: &PgRelation) -> Vec<String> {
    unsafe {
        crate::storage::fields_meta(index.as_ptr())
            .map(|meta| meta.names)
            .unwrap_or_else(|| {
                pgrx::error!("stannum.search() requires a field-aware multi-column index")
            })
    }
}

fn validate_snippet(snippet: &str) -> &'static str {
    match snippet {
        "none" => "none",
        "html" => "html",
        "ansi" => "ansi",
        _ => pgrx::error!("stannum.search() snippet must be one of: none, html, ansi"),
    }
}

struct FieldedTerm {
    text: String,
    mask: u16,
    boost: f32,
}

enum FieldedNode {
    Leaf(usize),
    And(Vec<FieldedNode>),
    Or {
        min: u32,
        children: Vec<FieldedNode>,
    },
    Not(Box<FieldedNode>),
    All,
}

/// Cadence for fielded set materialization. Matches the BM25F ordinal walk
/// in `score.rs`: a cancel or `statement_timeout` is noticed after at most
/// this many tids, ordinals, or postings inside one large clone / union /
/// intersect / complement / collect, not only once per recursive node.
const FIELDED_INTERRUPT_CHUNK: usize = 64;

fn interrupt_at(i: usize) {
    if i.is_multiple_of(FIELDED_INTERRUPT_CHUNK) {
        pgrx::check_for_interrupts!();
    }
}

fn clone_tids_interruptible(src: &FxHashSet<Tid>) -> FxHashSet<Tid> {
    let mut out = FxHashSet::with_capacity_and_hasher(src.len(), Default::default());
    for (i, tid) in src.iter().copied().enumerate() {
        interrupt_at(i);
        out.insert(tid);
    }
    out
}

fn insert_tids_interruptible(dst: &mut FxHashSet<Tid>, src: FxHashSet<Tid>) {
    for (i, tid) in src.into_iter().enumerate() {
        interrupt_at(i);
        dst.insert(tid);
    }
}

fn retain_tids_interruptible(dst: &mut FxHashSet<Tid>, src: &FxHashSet<Tid>) {
    let mut n = 0usize;
    dst.retain(|tid| {
        interrupt_at(n);
        n += 1;
        src.contains(tid)
    });
}

fn complement_tids_interruptible(
    universe: &FxHashSet<Tid>,
    inner: &FxHashSet<Tid>,
) -> FxHashSet<Tid> {
    let mut out = FxHashSet::with_capacity_and_hasher(universe.len(), Default::default());
    for (i, tid) in universe.iter().copied().enumerate() {
        interrupt_at(i);
        if !inner.contains(&tid) {
            out.insert(tid);
        }
    }
    out
}

fn unsupported_fielded_query() -> ! {
    pgrx::error!("stannum.search() does not support this query on a multi-column index")
}

fn fielded_query_supported(query: &Query) -> bool {
    unsafe { pg_sys::check_stack_depth() };
    match query {
        Query::Term(_) | Query::MatchAll => true,
        Query::And(left, right) | Query::Or(left, right) => {
            fielded_query_supported(left) && fielded_query_supported(right)
        }
        Query::Conjunction(children)
        | Query::Disjunction { children, .. }
        | Query::AtLeast { children, .. } => children.iter().all(fielded_query_supported),
        Query::Not(inner) | Query::Boost { inner, .. } | Query::Field { inner, .. } => {
            fielded_query_supported(inner)
        }
        Query::Span { .. }
        | Query::SpanExpr { .. }
        | Query::Regex(_)
        | Query::Range { .. }
        | Query::Fuzzy { .. } => false,
    }
}

fn intern_fielded_key(keys: &mut Vec<(String, u16)>, text: &str, mask: u16) -> usize {
    keys.iter()
        .position(|(existing, existing_mask)| existing == text && *existing_mask == mask)
        .unwrap_or_else(|| {
            let index = keys.len();
            keys.push((text.to_owned(), mask));
            index
        })
}

fn fielded_key_index(keys: &[(String, u16)], text: &str, mask: u16) -> usize {
    keys.iter()
        .position(|(existing, existing_mask)| existing == text && *existing_mask == mask)
        .unwrap_or_else(|| pgrx::error!("stannum: fielded scoring term was not compiled"))
}

fn compile_fielded(
    query: &Query,
    names: &[String],
    mask: u16,
    keys: &mut Vec<(String, u16)>,
) -> FieldedNode {
    unsafe { pg_sys::check_stack_depth() };
    match query {
        Query::Term(text) => FieldedNode::Leaf(intern_fielded_key(keys, text, mask)),
        Query::Field { name, inner } => {
            let Some(ordinal) = names.iter().position(|stored| stored == name) else {
                pgrx::error!("stannum: unknown field '{name}'");
            };
            compile_fielded(inner, names, 1u16 << ordinal, keys)
        }
        Query::Boost { inner, .. } => compile_fielded(inner, names, mask, keys),
        Query::And(left, right) => FieldedNode::And(vec![
            compile_fielded(left, names, mask, keys),
            compile_fielded(right, names, mask, keys),
        ]),
        Query::Or(left, right) => FieldedNode::Or {
            min: 1,
            children: vec![
                compile_fielded(left, names, mask, keys),
                compile_fielded(right, names, mask, keys),
            ],
        },
        Query::Conjunction(children) => FieldedNode::And(
            children
                .iter()
                .map(|child| compile_fielded(child, names, mask, keys))
                .collect(),
        ),
        Query::Disjunction { min, children } | Query::AtLeast { min, children } => {
            FieldedNode::Or {
                min: *min,
                children: children
                    .iter()
                    .map(|child| compile_fielded(child, names, mask, keys))
                    .collect(),
            }
        }
        Query::Not(inner) => FieldedNode::Not(Box::new(compile_fielded(inner, names, mask, keys))),
        Query::MatchAll => FieldedNode::All,
        Query::Span { .. }
        | Query::SpanExpr { .. }
        | Query::Regex(_)
        | Query::Range { .. }
        | Query::Fuzzy { .. } => unsupported_fielded_query(),
    }
}

fn collect_fielded_terms(
    query: &Query,
    names: &[String],
    mask: u16,
    boost: f32,
    out: &mut Vec<FieldedTerm>,
) {
    unsafe { pg_sys::check_stack_depth() };
    match query {
        Query::Term(text) => out.push(FieldedTerm {
            text: text.clone(),
            mask,
            boost,
        }),
        Query::Field { name, inner } => {
            let Some(ordinal) = names.iter().position(|stored| stored == name) else {
                pgrx::error!("stannum: unknown field '{name}'");
            };
            collect_fielded_terms(inner, names, 1u16 << ordinal, boost, out);
        }
        Query::Boost { factor, inner } => {
            collect_fielded_terms(inner, names, mask, boost * factor, out);
        }
        Query::And(left, right) => {
            collect_fielded_terms(left, names, mask, boost, out);
            collect_fielded_terms(right, names, mask, boost, out);
        }
        Query::Or(left, right) => {
            collect_fielded_terms(left, names, mask, boost, out);
            collect_fielded_terms(right, names, mask, boost, out);
        }
        Query::Conjunction(children)
        | Query::Disjunction { children, .. }
        | Query::AtLeast { children, .. } => {
            for child in children {
                collect_fielded_terms(child, names, mask, boost, out);
            }
        }
        Query::Not(_) | Query::MatchAll => {}
        Query::Span { .. }
        | Query::SpanExpr { .. }
        | Query::Regex(_)
        | Query::Range { .. }
        | Query::Fuzzy { .. } => unsupported_fielded_query(),
    }
}

fn fielded_needs_universe(node: &FieldedNode) -> bool {
    match node {
        FieldedNode::All | FieldedNode::Not(_) => true,
        FieldedNode::Leaf(_) => false,
        FieldedNode::And(children) => {
            children.is_empty() || children.iter().any(fielded_needs_universe)
        }
        FieldedNode::Or { min, children } => {
            *min == 0 || children.iter().any(fielded_needs_universe)
        }
    }
}

fn indexed_live_tids(view: &crate::storage::View) -> FxHashSet<Tid> {
    let mut tids = FxHashSet::default();
    for ((source, _), dead) in view.sources.iter().zip(view.dead_sets.iter()) {
        let table = source
            .doc_table()
            .unwrap_or_else(|error| pgrx::error!("Stannum document table: {error}"));
        for ordinal in 0..source.document_count() {
            interrupt_at(ordinal as usize);
            if dead.contains(ordinal) {
                continue;
            }
            let tid = table
                .tid_at(ordinal)
                .unwrap_or_else(|error| pgrx::error!("Stannum document table: {error}"));
            tids.insert(tid);
        }
    }
    tids
}

fn eval_fielded(
    node: &FieldedNode,
    postings: &[FxHashSet<Tid>],
    universe: &FxHashSet<Tid>,
) -> FxHashSet<Tid> {
    unsafe { pg_sys::check_stack_depth() };
    match node {
        FieldedNode::Leaf(index) => clone_tids_interruptible(&postings[*index]),
        FieldedNode::All => clone_tids_interruptible(universe),
        FieldedNode::Not(inner) => {
            let inner_members = eval_fielded(inner, postings, universe);
            complement_tids_interruptible(universe, &inner_members)
        }
        FieldedNode::And(children) if children.is_empty() => clone_tids_interruptible(universe),
        FieldedNode::And(children) => {
            let mut members = eval_fielded(&children[0], postings, universe);
            for child in &children[1..] {
                pgrx::check_for_interrupts!();
                if members.is_empty() {
                    break;
                }
                let child_members = eval_fielded(child, postings, universe);
                retain_tids_interruptible(&mut members, &child_members);
            }
            members
        }
        FieldedNode::Or { min, children } => {
            if *min == 0 {
                return clone_tids_interruptible(universe);
            }
            if (*min as usize) > children.len() {
                return FxHashSet::default();
            }
            if *min == 1 {
                let mut members = FxHashSet::default();
                for child in children {
                    pgrx::check_for_interrupts!();
                    insert_tids_interruptible(
                        &mut members,
                        eval_fielded(child, postings, universe),
                    );
                }
                return members;
            }
            let mut counts: FxHashMap<Tid, u32> = FxHashMap::default();
            for child in children {
                pgrx::check_for_interrupts!();
                for (i, tid) in eval_fielded(child, postings, universe)
                    .into_iter()
                    .enumerate()
                {
                    interrupt_at(i);
                    *counts.entry(tid).or_insert(0) += 1;
                }
            }
            let mut members = FxHashSet::default();
            for (i, (tid, hits)) in counts.into_iter().enumerate() {
                interrupt_at(i);
                if hits >= *min {
                    members.insert(tid);
                }
            }
            members
        }
    }
}

fn fielded_scores(
    index: &PgRelation,
    query: &str,
    fields: &FieldMeta,
    k1: Option<f32>,
    b: Option<f32>,
) -> FxHashMap<Tid, f32> {
    fielded_eval(index, query, fields, k1, b, true).1
}

fn fielded_matching_tids(index: &PgRelation, query: &str, fields: &FieldMeta) -> BTreeSet<Tid> {
    fielded_eval(index, query, fields, None, None, false).0
}

fn fielded_eval(
    index: &PgRelation,
    query: &str,
    fields: &FieldMeta,
    k1: Option<f32>,
    b: Option<f32>,
    want_scores: bool,
) -> (BTreeSet<Tid>, FxHashMap<Tid, f32>) {
    let tokenizer = unsafe { crate::storage::index_tokenizer(index.as_ptr()) };
    let parsed = parse_tinql_to_query(query, tokenizer.as_ref()).unwrap_or_else(|error| {
        crate::operator::raise_query_error(&error, format!("Stannum score query error: {error}"))
    });
    check_query_fields_on(&parsed, Some(&fields.names));
    if !fielded_query_supported(&parsed) {
        unsupported_fielded_query();
    }
    let field_count = u8::try_from(fields.names.len()).unwrap_or(16);
    let default_mask = all_fields_mask(field_count);
    let mut keys = Vec::new();
    let root = compile_fielded(&parsed, &fields.names, default_mask, &mut keys);
    let mut terms = Vec::new();
    if want_scores {
        // Membership and scoring share the structural parse. A scoring parse
        // would keep `alpha AND alpha` as two clauses and double the BM25F
        // contribution; 0.4.0's IndexScorer saw the folded term list.
        collect_fielded_terms(&parsed, &fields.names, default_mask, 1.0, &mut terms);
        for term in &terms {
            intern_fielded_key(&mut keys, &term.text, term.mask);
        }
    }
    let view = unsafe { crate::storage::view(index.oid()) };
    if view.field_norms.iter().any(Option::is_none) {
        pgrx::error!("stannum: multi-column index is missing the STNF field-norms trailer");
    }
    let mut postings: Vec<FxHashSet<Tid>> = vec![FxHashSet::default(); keys.len()];
    let mut scanned = vec![false; keys.len()];
    let mut scores: FxHashMap<Tid, f32> = FxHashMap::default();

    if want_scores {
        let defaults = unsafe { crate::options::bm25(index.as_ptr()) };
        let params = Bm25Overrides { k1, b }
            .resolve(defaults)
            .checked()
            .unwrap_or_else(|error| pgrx::error!("stannum score parameters: {error}"));
        let mut total_docs = 0u64;
        let mut field_totals = vec![0u64; fields.names.len()];
        let mut df = vec![0u64; terms.len()];
        for ((source, _), norms) in view.sources.iter().zip(view.field_norms.iter()) {
            total_docs += u64::from(source.document_count());
            if let Some(norms) = norms {
                for (total, extra) in field_totals.iter_mut().zip(&norms.field_totals) {
                    *total += extra;
                }
            }
            for (term_index, term) in terms.iter().enumerate() {
                match lookup(&**source, &term.text, term.mask, field_count) {
                    Ok(Lookup::Term(logical)) => df[term_index] += logical.df_agg,
                    Ok(Lookup::Terms(_) | Lookup::Overflow) => {}
                    Err(error) => pgrx::error!("Stannum fielded lookup: {error}"),
                }
            }
        }
        for ((source, _), (dead, norms)) in view
            .sources
            .iter()
            .zip(view.dead_sets.iter().zip(view.field_norms.iter()))
        {
            let Some(norms) = norms else {
                pgrx::error!("stannum: multi-column index is missing the STNF field-norms trailer");
            };
            let table = source
                .doc_table()
                .unwrap_or_else(|error| pgrx::error!("Stannum document table: {error}"));
            for (term_index, term) in terms.iter().enumerate() {
                let key = fielded_key_index(&keys, &term.text, term.mask);
                let logical = match lookup(&**source, &term.text, term.mask, field_count) {
                    Ok(Lookup::Term(logical)) => logical,
                    Ok(Lookup::Terms(_) | Lookup::Overflow) => continue,
                    Err(error) => pgrx::error!("Stannum fielded lookup: {error}"),
                };
                if logical.streams.is_empty() {
                    scanned[key] = true;
                    continue;
                }
                let mut cursor = logical
                    .cursor()
                    .unwrap_or_else(|error| pgrx::error!("Stannum fielded cursor: {error}"));
                let mut n = 0usize;
                while let Some(ordinal) = cursor.current_ordinal() {
                    interrupt_at(n);
                    n += 1;
                    if dead.contains(ordinal) {
                        cursor
                            .advance(ordinal.saturating_add(1))
                            .unwrap_or_else(|error| {
                                pgrx::error!("Stannum fielded cursor: {error}")
                            });
                        continue;
                    }
                    let tid = table
                        .tid_at(ordinal)
                        .unwrap_or_else(|error| pgrx::error!("Stannum document table: {error}"));
                    let hits = cursor
                        .field_hits()
                        .unwrap_or_else(|error| pgrx::error!("Stannum fielded cursor: {error}"));
                    let raw = raw_tf_from_hits(&hits, field_count);
                    let lengths = norms.lengths(ordinal).unwrap_or_else(|| {
                        pgrx::error!("stannum: STNF row missing for ordinal {ordinal}")
                    });
                    let score = fused_score(
                        term.mask,
                        &fields.weights,
                        &raw,
                        &lengths,
                        &field_totals,
                        total_docs,
                        df[term_index],
                        term.boost,
                        params,
                    );
                    *scores.entry(tid).or_insert(0.0) += score;
                    postings[key].insert(tid);
                    cursor
                        .advance(ordinal.saturating_add(1))
                        .unwrap_or_else(|error| pgrx::error!("Stannum fielded cursor: {error}"));
                }
                scanned[key] = true;
            }
        }
    }

    for (key, (text, mask)) in keys.iter().enumerate() {
        if scanned[key] {
            continue;
        }
        collect_fielded_postings(&view, text, *mask, field_count, &mut postings[key]);
        scanned[key] = true;
    }

    let universe = if fielded_needs_universe(&root) {
        indexed_live_tids(&view)
    } else {
        FxHashSet::default()
    };
    let members = eval_fielded(&root, &postings, &universe);
    let mut member_set = BTreeSet::new();
    for (i, tid) in members.iter().copied().enumerate() {
        interrupt_at(i);
        member_set.insert(tid);
    }
    if !want_scores {
        return (member_set, FxHashMap::default());
    }
    let mut out = FxHashMap::default();
    for (i, tid) in members.iter().enumerate() {
        interrupt_at(i);
        out.insert(*tid, scores.get(tid).copied().unwrap_or(0.0));
    }
    (member_set, out)
}

fn collect_fielded_postings(
    view: &crate::storage::View,
    text: &str,
    mask: u16,
    field_count: u8,
    postings: &mut FxHashSet<Tid>,
) {
    for ((source, _), dead) in view.sources.iter().zip(view.dead_sets.iter()) {
        let logical = match lookup(&**source, text, mask, field_count) {
            Ok(Lookup::Term(logical)) => logical,
            Ok(Lookup::Terms(_) | Lookup::Overflow) => continue,
            Err(error) => pgrx::error!("Stannum fielded lookup: {error}"),
        };
        if logical.streams.is_empty() {
            continue;
        }
        let table = source
            .doc_table()
            .unwrap_or_else(|error| pgrx::error!("Stannum document table: {error}"));
        let mut cursor = logical
            .cursor()
            .unwrap_or_else(|error| pgrx::error!("Stannum fielded cursor: {error}"));
        let mut n = 0usize;
        while let Some(ordinal) = cursor.current_ordinal() {
            interrupt_at(n);
            n += 1;
            if dead.contains(ordinal) {
                cursor
                    .advance(ordinal.saturating_add(1))
                    .unwrap_or_else(|error| pgrx::error!("Stannum fielded cursor: {error}"));
                continue;
            }
            let tid = table
                .tid_at(ordinal)
                .unwrap_or_else(|error| pgrx::error!("Stannum document table: {error}"));
            postings.insert(tid);
            cursor
                .advance(ordinal.saturating_add(1))
                .unwrap_or_else(|error| pgrx::error!("Stannum fielded cursor: {error}"));
        }
    }
}

fn fielded_ranked_rows(
    index: &PgRelation,
    query: &str,
    fields: &FieldMeta,
    k1: Option<f32>,
    b: Option<f32>,
) -> Vec<(f32, VisibleTid)> {
    let scores = fielded_scores(index, query, fields, k1, b);
    let ranked_span = crate::fields::profile::Span::begin();
    let heap_oid = unsafe { pg_sys::IndexGetRelation(index.oid(), false) };
    let roots: BTreeSet<Tid> = scores.keys().copied().collect();
    let visible = unsafe { visible_tid_pairs(heap_oid, roots) };
    let mut rows: Vec<_> = visible
        .into_iter()
        .filter_map(|row| {
            scores
                .get(&row.indexed_tid)
                .copied()
                .map(|score| (score, row))
        })
        .collect();
    rank_rows(&mut rows);
    rows
}

fn pointer_of(tid: Tid) -> pg_sys::ItemPointerData {
    pg_sys::ItemPointerData {
        ip_blkid: pg_sys::BlockIdData {
            bi_hi: (tid.block >> 16) as u16,
            bi_lo: tid.block as u16,
        },
        ip_posid: tid.offset,
    }
}

fn tid_of(pointer: pg_sys::ItemPointerData) -> Tid {
    let block = (u32::from(pointer.ip_blkid.bi_hi) << 16) | u32::from(pointer.ip_blkid.bi_lo);
    Tid::new(block, pointer.ip_posid)
        .unwrap_or_else(|_| pgrx::error!("invalid visible heap tuple location"))
}

fn rank_rows(rows: &mut [(f32, VisibleTid)]) {
    rows.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .total_cmp(left_score)
            .then(left.visible_tid.cmp(&right.visible_tid))
    });
}

fn exhaustive_rows(
    scorer: &mut crate::score::IndexScorer,
    heap_oid: pg_sys::Oid,
) -> Vec<(f32, VisibleTid)> {
    let roots = scorer.matching_tids();
    let visible = unsafe { visible_tid_pairs(heap_oid, roots) };
    let visible_roots: BTreeSet<Tid> = visible.iter().map(|row| row.indexed_tid).collect();
    let scored = scorer.score_matching_tids(&visible_roots);
    let scores: FxHashMap<Tid, f32> = scored
        .into_iter()
        .map(|row| (row.indexed_tid, row.score))
        .collect();
    let mut rows: Vec<_> = visible
        .into_iter()
        .filter_map(|row| {
            scores
                .get(&row.indexed_tid)
                .copied()
                .map(|score| (score, row))
        })
        .collect();
    rank_rows(&mut rows);
    rows
}

fn accepted_pruned_rows(
    pruned: PrunedCandidates,
    heap_oid: pg_sys::Oid,
    limit: usize,
) -> Option<Vec<(f32, VisibleTid)>> {
    if !pruned.complete {
        return None;
    }
    let roots: BTreeSet<Tid> = pruned.rows.iter().map(|row| row.indexed_tid).collect();
    let visible = unsafe { visible_tid_pairs(heap_oid, roots) };
    // This simultaneously rejects invisible top rows and any HOT dedupe that
    // would make the distinct-visible-document result underfill.
    if visible.len() != pruned.rows.len() {
        return None;
    }
    let scores: FxHashMap<Tid, f32> = pruned
        .rows
        .into_iter()
        .map(|row| (row.indexed_tid, row.score))
        .collect();
    let mut rows: Vec<_> = visible
        .into_iter()
        .filter_map(|row| {
            scores
                .get(&row.indexed_tid)
                .copied()
                .map(|score| (score, row))
        })
        .collect();
    rank_rows(&mut rows);
    rows.truncate(limit);
    Some(rows)
}

#[allow(clippy::too_many_arguments)] // snippet refetch bundles its fixed context
fn fetch_snippet(
    heap_oid: pg_sys::Oid,
    keys: &SnippetKeys,
    pipeline: &CompiledTokenizerPipeline,
    query: &str,
    mode: &str,
    begin_tag: &str,
    end_tag: &str,
    rows: Vec<(f32, VisibleTid)>,
) -> Vec<SearchRow> {
    if mode == "none" {
        return rows
            .into_iter()
            .map(|(score, row)| (pointer_of(row.visible_tid), score, None))
            .collect();
    }
    // The query's single top-level field wrapper names the snippet's field;
    // absent one, the first field holding a match wins, else the first
    // non-NULL column renders plain (RFC §5.11).
    let wrapper_field = top_level_field_name(pipeline, query);
    let render = |text: &str, positions: &[crate::match_positions::MatchPosition]| -> String {
        let rendered = if mode == "ansi" {
            highlight_text_ansi(pipeline, text, positions)
        } else {
            highlight_text(pipeline, text, begin_tag, end_tag, positions)
        };
        rendered.unwrap_or_else(|error| {
            pgrx::error!("stannum.search() snippet rendering failed: {error}")
        })
    };
    unsafe {
        let heap = pg_sys::table_open(heap_oid, pg_sys::AccessShareLock as _);
        let fetch = pg_sys::table_index_fetch_begin(heap);
        let slot = pg_sys::table_slot_create(heap, std::ptr::null_mut());
        let snapshot = pg_sys::GetActiveSnapshot();
        let mut output = Vec::with_capacity(rows.len());
        for (score, row) in rows {
            pgrx::check_for_interrupts!();
            let mut pointer = pointer_of(row.indexed_tid);
            let mut call_again = false;
            let mut all_dead = false;
            let mut found = false;
            loop {
                if pg_sys::table_index_fetch_tuple(
                    fetch,
                    &mut pointer,
                    snapshot,
                    slot,
                    &mut call_again,
                    &mut all_dead,
                ) {
                    found = true;
                    break;
                }
                if !call_again {
                    break;
                }
            }
            if !found || tid_of((*slot).tts_tid) != row.visible_tid {
                continue;
            }
            let snippet = match keys {
                SnippetKeys::Single(attnum) => {
                    let mut isnull = false;
                    let datum = pg_sys::slot_getattr(slot, i32::from(*attnum), &mut isnull);
                    if isnull {
                        None
                    } else {
                        let text = String::from_datum(datum, false).unwrap_or_else(|| {
                            pgrx::error!("stannum.search() indexed column is not text")
                        });
                        let positions = positions_from_query(pipeline, query, &text);
                        Some(render(&text, &positions))
                    }
                }
                SnippetKeys::Fields { attnums, names } => {
                    let texts: Vec<Option<String>> = attnums
                        .iter()
                        .map(|attnum| {
                            let mut isnull = false;
                            let datum = pg_sys::slot_getattr(slot, i32::from(*attnum), &mut isnull);
                            if isnull {
                                None
                            } else {
                                Some(String::from_datum(datum, false).unwrap_or_else(|| {
                                    pgrx::error!("stannum.search() indexed column is not text")
                                }))
                            }
                        })
                        .collect();
                    snippet_of_fields(pipeline, query, &texts, names, wrapper_field.as_deref())
                        .map(|(text, positions)| render(&text, &positions))
                }
            };
            output.push((pointer_of(row.visible_tid), score, snippet));
        }
        pg_sys::ExecDropSingleTupleTableSlot(slot);
        pg_sys::table_index_fetch_end(fetch);
        pg_sys::table_close(heap, pg_sys::AccessShareLock as _);
        output
    }
}

/// The name a query's single top-level `Field` wrapper carries.
///
/// `Query::Field` is Phase 4/5. Single-column snippets never wrap a field.
fn top_level_field_name(_pipeline: &CompiledTokenizerPipeline, _query: &str) -> Option<String> {
    None
}

/// Field-scoped match positions. STN3 has no `positions_from_query_for_field`
/// until Phase 5; the Fields snippet path is unreachable while
/// `amcanmulticol` is false.
fn positions_from_query_for_field(
    pipeline: &CompiledTokenizerPipeline,
    query: &str,
    text: &str,
    _field_name: Option<&str>,
    _field: u16,
) -> Vec<crate::match_positions::MatchPosition> {
    positions_from_query(pipeline, query, text)
}

/// One field's text and its field-scoped marks, the scalar snippet a
/// multi-column row renders: the wrapper's field; else the first field
/// holding a mark (marks never leave their field); else the first non-NULL
/// field's plain text. `None` when every field is NULL (RFC §5.11).
fn snippet_of_fields(
    pipeline: &CompiledTokenizerPipeline,
    query: &str,
    texts: &[Option<String>],
    names: &[String],
    wrapper_field: Option<&str>,
) -> Option<(String, Vec<crate::match_positions::MatchPosition>)> {
    if let Some(name) = wrapper_field {
        let field = names.iter().position(|candidate| candidate == name)?;
        let text = texts[field].as_ref()?;
        return Some((
            text.clone(),
            positions_from_query_for_field(pipeline, query, text, Some(name), field as u16),
        ));
    }
    for (field, text) in texts.iter().enumerate() {
        let Some(text) = text else { continue };
        let positions = positions_from_query_for_field(
            pipeline,
            query,
            text,
            Some(&names[field]),
            field as u16,
        );
        if !positions.is_empty() {
            return Some((text.clone(), positions));
        }
    }
    texts
        .iter()
        .flatten()
        .next()
        .map(|text| (text.clone(), Vec::new()))
}

/// Standalone `search()` body. The §4.1 `#[pg_extern]` lives in `tool::search`.
#[expect(
    clippy::too_many_arguments,
    reason = "matches the SQL surface the tool-layer wrapper exposes"
)]
pub(crate) fn search(
    index: PgRelation,
    query: Option<&str>,
    limit: i32,
    snippet: &str,
    begin_tag: &str,
    end_tag: &str,
    k1: Option<f32>,
    b: Option<f32>,
) -> TableIterator<
    'static,
    (
        name!(ctid, pg_sys::ItemPointerData),
        name!(score, f32),
        name!(snippet, Option<String>),
    ),
> {
    crate::udfs::require_stannum_index(&index, "search");
    let query = query.unwrap_or_else(|| pgrx::error!("stannum.search() query must not be NULL"));
    if !unsafe { crate::storage::present(index.as_ptr()) } {
        pgrx::error!("stannum.search() requires a segmented stannum index");
    }
    if limit < 0 {
        pgrx::error!("stannum.search() limit must be non-negative");
    }
    let mode = validate_snippet(snippet);
    let keys = validate_shape(&index, mode != "none");
    let heap_oid = unsafe { pg_sys::IndexGetRelation(index.oid(), false) };
    let index_oid = index.oid();
    if let Some(fields) = unsafe { crate::storage::fields_meta(index.as_ptr()) } {
        let rows = fielded_ranked_rows(&index, query, &fields, k1, b);
        let limit = if limit == 0 {
            return TableIterator::new(Vec::new());
        } else {
            usize::try_from(limit).expect("non-negative limit fits usize")
        };
        let rows: Vec<_> = rows.into_iter().take(limit).collect();
        let pipeline = unsafe { crate::storage::tokenizer_by_oid(index_oid) };
        let rows = fetch_snippet(
            heap_oid,
            &keys,
            pipeline.as_ref(),
            query,
            mode,
            begin_tag,
            end_tag,
            rows,
        );
        return TableIterator::new(rows);
    }
    let mut scorer = build_standalone_scorer(heap_oid, index_oid, query, k1, b);
    if limit == 0 {
        return TableIterator::new(Vec::new());
    }
    let limit = usize::try_from(limit).expect("non-negative limit fits usize");
    let rows = if limit <= PRUNE_MAX_K {
        let pruned = scorer
            .pruned_top_k(limit)
            .and_then(|pruned| accepted_pruned_rows(pruned, heap_oid, limit));
        pruned.unwrap_or_else(|| exhaustive_rows(&mut scorer, heap_oid))
    } else {
        exhaustive_rows(&mut scorer, heap_oid)
    };
    let rows = rows.into_iter().take(limit).collect();
    // IndexScorer has no pipeline() on STN3; snippets use the index tokenizer.
    let pipeline = unsafe { crate::storage::tokenizer_by_oid(index_oid) };
    let rows = fetch_snippet(
        heap_oid,
        &keys,
        pipeline.as_ref(),
        query,
        mode,
        begin_tag,
        end_tag,
        rows,
    );
    TableIterator::new(rows)
}

/// Standalone `search_count()` body. The §4.1 `#[pg_extern]` lives in `tool::search`.
pub(crate) fn search_count(index: PgRelation, query: Option<&str>) -> i64 {
    crate::udfs::require_stannum_index(&index, "search_count");
    let query =
        query.unwrap_or_else(|| pgrx::error!("stannum.search_count() query must not be NULL"));
    if !unsafe { crate::storage::present(index.as_ptr()) } {
        pgrx::error!("stannum.search_count() requires a segmented stannum index");
    }
    validate_shape(&index, false);
    let heap_oid = unsafe { pg_sys::IndexGetRelation(index.oid(), false) };
    let roots = if let Some(fields) = unsafe { crate::storage::fields_meta(index.as_ptr()) } {
        fielded_matching_tids(&index, query, &fields)
    } else {
        let scorer = build_standalone_scorer(heap_oid, index.oid(), query, None, None);
        scorer.matching_tids()
    };
    let visible = unsafe { visible_tid_pairs(heap_oid, roots) };
    i64::try_from(visible.len())
        .unwrap_or_else(|_| pgrx::error!("stannum.search_count() result is too large"))
}

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use crate::score::RankedCandidate;
    use pgrx::prelude::*;

    fn oid(name: &str) -> pg_sys::Oid {
        Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{name}'::regclass::oid"))
            .unwrap()
            .unwrap()
    }

    fn row_tid(id: i32) -> Tid {
        tid_of(
            Spi::get_one::<pg_sys::ItemPointerData>(&format!(
                "SELECT ctid FROM hardening_docs WHERE id = {id}"
            ))
            .unwrap()
            .unwrap(),
        )
    }

    struct CurrentSnapshot;

    impl CurrentSnapshot {
        fn push() -> Self {
            // The outer pg_test SELECT predates all of our SPI writes. Private
            // heap-refetch helpers need a fresh command snapshot, just as a
            // subsequent SQL search() invocation would receive.
            unsafe {
                pg_sys::CommandCounterIncrement();
                pg_sys::PushActiveSnapshot(pg_sys::GetTransactionSnapshot());
            }
            Self
        }
    }

    impl Drop for CurrentSnapshot {
        fn drop(&mut self) {
            unsafe { pg_sys::PopActiveSnapshot() };
        }
    }

    fn fixture() {
        Spi::run(
            "CREATE TABLE hardening_docs(id int, body text, revision int DEFAULT 0)
               WITH (fillfactor=50);
             INSERT INTO hardening_docs SELECT n, 'needle ' || repeat('pad ', n), 0
               FROM generate_series(1, 20) n;
             CREATE INDEX hardening_idx ON hardening_docs USING stannum(body);",
        )
        .unwrap();
    }

    #[pg_test]
    fn search_shape_accepts_include_catalog_layout() {
        fixture();
        // Stannum does not advertise amcaninclude yet. Use a real INCLUDE
        // descriptor to test the shape validator independently of the AM gate,
        // rather than mutating pg_index or pretending this is end-to-end support.
        Spi::run("CREATE INDEX hardening_include ON hardening_docs(body) INCLUDE(revision)")
            .unwrap();
        let index = unsafe {
            PgRelation::with_lock(oid("hardening_include"), pg_sys::AccessShareLock as _)
        };
        let metadata = unsafe { &*(*index.as_ptr()).rd_index };
        assert_eq!(metadata.indnatts, 2);
        assert_eq!(metadata.indnkeyatts, 1);
        assert!(matches!(
            validate_shape(&index, true),
            SnippetKeys::Single(2)
        ));
    }

    /// A multi-column index is scanned and scored (its field-aware bounds
    /// make the walk prunable); its snippets render one field per row
    /// (§P0-2 phase 3), so the shape validator hands the refetch every key
    /// column with the recorded field names.
    ///
    /// Scaffold until Phase 5. `amcanmulticol` stays false.
    #[pg_test]
    #[ignore]
    fn search_shape_accepts_multiple_keys_with_snippets() {
        fixture();
        Spi::run(
            "CREATE TABLE hardening_two(id int primary key, a text, b text);
             CREATE INDEX hardening_two_idx ON hardening_two USING stannum(a, b);",
        )
        .unwrap();
        let index = unsafe {
            PgRelation::with_lock(oid("hardening_two_idx"), pg_sys::AccessShareLock as _)
        };
        let metadata = unsafe { &*(*index.as_ptr()).rd_index };
        assert_eq!(metadata.indnkeyatts, 2);
        for snippets in [true, false] {
            match validate_shape(&index, snippets) {
                SnippetKeys::Fields { attnums, names } => {
                    assert_eq!(attnums.len(), 2);
                    assert_eq!(names, ["a", "b"]);
                }
                other => panic!("expected field keys, got {other:?}"),
            }
        }
    }

    /// Snippets still refuse a non-text key column on a multi-column index:
    /// `revision` is an int, and rendering it would not be a snippet.
    ///
    /// Scaffold until Phase 5. `amcanmulticol` stays false.
    #[pg_test]
    #[ignore]
    fn search_shape_rejects_non_text_keys_with_snippets() {
        fixture();
        Spi::run("CREATE INDEX hardening_multikey_snippets ON hardening_docs(body, revision)")
            .unwrap();
        let index = unsafe {
            PgRelation::with_lock(
                oid("hardening_multikey_snippets"),
                pg_sys::AccessShareLock as _,
            )
        };
        validate_shape(&index, true);
    }

    /// `search()` and `search_count()` answer on a multi-column index through
    /// the field-aware scorer: the term matches in any field, the strongest
    /// field hit ranks first, and snippets render one field per row.
    #[pg_test]
    fn search_scores_every_field_of_a_multi_column_index() {
        Spi::run(
            "CREATE TABLE mc_docs(id int primary key, title text, body text);
             INSERT INTO mc_docs VALUES
               (1, 'needle', 'pad'),
               (2, 'pad', 'needle'),
               (3, 'needle needle', 'pad');
             CREATE INDEX mc_docs_idx ON mc_docs USING stannum(title, body);",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT stannum.search_count('mc_docs_idx', 'needle')").unwrap(),
            Some(3)
        );
        let rows = |sql: &str| -> Vec<(i32, u32)> {
            Spi::connect(|client| {
                client
                    .select(sql, None, &[])
                    .unwrap_or_else(|error| panic!("{error}"))
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<f32>(2).unwrap().unwrap().to_bits(),
                        )
                    })
                    .collect()
            })
        };
        // Only id 3 carries the term twice in its title, so it ranks first;
        // ids 1 and 2 tie exactly, on equal weighted lengths.
        let ranked = rows(
            "SELECT d.id, s.score FROM mc_docs d JOIN
             stannum.search('mc_docs_idx', 'needle', 3, 'none') s ON d.ctid = s.ctid
             ORDER BY s.score DESC, d.id",
        );
        assert_eq!(ranked.len(), 3);
        assert_eq!(ranked[0].0, 3, "{ranked:?}");
        assert!(ranked[0].1 > ranked[1].1, "{ranked:?}");
        assert_eq!(ranked[1].1, ranked[2].1, "{ranked:?}");
        let snippets = |query: &str| -> Vec<(i32, String)> {
            Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT d.id, s.snippet FROM mc_docs d JOIN
                             stannum.search('mc_docs_idx', '{query}', 3) s ON d.ctid = s.ctid
                             ORDER BY d.id"
                        ),
                        None,
                        &[],
                    )
                    .unwrap_or_else(|error| panic!("{error}"))
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<String>(2).unwrap().unwrap(),
                        )
                    })
                    .collect()
            })
        };
        assert_eq!(
            snippets("needle"),
            vec![
                (1, "<mark>needle</mark>".into()),
                (2, "<mark>needle</mark>".into()),
                (3, "<mark>needle</mark> <mark>needle</mark>".into())
            ]
        );
        assert_eq!(
            snippets("title:(needle)"),
            vec![
                (1, "<mark>needle</mark>".into()),
                (3, "<mark>needle</mark> <mark>needle</mark>".into())
            ]
        );
        assert_eq!(
            snippets("* AND NOT absenttoken"),
            vec![
                (1, "needle".into()),
                (2, "pad".into()),
                (3, "needle needle".into())
            ]
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT stannum.search_count('mc_docs_idx', 'title:(needle)')")
                .unwrap(),
            Some(2)
        );
        let scoped = rows(
            "SELECT d.id, s.score FROM mc_docs d JOIN
             stannum.search('mc_docs_idx', 'title:(needle)', 2, 'none') s ON d.ctid = s.ctid
             ORDER BY s.score DESC, d.id",
        );
        assert_eq!(scoped.len(), 2, "{scoped:?}");
        assert_eq!(scoped[0].0, 3, "{scoped:?}");
        assert_eq!(
            scoped,
            rows(
                "SELECT d.id, s.score FROM mc_docs d JOIN
                 stannum.search('mc_docs_idx', 'title:(needle)', 5000, 'none') s ON d.ctid = s.ctid
                 ORDER BY s.score DESC, d.id"
            )
        );
    }

    fn bool_fixture() {
        Spi::run(
            "CREATE TABLE bool_docs (
                 id int primary key,
                 title text not null,
                 body text not null,
                 concat text generated always as (title || ' ' || body) stored
             );
             INSERT INTO bool_docs VALUES
               (1, 'alpha', 'bravo'),
               (2, 'alpha bravo', 'charlie'),
               (3, 'charlie', 'alpha'),
               (4, 'bravo', 'bravo'),
               (5, 'other', 'other'),
               (6, 'alpha charlie', 'bravo');
             CREATE INDEX bool_docs_multi ON bool_docs USING stannum (title, body);
             CREATE INDEX bool_docs_single ON bool_docs USING stannum (concat);",
        )
        .unwrap();
    }

    fn search_ids(index: &str, query: &str) -> Vec<i32> {
        Spi::get_one::<Vec<i32>>(&format!(
            "SELECT coalesce(array_agg(d.id ORDER BY d.id), '{{}}'::int[])
             FROM bool_docs d
             JOIN stannum.search('{index}', $q${query}$q$, 50, 'none') s
             ON d.ctid = s.ctid"
        ))
        .unwrap()
        .unwrap()
    }

    fn search_count_of(index: &str, query: &str) -> i64 {
        Spi::get_one::<i64>(&format!(
            "SELECT stannum.search_count('{index}', $q${query}$q$)"
        ))
        .unwrap()
        .unwrap()
    }

    fn search_score_bits(index: &str, query: &str) -> Vec<(i32, u32)> {
        Spi::connect(|client| {
            client
                .select(
                    &format!(
                        "SELECT d.id, s.score FROM bool_docs d
                         JOIN stannum.search('{index}', $q${query}$q$, 50, 'none') s
                         ON d.ctid = s.ctid
                         ORDER BY d.id"
                    ),
                    None,
                    &[],
                )
                .unwrap_or_else(|error| panic!("{error}"))
                .map(|row| {
                    (
                        row.get::<i32>(1).unwrap().unwrap(),
                        row.get::<f32>(2).unwrap().unwrap().to_bits(),
                    )
                })
                .collect()
        })
    }

    fn assert_ids_on(index: &str, query: &str, expected: &[i32]) {
        assert_eq!(search_ids(index, query), expected, "{index} {query}");
        assert_eq!(
            search_count_of(index, query),
            expected.len() as i64,
            "{index} {query} count"
        );
    }

    fn assert_ids_both(query: &str, expected: &[i32]) {
        assert_ids_on("bool_docs_multi", query, expected);
        assert_ids_on("bool_docs_single", query, expected);
    }

    fn assert_same_as_term_score(index: &str, repeated: &str, once: &str) {
        let repeated_bits = search_score_bits(index, repeated);
        let once_rows = search_score_bits(index, once);
        assert_eq!(repeated_bits.len(), once_rows.len(), "{index} {repeated}");
        for ((id, repeated_score), (once_id, once_score)) in
            repeated_bits.iter().zip(once_rows.iter())
        {
            assert_eq!(id, once_id, "{index} {repeated} id");
            assert_eq!(*repeated_score, *once_score, "{index} {repeated} id={id}");
        }
    }

    fn assert_twice_the_term_score(index: &str, repeated: &str, once: &str) {
        let repeated_bits = search_score_bits(index, repeated);
        let once_rows = search_score_bits(index, once);
        assert_eq!(repeated_bits.len(), once_rows.len(), "{index} {repeated}");
        for ((id, twice), (once_id, once_score)) in repeated_bits.iter().zip(once_rows.iter()) {
            assert_eq!(id, once_id, "{index} {repeated} id");
            let added = f32::from_bits(*once_score) + f32::from_bits(*once_score);
            assert_eq!(*twice, added.to_bits(), "{index} {repeated} id={id}");
        }
    }

    fn assert_boosted_and_scores(index: &str, factor: f32) {
        let query = format!("(alpha AND bravo)^{factor}");
        let base = search_score_bits(index, "alpha AND bravo");
        let boosted = search_score_bits(index, &query);
        let alpha: FxHashMap<i32, u32> = search_score_bits(index, &format!("alpha^{factor}"))
            .into_iter()
            .collect();
        let bravo: FxHashMap<i32, u32> = search_score_bits(index, &format!("bravo^{factor}"))
            .into_iter()
            .collect();
        assert_eq!(base.len(), boosted.len(), "{index} {query} len");
        for ((id, _), (boosted_id, boosted_bits)) in base.iter().zip(boosted.iter()) {
            assert_eq!(id, boosted_id, "{index} {query} id");
            // `fused_score` does `idf_f32 * boost` inside the BM25 fraction,
            // so the AND total is the sum of per-leaf boosted scores on this
            // index, not `(unboosted_and * factor)`.
            let expected = (f32::from_bits(alpha[id]) + f32::from_bits(bravo[id])).to_bits();
            assert_eq!(*boosted_bits, expected, "{index} {query} id={id}");
        }
    }

    fn assert_query_error_contains(index: &str, query: &str, needle: &str) {
        let needle_sql = needle.replace('\'', "''");
        Spi::run(&format!(
            "DO $test$ DECLARE caught text; BEGIN
               BEGIN PERFORM stannum.search_count('{index}', $q${query}$q$);
               EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
               IF caught IS NULL OR position('{needle_sql}' in caught) = 0 THEN
                 RAISE EXCEPTION 'expected substring %, got %', '{needle_sql}', caught;
               END IF;
               caught := NULL;
               BEGIN PERFORM * FROM stannum.search('{index}', $q${query}$q$, 50, 'none');
               EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
               IF caught IS NULL OR position('{needle_sql}' in caught) = 0 THEN
                 RAISE EXCEPTION 'expected substring %, got %', '{needle_sql}', caught;
               END IF;
             END $test$;"
        ))
        .unwrap();
    }

    fn test_tid(id: u16) -> Tid {
        Tid::new(0, id).expect("test tid")
    }

    fn tid_set(ids: &[u16]) -> FxHashSet<Tid> {
        ids.iter().copied().map(test_tid).collect()
    }

    fn sorted_offsets(set: &FxHashSet<Tid>) -> Vec<i32> {
        let mut ids: Vec<i32> = set.iter().map(|tid| i32::from(tid.offset)).collect();
        ids.sort();
        ids
    }

    /// Compile a tinql `Query` against synthetic fixture postings (ids 1–6
    /// matching `bool_fixture` token membership) and return sorted ids.
    fn eval_query_on_fixture(query: &Query) -> Vec<i32> {
        let names: Vec<String> = Vec::new();
        let mut keys = Vec::new();
        let root = compile_fielded(query, &names, 1, &mut keys);
        let universe = tid_set(&[1, 2, 3, 4, 5, 6]);
        let mut postings = vec![FxHashSet::default(); keys.len()];
        for (text, ids) in [
            ("alpha", vec![1u16, 2, 3, 6]),
            ("bravo", vec![1, 2, 4, 6]),
            ("charlie", vec![2, 3, 6]),
        ] {
            if let Some(index) = keys.iter().position(|(existing, _)| existing == text) {
                postings[index] = tid_set(&ids);
            }
        }
        sorted_offsets(&eval_fielded(&root, &postings, &universe))
    }

    fn assert_query_error(index: &str, query: &str, message: &str) {
        let message = message.replace('\'', "''");
        Spi::run(&format!(
            "DO $test$ DECLARE caught text; BEGIN
               BEGIN PERFORM stannum.search_count('{index}', $q${query}$q$);
               EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
               IF caught IS DISTINCT FROM '{message}' THEN
                 RAISE EXCEPTION 'expected %, got %', '{message}', caught;
               END IF;
               caught := NULL;
               BEGIN PERFORM * FROM stannum.search('{index}', $q${query}$q$, 50, 'none');
               EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
               IF caught IS DISTINCT FROM '{message}' THEN
                 RAISE EXCEPTION 'expected %, got %', '{message}', caught;
               END IF;
             END $test$;"
        ))
        .unwrap();
    }

    /// Truth table for the fielded boolean evaluator. Docs 1–6 distribute
    /// alpha/bravo/charlie across title and body so under-match and over-match
    /// are both observable; concat is the single-column control.
    #[pg_test]
    fn fielded_boolean_truth_table_agrees_with_single_column() {
        bool_fixture();
        assert_ids_both("alpha AND bravo", &[1, 2, 6]);
        assert_ids_both("alpha OR bravo", &[1, 2, 3, 4, 6]);
        assert_ids_both("(alpha OR bravo) AND charlie", &[2, 3, 6]);
        assert_ids_both("alpha OR (bravo AND charlie)", &[1, 2, 3, 6]);
        assert_ids_both("alpha AND NOT bravo", &[3]);
        assert_ids_both("* AND NOT alpha", &[4, 5]);
        assert_ids_both("(alpha AND bravo)^2", &[1, 2, 6]);
        assert_ids_both("AT LEAST 2 OF [alpha bravo charlie]", &[1, 2, 3, 6]);
        assert_ids_on("bool_docs_multi", "title:(alpha AND bravo)", &[2]);
        assert_query_error(
            "bool_docs_single",
            "title:(alpha AND bravo)",
            "stannum: field syntax requires a multi-column index",
        );

        // Compound AtLeast children count once, not per leaf.
        assert_ids_both("AT LEAST 2 OF [(alpha AND bravo) charlie]", &[2, 6]);
        assert_ids_both("AT LEAST 2 OF [(alpha OR bravo) charlie]", &[2, 3, 6]);
        // Prefix NOT is not tinql syntax (NOT is a reserved keyword). The
        // expressible form is `AND NOT` against `*`.
        assert_ids_both("alpha OR (* AND NOT bravo)", &[1, 2, 3, 5, 6]);
        assert_ids_both("* AND NOT bravo", &[3, 5]);
        assert_ids_both("* AND NOT (* AND NOT alpha)", &[1, 2, 3, 6]);
        assert_ids_on("bool_docs_multi", "title:(bravo) AND body:(bravo)", &[4]);
        assert_ids_on(
            "bool_docs_multi",
            "title:(bravo) OR body:(bravo)",
            &[1, 2, 4, 6],
        );
        assert_query_error(
            "bool_docs_single",
            "title:(bravo) AND body:(bravo)",
            "stannum: field syntax requires a multi-column index",
        );
        assert_query_error(
            "bool_docs_single",
            "title:(bravo) OR body:(bravo)",
            "stannum: field syntax requires a multi-column index",
        );
        // Bare `name:term` is a colon-bearing token per grammar.pest, not
        // field syntax; field scope requires `name:(…)`. No error — empty.
        assert_ids_both("title:bravo", &[]);
        assert_ids_both("title:bravo AND body:bravo", &[]);
        assert_ids_both("alpha AND absenttoken", &[]);
        assert_ids_both("* AND NOT absenttoken", &[1, 2, 3, 4, 5, 6]);
        assert_ids_both("* AND NOT *", &[]);
        assert_ids_both("(alpha AND bravo)^1.3", &[1, 2, 6]);
        assert_boosted_and_scores("bool_docs_multi", 1.3);
        assert_boosted_and_scores("bool_docs_single", 1.3);
        // `AT LEAST 0` folds to MatchAll; `min > children.len()` is empty.
        assert_ids_both("AT LEAST 0 OF [alpha]", &[1, 2, 3, 4, 5, 6]);
        assert_ids_both("AT LEAST 3 OF [alpha]", &[]);

        let and_bits = search_score_bits("bool_docs_multi", "alpha AND bravo");
        let or_bits = search_score_bits("bool_docs_multi", "alpha OR bravo");
        let alpha_bits = search_score_bits("bool_docs_multi", "alpha");
        let bravo_bits = search_score_bits("bool_docs_multi", "bravo");
        let and_map: FxHashMap<_, _> = and_bits.iter().copied().collect();
        let or_map: FxHashMap<_, _> = or_bits.iter().copied().collect();
        let alpha_map: FxHashMap<_, _> = alpha_bits.iter().copied().collect();
        let bravo_map: FxHashMap<_, _> = bravo_bits.iter().copied().collect();
        for id in [1, 2, 6] {
            assert_eq!(and_map[&id], or_map[&id], "AND/OR score bits id={id}");
        }
        assert_eq!(or_map[&3], alpha_map[&3], "OR-only alpha score bits");
        assert_eq!(or_map[&4], bravo_map[&4], "OR-only bravo score bits");
    }

    #[pg_test]
    fn fielded_duplicate_leaf_membership() {
        bool_fixture();
        assert_ids_both("alpha AND alpha", &[1, 2, 3, 6]);
        // Multi-column fielded scoring uses the structural term list (0.4.0
        // parity): a duplicate conjunct is one contribution. Single-column
        // IndexScorer still scores the repeated clause twice. That 2× behaviour
        // is a pre-existing divergence from 0.4.0 (which folded duplicates on
        // both paths); this step did not introduce it, and the contract suite
        // does not yet pin it on the IndexScorer path except by recording
        // fields.boolean_duplicate_leaf flat_ranked.
        assert_same_as_term_score("bool_docs_multi", "alpha AND alpha", "alpha");
        assert_twice_the_term_score("bool_docs_single", "alpha AND alpha", "alpha");
    }

    #[pg_test]
    fn fielded_dropped_construct_mixed_with_term_errors() {
        bool_fixture();
        assert_query_error(
            "bool_docs_multi",
            "alpha AND foo*",
            "stannum.search() does not support this query on a multi-column index",
        );
        // Single-column still evaluates the regex; it must not equal `alpha`
        // (the result of silently dropping the unsupported clause).
        assert_ne!(
            search_ids("bool_docs_single", "alpha AND foo*"),
            search_ids("bool_docs_single", "alpha"),
        );
    }

    #[pg_test]
    fn fielded_boolean_prefix_not_and_degenerate_ast() {
        bool_fixture();
        for query in ["NOT bravo", "alpha OR NOT bravo", "NOT NOT alpha"] {
            assert_query_error_contains("bool_docs_multi", query, "expected a term");
            assert_query_error_contains("bool_docs_single", query, "expected a term");
        }
        for query in ["AT LEAST 1 OF []", "ALL OF []"] {
            assert_query_error_contains("bool_docs_multi", query, "empty alternatives");
            assert_query_error_contains("bool_docs_single", query, "empty alternatives");
        }

        // Prefix NOT is rejected at SQL; the runtime `Query::Not` identities
        // match the `AND NOT` surface above.
        assert_eq!(
            eval_query_on_fixture(&Query::Not(Box::new(Query::Term("bravo".into())))),
            vec![3, 5]
        );
        assert_eq!(
            eval_query_on_fixture(&Query::Or(
                Box::new(Query::Term("alpha".into())),
                Box::new(Query::Not(Box::new(Query::Term("bravo".into())))),
            )),
            vec![1, 2, 3, 5, 6]
        );
        assert_eq!(
            eval_query_on_fixture(&Query::Not(Box::new(Query::Not(Box::new(Query::Term(
                "alpha".into()
            ),))))),
            vec![1, 2, 3, 6]
        );

        // `collect_fielded_terms` skips every `Query::Not` subtree, so nested
        // Not contributes no scoring leaves even when membership is the inner
        // term. SQL `NOT NOT alpha` never reaches this AST: Structural and
        // StructuralScoring both run `simplify_not`, and `* AND NOT (* AND NOT
        // alpha)` therefore scores as `alpha`.
        let mut skipped = Vec::new();
        collect_fielded_terms(
            &Query::Not(Box::new(Query::Not(Box::new(Query::Term("alpha".into()))))),
            &[],
            1,
            1.0,
            &mut skipped,
        );
        assert!(
            skipped.is_empty(),
            "scoring walk skips the whole Not subtree"
        );
        let not_bravo = search_score_bits("bool_docs_multi", "* AND NOT bravo");
        assert_eq!(
            not_bravo,
            vec![(3, 0.0f32.to_bits()), (5, 0.0f32.to_bits())]
        );
        assert_eq!(
            search_score_bits("bool_docs_single", "* AND NOT bravo"),
            vec![(3, 0.0f32.to_bits()), (5, 0.0f32.to_bits())]
        );
        assert_eq!(
            search_score_bits("bool_docs_multi", "* AND NOT (* AND NOT alpha)"),
            search_score_bits("bool_docs_multi", "alpha")
        );
        assert_eq!(
            search_score_bits("bool_docs_single", "* AND NOT (* AND NOT alpha)"),
            search_score_bits("bool_docs_single", "alpha")
        );

        // Degenerate nodes the SQL simplifier folds away: empty And is the
        // universe; empty Or / min above child count is empty; min == 0 is
        // the universe.
        assert_eq!(
            eval_query_on_fixture(&Query::Conjunction(Vec::new())),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            eval_query_on_fixture(&Query::Disjunction {
                min: 1,
                children: Vec::new(),
            }),
            Vec::<i32>::new()
        );
        assert_eq!(
            eval_query_on_fixture(&Query::Disjunction {
                min: 0,
                children: vec![Query::Term("alpha".into())],
            }),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            eval_query_on_fixture(&Query::Disjunction {
                min: 3,
                children: vec![Query::Term("alpha".into())],
            }),
            Vec::<i32>::new()
        );
        assert_eq!(
            eval_query_on_fixture(&Query::AtLeast {
                min: 0,
                children: vec![Query::Term("bravo".into())],
            }),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            eval_query_on_fixture(&Query::AtLeast {
                min: 5,
                children: vec![Query::Term("alpha".into()), Query::Term("bravo".into())],
            }),
            Vec::<i32>::new()
        );
        assert_eq!(
            sorted_offsets(&eval_fielded(
                &FieldedNode::And(Vec::new()),
                &[],
                &tid_set(&[1, 2, 3, 4, 5, 6]),
            )),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            sorted_offsets(&eval_fielded(
                &FieldedNode::Or {
                    min: 1,
                    children: Vec::new(),
                },
                &[],
                &tid_set(&[1, 2, 3, 4, 5, 6]),
            )),
            Vec::<i32>::new()
        );
    }

    #[pg_test]
    fn search_zero_limit_still_validates_query_mode_and_bm25() {
        fixture();
        // Catch errors in real SQL subtransactions, then continue using the
        // same index. Raising our own error is outside the caught block.
        for (arguments, message) in [
            (
                "'needle', 0, 'bogus'",
                "stannum.search() snippet must be one of: none, html, ansi",
            ),
            (
                "'needle', 0, 'none', '<m>', '</m>', -1, NULL",
                "stannum score parameters: invalid BM25 parameters",
            ),
            (
                "'needle', 0, 'none', '<m>', '</m>', NULL, 'NaN'::real",
                "stannum score parameters: invalid BM25 parameters",
            ),
            (
                "'needle', 0, 'none', '<m>', '</m>', 'Infinity'::real, NULL",
                "stannum score parameters: invalid BM25 parameters",
            ),
        ] {
            let message = message.replace('\'', "''");
            Spi::run(&format!(
                "DO $test$ DECLARE caught text; BEGIN
                   BEGIN PERFORM * FROM stannum.search('hardening_idx', {arguments});
                   EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
                   IF caught IS DISTINCT FROM '{message}' THEN
                     RAISE EXCEPTION 'expected %, got %', '{message}', caught;
                   END IF;
                 END $test$;"
            ))
            .unwrap();
        }
        Spi::run(
            "DO $test$ DECLARE caught boolean := false; BEGIN
               BEGIN PERFORM * FROM stannum.search('hardening_idx', '(', 0);
               EXCEPTION WHEN OTHERS THEN caught := true; END;
               IF NOT caught THEN RAISE EXCEPTION 'zero limit bypassed parser'; END IF;
             END $test$;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM stannum.search('hardening_idx', 'needle', 0)"
            )
            .unwrap(),
            Some(0)
        );
    }

    #[pg_test]
    fn search_large_limit_and_expression_degraded_mode_keep_visible_matches() {
        fixture();
        Spi::run(
            "CREATE INDEX hardening_expr ON hardening_docs USING stannum(lower(body));
             DELETE FROM hardening_docs WHERE id IN (1, 4, 7);",
        )
        .unwrap();
        let large_limit = PRUNE_MAX_K + 1;
        for index in ["hardening_idx", "hardening_expr"] {
            assert_eq!(
                Spi::get_one::<bool>(&format!(
                    "SELECT stannum.search_count('{index}', 'needle') =
                   (SELECT count(*) FROM hardening_docs WHERE body ==> 'needle')"
                ))
                .unwrap(),
                Some(true)
            );
            assert_eq!(
                Spi::get_one::<i64>(&format!(
                    "SELECT count(*) FROM hardening_docs d JOIN
                   stannum.search('{index}', 'needle', {large_limit}, 'none') s
                   ON d.ctid = s.ctid WHERE s.snippet IS NULL"
                ))
                .unwrap(),
                Some(17)
            );
        }
        // The default limit is a real ten-row bound, not all remaining rows.
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM stannum.search('hardening_idx', 'needle')")
                .unwrap(),
            Some(10)
        );
        Spi::run(
            "DO $test$ DECLARE caught text; BEGIN
               BEGIN PERFORM * FROM stannum.search('hardening_expr', 'needle', 0, 'html');
               EXCEPTION WHEN OTHERS THEN caught := SQLERRM; END;
               IF caught IS NULL OR position('use snippet => ''none''' in caught) = 0 THEN
                 RAISE EXCEPTION 'missing actionable expression-key error: %', caught;
               END IF;
             END $test$;",
        )
        .unwrap();
    }

    /// A NULL first field never blocks the snippet: the refetch reads every
    /// key column, and the first field holding a mark decides (a NULL field
    /// holds none); when nothing marks, the first non-NULL field renders
    /// plain (RFC §5.11).
    ///
    /// Scaffold until Phase 5. `amcanmulticol` stays false.
    #[pg_test]
    #[ignore]
    fn search_snippets_skip_null_fields() {
        Spi::run(
            "CREATE TABLE snip_null(id int primary key, title text, body text);
             INSERT INTO snip_null VALUES
               (1, NULL, 'needle pad'),
               (2, 'pad', 'needle pad');
             CREATE INDEX snip_null_idx ON snip_null USING stannum(title, body);",
        )
        .unwrap();
        let snippets = |query: &str| -> Vec<(i32, String)> {
            Spi::connect(|client| {
                client
                    .select(
                        &format!(
                            "SELECT d.id, s.snippet FROM snip_null d JOIN
                             stannum.search('snip_null_idx', '{query}', 5) s ON d.ctid = s.ctid
                             ORDER BY d.id"
                        ),
                        None,
                        &[],
                    )
                    .unwrap_or_else(|error| panic!("{error}"))
                    .map(|row| {
                        (
                            row.get::<i32>(1).unwrap().unwrap(),
                            row.get::<String>(2).unwrap().unwrap(),
                        )
                    })
                    .collect()
            })
        };
        assert_eq!(
            snippets("needle"),
            vec![
                (1, "<mark>needle</mark> pad".into()),
                (2, "<mark>needle</mark> pad".into())
            ]
        );
        assert_eq!(
            snippets("title:(pad)"),
            vec![(2, "<mark>pad</mark>".into())]
        );
    }

    #[pg_test]
    fn search_acceptance_rejects_invisible_top_row_before_exhaustive_fallback() {
        fixture();
        let heap = oid("hardening_docs");
        let index = oid("hardening_idx");
        let mut scorer = build_standalone_scorer(heap, index, "needle", None, None);
        // complete means every matching candidate fits (strictly fewer than
        // k), not merely that the WAND traversal completed. Use an oversized
        // pool to reach the visibility guard rather than the incomplete guard.
        // STN3's top_k applies heap visibility, so the walk needs a snapshot
        // that sees the SPI-inserted rows (the outer pg_test SELECT does not).
        let pruned = {
            let _snapshot = CurrentSnapshot::push();
            let pruned = scorer.pruned_top_k(21).unwrap();
            assert!(pruned.complete);
            assert_eq!(pruned.rows.len(), 20);
            pruned
        };
        {
            let _snapshot = CurrentSnapshot::push();
            assert_eq!(
                accepted_pruned_rows(scorer.pruned_top_k(21).unwrap(), heap, 3)
                    .unwrap()
                    .len(),
                3
            );
        }
        let deleted = pruned.rows[0].indexed_tid;
        Spi::run(&format!(
            "DELETE FROM hardening_docs WHERE ctid = '({},{})'::tid",
            deleted.block, deleted.offset
        ))
        .unwrap();
        let _snapshot = CurrentSnapshot::push();
        assert!(accepted_pruned_rows(pruned, heap, 3).is_none());
        let expected = exhaustive_rows(&mut scorer, heap);
        assert!(expected.len() >= 3);
        assert!(expected.iter().all(|(_, row)| row.visible_tid != deleted));
        let actual: Vec<_> = search(
            unsafe { PgRelation::with_lock(index, pg_sys::AccessShareLock as _) },
            Some("needle"),
            3,
            "none",
            "",
            "",
            None,
            None,
        )
        .map(|(tid, score, _)| (tid_of(tid), score.to_bits()))
        .collect();
        assert_eq!(
            actual,
            expected[..3]
                .iter()
                .map(|(score, row)| (row.visible_tid, score.to_bits()))
                .collect::<Vec<_>>()
        );
    }

    #[pg_test]
    fn search_acceptance_rejects_hot_root_and_member_underfill() {
        fixture();
        let heap = oid("hardening_docs");
        let root = row_tid(1);
        Spi::run("UPDATE hardening_docs SET revision = 1 WHERE id = 1").unwrap();
        let member = row_tid(1);
        assert_ne!(root, member);
        let _snapshot = CurrentSnapshot::push();
        let visible = unsafe { visible_tid_pairs(heap, BTreeSet::from([root, member])) };
        assert_eq!(visible.len(), 1, "fixture must be an actual HOT chain");
        assert_eq!(visible[0].visible_tid, member);
        assert_eq!(
            visible[0].indexed_tid, root,
            "the indexed root must follow the HOT chain"
        );
        let member_only = unsafe { visible_tid_pairs(heap, BTreeSet::from([member])) };
        assert!(
            member_only.is_empty(),
            "a heap-only member is not an index root"
        );
        // A plain HOT update only leaves the root indexed. Inject both
        // candidates to exercise underfill rejection: PG follows the real
        // root to the member, but cannot fetch the heap-only member as another
        // independent index root. Do not mistake this for two visible roots.
        let pruned = PrunedCandidates {
            complete: true,
            rows: vec![
                RankedCandidate {
                    indexed_tid: root,
                    score: 2.0,
                },
                RankedCandidate {
                    indexed_tid: member,
                    score: 1.0,
                },
            ],
        };
        assert!(accepted_pruned_rows(pruned, heap, 2).is_none());
        let incomplete = PrunedCandidates {
            complete: false,
            rows: vec![RankedCandidate {
                indexed_tid: root,
                score: 2.0,
            }],
        };
        assert!(accepted_pruned_rows(incomplete, heap, 1).is_none());
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(DISTINCT d.id) FROM hardening_docs d JOIN
               stannum.search('hardening_idx', 'needle', 2, 'none') s ON d.ctid = s.ctid"
            )
            .unwrap(),
            Some(2)
        );
    }

    #[pg_test]
    fn snippet_refetch_keeps_null_but_skips_missing_and_changed_members() {
        fixture();
        Spi::run("UPDATE hardening_docs SET body = NULL WHERE id = 1").unwrap();
        let null_tid = row_tid(1);
        let gone = row_tid(2);
        let changed = row_tid(3);
        Spi::run(
            "DELETE FROM hardening_docs WHERE id = 2;
             UPDATE hardening_docs SET revision = 1 WHERE id = 3;",
        )
        .unwrap();
        assert_ne!(row_tid(3), changed);
        let rows = [null_tid, gone, changed]
            .into_iter()
            .map(|tid| {
                (
                    1.0,
                    VisibleTid {
                        indexed_tid: tid,
                        visible_tid: tid,
                    },
                )
            })
            .collect();
        let pipeline = tokenizer::TokenizerPipelineSpec::default()
            .compile()
            .unwrap();
        let _snapshot = CurrentSnapshot::push();
        let output = fetch_snippet(
            oid("hardening_docs"),
            &SnippetKeys::Single(2),
            &pipeline,
            "needle",
            "html",
            "<b>",
            "</b>",
            rows,
        );
        assert_eq!(output.len(), 1);
        assert_eq!(tid_of(output[0].0), null_tid);
        assert_eq!(output[0].1, 1.0);
        assert_eq!(output[0].2, None);
    }
}
