# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Unit tests for current kind-5 / current-stride dump-segments parsers.

No live PostgreSQL: these encode the layout.rs directory in memory so Item 2
can smoke a real kind-5 relation with the same helpers.
"""
import importlib.util
from pathlib import Path
import struct
import unittest

HELPER = Path(__file__).resolve().parent / "dump-segments.py"


def load_dump_segments():
    spec = importlib.util.spec_from_file_location("dump_segments", HELPER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ds = load_dump_segments()
NONE = ds.NONE


def put_run(first, blocks, nbytes, last):
    return struct.pack("<IIII", first, blocks, nbytes, last)


def empty_run():
    return put_run(NONE, 0, 0, NONE)


def encode_header(identity, spec, buffer, next_generation, nseg, npend):
    out = bytearray()
    out += struct.pack("<Q", identity)
    out += bytes(spec)
    out += struct.pack("<IIIIIII", *buffer)
    out += struct.pack("<III", next_generation, nseg, npend)
    return bytes(out)


def encode_entry(run, mapped, dead, dead_stamp, docs, total_length, generation):
    return (
        run
        + mapped
        + dead
        + struct.pack("<IIQI", dead_stamp, docs, total_length, generation)
    )


def sample_image():
    """Directory matching layout.rs sample_meta (without STNM)."""
    header = encode_header(
        0x1234_5678_9ABC_DEF0,
        [0, 1, 1, 2, 0, 1, 1, 1],
        (5, 2, 1, 7, 300, 40_000, 12),
        3,
        2,
        1,
    )
    entry0 = encode_entry(
        put_run(2, 5, 40_123, 6),
        put_run(12, 1, 20, 12),
        empty_run(),
        0,
        100,
        12_345,
        1,
    )
    entry1 = encode_entry(
        put_run(9, 1, 12, 9),
        empty_run(),
        put_run(10, 1, 5, 10),
        7,
        1,
        2,
        2,
    )
    pending = put_run(11, 2, 9_000, 12) + struct.pack("<I", 77)
    image = header + entry0 + entry1 + pending
    assert len(entry0) == ds.ENTRY_BYTES
    assert len(entry1) == ds.ENTRY_BYTES
    return image


def flag0_stnm():
    body = bytearray()
    body.append(1)
    body += struct.pack("<H", 4)
    body += b"expr"
    body += struct.pack("<f", 1.0)
    body.append(0)
    return b"STNM" + bytes([1]) + struct.pack("<I", len(body)) + bytes(body)


def encode_page(kind, payload):
    page = bytearray(ds.PAGE_SIZE)
    page[ds.PAGE_HEADER:ds.PAGE_HEADER + len(payload)] = payload
    lower = ds.PAGE_HEADER + len(payload)
    special = ds.PAGE_SIZE - ds.SPECIAL_SIZE
    struct.pack_into("<H", page, 12, lower)
    struct.pack_into("<H", page, 16, special)
    struct.pack_into("<H", page, 18, special)
    struct.pack_into("<I", page, special, ds.MAGIC)
    page[special + 4] = kind
    page[special + 5] = ds.VERSION
    return bytes(page)


class CurrentDirectoryTests(unittest.TestCase):
    def test_kind5_envelope_dumps_segment_runs_not_map_or_dead(self):
        image = sample_image()
        payload = image + flag0_stnm()
        entries = ds.entries_from_meta(ds.KIND_ENVELOPE, payload)
        self.assertEqual(len(entries), 2)
        self.assertEqual(entries[0].run, ds.Run(2, 5, 40_123, 6))
        self.assertEqual(entries[0].map.first, 12)
        self.assertEqual(entries[0].dead.first, NONE)
        self.assertEqual(entries[0].docs, 100)
        self.assertEqual(entries[0].total_length, 12_345)
        self.assertEqual(entries[0].generation, 1)
        self.assertEqual(entries[1].run, ds.Run(9, 1, 12, 9))
        self.assertEqual(entries[1].dead.first, 10)
        self.assertEqual(entries[1].dead_stamp, 7)
        self.assertEqual(entries[1].generation, 2)

    def test_kind5_page_inspect_then_parse(self):
        payload = sample_image() + flag0_stnm()
        page = encode_page(ds.KIND_ENVELOPE, payload)
        kind, got = ds.inspect_page(page)
        self.assertEqual(kind, ds.KIND_ENVELOPE)
        self.assertEqual(got, payload)
        self.assertEqual(ds.entries_from_meta(kind, got)[0].run.first, 2)

    def test_kind1_current_stride_is_accepted(self):
        image = sample_image()
        entries = ds.entries_from_meta(ds.KIND_META, image)
        self.assertEqual([e.generation for e in entries], [1, 2])
        self.assertEqual(entries[0].run.nbytes, 40_123)

    def test_kind1_legacy_52_byte_entry_is_refused(self):
        header = encode_header(1, [0] * 8, (0, 0, 1, 1, 0, 0, 0), 1, 1, 0)
        payload = header + bytes(ds.LEGACY_ENTRY_BYTES)
        self.assertEqual(len(payload), ds.legacy_image_len(ds.parse_meta_header(payload)))
        self.assertLess(len(payload), ds.current_image_len(ds.parse_meta_header(payload)))
        with self.assertRaisesRegex(ValueError, "0.4.0 52-byte directory"):
            ds.entries_from_meta(ds.KIND_META, payload)

    def test_kind5_without_stnm_is_refused(self):
        with self.assertRaisesRegex(ValueError, "STNM"):
            ds.entries_from_meta(ds.KIND_ENVELOPE, sample_image())

    def test_empty_current_directory(self):
        header = encode_header(1, [0] * 8, (0, 0, 1, 1, 0, 0, 0), 1, 0, 0)
        self.assertEqual(ds.current_image_len(ds.parse_meta_header(header)), ds.META_HEADER)
        self.assertEqual(ds.entries_from_meta(ds.KIND_META, header), [])
        envelope = header + flag0_stnm()
        self.assertEqual(ds.entries_from_meta(ds.KIND_ENVELOPE, envelope), [])

    def test_constants_match_layout_rs(self):
        self.assertEqual(ds.KIND_ENVELOPE, 5)
        self.assertEqual(ds.RUN_BYTES, 16)
        self.assertEqual(ds.ENTRY_BYTES, 68)
        self.assertEqual(ds.SPEC_BYTES, 8)
        self.assertEqual(ds.META_HEADER, 56)


if __name__ == "__main__":
    unittest.main()
