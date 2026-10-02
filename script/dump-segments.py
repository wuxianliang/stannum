#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Dump every immutable segment blob of a current Stannum index to files.

Reads the index's relation file directly (after a CHECKPOINT), so it needs
filesystem access to the data directory: the pgrx development server runs as
the developer, which is the intended use. The blobs feed the `segment` crate's
`breakdown` example:

    script/dump-segments.py --dbname mydb --index documents_body_idx --out /tmp/blobs
    cargo run -p segment --release --example breakdown -- /tmp/blobs/*.segment

This script dumps **current** (0.5.0) directories only:

* kind 5 (`KIND_ENVELOPE`): meta image, then one framed STNM record
* kind 1 (`KIND_META`): only when the image uses the current 16-byte Run /
  68-byte entry stride (not the 0.4.0 52-byte directory)

It writes each directory entry's **segment run**, never the map or dead-list
runs. 0.4.0 indexes stay on the main-worktree copy of this script.

Connection settings come from the usual PG* environment variables; --dbname
defaults to PGDATABASE.
"""
import argparse
import struct
from collections import namedtuple
from pathlib import Path
import subprocess

PAGE_SIZE = 8192
PAGE_HEADER = 24
SPECIAL_SIZE = 8
MAGIC = 0x4C445032
VERSION = 2
KIND_META = 1
KIND_RUN = 3
KIND_ENVELOPE = 5
SPEC_BYTES = 8
NONE = 0xFFFFFFFF
FILE_BLOCKS = (1 << 30) // PAGE_SIZE
RUN_BYTES = 16
ENTRY_BYTES = RUN_BYTES * 3 + 4 + 4 + 8 + 4  # 68; layout.rs SegmentEntry
PENDING_BYTES = RUN_BYTES + 4
LEGACY_ENTRY_BYTES = 52
LEGACY_PENDING_BYTES = 16
META_HEADER = 8 + SPEC_BYTES + 28 + 4 + 4 + 4
MAX_SEGMENTS = 96
MAX_PENDING = 48
STNM_MAGIC = b"STNM"

Run = namedtuple("Run", "first blocks nbytes last")
SegmentEntry = namedtuple(
    "SegmentEntry", "run map dead dead_stamp docs total_length generation"
)
MetaHeader = namedtuple(
    "MetaHeader",
    "identity spec buffer next_generation segment_count pending_count dir_at",
)


def psql(dbname, sql, **variables):
    """Run one statement; psql quotes the :'name' variables it references."""
    command = ["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1"]
    if dbname:
        command += ["-d", dbname]
    for name, value in variables.items():
        command += ["-v", f"{name}={value}"]
    result = subprocess.run(command, input=sql, check=True, text=True, capture_output=True)
    return result.stdout.strip()


def inspect_page(page):
    """Return (kind, payload) for one LDP2 page image."""
    if len(page) != PAGE_SIZE:
        raise ValueError(f"invalid Stannum page size ({len(page)})")
    lower, = struct.unpack_from("<H", page, 12)
    special = PAGE_SIZE - SPECIAL_SIZE
    magic, kind, version = struct.unpack_from("<IBB", page, special)
    if magic != MAGIC or version != VERSION:
        raise ValueError("not an LDP2 page")
    if lower < PAGE_HEADER or lower > special:
        raise ValueError("invalid Stannum page bounds")
    return kind, page[PAGE_HEADER:lower]


def parse_meta_header(payload):
    """Parse the current meta-image header. `dir_at` is the first directory byte."""
    if len(payload) < META_HEADER:
        raise ValueError("truncated Stannum meta page")
    identity, = struct.unpack_from("<Q", payload, 0)
    spec = payload[8:8 + SPEC_BYTES]
    buffer = struct.unpack_from("<IIIIIII", payload, 8 + SPEC_BYTES)
    at = 8 + SPEC_BYTES + 28
    next_generation, segment_count, pending_count = struct.unpack_from("<III", payload, at)
    at += 12
    if segment_count > MAX_SEGMENTS or pending_count > MAX_PENDING:
        raise ValueError("invalid Stannum meta page")
    return MetaHeader(
        identity, spec, buffer, next_generation, segment_count, pending_count, at
    )


def current_image_len(header):
    return header.dir_at + header.segment_count * ENTRY_BYTES + header.pending_count * PENDING_BYTES


def legacy_image_len(header):
    return (
        header.dir_at
        + header.segment_count * LEGACY_ENTRY_BYTES
        + header.pending_count * LEGACY_PENDING_BYTES
    )


def get_run(payload, at):
    first, blocks, nbytes, last = struct.unpack_from("<IIII", payload, at)
    return Run(first, blocks, nbytes, last)


def parse_current_directory(payload, header, validate_entries=True):
    """Parse 16-byte runs and 68-byte segment entries. Does not read map/dead blobs."""
    image_len = current_image_len(header)
    if len(payload) < image_len:
        raise ValueError("invalid Stannum meta page")
    at = header.dir_at
    segments = []
    for _ in range(header.segment_count):
        run = get_run(payload, at)
        mapped = get_run(payload, at + RUN_BYTES)
        dead = get_run(payload, at + 2 * RUN_BYTES)
        dead_stamp, docs = struct.unpack_from("<II", payload, at + 3 * RUN_BYTES)
        total_length, generation = struct.unpack_from("<QI", payload, at + 3 * RUN_BYTES + 8)
        entry = SegmentEntry(run, mapped, dead, dead_stamp, docs, total_length, generation)
        if validate_entries and (entry.run.first == NONE or entry.run.blocks == 0):
            raise ValueError("invalid Stannum segment entry")
        segments.append(entry)
        at += ENTRY_BYTES
    return segments


def entries_from_meta(kind, payload):
    """Directory of a current kind-5 envelope or current-stride kind-1 page.

    Raises ValueError for 0.4.0 52-byte directories and other unreadable pages.
    """
    header = parse_meta_header(payload)
    current_len = current_image_len(header)
    legacy_len = legacy_image_len(header)
    if kind == KIND_ENVELOPE:
        if len(payload) < current_len:
            raise ValueError("invalid Stannum meta page")
        rest = payload[current_len:]
        if len(rest) < 5 or rest[:4] != STNM_MAGIC:
            raise ValueError("kind-5 page is missing the STNM record")
        return parse_current_directory(payload, header)
    if kind == KIND_META:
        if len(payload) >= current_len:
            return parse_current_directory(payload, header)
        if len(payload) == legacy_len:
            raise ValueError(
                "0.4.0 52-byte directory; use the main-worktree dump-segments.py"
            )
        raise ValueError("invalid Stannum meta page")
    raise ValueError(f"block 0 is not a meta/envelope page (kind {kind})")


class Relation:
    def __init__(self, path):
        self.path = Path(path)

    def page(self, block):
        suffix = "" if block < FILE_BLOCKS else f".{block // FILE_BLOCKS}"
        with open(str(self.path) + suffix, "rb") as file:
            file.seek((block % FILE_BLOCKS) * PAGE_SIZE)
            page = file.read(PAGE_SIZE)
        if len(page) != PAGE_SIZE:
            raise SystemExit(f"block {block}: short read")
        try:
            return inspect_page(page)
        except ValueError as error:
            raise SystemExit(f"block {block}: {error}") from error

    def run(self, first, blocks, nbytes):
        out = bytearray()
        block = first
        for _ in range(blocks):
            if block == NONE:
                raise SystemExit("run chain ends early")
            kind, payload = self.page(block)
            if kind != KIND_RUN:
                raise SystemExit(f"block {block}: kind {kind} is not a run page")
            block, = struct.unpack_from("<I", payload, 0)
            out += payload[4:4 + min(len(payload) - 4, nbytes - len(out))]
        if len(out) != nbytes:
            raise SystemExit(f"run: {len(out)} of {nbytes} bytes")
        return bytes(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--dbname", help="database name (default: PGDATABASE)")
    parser.add_argument("--index", required=True, help="index name, schema-qualified if not on search_path")
    parser.add_argument("--out", required=True, help="directory for gen<N>.segment files")
    parser.add_argument("--data-directory", help="the server's data directory as this host sees it, "
                        "for a server in a container whose directory is bind-mounted")
    args = parser.parse_args()
    psql(args.dbname, "CHECKPOINT")
    data_directory = args.data_directory or psql(args.dbname, "SHOW data_directory")
    relative = psql(args.dbname, "SELECT pg_relation_filepath(:'index'::regclass)", index=args.index)
    relation = Relation(Path(data_directory) / relative)
    kind, meta = relation.page(0)
    try:
        entries = entries_from_meta(kind, meta)
    except ValueError as error:
        raise SystemExit(str(error)) from error
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    total = 0
    for entry in entries:
        blob = relation.run(entry.run.first, entry.run.blocks, entry.run.nbytes)
        path = out / f"gen{entry.generation}.segment"
        path.write_bytes(blob)
        total += len(blob)
        print(f"{path}: {len(blob)} bytes in {entry.run.blocks} pages, {entry.docs} documents, "
              f"{entry.total_length} tokens, {blob[:4].decode('ascii', 'replace')}")
    print(f"{len(entries)} segments, {total} bytes")


if __name__ == "__main__":
    main()
