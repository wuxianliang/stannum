#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Parser and prune-identity checks for explain_counters."""
import unittest

import explain_counters


PLAN = """Aggregate  (cost=...) (actual ...)
  ->  Custom Scan  (cost=...) (actual ...)
        Index: search_idx
        Query: rare
        Order: score DESC
        Top K: 20
        Segments: 5
        Segments Visited: 5
        Immutable Segments: 4
        Write-Buffer Segments: 1
        Dictionary Pages Read: 12
        Postings Blocks Read: 34
        Candidates: 100
        Scored Candidates: 100
        Pruning: block-max
        Scored Candidates: 41
        Pruned by Block-Max: 59
        Heap Fetches: 41
        Heap Rechecks: 0
        Dead Skipped: 3
"""


class ExplainCounters(unittest.TestCase):
    def test_parses_integer_properties_and_last_occurrence_wins(self):
        counters = explain_counters.parse(PLAN)
        self.assertEqual(counters['Segments'], 5)
        self.assertEqual(counters['Dictionary Pages Read'], 12)
        self.assertEqual(counters['Postings Blocks Read'], 34)
        self.assertEqual(counters['Heap Rechecks'], 0)
        self.assertEqual(counters['Dead Skipped'], 3)
        # Two scored reports (count scan then ranked scan shape); the last
        # one is the ranked scan's own counter.
        self.assertEqual(counters['Scored Candidates'], 41)
        self.assertNotIn('Top K', counters)
        self.assertNotIn('Pruning', counters)
        self.assertNotIn('Query', counters)

    def test_identity_checked_only_when_all_three_present(self):
        # Incomplete pruned scans do not know Candidates: no assertion.
        explain_counters.check({'Scored Candidates': 41, 'Pruned by Block-Max': 0})
        explain_counters.check({'Candidates': 100, 'Scored Candidates': 41,
                                'Pruned by Block-Max': 59})
        with self.assertRaises(AssertionError):
            explain_counters.check({'Candidates': 100, 'Scored Candidates': 101,
                                    'Pruned by Block-Max': 0})
        with self.assertRaises(AssertionError):
            explain_counters.check({'Candidates': 100, 'Scored Candidates': 41,
                                    'Pruned by Block-Max': 58})

    def test_observe_runs_text_explain_and_returns_counters(self):
        def run(statement):
            self.assertTrue(statement.startswith('EXPLAIN (ANALYZE, FORMAT TEXT) '))
            self.assertIn('rare', statement)
            return PLAN

        counters = explain_counters.observe(run, 'SELECT 1 WHERE rare')
        self.assertEqual(counters['Pruned by Block-Max'], 59)


if __name__ == '__main__':
    unittest.main()
