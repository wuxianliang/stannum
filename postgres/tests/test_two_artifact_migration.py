#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Unit tests for E.1 two-artifact codecs and pinned error strings. No live PostgreSQL."""
import importlib.util
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
HELPER = Path(__file__).resolve().parent / "two_artifact_migration.py"


def load():
    spec = importlib.util.spec_from_file_location("two_artifact_migration", HELPER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


m = load()


class SourcePins(unittest.TestCase):
    def test_four_strings_are_in_production_sources(self):
        self.assertTrue(m.check_source_pins(ROOT))

    def test_fixture_table_is_the_design_matrix(self):
        self.assertIn("old_single", m.FIXTURE_ROWS)
        self.assertIn("kind5_lsg", m.FIXTURE_ROWS)
        self.assertIn("untagged_zero_insert", m.FIXTURE_ROWS)
        self.assertEqual(len(set(m.FIXTURE_ROWS)), len(m.FIXTURE_ROWS))


class Codecs(unittest.TestCase):
    def test_untagged_forward_record_is_not_the_stn4_tag(self):
        stream = m.untagged_legacy_stream()
        self.assertFalse(stream.startswith(m.STN4_BUFFER_TAG))
        self.assertGreater(stream[0], 0)
        zero = m.untagged_zero_term()
        self.assertFalse(zero.startswith(m.STN4_BUFFER_TAG))

    def test_tagged_stream_starts_with_the_discriminator(self):
        stream = m.tagged_stream(((0, 1, (("~0~foo", [1]),)),))
        self.assertTrue(stream.startswith(m.STN4_BUFFER_TAG))
        self.assertIn(b"~0~foo", stream)

    def test_demote_v1_keeps_stock_bytes_and_writes_stnf_v1(self):
        header = bytearray(b"STN3")
        for _ in range(6):
            header.append(0)
        crc = __import__("zlib").crc32(b"") & 0xFFFFFFFF
        norms = bytes([2]) + (0).to_bytes(8, "little") * 2 + crc.to_bytes(4, "little")
        trailer = b"STNF" + bytes([2]) + len(norms).to_bytes(4, "little") + norms
        blob = bytes(header) + trailer
        self.assertEqual(m.stn3_pages_end(blob), len(header))
        v1 = m.demote_v1(blob)
        self.assertEqual(v1[: len(header)], bytes(header))
        self.assertEqual(v1[len(header) : len(header) + 5], b"STNF\x01")
        self.assertEqual(m.strip_trailer(blob), bytes(header))

    def test_break_fch1_requires_a_marker(self):
        with self.assertRaises(ValueError):
            m.break_fch1(b"STN3" + b"\x00" * 20)
        broken = m.break_fch1(b"xxxxFCH1yyyy")
        self.assertEqual(broken[4:8], b"XCH1")


if __name__ == "__main__":
    unittest.main()
