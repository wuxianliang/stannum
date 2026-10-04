# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import struct
import tempfile
import unittest

import stn3_fielded_poc as stn3
import stn4_per_field_poc as poc


class EnglishReuseTests(unittest.TestCase):
    def test_english_sha256_stays_on_the_4_6_pin(self):
        sha, payload = stn3.corpus_sha256()
        self.assertEqual(sha, poc.ENGLISH_SHA256)
        self.assertEqual(payload[:20], b'"id","title","body"\n')
        self.assertTrue(payload.endswith(b"\n"))
        self.assertNotIn(b"\r\n", payload)

    def test_english_queries_still_twelve_unscoped(self):
        queries = poc.load_queries(poc.DEFAULT_QUERIES_EN)
        self.assertEqual(len(queries), 12)
        self.assertNotIn("title:(", "\n".join(queries))


class ChineseCorpusTests(unittest.TestCase):
    def test_locked_vocab_is_2048_unique_han_words(self):
        self.assertEqual(len(poc.ZH_VOCAB), 2048)
        self.assertEqual(len(set(poc.ZH_VOCAB)), 2048)
        self.assertEqual(poc.ZH_VOCAB[0], "一个")
        self.assertEqual(poc.ZH_VOCAB[-1], "望远镜")
        for word in poc.ZH_VOCAB:
            self.assertGreaterEqual(len(word), 2)
            self.assertLessEqual(len(word), 4)
            self.assertTrue(all("\u4e00" <= ch <= "\u9fff" for ch in word), word)

    def test_chinese_sha256_is_pinned(self):
        sha, payload = poc.zh_corpus_sha256()
        self.assertEqual(sha, poc.CHINESE_SHA256)
        second, again = poc.zh_corpus_sha256()
        self.assertEqual(sha, second)
        self.assertEqual(payload, again)
        self.assertEqual(payload[:20], b'"id","title","body"\n')
        self.assertTrue(payload.endswith(b"\n"))
        self.assertNotIn(b"\r\n", payload)
        self.assertNotEqual(sha, poc.ENGLISH_SHA256)

    def test_small_chinese_generate_is_stable(self):
        a, _ = poc.zh_corpus_sha256(rows=32, seed=poc.ZH_SEED)
        b, _ = poc.zh_corpus_sha256(rows=32, seed=poc.ZH_SEED)
        self.assertEqual(a, b)
        self.assertNotEqual(a, poc.zh_corpus_sha256(rows=31, seed=poc.ZH_SEED)[0])

    def test_chinese_queries_are_twelve_unscoped(self):
        queries = poc.load_queries(poc.DEFAULT_QUERIES_ZH)
        self.assertEqual(len(queries), 12)
        joined = "\n".join(queries)
        self.assertNotIn("title:(", joined)
        self.assertNotIn("标题:(", joined)
        self.assertIn("类星体 OR 星云", queries)
        self.assertIn("(索引 OR 查询) AND 搜索", queries)

    def test_cjk_field_syntax_is_rejected(self):
        with tempfile.NamedTemporaryFile("w", encoding="utf-8", suffix=".txt") as handle:
            handle.write("我们\n" * 11 + "标题:(历史)\n")
            handle.flush()
            with self.assertRaisesRegex(ValueError, "field syntax"):
                poc.load_queries(handle.name)


