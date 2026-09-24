#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Check Stannum's EXPLAIN ANALYZE counters on TEXT plans.

The paired harness runs one ``EXPLAIN (ANALYZE, FORMAT TEXT)`` per workload
query before timed traffic and asserts the block-max prune identity
(``Pruned by Block-Max == Candidates - Scored Candidates``) whenever all
three fields are present, so a ranking-behavior change that silently alters
pruning surfaces in perf runs instead of only in pg tests. The parsed
counters are persisted by the caller into the run's result JSON.

This module only parses and asserts; it never writes files, so it can never
rewrite the published baselines under ``docs/benchmarks``.
"""
import re

# Every integer property the Stannum custom scans may report, in explain
# order. Non-integer properties (Query, Order, Analysis, strategies) are not
# collected.
FIELDS = (
    'Segments',
    'Segments Visited',
    'Immutable Segments',
    'Write-Buffer Segments',
    'Dictionary Pages Read',
    'Postings Blocks Read',
    'Candidates',
    'Scored Candidates',
    'Pruned by Block-Max',
    'Heap Fetches',
    'Heap Rechecks',
    'Dead Skipped',
)

_PATTERN = re.compile(
    r'^\s*(' + '|'.join(re.escape(field) for field in FIELDS) + r'): (\d+)\s*$', re.M)


def parse(text):
    """The integer counters of one TEXT explain plan.

    Later occurrences win, so a plan with several nodes reports the last
    scan's counters; the harness asserts per query, one scan at a time.
    """
    return {name: int(value) for name, value in _PATTERN.findall(text)}


def check(counters):
    """Asserts the prune identity when both of its sides are observable.

    A pruned scan that has not completed does not know its candidate count,
    so ``Candidates`` is absent there; the identity is asserted only when
    ``Candidates``, ``Scored Candidates`` and ``Pruned by Block-Max`` are all
    present.
    """
    required = ('Candidates', 'Scored Candidates', 'Pruned by Block-Max')
    if not all(field in counters for field in required):
        return
    candidates = counters['Candidates']
    scored = counters['Scored Candidates']
    pruned = counters['Pruned by Block-Max']
    if scored > candidates:
        raise AssertionError(f'scored {scored} exceeds candidates {candidates}: {counters}')
    if pruned != candidates - scored:
        raise AssertionError(
            f'prune identity violated: {pruned} != {candidates} - {scored}: {counters}')


def observe(run, sql):
    """Explains ``sql`` with ANALYZE TEXT through ``run`` and checks it.

    ``run`` executes one SQL statement and returns its text output. Returns
    the parsed counters for persistence.
    """
    text = run('EXPLAIN (ANALYZE, FORMAT TEXT) ' + sql)
    counters = parse(text)
    check(counters)
    return counters
