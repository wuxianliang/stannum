// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Compiles a lowered [`Query`] into a cursor over one immutable segment.
//!
//! Every plan node carries an exactness flag. An exact node's cursor yields
//! precisely the documents the reference evaluator would match, so the caller
//! can skip the heap recheck. An inexact node yields a superset, never a
//! subset, and the caller must recheck. The rules:
//!
//! * Terms, Boolean operators, `AT LEAST`, expansions within the expansion cap,
//!   and positional queries over stored positions are exact.
//! * `NOT` is exact only over an exact child, and complements against the
//!   segment's non-empty documents. Complementing a superset is never sound.
//! * An expansion beyond the cap, or anything containing one, degrades to the
//!   non-empty document universe and is inexact.
//!
//! Empty documents match nothing in the reference evaluator, including `*`, so
//! the universe used here excludes them.

use boldi_vigna::{SpanQuery, SpanSolver, TermPositions};
use segment::Tid;
use segment::index::{Expanded, Index, Window};
use segment::payload::PayloadCursor;
use segment::postings::PostingsCursor;
use segment::segment::{Lengths, Term};
use segment::set::{AtLeast, Cursor, Difference, Empty, Intersection, Union};

use super::eval::FuzzyMatcher;
use super::span_expr::SpanExpr;
use super::{Query, SpanPositionFilter, SpanTermSlot};

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Most dictionary terms one wildcard, regex, range or fuzzy node may
    /// expand to before the node degrades to an inexact universe.
    pub max_expansion: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_expansion: 1024,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Segment(#[from] segment::Error),
    #[error("span evaluation failed: {0}")]
    Span(#[from] boldi_vigna::SpanError),
    #[error("unknown field '{0}'")]
    UnknownField(String),
    #[error("field scopes are not supported for this query shape")]
    FieldScopeShape,
}

/// Resolves the field names a query scopes against (`title:(…)`).
///
/// The segment format stores field ids, not names, so the name plan of an
/// index comes from its metadata: a PostgreSQL adapter resolves the recorded
/// column names, an in-memory segment has none.
pub trait FieldScope {
    /// The field id `name` names, or `None` when this index has no such
    /// field (including a fieldless index).
    fn field_id(&self, name: &str) -> Option<u8>;
}

/// The scope of an index with no field names: every name is unknown.
pub struct NoFields;

impl FieldScope for NoFields {
    fn field_id(&self, _name: &str) -> Option<u8> {
        None
    }
}

type Result<T> = std::result::Result<T, PlanError>;
type DynCursor<'a> = Box<dyn Cursor + 'a>;

pub struct Plan<'a> {
    pub cursor: DynCursor<'a>,
    /// True when `cursor` yields exactly the matching documents.
    pub exact: bool,
    /// Rough upper bound on the cursor's cardinality, for ordering conjunctions.
    pub estimate: u64,
}

/// Compiles `query` against `segment` with no field names: a `Query::Field`
/// node cannot be resolved and fails as an unknown field.
pub fn plan<'a, I: Index + ?Sized>(
    query: &Query,
    segment: &'a I,
    limits: &Limits,
) -> Result<Plan<'a>> {
    plan_scoped(query, segment, limits, &NoFields)
}

/// Compiles `query` against `segment` with its field names.
pub fn plan_scoped<'a, I: Index + ?Sized>(
    query: &Query,
    segment: &'a I,
    limits: &Limits,
    fields: &dyn FieldScope,
) -> Result<Plan<'a>> {
    Planner {
        segment,
        limits,
        fields,
    }
    .query(query)
}

/// Prefer bulk execution when a Boolean term has dense grouped postings. Purely
/// sparse and positional plans retain scalar execution: building a five-word
/// mask for each isolated tuple costs more than walking its existing cursor.
pub fn prefers_pages<I: Index + ?Sized>(query: &Query, segment: &I) -> Result<bool> {
    Ok(match query {
        Query::Term(term) => segment
            .term(term)?
            .map(|t| t.postings().and_then(|p| p.prefers_pages()))
            .transpose()?
            .unwrap_or(false),
        Query::And(a, b) | Query::Or(a, b) => {
            prefers_pages(a, segment)? || prefers_pages(b, segment)?
        }
        Query::Conjunction(children)
        | Query::Disjunction { min: 1, children }
        | Query::AtLeast { min: 1, children } => {
            let mut any = false;
            for child in children {
                if prefers_pages(child, segment)? {
                    any = true;
                    break;
                }
            }
            any
        }
        Query::Boost { inner, .. } => prefers_pages(inner, segment)?,
        _ => false,
    })
}

/// A page-oriented plan. Exactness has the same meaning as [`Plan`].
pub struct PagePlan<'a> {
    pub cursor: Box<dyn segment::pages::Cursor + 'a>,
    pub exact: bool,
}

/// Boolean nodes combine offset masks; positional and capped expansion nodes
/// retain the existing evaluator and its conservative exactness contract.
pub fn page_plan<'a, I: Index + ?Sized>(
    query: &Query,
    segment: &'a I,
    limits: &Limits,
) -> Result<PagePlan<'a>> {
    page_plan_scoped(query, segment, limits, &NoFields)
}

/// [`page_plan`] with the index's field names for `Query::Field` nodes.
pub fn page_plan_scoped<'a, I: Index + ?Sized>(
    query: &Query,
    segment: &'a I,
    limits: &Limits,
    fields: &dyn FieldScope,
) -> Result<PagePlan<'a>> {
    use segment::pages;
    let scalar = || -> Result<PagePlan<'a>> {
        let plan = plan_scoped(query, segment, limits, fields)?;
        Ok(PagePlan {
            cursor: Box::new(pages::Rows::new(plan.cursor)?),
            exact: plan.exact,
        })
    };
    let children = |queries: Vec<&Query>, intersection: bool| -> Result<PagePlan<'a>> {
        let plans = queries
            .into_iter()
            .map(|q| page_plan_scoped(q, segment, limits, fields))
            .collect::<Result<Vec<_>>>()?;
        let exact = plans.iter().all(|p| p.exact);
        let cursors = plans.into_iter().map(|p| p.cursor).collect();
        let cursor: Box<dyn pages::Cursor> = if intersection {
            Box::new(pages::Intersection::new(cursors)?)
        } else {
            Box::new(pages::Union::new(cursors))
        };
        Ok(PagePlan { cursor, exact })
    };
    match query {
        Query::Term(term) => match segment.term(term)? {
            Some(term) => Ok(PagePlan {
                cursor: term.postings()?.pages()?,
                exact: true,
            }),
            None => Ok(PagePlan {
                cursor: Box::new(pages::Rows::new(Empty)?),
                exact: true,
            }),
        },
        Query::And(a, b) => children(vec![a, b], true),
        Query::Or(a, b) => children(vec![a, b], false),
        Query::Conjunction(items) if !items.is_empty() => children(items.iter().collect(), true),
        Query::Disjunction {
            min: 1,
            children: items,
        }
        | Query::AtLeast {
            min: 1,
            children: items,
        } => children(items.iter().collect(), false),
        Query::Not(inner) => {
            let inner = page_plan(inner, segment, limits)?;
            if !inner.exact {
                return scalar();
            }
            let universe = Planner {
                segment,
                limits,
                fields,
            }
            .universe()?;
            Ok(PagePlan {
                cursor: Box::new(pages::Difference::new(
                    pages::Rows::new(universe.cursor)?,
                    inner.cursor,
                )?),
                exact: true,
            })
        }
        Query::Boost { inner, .. } => page_plan(inner, segment, limits),
        _ => scalar(),
    }
}