class StnfAndBreakdownTests(unittest.TestCase):
    def test_stnf_v1_prefix_still_parses(self):
        norms = bytes([2]) + struct.pack("<QQ", 0, 0) + struct.pack("<I", 0)
        df = struct.pack("<I", 0)
        blob = (
            b"STN3xxxx"
            + b"STNF"
            + bytes([1])
            + struct.pack("<II", len(norms), len(df))
            + norms
            + df
        )
        self.assertEqual(stn3.stnf_sections(blob), (len(norms), len(df)))
        self.assertEqual(stn3.stnf_sections(b"STN3notrailer"), (0, 0))

    def test_stnf_v2_prefix_df_len_is_zero_and_must_end_at_eof(self):
        norms = bytes([2]) + struct.pack("<QQ", 1, 2) + struct.pack("<II", 3, 4)
        blob = b"STN3xxxx" + b"STNF" + bytes([2]) + struct.pack("<I", len(norms)) + norms
        self.assertEqual(stn3.stnf_sections(blob), (len(norms), 0))
        self.assertEqual(len(b"STNF") + 1 + 4 + len(norms), stn3.STNF_PREFIX_LEN_V2 + len(norms))
        truncated = blob[:-1]
        self.assertEqual(stn3.stnf_sections(truncated), (0, 0))
        extra = blob + b"x"
        self.assertEqual(stn3.stnf_sections(extra), (0, 0))

    def test_v2_is_not_silently_zero_when_prefix_is_valid(self):
        # The 4.6 v1-only parser returned (0, 0) for version==2. That is the bug.
        norms = b"n" * 12
        blob = b"head" + b"STNF" + bytes([2]) + struct.pack("<I", 12) + norms
        self.assertEqual(stn3.stnf_sections(blob), (12, 0))

    def test_parse_ordinals_head_from_breakdown_all_row(self):
        text = (
            "/tmp/gen1.segment: STN3, 1000 bytes\n"
            "as stored: 1 blob(s), 1000 bytes\n"
            "  section          bytes   share\n"
            "  header              64    6.4%\n"
            "  dictionary         321   32.1%\n"
            "  ordinals           100   10.0%\n"
            "  terms        count  bitmaps  dictionary   ord_head   ord_body   "
            "pay_hdr    skips   pay_data      total\n"
            "  all              17        3         170         38        57       "
            "8        8         70        352\n"
        )
        self.assertEqual(poc.parse_ordinals_head(text), 38)
        dictionary, total = stn3.parse_breakdown(text)
        self.assertEqual(dictionary, 321)
        self.assertEqual(total, 1000)

    def test_parse_ordinals_head_missing_is_null_not_invented(self):
        text = (
            "as stored: 1 blob(s), 1000 bytes\n"
            "  dictionary         321   32.1%\n"
        )
        self.assertIsNone(poc.parse_ordinals_head(text))
        self.assertIsNone(poc.parse_ordinals_head(""))

    def test_dict_bytes_formula_excludes_norms(self):
        # v2: df_len=0, dict_bytes = sections.dictionary + 0, norms labeled aside.
        sizes = poc.measure_dictionary([])
        self.assertEqual(sizes["dict_bytes"], 0)
        self.assertIsNone(sizes["channel_directory_bytes"])
        self.assertIn("not in dict_bytes", sizes["dict_breakdown"]["channel_directory_note"])
        self.assertEqual(sizes["dict_breakdown"]["trailer_count"], 0)

    def test_trailer_count_follows_norms_len(self):
        with tempfile.TemporaryDirectory() as directory:
            blob = b"STN3xxxx" + b"STNF" + bytes([2]) + struct.pack("<I", 4) + b"abcd"
            path = Path(directory) / "gen1.segment"
            path.write_bytes(blob)
            norms_len, df_len = stn3.stnf_sections(blob)
            self.assertEqual((norms_len, df_len), (4, 0))
            # measure_dictionary would cargo-run breakdown; unit-test the rule only.
            trailers = 1 if norms_len > 0 else 0
            self.assertEqual(trailers, 1)
            empty_norms = b"STN3xxxx"
            self.assertEqual(stn3.stnf_sections(empty_norms), (0, 0))


