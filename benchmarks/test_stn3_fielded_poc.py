# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import struct
import tempfile
import unittest

import stn3_fielded_poc as poc


class CorpusTests(unittest.TestCase):
    def test_locked_vocab_is_2048_unique_english_tokens(self):
        self.assertEqual(len(poc.VOCAB), 2048)
        self.assertEqual(len(set(poc.VOCAB)), 2048)
        self.assertEqual(poc.VOCAB[0], "the")
        self.assertEqual(poc.VOCAB[-1], "fjord")

    def test_generate_twice_same_sha256(self):
        first, payload = poc.corpus_sha256()
        second, again = poc.corpus_sha256()
        self.assertEqual(first, second)
        self.assertEqual(payload, again)
        self.assertEqual(payload[:20], b'"id","title","body"\n')
        self.assertTrue(payload.endswith(b"\n"))
        self.assertNotIn(b"\r\n", payload)

    def test_small_generate_is_stable(self):
        a, _ = poc.corpus_sha256(rows=32, seed=poc.SEED)
        b, _ = poc.corpus_sha256(rows=32, seed=poc.SEED)
        self.assertEqual(a, b)
        self.assertNotEqual(a, poc.corpus_sha256(rows=31, seed=poc.SEED)[0])


class QueryAndQuantileTests(unittest.TestCase):
    def test_queries_file_is_twelve_unscoped(self):
        queries = poc.load_queries(poc.DEFAULT_QUERIES)
        self.assertEqual(len(queries), 12)
        self.assertNotIn("title:(", "\n".join(queries))
        self.assertIn("quasar OR nebula", queries)

    def test_field_syntax_is_rejected(self):
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", suffix=".txt") as handle:
            handle.write("the\n" * 11 + "title:(history)\n")
            handle.flush()
            with self.assertRaisesRegex(ValueError, "field syntax"):
                poc.load_queries(handle.name)

    def test_nearest_rank_n20(self):
        samples = list(range(1, 21))
        self.assertEqual(poc.nearest_rank(samples, 0.50), 10)
        self.assertEqual(poc.nearest_rank(samples, 0.99), 20)
        p50, p99 = poc.p50_p99(list(reversed(samples)))
        self.assertEqual((p50, p99), (10, 20))


class DictionaryAndGateTests(unittest.TestCase):
    def test_stnf_prefix_df_len(self):
        norms = bytes([2]) + struct.pack("<QQ", 0, 0) + struct.pack("<I", 0)
        df = struct.pack("<I", 0)
        blob = b"STN3xxxx" + b"STNF" + bytes([1]) + struct.pack("<II", len(norms), len(df)) + norms + df
        self.assertEqual(poc.stnf_sections(blob), (len(norms), len(df)))
        self.assertEqual(poc.stnf_sections(b"STN3notrailer"), (0, 0))

    def test_parse_breakdown_dictionary(self):
        text = (
            "/tmp/gen1.segment: STN3, 1000 bytes\n"
            "as stored: 1 blob(s), 1000 bytes\n"
            "  section          bytes   share\n"
            "  header              64    6.4%\n"
            "  dictionary         321   32.1%\n"
            "  ordinals           100   10.0%\n"
        )
        dictionary, total = poc.parse_breakdown(text)
        self.assertEqual(dictionary, 321)
        self.assertEqual(total, 1000)

    def test_gate_table(self):
        def system(dict_bytes, build_s, p50):
            return {
                "dict_bytes": dict_bytes,
                "build_s": build_s,
                "queries": {"the": {"p50_s": p50}},
            }

        green = poc.decide(system(100, 10, 1.0), system(100, 10, 1.0))
        self.assertEqual(green["decision"], "fielded-terms")
        self.assertTrue(green["mandatory_gate"]["pass"])
        self.assertTrue(green["advisory_p50_gate"]["pass"])
        self.assertIsNone(green["advisory_p50_gate"]["waiver"])

        stn4 = poc.decide(system(200, 10, 1.0), system(100, 10, 1.0))
        self.assertEqual(stn4["decision"], "STN4")
        self.assertGreater(stn4["mandatory_gate"]["dict_ratio"], 1.8)

        escalate = poc.decide(system(100, 10, 2.0), system(100, 10, 1.0))
        self.assertEqual(escalate["decision"], "escalate")
        self.assertTrue(escalate["mandatory_gate"]["pass"])
        self.assertFalse(escalate["advisory_p50_gate"]["pass"])
        self.assertIsNone(escalate["advisory_p50_gate"]["waiver"])

        blocked = poc.decide(None, system(100, 10, 1.0))
        self.assertEqual(blocked["decision"], "escalate")
        self.assertEqual(blocked["blocked"], ["stn3_fielded"])

    def test_merge_writes_schema_without_live_server(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fragment = {
                "system": "stn3_single",
                "server_version": "17.11",
                "corpus": {
                    "rows": poc.ROWS,
                    "seed": poc.SEED,
                    "sha256": "abc",
                    "columns": ["id", "title", "body"],
                },
                "dict_bytes": 100,
                "build_s": 1.0,
                "queries": {"the": {"p50_s": 0.01, "p99_s": 0.02, "samples_s": [0.01] * 20}},
            }
            single = root / "single.json"
            single.write_text(json.dumps(fragment) + "\n")
            fielded = dict(fragment)
            fielded["system"] = "stn3_fielded"
            fielded["dict_bytes"] = 110
            fielded_path = root / "fielded.json"
            fielded_path.write_text(json.dumps(fielded) + "\n")
            out = root / "stn3-fielded-poc.json"
            poc.main(
                [
                    "merge",
                    "--fielded",
                    str(fielded_path),
                    "--single",
                    str(single),
                    "--out",
                    str(out),
                    "--stn3-sha",
                    "deadbeef",
                    "--topology",
                    "unit-test",
                ]
            )
            result = json.loads(out.read_text())
            self.assertEqual(result["decision"], "fielded-terms")
            self.assertEqual(result["chinese_corpus"], "deferred")
            self.assertEqual(result["git"]["v040"], poc.V040_PIN)
            self.assertIn("stn3_fielded", result["systems"])
            self.assertIn("mandatory_gate", result)
            self.assertIn("advisory_p50_gate", result)


if __name__ == "__main__":
    unittest.main()