/// Drains a plan, returning the documents and whether they are exact.
pub fn matches<I: Index + ?Sized>(
    query: &Query,
    segment: &I,
    limits: &Limits,
) -> Result<(Vec<Tid>, bool)> {
    let plan = plan(query, segment, limits)?;
    Ok((segment::set::collect(plan.cursor)?, plan.exact))
}

/// A term's postings restricted to a field mask.
///
/// Postings alone cannot field-restrict a term — `df` is aggregate over every
/// field (§5.5) — so each candidate's payload entry is decoded and its field
/// set checked (RFC §5.11). The payload cursor walks the same ordinals as the
/// postings cursor, so each skip decodes one entry.
struct FieldFilter<'a> {
    postings: PostingsCursor<'a>,
    payload: PayloadCursor<'a>,
    /// Bit i set: field i satisfies the scope.
    mask: u16,
}

impl<'a> FieldFilter<'a> {
    fn new(
        postings: PostingsCursor<'a>,
        payload: PayloadCursor<'a>,
        mask: u16,
    ) -> segment::Result<Self> {
        let mut filter = Self {
            postings,
            payload,
            mask,
        };
        filter.skip_unscoped()?;
        Ok(filter)
    }

    /// Advances past postings whose payload entry carries no field in the
    /// mask, leaving the cursor on the first that does.
    fn skip_unscoped(&mut self) -> segment::Result<()> {
        while self.postings.current().is_some() {
            self.payload.seek(self.postings.ordinal())?;
            let entry = self.payload.next_fields()?;
            if entry
                .fields
                .iter()
                .any(|hit| self.mask & (1 << hit.field) != 0)
            {
                return Ok(());
            }
            self.postings.advance()?;
        }
        Ok(())
    }
}

impl Cursor for FieldFilter<'_> {
    fn current(&self) -> Option<Tid> {
        self.postings.current()
    }

    fn advance(&mut self) -> segment::Result<()> {
        self.postings.advance()?;
        self.skip_unscoped()
    }

    fn seek(&mut self, target: Tid) -> segment::Result<()> {
        self.postings.seek(target)?;
        self.skip_unscoped()
    }
}

struct Planner<'a, 'l, I: Index + ?Sized> {
    segment: &'a I,
    limits: &'l Limits,
    fields: &'l dyn FieldScope,
}

enum Expansion<'a> {
    Terms(Vec<Term<'a>>),
    Overflow,
}