class IndexSqlAndJiebaTests(unittest.TestCase):
    def test_systems_sql(self):
        self.assertEqual(
            poc.index_sql("stn4_multi", "english"),
            "CREATE INDEX idx ON documents USING stannum (title, body)",
        )
        self.assertEqual(
            poc.index_sql("stn3_single", "english"),
            "CREATE INDEX idx ON documents USING stannum (concat)",
        )
        self.assertEqual(
            poc.index_sql("stn4_multi", "chinese"),
            "CREATE INDEX idx ON documents USING stannum (title, body) WITH (tokenizer='jieba')",
        )
        self.assertEqual(
            poc.index_sql("stn3_single", "chinese"),
            "CREATE INDEX idx ON documents USING stannum (concat) WITH (tokenizer='jieba')",
        )
        self.assertEqual(
            poc.index_sql("v040", "chinese"),
            "CREATE INDEX idx ON documents USING stannum (title, body) WITH (tokenizer='jieba')",
        )

    def test_jieba_schema_pins(self):
        block = poc.jieba_schema()
        self.assertEqual(block["jieba_rs_version"], (0 << 16) | (7 << 8) | 4)
        self.assertEqual(block["dict_fingerprint"], 0x6855A0736155F3DD)
        self.assertEqual(block["dict_fingerprint_hex"], "6855a0736155f3dd")

    def test_jieba_stability_hook(self):
        sql = poc.jieba_stability_sql()
        self.assertIn("stannum.tokenize", sql)
        self.assertIn("jieba", sql)
        words = poc.jieba_stability_words()
        self.assertEqual(len(words), 2048)
        self.assertEqual(words[0], "一个")


