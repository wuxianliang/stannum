// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

//! Posting-list intersection for AND / AtLeast / Disjunction{min>1}.
//!
//! A gallop / advance-the-shorter-list walk: the rarest child leads, and
//! every other child seeks to that ordinal instead of stepping both lists
//! by `ordinal+1` over the union. Production streams implement [`Front`];
//! plain unit tests use slice fronts and a no-op interrupt step so the
//! test binary never sees PostgreSQL's `InterruptPending`.

use super::profile;

/// Interrupt / progress hook. Production passes `interrupt_at`; tests pass a
/// no-op.
pub(crate) struct Intersect<'a> {
    n: usize,
    step: &'a mut dyn FnMut(usize),
}

impl<'a> Intersect<'a> {
    pub(crate) fn new(step: &'a mut dyn FnMut(usize)) -> Self {
        Self { n: 0, step }
    }

    pub(crate) fn tick(&mut self) {
        (self.step)(self.n);
        self.n = self.n.saturating_add(1);
    }
}

/// One sorted ordinal stream. `advance` never moves backwards: a target at
/// or before the published current is a no-op, so a lead that lands behind
/// a sibling does not rewind that sibling.
pub(crate) trait Front {
    fn current(&self) -> Option<u32>;
    fn advance(&mut self, target: u32, ix: &mut Intersect<'_>);
    fn hint(&self) -> u64;
}

fn record_span(from: Option<u32>, to: u32, target: u32) {
    let start = from.unwrap_or(target);
    profile::add_and_span(to.saturating_sub(start));
}

/// Next ordinal at or after `target` that every child holds.
pub(crate) fn next_conjunction<F: Front>(
    children: &mut [F],
    mut target: u32,
    ix: &mut Intersect<'_>,
) -> Option<u32> {
    if children.is_empty() {
        return None;
    }
    loop {
        ix.tick();
        profile::add_and_advance();
        let lead = (0..children.len())
            .min_by_key(|&i| children[i].hint())
            .expect("conjunction children");
        let before = children[lead].current();
        children[lead].advance(target, ix);
        let mut cand = children[lead].current()?;
        record_span(before, cand, target);
        let mut aligned = true;
        for (i, child) in children.iter_mut().enumerate() {
            if i == lead {
                continue;
            }
            profile::add_and_advance();
            let before = child.current();
            child.advance(cand, ix);
            let at = child.current()?;
            record_span(before, at, cand);
            if at != cand {
                aligned = false;
                cand = cand.max(at);
            }
        }
        if aligned {
            profile::add_and_hit();
            return Some(cand);
        }
        if cand <= target {
            target = target.saturating_add(1);
            if target == 0 {
                return None;
            }
        } else {
            target = cand;
        }
    }
}

/// Next ordinal at or after `target` that at least `min` children hold.
/// Same gallop shape as conjunction: laggards seek to the `min`-th current.
pub(crate) fn next_atleast<F: Front>(
    children: &mut [F],
    min: u32,
    mut target: u32,
    ix: &mut Intersect<'_>,
) -> Option<u32> {
    let need = min as usize;
    if need == 0 || children.is_empty() || need > children.len() {
        return None;
    }
    loop {
        ix.tick();
        profile::add_and_advance();
        let mut hits = Vec::with_capacity(children.len());
        for child in children.iter_mut() {
            let before = child.current();
            child.advance(target, ix);
            if let Some(at) = child.current() {
                record_span(before, at, target);
                hits.push(at);
            }
        }
        if hits.len() < need {
            return None;
        }
        hits.sort_unstable();
        let pivot = hits[need - 1];
        if hits[0] == pivot {
            profile::add_and_hit();
            return Some(pivot);
        }
        if pivot <= target {
            target = target.saturating_add(1);
            if target == 0 {
                return None;
            }
        } else {
            target = pivot;
        }
    }
}

/// Next ordinal at or after `target` that any child holds (OR min=1).
pub(crate) fn next_union<F: Front>(
    children: &mut [F],
    target: u32,
    ix: &mut Intersect<'_>,
) -> Option<u32> {
    ix.tick();
    let mut best = None;
    for child in children.iter_mut() {
        child.advance(target, ix);
        if let Some(at) = child.current() {
            best = Some(best.map_or(at, |seen: u32| seen.min(at)));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::{Front, Intersect, next_atleast, next_conjunction, next_union};

    struct SliceFront<'a> {
        items: &'a [u32],
        pos: usize,
        advances: usize,
    }

    impl<'a> SliceFront<'a> {
        fn new(items: &'a [u32]) -> Self {
            Self {
                items,
                pos: 0,
                advances: 0,
            }
        }
    }

    impl Front for SliceFront<'_> {
        fn current(&self) -> Option<u32> {
            self.items.get(self.pos).copied()
        }

        fn advance(&mut self, target: u32, ix: &mut Intersect<'_>) {
            ix.tick();
            self.advances += 1;
            if self.current().is_some_and(|at| at >= target) {
                return;
            }
            self.pos += self.items[self.pos..].partition_point(|&o| o < target);
        }

        fn hint(&self) -> u64 {
            (self.items.len() - self.pos) as u64
        }
    }

    struct DeadFront<'a> {
        inner: SliceFront<'a>,
        dead: &'a [u32],
    }

    impl<'a> DeadFront<'a> {
        fn new(items: &'a [u32], dead: &'a [u32]) -> Self {
            Self {
                inner: SliceFront::new(items),
                dead,
            }
        }
    }