impl<'a, I: Index + ?Sized> Planner<'a, '_, I> {
    fn universe(&self) -> Result<Plan<'a>> {
        Ok(Plan {
            cursor: Box::new(NonEmptyDocuments::new(
                self.segment.documents()?,
                self.segment.lengths(),
            )?),
            exact: true,
            estimate: u64::from(self.segment.document_count()),
        })
    }

    fn inexact_universe(&self) -> Result<Plan<'a>> {
        let mut plan = self.universe()?;
        plan.exact = false;
        Ok(plan)
    }

    fn empty() -> Plan<'a> {
        Plan {
            cursor: Box::new(Empty),
            exact: true,
            estimate: 0,
        }
    }

    fn term_plan(term: Option<Term<'a>>) -> Result<Plan<'a>> {
        match term {
            Some(term) => Ok(Plan {
                cursor: Box::new(term.cursor()?),
                exact: true,
                estimate: u64::from(term.df()),
            }),
            None => Ok(Self::empty()),
        }
    }

    fn and(&self, mut children: Vec<Plan<'a>>) -> Result<Plan<'a>> {
        if children.is_empty() {
            return self.universe();
        }
        // Rarest first: the lead cursor drives and the rest are probed by seek.
        children.sort_by_key(|child| child.estimate);
        let exact = children.iter().all(|child| child.exact);
        let estimate = children[0].estimate;
        let cursors = children.into_iter().map(|child| child.cursor).collect();
        Ok(Plan {
            cursor: Box::new(Intersection::new(cursors)?),
            exact,
            estimate,
        })
    }

    fn or(&self, children: Vec<Plan<'a>>) -> Result<Plan<'a>> {
        let exact = children.iter().all(|child| child.exact);
        let estimate = children
            .iter()
            .map(|child| child.estimate)
            .sum::<u64>()
            .min(u64::from(self.segment.document_count()));
        let cursors = children.into_iter().map(|child| child.cursor).collect();
        Ok(Plan {
            cursor: Box::new(Union::new(cursors)),
            exact,
            estimate,
        })
    }

    fn at_least(&self, min: u32, children: Vec<Plan<'a>>) -> Result<Plan<'a>> {
        let min = min as usize;
        if min == 0 {
            return self.universe();
        }
        if min > children.len() {
            return Ok(Self::empty());
        }
        if min == 1 {
            return self.or(children);
        }
        let exact = children.iter().all(|child| child.exact);
        let mut estimates: Vec<u64> = children.iter().map(|child| child.estimate).collect();
        estimates.sort_unstable();
        let estimate = estimates[estimates.len() - min];
        let cursors = children.into_iter().map(|child| child.cursor).collect();
        Ok(Plan {
            cursor: Box::new(AtLeast::new(cursors, min)?),
            exact,
            estimate,
        })
    }

    fn not(&self, inner: Plan<'a>) -> Result<Plan<'a>> {
        if !inner.exact {
            return self.inexact_universe();
        }
        let universe = self.universe()?;
        Ok(Plan {
            cursor: Box::new(Difference::new(universe.cursor, inner.cursor)?),
            exact: true,
            estimate: universe.estimate.saturating_sub(inner.estimate),
        })
    }

    fn expansion_plan(&self, expansion: Expansion<'a>, scope: Option<u16>) -> Result<Plan<'a>> {
        match expansion {
            Expansion::Overflow => self.inexact_universe(),
            Expansion::Terms(terms) => {
                let children = terms
                    .into_iter()
                    .map(|term| self.term_scoped(Some(term), scope))
                    .collect::<Result<Vec<_>>>()?;
                self.or(children)
            }
        }
    }

    fn expand_window(
        &self,
        window: Window<'_>,
        filter: &dyn Fn(&str) -> bool,
    ) -> Result<Expansion<'a>> {
        Ok(
            match self
                .segment
                .expand(window, filter, self.limits.max_expansion)?
            {
                Expanded::Terms(terms) => {
                    Expansion::Terms(terms.into_iter().map(|(_, t)| t).collect())
                }
                Expanded::Overflow => Expansion::Overflow,
            },
        )
    }

    fn expand_regex(&self, regex: &super::CompiledRegex) -> Result<Expansion<'a>> {
        match regex.pure_prefix() {
            Some(prefix) => self.expand_window(Window::Prefix(&prefix), &|_| true),
            None => self.expand_window(Window::All, &|term| regex.is_match(term)),
        }
    }

    fn expand_range(
        &self,
        lower: &super::RangeBound,
        upper: &super::RangeBound,
    ) -> Result<Expansion<'a>> {
        fn bound(bound: &super::RangeBound) -> Option<&str> {
            match bound {
                super::RangeBound::Open => None,
                super::RangeBound::Term(term) => Some(term.as_str()),
            }
        }
        self.expand_window(Window::Range(bound(lower), bound(upper)), &|_| true)
    }

    fn expand_fuzzy(&self, term: &str, prefix: u32, distance: u32) -> Result<Expansion<'a>> {
        let matcher = FuzzyMatcher::new(term, prefix, distance);
        let fixed: String = term.chars().take(prefix as usize).collect();
        self.expand_window(Window::Prefix(&fixed), &|candidate| {
            matcher.is_match(candidate)
        })
    }

    fn slot(&self, slot: &SpanTermSlot) -> Result<Expansion<'a>> {
        Ok(match slot {
            SpanTermSlot::Term(term) => {
                Expansion::Terms(self.segment.term(term)?.into_iter().collect())
            }
            SpanTermSlot::Regex(regex) => self.expand_regex(regex)?,
            SpanTermSlot::Range { lower, upper } => self.expand_range(lower, upper)?,
            SpanTermSlot::Fuzzy {
                term,
                prefix,
                distance,
            } => self.expand_fuzzy(term, *prefix, *distance)?,
        })
    }

    /// Plans `query`, restricted to the fields in `scope` when one is set: a
    /// `Query::Field` node pushes its field down to the leaves, which decode
    /// each candidate's payload entry to check it (RFC §5.11 phase 1).
    fn query_scoped(&self, query: &Query, scope: Option<u16>) -> Result<Plan<'a>> {
        match query {
            Query::Term(term) => self.term_scoped(self.segment.term(term)?, scope),
            Query::And(left, right) => self.and(vec![
                self.query_scoped(left, scope)?,
                self.query_scoped(right, scope)?,
            ]),
            Query::Or(left, right) => self.or(vec![
                self.query_scoped(left, scope)?,
                self.query_scoped(right, scope)?,
            ]),
            Query::Conjunction(children) => self.and(
                children
                    .iter()
                    .map(|c| self.query_scoped(c, scope))
                    .collect::<Result<_>>()?,
            ),
            Query::Disjunction { min, children } | Query::AtLeast { min, children } => self
                .at_least(
                    *min,
                    children
                        .iter()
                        .map(|c| self.query_scoped(c, scope))
                        .collect::<Result<_>>()?,
                ),
            Query::Not(inner) => {
                let inner = self.query_scoped(inner, scope)?;
                self.not(inner)
            }
            Query::MatchAll => {
                // A scope over the document universe is not a field-presence
                // test; refuse it rather than widen the scope silently.
                if scope.is_some() {
                    return Err(PlanError::FieldScopeShape);
                }
                self.universe()
            }
            Query::Regex(regex) => {
                let expansion = self.expand_regex(regex)?;
                self.expansion_plan(expansion, scope)
            }
            Query::Range { lower, upper } => {
                let expansion = self.expand_range(lower, upper)?;
                self.expansion_plan(expansion, scope)
            }
            Query::Fuzzy {
                term,
                prefix,
                distance,
            } => {
                let expansion = self.expand_fuzzy(term, *prefix, *distance)?;
                self.expansion_plan(expansion, scope)
            }
            Query::Boost { inner, .. } => self.query_scoped(inner, scope),
            Query::Field { name, inner } => {
                let Some(field) = self.fields.field_id(name) else {
                    return Err(PlanError::UnknownField(name.clone()));
                };
                // An inner scope wins over an outer one.
                self.query_scoped(inner, Some(1u16 << field))
            }
            Query::Span { .. } | Query::SpanExpr { .. } => self.positional(query, scope),
        }
    }

    /// The two positional shapes. Positions are partitioned by field and the
    /// solver runs once per field in `scope` (every field when unscoped —
    /// RFC §5.11: an unscoped phrase matches when any ONE field contains it,
    /// never across fields; each field's positions start at zero, so
    /// intervals from different fields are never compared).
    fn positional(&self, query: &Query, scope: Option<u16>) -> Result<Plan<'a>> {
        let field_count = self.segment.field_count();
        let try_fields = scope.unwrap_or_else(|| all_fields_mask(u32::from(field_count)));
        match query {
            Query::Span {
                term_slots,
                span_query,
                position_filter,
            } => {
                let Some(slots) = self.slots(term_slots)? else {
                    return self.inexact_universe();
                };
                let skeleton = self.span_skeleton(span_query, &slots)?;
                let solver = SpanSolver::new(span_query)?;
                self.span_plan(
                    skeleton,
                    slots,
                    SpanKind::Fixed {
                        solver,
                        filter: position_filter.clone(),
                    },
                    try_fields,
                    field_count,
                )
            }
            Query::SpanExpr {
                term_slots,
                span_expr,
            } => {
                let Some(slots) = self.slots(term_slots)? else {
                    return self.inexact_universe();
                };
                let skeleton = self.span_expr_skeleton(span_expr, &slots)?;
                self.span_plan(
                    skeleton,
                    slots,
                    SpanKind::Dynamic {
                        expr: span_expr.clone(),
                    },
                    try_fields,
                    field_count,
                )
            }
            _ => unreachable!("positional plans come from span nodes"),
        }
    }

    /// A term's plan, restricted to `scope`'s fields when one is set.
    fn term_scoped(&self, term: Option<Term<'a>>, scope: Option<u16>) -> Result<Plan<'a>> {
        let Some(term) = term else {
            return Ok(Self::empty());
        };
        // `df` stays the estimate: it is aggregate over every field (§5.5),
        // so it is a conservative upper bound whatever the scope.
        let estimate = u64::from(term.df());
        let cursor = term.cursor()?;
        let cursor: DynCursor<'a> = match scope {
            None => Box::new(cursor),
            Some(mask) => Box::new(FieldFilter::new(cursor, term.payload()?.cursor(), mask)?),
        };
        Ok(Plan {
            cursor,
            exact: true,
            estimate,
        })
    }

    /// Compiles `query` against `segment`.
    fn query(&self, query: &Query) -> Result<Plan<'a>> {
        self.query_scoped(query, None)
    }

    /// Resolves every slot, or `None` if any expansion overflowed.
    fn slots(&self, slots: &[SpanTermSlot]) -> Result<Option<Vec<Vec<Term<'a>>>>> {
        let mut resolved = Vec::with_capacity(slots.len());
        for slot in slots {
            match self.slot(slot)? {
                Expansion::Terms(terms) => resolved.push(terms),
                Expansion::Overflow => return Ok(None),
            }
        }
        Ok(Some(resolved))
    }

    fn slot_plan(&self, slot: usize, slots: &[Vec<Term<'a>>]) -> Result<Plan<'a>> {
        let children = slots
            .get(slot)
            .map(|terms| {
                terms
                    .iter()
                    .map(|term| Self::term_plan(Some(*term)))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        self.or(children)
    }

    /// Boolean skeleton of a span query: a superset of documents that could
    /// contain a match, using only which terms are present.
    fn span_skeleton(&self, query: &SpanQuery, slots: &[Vec<Term<'a>>]) -> Result<Plan<'a>> {
        Ok(match query {
            SpanQuery::Empty => Self::empty(),
            SpanQuery::Term(slot) => self.slot_plan(*slot, slots)?,
            SpanQuery::Ordered(children) | SpanQuery::Unordered(children) => self.and(
                children
                    .iter()
                    .map(|child| self.span_skeleton(child, slots))
                    .collect::<Result<_>>()?,
            )?,
            SpanQuery::Or(children) => self.or(children
                .iter()
                .map(|child| self.span_skeleton(child, slots))
                .collect::<Result<_>>()?)?,
            SpanQuery::MaxGaps { inner, .. }
            | SpanQuery::GapsInRange { inner, .. }
            | SpanQuery::MaxWidth { inner, .. }
            | SpanQuery::WithinPositions { inner, .. } => self.span_skeleton(inner, slots)?,
            // Negative relations keep only their retained side.
            SpanQuery::NotContaining { big, .. } => self.span_skeleton(big, slots)?,
            SpanQuery::NotContainedBy { little, .. } => self.span_skeleton(little, slots)?,
            SpanQuery::NonOverlapping { a, .. } => self.span_skeleton(a, slots)?,
            SpanQuery::Containing { big: a, little: b }
            | SpanQuery::ContainedBy { little: a, big: b }
            | SpanQuery::Overlapping { a, b }
            | SpanQuery::Before { a, b }
            | SpanQuery::After { a, b } => self.and(vec![
                self.span_skeleton(a, slots)?,
                self.span_skeleton(b, slots)?,
            ])?,
        })
    }

    fn span_expr_skeleton(&self, expr: &SpanExpr, slots: &[Vec<Term<'a>>]) -> Result<Plan<'a>> {
        Ok(match expr {
            SpanExpr::Empty => Self::empty(),
            SpanExpr::Term(slot) => self.slot_plan(*slot, slots)?,
            SpanExpr::Ordered(children) | SpanExpr::Unordered(children) => self.and(
                children
                    .iter()
                    .map(|child| self.span_expr_skeleton(child, slots))
                    .collect::<Result<_>>()?,
            )?,
            SpanExpr::Or(children) => self.or(children
                .iter()
                .map(|child| self.span_expr_skeleton(child, slots))
                .collect::<Result<_>>()?)?,
            SpanExpr::AtLeast { min, children } => self.at_least(
                *min,
                children
                    .iter()
                    .map(|child| self.span_expr_skeleton(child, slots))
                    .collect::<Result<_>>()?,
            )?,
            SpanExpr::MaxGaps { inner, .. }
            | SpanExpr::GapsInRange { inner, .. }
            | SpanExpr::MaxWidth { inner, .. }
            | SpanExpr::WithinPositions { inner, .. }
            | SpanExpr::PositionFilter { inner, .. } => self.span_expr_skeleton(inner, slots)?,
            SpanExpr::NotContaining { big, .. } => self.span_expr_skeleton(big, slots)?,
            SpanExpr::NotContainedBy { little, .. } => self.span_expr_skeleton(little, slots)?,
            SpanExpr::NonOverlapping { a, .. } => self.span_expr_skeleton(a, slots)?,
            SpanExpr::Containing { big: a, little: b }
            | SpanExpr::ContainedBy { little: a, big: b }
            | SpanExpr::Overlapping { a, b }
            | SpanExpr::Before { a, b }
            | SpanExpr::After { a, b } => self.and(vec![
                self.span_expr_skeleton(a, slots)?,
                self.span_expr_skeleton(b, slots)?,
            ])?,
        })
    }

    fn span_plan(
        &self,
        skeleton: Plan<'a>,
        slots: Vec<Vec<Term<'a>>>,
        kind: SpanKind,
        try_fields: u16,
        field_count: u8,
    ) -> Result<Plan<'a>> {
        let mut slot_readers = Vec::with_capacity(slots.len());
        for terms in slots {
            let mut readers = Vec::with_capacity(terms.len());
            for term in terms {
                let payload = term.payload()?;
                let field_aware = payload.is_field_aware();
                readers.push(SlotTerm {
                    postings: term.cursor()?,
                    payload: payload.cursor(),
                    field_aware,
                });
            }
            slot_readers.push(readers);
        }
        let filter = SpanFilter::new(
            skeleton.cursor,
            slot_readers,
            kind,
            self.segment.documents()?,
            self.segment.lengths(),
            try_fields,
            field_count,
        )?;
        Ok(Plan {
            cursor: Box::new(filter),
            exact: skeleton.exact,
            estimate: skeleton.estimate,
        })
    }
}

/// The segment's documents with at least one token.
struct NonEmptyDocuments<'a> {
    documents: PostingsCursor<'a>,
    lengths: Lengths<'a>,
}

impl<'a> NonEmptyDocuments<'a> {
    fn new(documents: PostingsCursor<'a>, lengths: Lengths<'a>) -> segment::Result<Self> {
        let mut this = Self { documents, lengths };
        this.align()?;
        Ok(this)
    }

    fn align(&mut self) -> segment::Result<()> {
        while self.documents.current().is_some() && !self.lengths.any(self.documents.ordinal())? {
            self.documents.advance()?;
        }
        Ok(())
    }
}

impl Cursor for NonEmptyDocuments<'_> {
    fn current(&self) -> Option<Tid> {
        self.documents.current()
    }
    fn advance(&mut self) -> segment::Result<()> {
        self.documents.advance()?;
        self.align()
    }
    fn seek(&mut self, target: Tid) -> segment::Result<()> {
        self.documents.seek(target)?;
        self.align()
    }
}

enum SpanKind {
    Fixed {
        solver: SpanSolver,
        filter: Option<SpanPositionFilter>,
    },
    Dynamic {
        expr: SpanExpr,
    },
}

struct SlotTerm<'a> {
    postings: PostingsCursor<'a>,
    payload: PayloadCursor<'a>,
    /// The payload carries `LSG4` field groups, so positions decode through
    /// `next_fields` and land in per-field buckets.
    field_aware: bool,
}

/// The fields a document may match in: bit `i` is field `i`. Every field of
/// the segment when the query is unscoped (RFC §5.11).
fn all_fields_mask(field_count: u32) -> u16 {
    debug_assert!((1..=16).contains(&field_count));
    ((1u32 << field_count.min(16)) - 1) as u16
}

/// One field's view of a document's slot positions: slot `i` reads the
/// positions term `i` holds in exactly this field.
struct FieldSlots<'a> {
    positions: &'a [Vec<Vec<u32>>],
    field: u8,
}

impl TermPositions for FieldSlots<'_> {
    fn positions(&self, term_index: usize) -> &[u32] {
        self.positions[term_index][usize::from(self.field)].as_slice()
    }
}

/// Keeps only skeleton candidates whose stored positions satisfy the span
/// query. Positions are partitioned by field and the solver runs once per
/// field in `try_fields`, so intervals are never compared across fields
/// (RFC §5.11); a fieldless segment has exactly one field bucket and behaves
/// as before. Candidates arrive in TID order, so every per-term lookup is a
/// forward seek.
struct SpanFilter<'a> {
    skeleton: DynCursor<'a>,
    slots: Vec<Vec<SlotTerm<'a>>>,
    kind: SpanKind,
    documents: PostingsCursor<'a>,
    lengths: Lengths<'a>,
    /// slot → field → positions (empty when the term does not occur there).
    positions: Vec<Vec<Vec<u32>>>,
    /// The fields the query may match in; bit `i` is field `i`.
    try_fields: u16,
    field_count: u8,
    current: Option<Tid>,
}