class GateAndMergeTests(unittest.TestCase):
    def _system(self, dict_bytes, build_s, p50, p99=None):
        return {
            "dict_bytes": dict_bytes,
            "build_s": build_s,
            "queries": {
                "我们": {"p50_s": p50, "p99_s": p50 if p99 is None else p99},
            },
        }

    def test_mandatory_fail_is_escalate_not_stn4(self):
        gate = poc.decide_corpus(self._system(200, 10, 1.0), self._system(100, 10, 1.0))
        self.assertEqual(gate["decision"], "escalate")
        self.assertFalse(gate["mandatory_gate"]["pass"])
        self.assertGreater(gate["mandatory_gate"]["dict_ratio"], 1.8)
        self.assertIsNone(gate["advisory_p50_gate"]["waiver"])

    def test_mandatory_pass_p50_miss_still_stay_without_inventing_waiver(self):
        gate = poc.decide_corpus(self._system(100, 10, 2.0), self._system(100, 10, 1.0))
        self.assertEqual(gate["decision"], "stay")
        self.assertTrue(gate["mandatory_gate"]["pass"])
        self.assertFalse(gate["advisory_p50_gate"]["pass"])
        self.assertIsNone(gate["advisory_p50_gate"]["waiver"])

    def test_both_green_is_stay(self):
        gate = poc.decide_corpus(self._system(100, 10, 1.0, 1.1), self._system(100, 10, 1.0, 1.0))
        self.assertEqual(gate["decision"], "stay")
        self.assertTrue(gate["advisory_p50_gate"]["pass"])
        self.assertAlmostEqual(gate["p99_ratio"], 1.1)

    def test_top_level_escalate_if_either_corpus_misses_mandatory(self):
        green = poc.decide_corpus(self._system(100, 10, 1.0), self._system(100, 10, 1.0))
        red = poc.decide_corpus(self._system(200, 10, 1.0), self._system(100, 10, 1.0))
        self.assertEqual(poc.top_level_decision(green, green), "stay")
        self.assertEqual(poc.top_level_decision(green, red), "escalate")
        self.assertEqual(poc.top_level_decision(red, green), "escalate")

    def _write_fragment(self, root, name, lang, dict_bytes, build_s, p50, p99):
        fragment = {
            "system": name,
            "language": lang,
            "server_version": "17.11",
            "corpus": {
                "language": lang,
                "rows": stn3.ROWS if lang == "english" else poc.ZH_ROWS,
                "seed": stn3.SEED if lang == "english" else poc.ZH_SEED,
                "sha256": poc.ENGLISH_SHA256 if lang == "english" else poc.CHINESE_SHA256,
                "columns": ["id", "title", "body"],
            },
            "dict_bytes": dict_bytes,
            "build_s": build_s,
            "channel_directory_bytes": None,
            "queries": {
                "the" if lang == "english" else "我们": {
                    "p50_s": p50,
                    "p99_s": p99,
                    "samples_s": [p50] * 20,
                }
            },
        }
        if lang == "chinese":
            fragment["jieba"] = poc.jieba_schema()
        path = root / f"{lang}-{name}.json"
        path.write_text(json.dumps(fragment) + "\n")
        return path

    def test_merge_emits_dual_corpus_stay_without_live_server(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            en_multi = self._write_fragment(root, "stn4_multi", "english", 110, 1.0, 0.01, 0.02)
            en_single = self._write_fragment(root, "stn3_single", "english", 100, 1.0, 0.01, 0.02)
            zh_multi = self._write_fragment(root, "stn4_multi", "chinese", 120, 1.1, 0.02, 0.03)
            zh_single = self._write_fragment(root, "stn3_single", "chinese", 100, 1.0, 0.02, 0.03)
            out = root / "stn4-per-field-poc.json"
            poc.main(
                [
                    "merge",
                    "--english-multi",
                    str(en_multi),
                    "--english-single",
                    str(en_single),
                    "--chinese-multi",
                    str(zh_multi),
                    "--chinese-single",
                    str(zh_single),
                    "--out",
                    str(out),
                    "--stn4-sha",
                    "deadbeef",
                    "--topology",
                    "unit-test",
                ]
            )
            result = json.loads(out.read_text())
            self.assertEqual(result["decision"], "stay")
            self.assertEqual(result["corpora"]["english"]["decision"], "stay")
            self.assertEqual(result["corpora"]["chinese"]["decision"], "stay")
            self.assertTrue(result["corpora"]["english"]["mandatory_gate"]["pass"])
            self.assertTrue(result["corpora"]["chinese"]["mandatory_gate"]["pass"])
            self.assertIsNone(result["corpora"]["english"]["advisory_p50_gate"]["waiver"])
            self.assertIsNone(result["corpora"]["chinese"]["advisory_p50_gate"]["waiver"])
            self.assertIn("p99_s", result["corpora"]["english"])
            self.assertIn("p99_ratio", result["corpora"]["english"])
            self.assertIn("p99_s", result["corpora"]["chinese"])
            self.assertEqual(
                result["corpora"]["english"]["corpus"]["sha256"], poc.ENGLISH_SHA256
            )
            self.assertEqual(
                result["corpora"]["chinese"]["corpus"]["sha256"], poc.CHINESE_SHA256
            )
            jieba = result["corpora"]["chinese"]["jieba"]
            self.assertEqual(jieba["jieba_rs_version"], poc.JIEBA_RS_VERSION)
            self.assertEqual(jieba["dict_fingerprint"], poc.JIEBA_EMPTY_DICT_FINGERPRINT)
            self.assertEqual(
                result["comparison_4_6"]["stn3_fielded"]["labeled"], "3.34×/1.95×/5.61×"
            )
            self.assertEqual(result["comparison_4_6"]["english_sha256"], poc.ENGLISH_SHA256)
            self.assertAlmostEqual(result["comparison_4_6"]["stn4_multi"]["dict_ratio"], 1.1)
            self.assertIsNone(result["corpora"]["english"]["systems"]["stn4_multi"]["channel_directory_bytes"])
            self.assertNotIn("jieba", result["corpora"]["english"])

    def test_merge_escalates_when_one_corpus_misses_mandatory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            en_multi = self._write_fragment(root, "stn4_multi", "english", 200, 1.0, 0.01, 0.02)
            en_single = self._write_fragment(root, "stn3_single", "english", 100, 1.0, 0.01, 0.02)
            zh_multi = self._write_fragment(root, "stn4_multi", "chinese", 110, 1.0, 0.01, 0.02)
            zh_single = self._write_fragment(root, "stn3_single", "chinese", 100, 1.0, 0.01, 0.02)
            out = root / "stn4-per-field-poc.json"
            poc.main(
                [
                    "merge",
                    "--english-multi",
                    str(en_multi),
                    "--english-single",
                    str(en_single),
                    "--chinese-multi",
                    str(zh_multi),
                    "--chinese-single",
                    str(zh_single),
                    "--out",
                    str(out),
                ]
            )
            result = json.loads(out.read_text())
            self.assertEqual(result["decision"], "escalate")
            self.assertEqual(result["corpora"]["english"]["decision"], "escalate")
            self.assertEqual(result["corpora"]["chinese"]["decision"], "stay")
            self.assertFalse(result["corpora"]["english"]["mandatory_gate"]["pass"])

    def test_merge_pg18_remeasurement_preserves_pg17_decision(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            en_multi = self._write_fragment(root, "stn4_multi", "english", 110, 1.0, 0.01, 0.02)
            en_single = self._write_fragment(root, "stn3_single", "english", 100, 1.0, 0.01, 0.02)
            zh_multi = self._write_fragment(root, "stn4_multi", "chinese", 120, 1.1, 0.02, 0.03)
            zh_single = self._write_fragment(root, "stn3_single", "chinese", 100, 1.0, 0.02, 0.03)
            existing = root / "stn4-per-field-poc.json"
            existing.write_text(
                json.dumps(
                    {
                        "decision": "escalate",
                        "corpora": {"english": {"marker": "pg17"}},
                        "protocol": {"pg": "17.11 (Homebrew)"},
                    }
                )
                + "\n"
            )
            profile = root / "profile.json"
            profile.write_text(json.dumps({"n_pairs": 12, "median_s": 1.5}) + "\n")
            out = root / "out.json"
            poc.main(
                [
                    "merge",
                    "--english-multi",
                    str(en_multi),
                    "--english-single",
                    str(en_single),
                    "--chinese-multi",
                    str(zh_multi),
                    "--chinese-single",
                    str(zh_single),
                    "--pg18-remeasurement",
                    "--existing",
                    str(existing),
                    "--profile",
                    str(profile),
                    "--out",
                    str(out),
                    "--pg-version",
                    "18.4",
                    "--stn4-sha",
                    "deadbeef",
                    "--topology",
                    "unit-test-pg18",
                ]
            )
            result = json.loads(out.read_text())
            self.assertEqual(result["decision"], "escalate")
            self.assertEqual(result["corpora"], {"english": {"marker": "pg17"}})
            self.assertEqual(result["protocol"]["pg"], "17.11 (Homebrew)")
            self.assertEqual(result["superseded_by"], "pg18_remeasurement.decision_pg18")
            pg18 = result["pg18_remeasurement"]
            self.assertEqual(pg18["engine"], "18.4")
            self.assertEqual(pg18["decision_pg18"], "stay")
            self.assertTrue(pg18["corpora"]["english"]["mandatory_gate"]["pass"])
            self.assertTrue(pg18["corpora"]["chinese"]["mandatory_gate"]["pass"])
            self.assertEqual(pg18["paired_build_profile"]["n_pairs"], 12)
            self.assertEqual(pg18["protocol"]["server_topology"], "unit-test-pg18")
            self.assertEqual(pg18["git"]["stn4"], "deadbeef")

    def test_merge_pg18_clean_baseline_preserves_remeasurement(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            en_multi = self._write_fragment(root, "stn4_multi", "english", 110, 2.0, 0.01, 0.02)
            en_single = self._write_fragment(root, "stn3_single", "english", 100, 1.0, 0.01, 0.02)
            zh_multi = self._write_fragment(root, "stn4_multi", "chinese", 120, 1.1, 0.02, 0.03)
            zh_single = self._write_fragment(root, "stn3_single", "chinese", 100, 1.0, 0.02, 0.03)
            existing = root / "stn4-per-field-poc.json"
            existing.write_text(
                json.dumps(
                    {
                        "decision": "escalate",
                        "corpora": {"english": {"marker": "pg17"}},
                        "protocol": {"pg": "17.11 (Homebrew)"},
                        "pg18_remeasurement": {
                            "engine": "18.4",
                            "decision_pg18": "stay",
                            "marker": "do-not-overwrite",
                        },
                    }
                )
                + "\n"
            )
            environment = root / "environment.json"
            environment.write_text(
                json.dumps({"loadavg_start": [1.0, 1.0, 1.0], "stopped_processes": []}) + "\n"
            )
            out = root / "out.json"
            poc.main(
                [
                    "merge",
                    "--english-multi",
                    str(en_multi),
                    "--english-single",
                    str(en_single),
                    "--chinese-multi",
                    str(zh_multi),
                    "--chinese-single",
                    str(zh_single),
                    "--pg18-clean-baseline",
                    "--existing",
                    str(existing),
                    "--environment",
                    str(environment),
                    "--out",
                    str(out),
                    "--pg-version",
                    "18.4",
                    "--stn4-sha",
                    "cafebabe",
                    "--topology",
                    "unit-test-clean",
                ]
            )
            result = json.loads(out.read_text())
            self.assertEqual(result["decision"], "escalate")
            self.assertEqual(result["corpora"], {"english": {"marker": "pg17"}})
            self.assertEqual(result["pg18_remeasurement"]["marker"], "do-not-overwrite")
            self.assertEqual(result["pg18_remeasurement"]["decision_pg18"], "stay")
            clean = result["pg18_clean_baseline"]
            self.assertEqual(clean["engine"], "18.4")
            self.assertEqual(clean["decision_clean"], "escalate")
            self.assertTrue(clean["english_oneshot_build_exceeds_1_8"])
            self.assertFalse(clean["corpora"]["english"]["mandatory_gate"]["pass"])
            self.assertTrue(clean["corpora"]["chinese"]["mandatory_gate"]["pass"])
            self.assertEqual(clean["protocol"]["server_topology"], "unit-test-clean")
            self.assertEqual(clean["git"]["stn4"], "cafebabe")
            self.assertEqual(clean["environment"]["loadavg_start"], [1.0, 1.0, 1.0])

    def test_generate_cli_prints_locked_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            en = root / "en.csv"
            zh = root / "zh.csv"
            self.assertEqual(poc.main(["generate", "--lang", "english", "--out", str(en)]), 0)
            self.assertEqual(poc.main(["generate", "--lang", "chinese", "--out", str(zh)]), 0)
            self.assertEqual(en.read_bytes()[:20], b'"id","title","body"\n')
            self.assertEqual(zh.read_bytes()[:20], b'"id","title","body"\n')


class FourSixRegressionTests(unittest.TestCase):
    def test_four_six_mandatory_fail_is_still_stn4(self):
        def system(dict_bytes, build_s, p50):
            return {
                "dict_bytes": dict_bytes,
                "build_s": build_s,
                "queries": {"the": {"p50_s": p50}},
            }

        stn4 = stn3.decide(system(200, 10, 1.0), system(100, 10, 1.0))
        self.assertEqual(stn4["decision"], "STN4")

    def test_four_six_merge_still_defers_chinese_corpus(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fragment = {
                "system": "stn3_single",
                "server_version": "17.11",
                "corpus": {
                    "rows": stn3.ROWS,
                    "seed": stn3.SEED,
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
            stn3.main(
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
            self.assertEqual(result["chinese_corpus"], "deferred")
            self.assertEqual(result["decision"], "fielded-terms")


if __name__ == "__main__":
    unittest.main()