    impl Front for DeadFront<'_> {
        fn current(&self) -> Option<u32> {
            self.inner.current()
        }

        fn advance(&mut self, target: u32, ix: &mut Intersect<'_>) {
            self.inner.advance(target, ix);
            while let Some(at) = self.inner.current() {
                if !self.dead.contains(&at) {
                    break;
                }
                let next = at.saturating_add(1);
                if next == 0 {
                    self.inner.pos = self.inner.items.len();
                    break;
                }
                self.inner.advance(next, ix);
            }
        }

        fn hint(&self) -> u64 {
            self.inner.hint()
        }
    }

    fn drain<F: Front>(
        children: &mut [F],
        mut next: impl FnMut(&mut [F], u32, &mut Intersect<'_>) -> Option<u32>,
    ) -> Vec<u32> {
        let mut step = |_n: usize| {};
        let mut ix = Intersect::new(&mut step);
        let mut out = Vec::new();
        let mut target = 0u32;
        while let Some(hit) = next(children, target, &mut ix) {
            out.push(hit);
            if hit == u32::MAX {
                break;
            }
            target = hit + 1;
        }
        out
    }

    #[test]
    fn conjunction_gallops_the_shorter_list() {
        let long: Vec<u32> = (0..200).collect();
        let short = [10u32, 50, 90, 150];
        let mut children = vec![SliceFront::new(&long), SliceFront::new(&short)];
        let got = drain(&mut children, next_conjunction);
        assert_eq!(got, vec![10, 50, 90, 150]);
        // Short list leads: the long list is sought to each hit, not walked.
        assert!(
            children[0].advances < long.len(),
            "long-list seeks {}",
            children[0].advances
        );
        assert!(
            children[0].advances <= 20,
            "long-list seeks {}",
            children[0].advances
        );
    }

    #[test]
    fn conjunction_three_children_and_empty() {
        let a = [1u32, 2, 3, 5, 8, 13];
        let b = [2u32, 3, 8, 9];
        let c = [0u32, 3, 8];
        let mut children = vec![
            SliceFront::new(&a),
            SliceFront::new(&b),
            SliceFront::new(&c),
        ];
        assert_eq!(drain(&mut children, next_conjunction), vec![3, 8]);

        let mut empty: Vec<SliceFront> = Vec::new();
        assert!(drain(&mut empty, next_conjunction).is_empty());

        let none: [u32; 0] = [];
        let mut miss = vec![SliceFront::new(&a), SliceFront::new(&none)];
        assert!(drain(&mut miss, next_conjunction).is_empty());
    }

    #[test]
    fn conjunction_skips_duplicate_ordinals() {
        let a = [1u32, 1, 2, 2, 5];
        let b = [1u32, 2, 2, 4, 5];
        let mut children = vec![SliceFront::new(&a), SliceFront::new(&b)];
        assert_eq!(drain(&mut children, next_conjunction), vec![1, 2, 5]);
    }

    #[test]
    fn atleast_two_of_three_gallops_to_the_pivot() {
        let a = [1u32, 2, 3, 5, 8];
        let b = [2u32, 3, 8, 9];
        let c = [0u32, 3, 8];
        let mut children = vec![
            SliceFront::new(&a),
            SliceFront::new(&b),
            SliceFront::new(&c),
        ];
        let got = drain(&mut children, |cs, t, ix| next_atleast(cs, 2, t, ix));
        assert_eq!(got, vec![2, 3, 8]);

        let mut children = vec![
            SliceFront::new(&a),
            SliceFront::new(&b),
            SliceFront::new(&c),
        ];
        let got = drain(&mut children, |cs, t, ix| next_atleast(cs, 3, t, ix));
        assert_eq!(got, vec![3, 8]);

        let mut children = vec![
            SliceFront::new(&a),
            SliceFront::new(&b),
            SliceFront::new(&c),
        ];
        assert!(drain(&mut children, |cs, t, ix| next_atleast(cs, 4, t, ix)).is_empty());
    }

    #[test]
    fn union_is_the_sorted_merge() {
        let a = [1u32, 5, 8];
        let b = [2u32, 5, 9];
        let mut children = vec![SliceFront::new(&a), SliceFront::new(&b)];
        assert_eq!(drain(&mut children, next_union), vec![1, 2, 5, 8, 9]);
    }

    #[test]
    fn dead_ordinals_are_skipped_before_alignment() {
        let a = [1u32, 2, 3, 5];
        let b = [2u32, 3, 5];
        let dead = [2u32];
        let mut children = vec![DeadFront::new(&a, &dead), DeadFront::new(&b, &dead)];
        assert_eq!(drain(&mut children, next_conjunction), vec![3, 5]);
    }

    #[test]
    fn interrupt_step_is_injected_and_plain_tests_pass_a_noop() {
        let a = [1u32, 4, 7];
        let b = [1u32, 7];
        let mut children = vec![SliceFront::new(&a), SliceFront::new(&b)];
        let mut ticks = 0usize;
        let mut step = |_n: usize| ticks += 1;
        let mut ix = Intersect::new(&mut step);
        let mut target = 0u32;
        let mut hits = Vec::new();
        while let Some(hit) = next_conjunction(&mut children, target, &mut ix) {
            hits.push(hit);
            target = hit + 1;
        }
        assert_eq!(hits, vec![1, 7]);
        assert!(ticks > 0);
    }
}