impl<'a> SpanFilter<'a> {
    fn new(
        skeleton: DynCursor<'a>,
        slots: Vec<Vec<SlotTerm<'a>>>,
        kind: SpanKind,
        documents: PostingsCursor<'a>,
        lengths: Lengths<'a>,
        try_fields: u16,
        field_count: u8,
    ) -> Result<Self> {
        let positions = vec![vec![Vec::new(); usize::from(field_count)]; slots.len()];
        let mut this = Self {
            skeleton,
            slots,
            kind,
            documents,
            lengths,
            positions,
            try_fields,
            field_count,
            current: None,
        };
        this.align()?;
        Ok(this)
    }

    fn align(&mut self) -> Result<()> {
        loop {
            let Some(candidate) = self.skeleton.current() else {
                self.current = None;
                return Ok(());
            };
            if self.matches(candidate)? {
                self.current = Some(candidate);
                return Ok(());
            }
            self.skeleton.advance()?;
        }
    }

    fn load_positions(&mut self, tid: Tid) -> Result<()> {
        for (slot, terms) in self.slots.iter_mut().enumerate() {
            for by_field in &mut self.positions[slot] {
                by_field.clear();
            }
            let mut sources = 0;
            for term in terms.iter_mut() {
                if let Some(ordinal) = term.postings.rank(tid)? {
                    term.payload.seek(ordinal)?;
                    if term.field_aware {
                        let entry = term.payload.next_fields()?;
                        for hit in &entry.fields {
                            let bucket = self.positions[slot]
                                .get_mut(usize::from(hit.field))
                                .ok_or(segment::Error::Corrupt("payload field id out of range"))?;
                            bucket.extend_from_slice(&hit.positions);
                        }
                    } else {
                        term.payload.next_into(&mut self.positions[slot][0])?;
                    }
                    sources += 1;
                }
            }
            if sources > 1 {
                // Expansion slots merge several terms; each field's merged
                // list must return to strictly increasing order.
                for by_field in &mut self.positions[slot] {
                    by_field.sort_unstable();
                    by_field.dedup();
                }
            }
        }
        Ok(())
    }

    /// The length of `tid`'s field `field` — the denominator a positional
    /// filter (`IN LAST n%`) resolves against inside that field.
    fn field_length(&mut self, tid: Tid, field: u8) -> Result<u32> {
        match self.documents.rank(tid)? {
            Some(ordinal) => Ok(self.lengths.field_get(ordinal, field)?),
            None => Err(segment::Error::Corrupt("candidate missing from document table").into()),
        }
    }

    fn needs_doc_length(&self) -> bool {
        match &self.kind {
            SpanKind::Fixed { filter, .. } => filter
                .as_ref()
                .is_some_and(SpanPositionFilter::needs_doc_length),
            SpanKind::Dynamic { expr } => expr.needs_doc_length(),
        }
    }

    fn matches(&mut self, tid: Tid) -> Result<bool> {
        self.load_positions(tid)?;
        let needs_len = self.needs_doc_length();
        for field in 0..self.field_count {
            if self.try_fields & (1u16 << field) == 0 {
                continue;
            }
            let doc_len = if needs_len || matches!(self.kind, SpanKind::Dynamic { .. }) {
                self.field_length(tid, field)?
            } else {
                0
            };
            let view = FieldSlots {
                positions: &self.positions,
                field,
            };
            match &mut self.kind {
                SpanKind::Fixed { solver, filter } => {
                    let mut intervals = solver.intervals(&view);
                    let hit = match filter {
                        None => intervals.next().is_some(),
                        Some(filter) => {
                            intervals.any(|interval| filter.matches_interval(doc_len, interval))
                        }
                    };
                    if hit {
                        return Ok(true);
                    }
                }
                SpanKind::Dynamic { expr } => {
                    let resolved = expr.resolve(doc_len);
                    let mut solver = SpanSolver::new(&resolved)?;
                    if solver.intervals(&view).next().is_some() {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
}

impl Cursor for SpanFilter<'_> {
    fn current(&self) -> Option<Tid> {
        self.current
    }
    fn advance(&mut self) -> segment::Result<()> {
        if self.current.is_none() {
            return Ok(());
        }
        self.skeleton.advance()?;
        self.align().map_err(span_to_segment_error)
    }
    fn seek(&mut self, target: Tid) -> segment::Result<()> {
        if self.current.is_some_and(|current| current >= target) {
            return Ok(());
        }
        self.skeleton.seek(target)?;
        self.align().map_err(span_to_segment_error)
    }
}

/// The cursor trait only speaks segment errors; solver failures are reported
/// as corruption because the query was validated when the plan was built.
fn span_to_segment_error(error: PlanError) -> segment::Error {
    match error {
        PlanError::Segment(error) => error,
        PlanError::Span(_) => segment::Error::Corrupt("span solver failed after planning"),
        PlanError::UnknownField(_) | PlanError::FieldScopeShape => {
            segment::Error::Corrupt("field scope failed after planning")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{evaluate, parse_tinql_to_query_default, tokenize_doc};
    use segment::segment::{Segment, SegmentBuilder};
    use tokenizer::presets::default_pipeline;

    fn tid(i: u32) -> Tid {
        Tid::new(i / 100, (i % 100 + 1) as u16).unwrap()
    }

    fn build(docs: &[&str]) -> Vec<u8> {
        let mut builder = SegmentBuilder::default();
        for (i, text) in docs.iter().enumerate() {
            let doc = tokenize_doc(text, default_pipeline());
            builder
                .add_document(tid(i as u32), doc.positioned_tokens())
                .unwrap();
        }
        builder.finish()
    }

    /// Reference answer: indexes of documents the evaluator matches.
    fn reference(docs: &[&str], query: &Query) -> Vec<Tid> {
        docs.iter()
            .enumerate()
            .filter(|(_, text)| {
                evaluate(query, &tokenize_doc(text, default_pipeline()))
                    .unwrap()
                    .matched
            })
            .map(|(i, _)| tid(i as u32))
            .collect()
    }

    fn page_matches(segment: &impl Index, query: &Query, limits: &Limits) -> (Vec<Tid>, bool) {
        let mut plan = page_plan(query, segment, limits).unwrap();
        let mut found = Vec::new();
        while let Some(page) = plan.cursor.current() {
            found.extend(page.offsets.iter().map(|offset| Tid {
                block: page.block,
                offset,
            }));
            plan.cursor.advance().unwrap();
        }
        (found, plan.exact)
    }

    fn check(docs: &[&str], query_text: &str, expect_exact: bool) {
        let bytes = build(docs);
        let segment = Segment::parse(&bytes).unwrap();
        let query = parse_tinql_to_query_default(query_text)
            .unwrap_or_else(|error| panic!("{query_text}: {error}"));
        let (found, exact) = matches(&query, &segment, &Limits::default()).unwrap();
        assert_eq!(
            page_matches(&segment, &query, &Limits::default()),
            (found.clone(), exact)
        );
        let expected = reference(docs, &query);
        assert_eq!(exact, expect_exact, "{query_text}");
        if exact {
            assert_eq!(found, expected, "{query_text}");
        } else {
            for tid in &expected {
                assert!(found.contains(tid), "{query_text}: missing {tid:?}");
            }
        }
    }

    #[test]
    fn dense_page_plans_agree_with_reference_for_nested_boolean_and_positional_queries() {
        let docs: Vec<_> = (0..2000)
            .map(|i| match i % 7 {
                0 => "",
                1 => "common red blue",
                2 => "common blue red",
                3 => "common green",
                4 => "rare green",
                5 => "red",
                _ => "common",
            })
            .collect();
        for query in [
            "common",
            "missing",
            "common AND red",
            "common OR rare",
            "(common OR rare) AND (red OR green)",
            "common AND NOT red",
            "* AND NOT common",
            "* AND NOT missing",
            "*",
            "common OR common",
            "red AND blue",
            "common AND \"red blue\"",
            "common OR \"red blue\"",
            "red NEAR/2 blue",
        ] {
            check(&docs, query, true);
        }
    }

    const DOCS: &[&str] = &[
        "craft beer bar",
        "beer for craft fans",
        "the big bad wolf",
        "big old wolf and a big bad dog",
        "",
        "...",
        "wine wine wine",
        "brewhouse jalapeno craft",
        "security threat critical buy",
        "security and a threat but not critical",
    ];

    #[test]
    fn boolean_queries_are_exact() {
        for query in [
            "craft",
            "absent",
            "craft AND beer",
            "craft OR wine",
            "craft AND NOT beer",
            "* AND NOT craft",
            "* AND NOT wolf",
            "*",
            "AT LEAST 2 OF [craft beer wolf]",
            "ALL OF [big bad wolf]",
            "beer^2",
        ] {
            check(DOCS, query, true);
        }
    }

    #[test]
    fn positional_queries_are_exact() {
        for query in [
            "\"craft beer\"",
            "\"beer craft\"",
            "\"big _ wolf\"",
            "\"big bad wolf\"~2",
            "\"[big large] bad wolf\"",
            "craft NEAR/5 beer",
            "craft THEN/0 beer",
            "beer THEN/0 craft",
            "(hops NEAR/10 malt) WITHIN 4",
            "wolf IN FIRST 3 WORDS",
            "wolf IN LAST 50%",
            "big IN MIDDLE 50%",
            "craft IN WORDS 2 TO 3",
            "(security NEAR/10 threat) ENCLOSES critical",
            "(security NEAR/10 threat) NOT ENCLOSES critical",
            "critical ENCLOSED BY (security NEAR/10 threat)",
            "security BEFORE critical",
            "critical AFTER security",
            "(beer NEAR/5 craft) OVERLAPPING (craft NEAR/5 bar)",
            "(big NEAR/1 wolf) NOT OVERLAPPING bad",
            "AT LEAST 1 OF [beer, wine] THEN/0 craft",
            "(craft THEN/5 beer) IN FIRST 200 WORDS",
        ] {
            check(DOCS, query, true);
        }
    }

    #[test]
    fn expansions_are_exact_within_the_cap_and_degrade_beyond_it() {
        for query in [
            "brew*",
            "*house",
            "b?er",
            "MATCHES hop.*s",
            "MATCHES wi.e",
            "a TO cat",
            "* TO cat",
            "monkey TO *",
            "jalapeño~1",
            "beer~0:2",
            "\"craft b*\"",
            "\"[MATCHES b.*] wolf\"",
            "big~1 NEAR/2 wolf",
        ] {
            check(DOCS, query, true);
        }
        let bytes = build(DOCS);
        let segment = Segment::parse(&bytes).unwrap();
        let tight = Limits { max_expansion: 1 };
        let query = parse_tinql_to_query_default("b*").unwrap();
        let (found, exact) = matches(&query, &segment, &tight).unwrap();
        assert!(!exact);
        for tid in reference(DOCS, &query) {
            assert!(found.contains(&tid));
        }
        // NOT over an inexact child cannot be exact either.
        let query = parse_tinql_to_query_default("* AND NOT b*").unwrap();
        let (found, exact) = matches(&query, &segment, &tight).unwrap();
        assert!(!exact);
        for tid in reference(DOCS, &query) {
            assert!(found.contains(&tid));
        }
    }

    mod random {
        use super::*;
        use proptest::prelude::*;

        /// Default case count, overridable with `PROPTEST_CASES` for heavier runs.
        fn cases(default: u32) -> u32 {
            std::env::var("PROPTEST_CASES")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        }

        const WORDS: [&str; 6] = ["alpha", "beta", "gamma", "delta", "alphabet", "zeta"];

        fn word() -> impl Strategy<Value = String> {
            prop::sample::select(WORDS.to_vec()).prop_map(str::to_owned)
        }

        fn document() -> impl Strategy<Value = String> {
            prop::collection::vec(word(), 0..8).prop_map(|words| words.join(" "))
        }

        fn leaf() -> impl Strategy<Value = String> {
            prop_oneof![
                4 => word(),
                1 => Just("absent".to_owned()),
                1 => Just("*".to_owned()),
                1 => Just("alph*".to_owned()),
                1 => Just("?eta".to_owned()),
                1 => Just("MATCHES .*a".to_owned()),
                1 => Just("alpha TO beta".to_owned()),
                1 => Just("beta~1".to_owned()),
                1 => Just("gamma~0:2".to_owned()),
                2 => (word(), word()).prop_map(|(a, b)| format!("\"{a} {b}\"")),
                1 => (word(), word()).prop_map(|(a, b)| format!("\"{a} _ {b}\"")),
                1 => (word(), word(), word()).prop_map(|(a, b, c)| format!("\"[{a} {b}] {c}\"~1")),
                1 => (word(), word()).prop_map(|(a, b)| format!("\"{a} [MATCHES {b}.*]\"")),
            ]
        }

        fn query() -> impl Strategy<Value = String> {
            leaf().prop_recursive(3, 24, 3, |inner| {
                prop_oneof![
                    (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a}) AND ({b})")),
                    (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a}) OR ({b})")),
                    (inner.clone(), inner.clone())
                        .prop_map(|(a, b)| format!("({a}) AND NOT ({b})")),
                    (word(), word(), 0u32..3).prop_map(|(a, b, n)| format!("{a} NEAR/{n} {b}")),
                    (word(), word(), 0u32..3).prop_map(|(a, b, n)| format!("{a} THEN/{n} {b}")),
                    (word(), word(), 0u32..3)
                        .prop_map(|(a, b, n)| format!("({a} NEAR/{n} {b}) WITHIN 3")),
                    (word(), 1u32..6).prop_map(|(a, n)| format!("{a} IN FIRST {n} WORDS")),
                    (word(), 1u32..6).prop_map(|(a, n)| format!("{a} IN LAST {n} WORDS")),
                    (word(), 10u32..100).prop_map(|(a, p)| format!("{a} IN LAST {p}%")),
                    (word(), 10u32..100).prop_map(|(a, p)| format!("{a} IN MIDDLE {p}%")),
                    (word(), word(), word())
                        .prop_map(|(a, b, c)| format!("({a} NEAR/3 {b}) ENCLOSES {c}")),
                    (word(), word(), word())
                        .prop_map(|(a, b, c)| format!("({a} NEAR/3 {b}) NOT ENCLOSES {c}")),
                    (word(), word()).prop_map(|(a, b)| format!("{a} BEFORE {b}")),
                    (word(), word(), word())
                        .prop_map(|(a, b, c)| format!("AT LEAST 2 OF [{a} {b} {c}]")),
                    (word(), word(), word())
                        .prop_map(|(a, b, c)| format!("AT LEAST 1 OF [{a} {b}] THEN/1 {c}")),
                    (word(), word())
                        .prop_map(|(a, b)| format!("({a} IN FIRST 3 WORDS) BEFORE {b}")),
                ]
            })
        }

        proptest! {
            #![proptest_config(ProptestConfig { cases: cases(400), ..ProptestConfig::default() })]

            #[test]
            fn plans_agree_with_the_reference_evaluator(
                docs in prop::collection::vec(document(), 0..12),
                query_text in query(),
                cap in prop_oneof![3 => Just(1024usize), 1 => 1usize..3],
            ) {
                let docs: Vec<&str> = docs.iter().map(String::as_str).collect();
                let Ok(query) = parse_tinql_to_query_default(&query_text) else {
                    return Ok(());
                };
                let bytes = build(&docs);
                let segment = Segment::parse(&bytes).unwrap();
                let limits = Limits { max_expansion: cap };
                let (found, exact) = matches(&query, &segment, &limits).unwrap();
                prop_assert_eq!(page_matches(&segment, &query, &limits), (found.clone(), exact));
                let expected = reference(&docs, &query);
                if exact {
                    prop_assert_eq!(&found, &expected, "{}", query_text);
                } else {
                    for tid in &expected {
                        prop_assert!(found.contains(tid), "{}: missing {:?}", query_text, tid);
                    }
                }
                prop_assert!(found.windows(2).all(|pair| pair[0] < pair[1]));
                // Mid-stream seeks land on the same documents.
                if let Some(middle) = found.get(found.len() / 2) {
                    let mut plan = plan(&query, &segment, &limits).unwrap();
                    plan.cursor.seek(*middle).unwrap();
                    prop_assert_eq!(plan.cursor.current(), Some(*middle));
                    let rest = segment::set::collect(plan.cursor).unwrap();
                    prop_assert_eq!(rest, found[found.len() / 2..].to_vec());
                }
            }
        }
    }

    /// Field-scoped and unscoped positional queries over a two-field
    /// (`LSG4`) segment: positions are partitioned by field, the solver runs
    /// per field, and intervals never cross fields (RFC §5.11).
    mod fields {
        use super::*;

        const NAMES: &[&str] = &["title", "body"];

        struct Names;

        impl FieldScope for Names {
            fn field_id(&self, name: &str) -> Option<u8> {
                NAMES
                    .iter()
                    .position(|field| *field == name)
                    .and_then(|field| u8::try_from(field).ok())
            }
        }

        /// Builds a two-field segment from explicit per-field tokens, so the
        /// tests never depend on how a tokenizer segments a string.
        fn build_fields(docs: &[&[(u8, &str, u32)]]) -> Vec<u8> {
            let mut builder = SegmentBuilder::with_field_count(2);
            for (i, tokens) in docs.iter().enumerate() {
                builder
                    .add_document_fields(
                        tid(i as u32),
                        2,
                        tokens.iter().map(|(f, t, p)| (*f, *t, *p)),
                    )
                    .unwrap();
            }
            builder.finish_fields()
        }

        fn check(docs: &[&[(u8, &str, u32)]], query: &str, expected: &[u32]) {
            let bytes = build_fields(docs);
            let segment = Segment::parse(&bytes).unwrap();
            assert_eq!(segment.field_count(), 2);
            let parsed = parse_tinql_to_query_default(query)
                .unwrap_or_else(|error| panic!("{query}: {error}"));
            let planned = plan_scoped(&parsed, &segment, &Limits::default(), &Names)
                .unwrap_or_else(|error| panic!("{query}: {error}"));
            assert!(planned.exact, "{query}");
            let found = segment::set::collect(planned.cursor).unwrap();
            let expected: Vec<Tid> = expected.iter().map(|i| tid(*i)).collect();
            assert_eq!(found, expected, "{query}");
            // The page plan agrees (it falls back to the scalar plan here).
            let mut pages = page_plan_scoped(&parsed, &segment, &Limits::default(), &Names)
                .unwrap_or_else(|error| panic!("{query}: {error}"));
            let mut page_found = Vec::new();
            while let Some(page) = pages.cursor.current() {
                page_found.extend(page.offsets.iter().map(|offset| Tid {
                    block: page.block,
                    offset,
                }));
                pages.cursor.advance().unwrap();
            }
            assert_eq!(page_found, expected, "{query} (page plan)");
        }

        /// 甲 adjacent in one field; 甲 in title with 乙 in body; both fields'
        /// positions starting at zero (cross-field both-0 parses cleanly).
        const PHRASE_DOCS: &[&[(u8, &str, u32)]] = &[
            &[(0, "甲", 0), (0, "乙", 1)],
            &[(0, "甲", 0), (1, "乙", 0)],
            &[(1, "甲", 0), (1, "乙", 1)],
            &[(1, "乙", 0), (1, "甲", 1)],
        ];

        #[test]
        fn field_scoped_phrase_matches_only_within_the_field() {
            // Doc 0 holds the phrase in title; doc 2 in body. Doc 1 holds 甲
            // in title and 乙 in body — never a match.
            check(PHRASE_DOCS, "title:(\"甲 乙\")", &[0]);
            check(PHRASE_DOCS, "body:(\"甲 乙\")", &[2]);
            check(
                PHRASE_DOCS,
                "title:(\"甲 乙\") OR body:(\"甲 乙\")",
                &[0, 2],
            );
        }

        #[test]
        fn unscoped_phrase_matches_any_one_field_never_across() {
            // The Lucene rule: any ONE field containing the phrase matches;
            // 甲 in one field plus 乙 in another never does.
            check(PHRASE_DOCS, "\"甲 乙\"", &[0, 2]);
            check(PHRASE_DOCS, "\"乙 甲\"", &[3]);
        }

        #[test]
        fn cross_field_positions_both_zero_decode_cleanly() {
            // Docs 1 and 3 carry the same position in different fields; term
            // and Boolean queries over them decode without interference.
            check(PHRASE_DOCS, "甲", &[0, 1, 2, 3]);
            check(PHRASE_DOCS, "甲 AND 乙", &[0, 1, 2, 3]);
            check(PHRASE_DOCS, "title:(甲) AND body:(乙)", &[1]);
        }

        #[test]
        fn field_scoped_spans_stay_within_the_field() {
            let docs: &[&[(u8, &str, u32)]] = &[
                &[(0, "a", 0), (0, "b", 2), (1, "a", 0), (1, "b", 1)],
                &[(0, "a", 0), (0, "b", 1)],
            ];
            // Doc 0 has the ordered pair only in body; doc 1 in title.
            check(docs, "title:(a THEN/0 b)", &[1]);
            check(docs, "body:(a THEN/0 b)", &[0]);
            // Unscoped near matches wherever one field holds it — doc 0 in
            // body, doc 1 in title; 甲-style cross-field adjacency is absent.
            check(docs, "a NEAR/1 b", &[0, 1]);
            check(docs, "\"a b\"", &[0, 1]);
        }

        #[test]
        fn positional_filters_use_the_owning_fields_length() {
            let docs: &[&[(u8, &str, u32)]] = &[
                &[(0, "a", 0), (0, "x", 1)], // title: x is last
                &[(0, "x", 0), (0, "a", 1), (1, "pad", 0), (1, "pad", 1)], // no
                &[(0, "x", 0), (1, "pad", 0), (1, "a", 1)], // title len 1
            ];
            check(docs, "title:(x IN LAST 1 WORDS)", &[0, 2]);
            check(docs, "body:(x IN LAST 1 WORDS)", &[]);
            // Unscoped: any field whose own last word is x.
            check(docs, "x IN LAST 1 WORDS", &[0, 2]);
            check(docs, "x IN FIRST 1 WORDS", &[1, 2]);
        }

        #[test]
        fn boosted_field_scoped_phrase_keeps_its_scope() {
            check(PHRASE_DOCS, "title:(\"甲 乙\"^2)", &[0]);
        }

        #[test]
        fn unknown_and_fieldless_scopes_fail_closed() {
            let bytes = build_fields(PHRASE_DOCS);
            let segment = Segment::parse(&bytes).unwrap();
            let query = parse_tinql_to_query_default("nope:(甲)").unwrap();
            assert!(matches!(
                plan_scoped(&query, &segment, &Limits::default(), &Names),
                Err(PlanError::UnknownField(name)) if name == "nope"
            ));
            // A fieldless plan resolves no names at all.
            let query = parse_tinql_to_query_default("title:(甲)").unwrap();
            assert!(matches!(
                plan(&query, &segment, &Limits::default()),
                Err(PlanError::UnknownField(_))
            ));
        }

        #[test]
        fn universe_counts_documents_with_only_later_field_tokens() {
            // Doc 1's title is empty; `*` and `NOT` must still see it.
            let docs: &[&[(u8, &str, u32)]] = &[&[(0, "a", 0)], &[(1, "b", 0)]];
            check(docs, "*", &[0, 1]);
            check(docs, "* AND NOT a", &[1]);
            check(docs, "* AND NOT b", &[0]);
        }
    }

    #[test]
    fn empty_documents_never_match_and_seek_composes() {
        check(&["", "...", "x"], "*", true);
        check(&["", "...", "x"], "* AND NOT y", true);
        let bytes = build(DOCS);
        let segment = Segment::parse(&bytes).unwrap();
        let query = parse_tinql_to_query_default("craft OR wolf").unwrap();
        let mut plan = plan(&query, &segment, &Limits::default()).unwrap();
        plan.cursor.seek(tid(2)).unwrap();
        assert_eq!(plan.cursor.current(), Some(tid(2)));
        plan.cursor.seek(tid(4)).unwrap();
        assert_eq!(plan.cursor.current(), Some(tid(7)));
    }
}
