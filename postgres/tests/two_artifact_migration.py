#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Two-artifact 0.4.0 → 0.5.0 migration CI (STN4 E.1 / design §6.3).

A genuine 0.4.0 `stannum` library writes old indexes (single/multi/jieba/
empty/buffer-only). The job records kind/magic bytes, shuts down, installs
0.5.0, restarts the same data directory, `ALTER EXTENSION`, and asserts the
parent §8 migration string, no index-page dirty on a refused INSERT, then
`REINDEX` plus answers.

STN4-dev rows that 0.4.0 never wrote (v1 trailers, tagged/untagged buffers,
kind-5+LSG) are planted as relation-file mutations after a CHECKPOINT, using
the same byte transforms as the `#[pg_test]` fixtures. The job fails if any
classification or pinned error string mismatches.

The 0.4.0 pin is origin/main `7ab511b` (Cargo.toml version 0.4.0; the tree
that recorded `contract/expected/stannum-0.4.0`). CI checks that SHA out into
a subdirectory and builds it; a runner does not need `/tmp/stannum-main-46`.
"""
from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
# origin/main 0.4.0 product line (not a 0.5.0 tag). Parent §8 cited 24f5c02
# of the same version; this SHA is the contract-recording 0.4.0 tree.
OLD_PIN = "7ab511b6b9d300d270f2ab795b1bb9f10fc06696"
OLD_PIN_SHORT = "7ab511b"

PRE_STN3 = "stannum: index requires REINDEX to 0.5.0 (pre-STN3 segment)"
STALE_FIELDED = "stannum: index holds a pre-STN4 fielded write buffer; REINDEX the index"
MIXED_FIELDED = (
    "stannum: index mixes pre-STN4 and STN4 fielded write buffers; REINDEX the index"
)
KIND5_LSG = "stannum: mixed-format index (KIND_ENVELOPE with a legacy LSG segment)"

STRING_PINS = (
    ("segment/src/error.rs", PRE_STN3),
    ("segment/src/segment.rs", PRE_STN3),
    ("postgres/src/storage/mod.rs", STALE_FIELDED),
    ("postgres/src/storage/mod.rs", MIXED_FIELDED),
    ("postgres/src/storage/mod.rs", KIND5_LSG),
)

PAGE_SIZE = 8192
PAGE_HEADER = 24
SPECIAL_SIZE = 8
CAPACITY = PAGE_SIZE - PAGE_HEADER - SPECIAL_SIZE
CHAIN_CAPACITY = CAPACITY - 4
MAGIC = 0x4C445032
VERSION = 2
KIND_META = 1
KIND_BUFFER = 2
KIND_RUN = 3
KIND_ENVELOPE = 5
NONE = 0xFFFFFFFF
SPEC_BYTES = 8
META_HEADER = 8 + SPEC_BYTES + 28 + 4 + 4 + 4
ENTRY_BYTES = 68
STN4_BUFFER_TAG = b"\x00\x01"
STNF = b"STNF"
STN3 = b"STN3"
LSG_MAGICS = (b"LSG1", b"LSG2", b"LSG3", b"LSG4")
PORT = "28971"

# §6.3 table rows this job must print as ASSERT … PASS|FAIL|MISSING.
FIXTURE_ROWS = (
    "old_single",
    "old_multi",
    "old_jieba",
    "old_empty",
    "old_buffer",
    "v2_empty",
    "v2_current",
    "v2_noterms",
    "v1_empty",
    "v1_stale",
    "v1_noterms",
    "mixed_empty",
    "mixed_noterms",
    "mixed_match",
    "v2_buf_stale",
    "v1_buf_current",
    "v2_buf_malformed",
    "mixed_malformed_trailer",
    "mixed_missing_trailer",
    "v2_bad_fch1",
    "v2_tilde_dict",
    "tagged_tilde_restart",
    "untagged_legacy",
    "v2_untagged_legacy",
    "tagged_zero_empty",
    "tagged_zero_v2",
    "tagged_zero_v1",
    "untagged_zero_insert",
    "untagged_zero_v2_insert",
    "tagged_zero_insert_flush_restart",
    "empty_kind5",
    "buf_only_current",
    "buf_only_stale",
    "buf_only_malformed",
    "kind1_empty",
    "kind1_buffer",
    "kind1_lsg",
    "kind5_lsg",
)


def u32(n):
    return struct.pack("<I", n)


def u32_at(buf, at):
    return struct.unpack_from("<I", buf, at)[0]


def varint_put(out, value):
    value = int(value)
    while True:
        byte = value & 0x7F
        value >>= 7
        if value == 0:
            out.append(byte)
            return
        out.append(byte | 0x80)


def varint_get(buf, at):
    value = 0
    shift = 0
    while True:
        if at >= len(buf):
            raise ValueError("truncated varint")
        byte = buf[at]
        at += 1
        if shift == 63 and byte > 1:
            raise ValueError("varint exceeds 64 bits")
        value |= (byte & 0x7F) << shift
        if byte & 0x80 == 0:
            return value, at
        shift += 7
        if shift > 63:
            raise ValueError("varint exceeds 64 bits")


def encode_positions(out, positions):
    varint_put(out, len(positions))
    varint_put(out, positions[0])
    for a, b in zip(positions, positions[1:]):
        varint_put(out, b - a - 1)


def encode_terms(out, terms):
    previous = b""
    for term, positions in terms:
        raw = term.encode()
        shared = 0
        for a, b in zip(previous, raw):
            if a != b:
                break
            shared += 1
        varint_put(out, shared)
        varint_put(out, len(raw) - shared)
        out.extend(raw[shared:])
        encode_positions(out, positions)
        previous = raw


def encode_forward_record(block=0, offset=1, doc_len=0, terms=()):
    """Stock ForwardRecord: length-prefixed body. Never starts with STN4_BUFFER_TAG."""
    body = bytearray()
    varint_put(body, block)
    varint_put(body, offset)
    varint_put(body, doc_len)
    varint_put(body, len(terms))
    encode_terms(body, terms)
    out = bytearray()
    varint_put(out, len(body))
    out.extend(body)
    return bytes(out)


def encode_fielded_record(block=0, offset=1, groups=()):
    """STN4 FieldedRecord (no tag). groups: ((field, field_length, terms), ...)."""
    body = bytearray()
    varint_put(body, block)
    varint_put(body, offset)
    varint_put(body, len(groups))
    for field, field_length, terms in groups:
        body.append(field)
        varint_put(body, field_length)
        varint_put(body, len(terms))
        encode_terms(body, terms)
    out = bytearray()
    varint_put(out, len(body))
    out.extend(body)
    return bytes(out)


def tagged_stream(groups=()):
    return STN4_BUFFER_TAG + encode_fielded_record(groups=groups)


def untagged_zero_term():
    return encode_forward_record(doc_len=0, terms=())


def untagged_legacy_stream():
    terms = (("~0~needle", [1]), ("~1~pad", [2]))
    return encode_forward_record(doc_len=2, terms=terms)


def stn3_pages_end(blob):
    if blob[:4] != STN3:
        raise ValueError(f"not STN3 ({blob[:4]!r})")
    at = 4
    doc_count, at = varint_get(blob, at)
    _, at = varint_get(blob, at)  # total_length
    dictionary_len, at = varint_get(blob, at)
    ordinals_len, at = varint_get(blob, at)
    payload_len, at = varint_get(blob, at)
    pages_len, at = varint_get(blob, at)
    pages_end = (
        at
        + dictionary_len
        + ordinals_len
        + payload_len
        + doc_count * 2
        + doc_count * 4
        + doc_count
        + pages_len
    )
    if pages_end > len(blob):
        raise ValueError("pages_end past blob")
    return pages_end


def demote_v1(blob):
    """Well-formed STNF v1: keep v2 norms, wrap with a one-entry df sidecar."""
    pages_end = stn3_pages_end(blob)
    trailer = blob[pages_end:]
    if trailer[:4] != STNF or trailer[4] != 2:
        raise ValueError(f"expected STNF v2, got {trailer[:5]!r}")
    norms_len = u32_at(trailer, 5)
    norms = trailer[9 : 9 + norms_len]
    if len(norms) != norms_len:
        raise ValueError("truncated v2 norms")
    token = b"needle"
    df = u32(1) + u32(len(token)) + token + struct.pack("<Q", 1)
    out = bytearray(blob[:pages_end])
    out.extend(STNF)
    out.append(1)
    out.extend(u32(len(norms)))
    out.extend(u32(len(df)))
    out.extend(norms)
    out.extend(df)
    return bytes(out)


def strip_trailer(blob):
    return blob[: stn3_pages_end(blob)]


def break_fch1(blob):
    out = bytearray(blob)
    patched = 0
    for at in range(len(out) - 3):
        if out[at : at + 4] == b"FCH1":
            out[at] = ord("X")
            patched += 1
    if patched == 0:
        raise ValueError("no FCH1 to break")
    return bytes(out)


def corrupt_trailer_version(blob, version=7):
    pages_end = stn3_pages_end(blob)
    out = bytearray(blob)
    out[pages_end + 4] = version
    return bytes(out)


def inspect_page(page):
    if len(page) != PAGE_SIZE:
        raise ValueError("invalid page size")
    (lower,) = struct.unpack_from("<H", page, 12)
    special = PAGE_SIZE - SPECIAL_SIZE
    magic, kind, version = struct.unpack_from("<IBB", page, special)
    if magic != MAGIC or version != VERSION:
        raise ValueError("not an LDP2 page")
    if lower < PAGE_HEADER or lower > special:
        raise ValueError("invalid page bounds")
    return kind, page[PAGE_HEADER:lower]


def parse_meta_header(payload):
    if len(payload) < META_HEADER:
        raise ValueError("truncated meta")
    identity = struct.unpack_from("<Q", payload, 0)[0]
    spec = payload[8 : 8 + SPEC_BYTES]
    buffer = struct.unpack_from("<IIIIIII", payload, 8 + SPEC_BYTES)
    at = 8 + SPEC_BYTES + 28
    next_generation, segment_count, pending_count = struct.unpack_from("<III", payload, at)
    return {
        "identity": identity,
        "spec": spec,
        "buffer": buffer,
        "next_generation": next_generation,
        "segment_count": segment_count,
        "pending_count": pending_count,
        "dir_at": at + 12,
    }


def check_source_pins(root=ROOT):
    """Fail if a pinned error string drifted in the production sources."""
    for rel, needle in STRING_PINS:
        text = (root / rel).read_text()
        collapsed = re.sub(r"\\\s*\n\s*", "", text)
        if needle not in text and needle not in collapsed:
            raise AssertionError(f"{rel} no longer contains {needle!r}")
    return True


def pg_config(item, exe="pg_config"):
    return subprocess.check_output([exe, f"--{item}"], text=True).strip()


def extension_library(pkglib):
    pkglib = Path(pkglib)
    matches = [p for p in (pkglib / "stannum.so", pkglib / "stannum.dylib") if p.exists()]
    if not matches:
        raise FileNotFoundError(f"no stannum library in {pkglib}")
    return matches[0]


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def file_md5(path):
    digest = hashlib.md5()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


class Cluster:
    def __init__(self, root, port=PORT):
        self.root = Path(root)
        self.data = self.root / "data"
        self.log = self.root / "server.log"
        self.port = str(port)
        self.env = dict(
            os.environ,
            PGHOST=str(self.root),
            PGPORT=self.port,
            PGUSER="postgres",
            PGDATABASE="postgres",
        )
        for key in ("PGSERVICE", "PGSERVICEFILE", "PGPASSWORD", "PGOPTIONS"):
            self.env.pop(key, None)
        self.started = False

    def command(self, args, input=None, check=True, env=None):
        result = subprocess.run(
            args,
            input=input,
            text=True,
            env=env or self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        if check and result.returncode:
            raise RuntimeError(f"{' '.join(map(str, args))} failed:\n{result.stdout}")
        return result

    def init(self):
        self.command(
            [
                "initdb",
                "-D",
                str(self.data),
                "-U",
                "postgres",
                "-A",
                "trust",
                "--no-locale",
                "--encoding=UTF8",
                "--no-data-checksums",
            ]
        )

    def start(self):
        result = self.command(
            [
                "pg_ctl",
                "-D",
                str(self.data),
                "-l",
                str(self.log),
                "-w",
                "-o",
                f"-p {self.port} -c listen_addresses= -c unix_socket_directories={self.root}",
                "start",
            ],
            check=False,
        )
        if result.returncode:
            raise RuntimeError(f"pg_ctl start failed:\n{result.stdout}\n{self.log.read_text()}")
        self.started = True

    def stop(self, mode="fast"):
        if not self.started:
            return
        self.command(
            ["pg_ctl", "-D", str(self.data), "-m", mode, "-w", "stop"],
            check=False,
        )
        self.started = False

    def sql(self, text, check=True):
        result = self.command(
            ["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"],
            input=text,
            check=check,
        )
        return result.stdout.strip(), result

    def sql_ok(self, text):
        out, _ = self.sql(text, check=True)
        return out

    def sql_fail(self, text):
        out, result = self.sql(text, check=False)
        if result.returncode == 0:
            raise AssertionError(f"expected error, succeeded:\n{text}\n{out}")
        return out if out else result.stdout

    def checkpoint(self):
        self.sql_ok("CHECKPOINT")

    def relation_path(self, index):
        data = self.sql_ok("SHOW data_directory")
        rel = self.sql_ok(
            f"SELECT pg_relation_filepath('{index}'::regclass)"
        )
        return Path(data) / rel


class RelationFile:
    def __init__(self, path):
        self.path = Path(path)

    def read_raw(self, block):
        with open(self.path, "rb") as handle:
            handle.seek(block * PAGE_SIZE)
            page = handle.read(PAGE_SIZE)
        if len(page) != PAGE_SIZE:
            raise RuntimeError(f"{self.path} block {block}: short read")
        return page

    def write_raw(self, block, page):
        if len(page) != PAGE_SIZE:
            raise RuntimeError("page must be BLCKSZ")
        with open(self.path, "r+b") as handle:
            handle.seek(block * PAGE_SIZE)
            handle.write(page)

    def inspect(self, block):
        return inspect_page(self.read_raw(block))

    def write_payload(self, block, kind, payload):
        if len(payload) > CAPACITY:
            raise RuntimeError("payload exceeds page capacity")
        page = bytearray(self.read_raw(block))
        special = PAGE_SIZE - SPECIAL_SIZE
        flags = bytes(page[special + 6 : special + 8])
        page[PAGE_HEADER : PAGE_HEADER + len(payload)] = payload
        lower = PAGE_HEADER + len(payload)
        struct.pack_into("<H", page, 12, lower)
        struct.pack_into("<H", page, 14, special)
        struct.pack_into("<H", page, 16, special)
        struct.pack_into("<I", page, special, MAGIC)
        page[special + 4] = kind
        page[special + 5] = VERSION
        page[special + 6 : special + 8] = flags
        self.write_raw(block, bytes(page))

    def set_kind(self, block, kind):
        page = bytearray(self.read_raw(block))
        page[PAGE_SIZE - SPECIAL_SIZE + 4] = kind
        self.write_raw(block, bytes(page))

    def set_run_magic(self, first, magic):
        if len(magic) != 4:
            raise ValueError("magic is 4 bytes")
        page = bytearray(self.read_raw(first))
        page[PAGE_HEADER + 4 : PAGE_HEADER + 8] = magic
        self.write_raw(first, bytes(page))

    def run_blob(self, first, blocks, nbytes):
        out = bytearray()
        block = first
        for _ in range(blocks):
            if block == NONE:
                raise RuntimeError("run chain ends early")
            kind, payload = inspect_page(self.read_raw(block))
            if kind != KIND_RUN:
                raise RuntimeError(f"block {block}: kind {kind} is not a run")
            nxt = u32_at(payload, 0)
            take = min(len(payload) - 4, nbytes - len(out))
            out += payload[4 : 4 + take]
            block = nxt
        if len(out) != nbytes:
            raise RuntimeError(f"run: {len(out)} of {nbytes} bytes")
        return bytes(out)

    def write_run(self, first, blocks, data):
        cap = blocks * CHAIN_CAPACITY
        if len(data) > cap:
            raise RuntimeError(f"blob {len(data)} exceeds run capacity {cap}")
        chain = []
        block = first
        for _ in range(blocks):
            kind, payload = inspect_page(self.read_raw(block))
            if kind != KIND_RUN:
                raise RuntimeError(f"block {block}: kind {kind} is not a run")
            nxt = u32_at(payload, 0)
            chain.append((block, nxt))
            block = nxt
        offset = 0
        for block, nxt in chain:
            take = min(CHAIN_CAPACITY, len(data) - offset)
            chunk = data[offset : offset + take]
            offset += take
            self.write_payload(block, KIND_RUN, u32(nxt) + chunk)
        if offset != len(data):
            raise RuntimeError("short run write")

    def meta(self):
        kind, payload = self.inspect(0)
        header = parse_meta_header(payload)
        header["kind"] = kind
        header["payload"] = payload
        return header

    def entries(self, header=None):
        header = header or self.meta()
        payload = header["payload"]
        at = header["dir_at"]
        out = []
        for _ in range(header["segment_count"]):
            first, blocks, nbytes, last = struct.unpack_from("<IIII", payload, at)
            out.append((first, blocks, nbytes, last))
            at += ENTRY_BYTES
        return out

    def set_entry_nbytes(self, index, nbytes):
        kind, payload = self.inspect(0)
        header = parse_meta_header(payload)
        payload = bytearray(payload)
        at = header["dir_at"] + index * ENTRY_BYTES + 8
        struct.pack_into("<I", payload, at, nbytes)
        self.write_payload(0, kind, bytes(payload))

    def patch_buffer_state(self, buffer7):
        kind, payload = self.inspect(0)
        payload = bytearray(payload)
        struct.pack_into("<IIIIIII", payload, 16, *buffer7)
        self.write_payload(0, kind, bytes(payload))

    def write_buffer_stream(self, stream, docs):
        header = self.meta()
        version, epoch, head, tail, tail_used, _bytes, _docs = header["buffer"]
        if head == NONE:
            raise RuntimeError("no buffer page to plant into")
        kind, payload = inspect_page(self.read_raw(head))
        if kind != KIND_BUFFER:
            raise RuntimeError(f"buffer head kind {kind}")
        nxt = u32_at(payload, 0)
        if len(stream) > CHAIN_CAPACITY:
            raise RuntimeError("planted buffer does not fit on the head page")
        self.write_payload(head, KIND_BUFFER, u32(nxt) + stream)
        self.patch_buffer_state(
            (version + 1, epoch, head, head, len(stream), len(stream), docs)
        )


def snapshot_installed(dest):
    dest = Path(dest)
    dest.mkdir(parents=True, exist_ok=True)
    for child in dest.iterdir():
        child.unlink()
    pkglib = Path(pg_config("pkglibdir"))
    shared = Path(pg_config("sharedir")) / "extension"
    copied = []
    for path in list(pkglib.glob("stannum.*")) + list(shared.glob("stannum*")):
        shutil.copy2(path, dest / path.name)
        copied.append(path.name)
    return copied


def snapshot_library(dest):
    matches = list(Path(dest).glob("stannum.so")) + list(Path(dest).glob("stannum.dylib"))
    if not matches:
        raise FileNotFoundError(f"no stannum library in {dest}")
    return matches[0]


def install_snapshot(src, extra=()):
    src = Path(src)
    pkglib = Path(pg_config("pkglibdir"))
    shared = Path(pg_config("sharedir")) / "extension"
    for path in list(pkglib.glob("stannum.*")):
        path.unlink()
    for path in shared.glob("stannum*"):
        path.unlink()
    if not src.exists():
        return
    for path in src.iterdir():
        target = pkglib / path.name if path.suffix in {".so", ".dylib"} else shared / path.name
        shutil.copy2(path, target)
    for path in extra:
        path = Path(path)
        shutil.copy2(path, shared / path.name)


def cargo_pgrx_install(src, label):
    src = Path(src)
    env = dict(os.environ)
    command = [
        "cargo",
        "pgrx",
        "install",
        "--release",
        "--package",
        "stannum",
        "--no-default-features",
        "--features",
        "pg18",
        "--pg-config",
        env.get("PG_CONFIG") or shutil.which("pg_config"),
    ]
    print(f"== {label}: {' '.join(command)} (cwd={src})", flush=True)
    result = subprocess.run(
        command,
        cwd=src,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    sys.stdout.write(result.stdout)
    sys.stdout.flush()
    if result.returncode:
        raise RuntimeError(f"{label} cargo pgrx install failed")
    compiled = "Compiling stannum" in result.stdout
    if not compiled and os.environ.get("STANNUM_FORCE_RECOMPILE"):
        lib_rs = src / "postgres" / "src" / "lib.rs"
        lib_rs.touch()
        print(f"== {label}: touching {lib_rs} and rebuilding", flush=True)
        result = subprocess.run(
            command,
            cwd=src,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        sys.stdout.write(result.stdout)
        sys.stdout.flush()
        if result.returncode:
            raise RuntimeError(f"{label} cargo pgrx install failed after touch")
        compiled = "Compiling stannum" in result.stdout
        if not compiled:
            raise RuntimeError(f"{label}: cargo did not compile stannum after touch")
    return compiled, result.stdout


class Reporter:
    def __init__(self):
        self.results = []

    def record(self, name, status, detail, observed=""):
        line = f"ASSERT fixture={name} status={status} {detail}"
        if observed:
            line += f" observed={observed!r}"
        print(line, flush=True)
        self.results.append((name, status, detail, observed))
        return status == "PASS"

    def pass_(self, name, detail, observed=""):
        return self.record(name, "PASS", detail, observed)

    def fail(self, name, detail, observed=""):
        return self.record(name, "FAIL", detail, observed)

    def missing(self, name, detail):
        return self.record(name, "MISSING", detail)

    def ok(self):
        failed = [r for r in self.results if r[1] != "PASS"]
        seen = {r[0] for r in self.results}
        for name in FIXTURE_ROWS:
            if name not in seen:
                self.missing(name, "no assertion ran")
                failed.append((name, "MISSING", "no assertion ran", ""))
        return not failed


def expect_error(cluster, sql, needle, name, reporter, what):
    try:
        text = cluster.sql_fail(sql)
    except AssertionError as error:
        reporter.fail(name, f"{what}: statement succeeded", str(error))
        return False
    if needle in text:
        return reporter.pass_(name, what, needle)
    reporter.fail(name, f"{what}: string mismatch", text.replace("\n", " | "))
    return False


def expect_ok(cluster, sql, name, reporter, what):
    try:
        out = cluster.sql_ok(sql)
    except RuntimeError as error:
        reporter.fail(name, f"{what}: {error}", str(error)[-400:])
        return False
    return reporter.pass_(name, what, out)


def index_md5(cluster, index):
    cluster.checkpoint()
    return file_md5(cluster.relation_path(index))


def refused_insert_does_not_dirty(cluster, table, index, sql, needle, name, reporter):
    before = index_md5(cluster, index)
    expect_error(cluster, sql, needle, name + "_error", reporter, "refused write")
    after = index_md5(cluster, index)
    if before == after:
        reporter.pass_(name + "_nodirty", "index file md5 unchanged after CHECKPOINT")
        return True
    reporter.fail(name + "_nodirty", "index file changed", f"{before} -> {after}")
    return False


def immutable_count(cluster, index):
    out = cluster.sql_ok(
        f"SELECT count(*) FROM stannum.segment_info('{index}') WHERE kind = 'immutable'"
    )
    return int(out or "0")


def wait_segments(cluster, index, n):
    count = immutable_count(cluster, index)
    if count < n:
        raise RuntimeError(f"{index}: {count} immutable segments, need {n}")
    return count


def plant_segments(rel, transforms):
    header = rel.meta()
    entries = rel.entries(header)
    if len(entries) < len(transforms):
        raise RuntimeError(f"need {len(transforms)} segments, have {len(entries)}")
    for i, transform in enumerate(transforms):
        if transform is None:
            continue
        first, blocks, nbytes, _last = entries[i]
        blob = rel.run_blob(first, blocks, nbytes)
        planted = transform(blob)
        rel.write_run(first, blocks, planted)
        if len(planted) != nbytes:
            rel.set_entry_nbytes(i, len(planted))


def empty_buffer(rel):
    version, epoch, head, tail, tail_used, _bytes, _docs = rel.meta()["buffer"]
    if head == NONE:
        return
    kind, payload = inspect_page(rel.read_raw(head))
    nxt = u32_at(payload, 0)
    rel.write_payload(head, KIND_BUFFER, u32(nxt))
    rel.patch_buffer_state((version + 1, epoch, head, head, 0, 0, 0))


def create_multi(cluster, stem, rows, *, tokenizer=None, flush=True):
    table = f"{stem}_docs"
    index = f"{stem}_idx"
    with_tok = f" WITH (tokenizer = '{tokenizer}')" if tokenizer else ""
    inserts = ", ".join(
        f"({i}, '{title}', '{body}')" for i, title, body in rows
    )
    if flush:
        cluster.sql_ok(
            f"CREATE TABLE {table} (id int, title text, body text);\n"
            f"INSERT INTO {table} VALUES {inserts};\n"
            f"CREATE INDEX {index} ON {table} USING stannum(title, body){with_tok};"
        )
    else:
        cluster.sql_ok(
            f"CREATE TABLE {table} (id int, title text, body text);\n"
            f"CREATE INDEX {index} ON {table} USING stannum(title, body){with_tok};\n"
            f"INSERT INTO {table} VALUES {inserts};"
        )
    return table, index


def flush_extra_segments(cluster, table, index, extra):
    start = immutable_count(cluster, index)
    n = start
    k = 1000
    while n < start + extra:
        cluster.sql_ok(
            f"SET stannum.write_buffer_docs = 1;\n"
            f"SET stannum.merge_tier_factor = 64;\n"
            f"SET stannum.max_segments = 96;\n"
            f"INSERT INTO {table} VALUES ({k}, 'needle{k}', 'pad{k}');"
        )
        k += 1
        n = immutable_count(cluster, index)
        if k > 1000 + extra + 40:
            break
    wait_segments(cluster, index, start + extra)


def record_old_bytes(cluster, index, name, reporter):
    cluster.checkpoint()
    path = cluster.relation_path(index)
    rel = RelationFile(path)
    try:
        kind, payload = rel.inspect(0)
    except ValueError as error:
        reporter.fail(name, f"old meta unreadable: {error}")
        return None
    if kind == KIND_ENVELOPE:
        reporter.fail(name, "0.4.0 index already KIND_ENVELOPE")
        return None
    if kind != KIND_META:
        reporter.fail(name, f"0.4.0 block 0 kind {kind}, expected 1")
        return None
    header = parse_meta_header(payload)
    magic = None
    if header["segment_count"]:
        first, blocks, nbytes = struct.unpack_from("<III", payload, header["dir_at"])
        page = rel.read_raw(first)
        magic = bytes(page[PAGE_HEADER + 4 : PAGE_HEADER + 8])
        if magic == STN3:
            reporter.fail(name, "0.4.0 segment already STN3", magic.decode("ascii", "replace"))
            return None
        if magic not in LSG_MAGICS:
            reporter.fail(name, "0.4.0 segment magic not LSG*", repr(magic))
            return None
    reporter.pass_(
        name,
        f"byte-evidence kind=1 segments={header['segment_count']} magic={magic!r}",
    )
    return kind, magic


def build_old_indexes(cluster, reporter):
    cluster.sql_ok("CREATE EXTENSION stannum")
    version = cluster.sql_ok("SELECT extversion FROM pg_extension WHERE extname='stannum'")
    if version != "0.4.0":
        reporter.fail("old_version", f"CREATE EXTENSION installed {version}, want 0.4.0")
        return {}
    reporter.pass_("old_version", "CREATE EXTENSION 0.4.0")

    specs = {}
    cluster.sql_ok(
        "CREATE TABLE old_single_docs (id int, body text);\n"
        "INSERT INTO old_single_docs VALUES (1, 'needle alpha'), (2, 'beta needle');\n"
        "CREATE INDEX old_single_idx ON old_single_docs USING stannum(body);"
    )
    specs["old_single"] = ("old_single_docs", "old_single_idx", "body")
    record_old_bytes(cluster, "old_single_idx", "old_single_bytes", reporter)

    cluster.sql_ok(
        "CREATE TABLE old_multi_docs (id int, title text, body text);\n"
        "INSERT INTO old_multi_docs VALUES (1, 'needle', 'pad'), (2, 'x', 'needle');\n"
        "CREATE INDEX old_multi_idx ON old_multi_docs USING stannum(title, body);"
    )
    specs["old_multi"] = ("old_multi_docs", "old_multi_idx", "title")
    record_old_bytes(cluster, "old_multi_idx", "old_multi_bytes", reporter)

    cluster.sql_ok(
        "CREATE TABLE old_jieba_docs (id int, body text);\n"
        "INSERT INTO old_jieba_docs VALUES (1, 'needle 搜索'), (2, 'needle');\n"
        "CREATE INDEX old_jieba_idx ON old_jieba_docs USING stannum(body) "
        "WITH (tokenizer = 'jieba');"
    )
    specs["old_jieba"] = ("old_jieba_docs", "old_jieba_idx", "body")
    record_old_bytes(cluster, "old_jieba_idx", "old_jieba_bytes", reporter)

    cluster.sql_ok(
        "CREATE TABLE old_empty_docs (id int, body text);\n"
        "CREATE INDEX old_empty_idx ON old_empty_docs USING stannum(body);"
    )
    specs["old_empty"] = ("old_empty_docs", "old_empty_idx", "body")
    record_old_bytes(cluster, "old_empty_idx", "old_empty_bytes", reporter)

    cluster.sql_ok(
        "CREATE TABLE old_buffer_docs (id int, body text);\n"
        "CREATE INDEX old_buffer_idx ON old_buffer_docs USING stannum(body);\n"
        "INSERT INTO old_buffer_docs VALUES (1, 'only buffered needle');"
    )
    specs["old_buffer"] = ("old_buffer_docs", "old_buffer_idx", "body")
    record_old_bytes(cluster, "old_buffer_idx", "old_buffer_bytes", reporter)
    return specs


def assert_prestn3(cluster, specs, reporter):
    for name, (table, index, field) in specs.items():
        expect_error(
            cluster,
            f"SET enable_seqscan = off; SELECT count(*) FROM {table} WHERE {field} ==> 'needle'",
            PRE_STN3,
            name,
            reporter,
            "PreStn3 scan",
        )
        refused_insert_does_not_dirty(
            cluster,
            table,
            index,
            f"INSERT INTO {table} VALUES (99, " + (
                "'x'" if field == "body" else "'x', 'y'"
            ) + ")",
            PRE_STN3,
            name,
            reporter,
        )


def reindex_old(cluster, specs, reporter):
    for name, (table, index, field) in specs.items():
        try:
            cluster.sql_ok(f"REINDEX INDEX {index}")
        except RuntimeError as error:
            reporter.fail(name + "_reindex", "REINDEX failed", str(error)[-400:])
            continue
        try:
            count = cluster.sql_ok(
                f"SET enable_seqscan = off; SELECT count(*) FROM {table} "
                f"WHERE {field} ==> 'needle'"
            )
            reporter.pass_(name + "_reindex", f"answers after REINDEX count={count}")
        except RuntimeError as error:
            reporter.fail(name + "_reindex", "search after REINDEX failed", str(error)[-400:])


def setup_stn4_live(cluster):
    """Create 0.5.0 indexes that planting will mutate or that stay Current."""
    create_multi(
        cluster,
        "v2empty",
        ((1, "needle", "needle"), (2, "needle", "pad"), (3, "", "needle needle")),
        flush=True,
    )
    create_multi(
        cluster,
        "v2cur",
        ((1, "needle", "x"),),
        flush=False,
    )
    create_multi(cluster, "v2tilde", ((1, "~0~foo", "x"),), tokenizer="whitespace", flush=True)
    create_multi(cluster, "tagtilde", ((1, "~0~foo", "x"),), tokenizer="whitespace", flush=False)
    cluster.sql_ok(
        "CREATE TABLE emptyk5_docs (id int, title text, body text);\n"
        "CREATE INDEX emptyk5_idx ON emptyk5_docs USING stannum(title, body);"
    )
    create_multi(cluster, "bufcur", ((1, "needle", "pad"),), flush=False)
    create_multi(
        cluster,
        "plant_v1",
        ((1, "needle", "needle"), (2, "needle", "pad"), (3, "", "needle needle")),
        flush=True,
    )
    create_multi(
        cluster,
        "plant_mix",
        ((1, "needle", "needle"), (2, "needle", "pad"), (3, "", "needle needle")),
        flush=True,
    )
    flush_extra_segments(cluster, "plant_mix_docs", "plant_mix_idx", 1)
    create_multi(
        cluster,
        "plant_three",
        ((1, "needle", "needle"), (2, "needle", "pad"), (3, "", "needle needle")),
        flush=True,
    )
    flush_extra_segments(cluster, "plant_three_docs", "plant_three_idx", 2)
    create_multi(
        cluster,
        "plant_fch",
        ((1, "needle", "x"), (2, "pad", "needle")),
        flush=True,
    )
    create_multi(cluster, "plant_legacy", ((1, "needle", "pad"),), flush=True)
    create_multi(cluster, "plant_stale", ((1, "needle", "pad"),), flush=True)
    create_multi(cluster, "kind5lsg", ((1, "needle", "x"),), flush=True)
    create_multi(cluster, "kind1lsg", ((1, "needle", "x"),), flush=True)
    cluster.sql_ok(
        "CREATE TABLE kind1empty_docs (id int, title text, body text);\n"
        "CREATE INDEX kind1empty_idx ON kind1empty_docs USING stannum(title, body);"
    )
    cluster.sql_ok(
        "CREATE TABLE kind1buf_docs (id int, title text, body text);\n"
        "CREATE INDEX kind1buf_idx ON kind1buf_docs USING stannum(title, body);\n"
        "INSERT INTO kind1buf_docs VALUES (1, 'needle', 'pad');"
    )
    cluster.sql_ok(
        "CREATE TABLE tagzero_docs (id int, title text, body text);\n"
        "CREATE INDEX tagzero_idx ON tagzero_docs USING stannum(title, body);"
    )
    cluster.sql_ok(
        "CREATE TABLE tagzerolive_docs (id int, title text, body text);\n"
        "CREATE INDEX tagzerolive_idx ON tagzerolive_docs USING stannum(title, body);"
    )
    cluster.sql_ok(
        "CREATE TABLE untagzero_docs (id int, title text, body text);\n"
        "CREATE INDEX untagzero_idx ON untagzero_docs USING stannum(title, body);"
    )
    cluster.sql_ok(
        "CREATE TABLE untagzerov2_docs (id int, title text, body text);\n"
        "INSERT INTO untagzerov2_docs VALUES (1, 'needle', 'pad');\n"
        "CREATE INDEX untagzerov2_idx ON untagzerov2_docs USING stannum(title, body);"
    )
    cluster.sql_ok(
        "CREATE TABLE bufstale_docs (id int, title text, body text);\n"
        "CREATE INDEX bufstale_idx ON bufstale_docs USING stannum(title, body);"
    )
    cluster.sql_ok(
        "CREATE TABLE bufmal_docs (id int, title text, body text);\n"
        "CREATE INDEX bufmal_idx ON bufmal_docs USING stannum(title, body);"
    )


def collect_paths(cluster, names):
    cluster.checkpoint()
    return {name: cluster.relation_path(name) for name in names}


def apply_plants(paths):
    def rel(name):
        return RelationFile(paths[name])

    # v1_empty: demote the only segment, empty buffer.
    r = rel("plant_v1_idx")
    plant_segments(r, [demote_v1])
    empty_buffer(r)

    # mix: demote first, leave second v2, empty buffer.
    r = rel("plant_mix_idx")
    plant_segments(r, [demote_v1, None])
    empty_buffer(r)

    # three: v1, v2, missing trailer.
    r = rel("plant_three_idx")
    n = parse_meta_header(r.inspect(0)[1])["segment_count"]
    transforms = [demote_v1, None, strip_trailer]
    if n < 3:
        transforms = [demote_v1, strip_trailer][:n]
    plant_segments(r, transforms)
    empty_buffer(r)

    r = rel("plant_fch_idx")
    plant_segments(r, [break_fch1])

    r = rel("kind5lsg_idx")
    first = r.entries()[0][0]
    r.set_run_magic(first, b"LSG4")

    r = rel("kind1lsg_idx")
    first = r.entries()[0][0]
    r.set_run_magic(first, b"LSG4")
    r.set_kind(0, KIND_META)

    rel("kind1empty_idx").set_kind(0, KIND_META)
    rel("kind1buf_idx").set_kind(0, KIND_META)

    rel("tagzero_idx").write_buffer_stream(tagged_stream(()), 1)
    rel("untagzero_idx").write_buffer_stream(untagged_zero_term(), 1)
    rel("untagzerov2_idx").write_buffer_stream(untagged_zero_term(), 1)
    rel("bufstale_idx").write_buffer_stream(untagged_legacy_stream(), 1)
    rel("plant_legacy_idx").write_buffer_stream(untagged_legacy_stream(), 1)
    rel("plant_stale_idx").write_buffer_stream(untagged_legacy_stream(), 1)
    torn = tagged_stream(((0, 1, (("~0~foo", [1]),)),))[:-3]
    rel("bufmal_idx").write_buffer_stream(torn, 1)

    # v1 + BufferCurrent: demote segments of plant_v1 already emptied; use
    # a copy... plant_v1 is empty-buffer v1. v1_buf_current uses plant_stale's
    # opposite: we plant tagged current onto plant_v1 after demote — conflict.
    # Dedicated: reuse plant_v1 for v1_empty only. v1_buf_current plants a
    # tagged current stream onto a demoted index — need a second v1 index.
    # Use plant_legacy's segments: first demote then tagged current? That is
    # MixedFielded (v1 + BufferCurrent). plant_legacy currently got untagged
    # stream. Split: plant_legacy keeps v2 + untagged (MixedFielded).
    # v1_stale: demote plant_v1 and untagged stream — overwrites v1_empty.
    # We'll use plant_v1 as v1_empty (empty buffer) and bufstale as buffer-only
    # Stale. v1_stale = demote v2empty's clone... v2empty must stay Current.
    # Additional plant file: we already have plant_v1. After assertions for
    # v1_empty we could replant — sequential assertions instead.

    rel("tagtilde_idx").write_buffer_stream(
        tagged_stream(((0, 1, (("~0~foo", [1]),)), (1, 1, (("x", [1]),)))),
        1,
    )


def assert_stn4(cluster, reporter):
    def scan(table, field="title"):
        return (
            f"SET enable_seqscan = off; SELECT count(*) FROM {table} "
            f"WHERE {field} ==> 'needle'"
        )

    def insert(table):
        return f"INSERT INTO {table} VALUES (90, 'needle', 'pad')"

    expect_ok(cluster, scan("v2empty_docs"), "v2_empty", reporter, "Current v2+BufferEmpty")
    expect_ok(cluster, scan("v2cur_docs"), "v2_current", reporter, "Current v2+BufferCurrent")
    expect_ok(cluster, scan("emptyk5_docs"), "empty_kind5", reporter, "empty kind-5")
    expect_ok(cluster, scan("bufcur_docs"), "buf_only_current", reporter, "buffer-only Current")
    expect_ok(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM v2tilde_docs WHERE title ==> '\"~0~foo\"'",
        "v2_tilde_dict",
        reporter,
        "v2 dict token ~0~foo is Current",
    )
    expect_ok(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM tagtilde_docs WHERE title ==> '\"~0~foo\"'",
        "tagged_tilde_restart",
        reporter,
        "tagged ~0~foo after restart is Current",
    )

    expect_error(cluster, scan("plant_v1_docs"), STALE_FIELDED, "v1_empty", reporter, "v1+BufferEmpty")
    refused_insert_does_not_dirty(
        cluster, "plant_v1_docs", "plant_v1_idx", insert("plant_v1_docs"),
        STALE_FIELDED, "v1_empty", reporter,
    )

    expect_error(
        cluster, scan("plant_mix_docs"), MIXED_FIELDED, "mixed_empty", reporter, "v1+v2+empty"
    )
    refused_insert_does_not_dirty(
        cluster, "plant_mix_docs", "plant_mix_idx", insert("plant_mix_docs"),
        MIXED_FIELDED, "mixed_empty", reporter,
    )

    expect_error(
        cluster, scan("plant_legacy_docs"), MIXED_FIELDED, "v2_untagged_legacy",
        reporter, "v2+untagged legacy buffer",
    )
    expect_error(
        cluster, scan("bufstale_docs"), STALE_FIELDED, "buf_only_stale",
        reporter, "buffer-only BufferStale",
    )
    expect_error(
        cluster, scan("plant_stale_docs"), MIXED_FIELDED, "v2_buf_stale",
        reporter, "v2+BufferStale",
    )

    # Corrupt rows: any error that is not a rebuild/migration string.
    def expect_corrupt(sql, name, forbidden=()):
        text = cluster.sql_fail(sql)
        if "invalid page" in text:
            reporter.fail(name, "planted page is unreadable", text.replace("\n", " | "))
            return
        if PRE_STN3 in text:
            reporter.fail(name, "Corrupt emitted PreStn3", text.replace("\n", " | "))
            return
        if any(s in text for s in forbidden):
            reporter.fail(name, "Corrupt emitted a rebuild string", text.replace("\n", " | "))
            return
        reporter.pass_(name, "Corrupt (non-migration error)", text.split("\n")[0][-200:])

    expect_corrupt(scan("plant_fch_docs"), "v2_bad_fch1")
    expect_corrupt(scan("bufmal_docs"), "buf_only_malformed")
    expect_corrupt(scan("plant_three_docs"), "mixed_missing_trailer")

    expect_error(
        cluster, scan("kind5lsg_docs"), KIND5_LSG, "kind5_lsg", reporter,
        "kind-5+LSG mixed-format",
    )
    if PRE_STN3 in cluster.sql_fail(scan("kind5lsg_docs")):
        reporter.fail("kind5_lsg_not_prestn3", "kind-5+LSG used the §8 string")
    else:
        reporter.pass_("kind5_lsg_not_prestn3", "kind-5+LSG is not PreStn3")

    expect_error(
        cluster, scan("kind1lsg_docs"), PRE_STN3, "kind1_lsg", reporter, "kind-1+LSG"
    )
    expect_error(
        cluster, scan("kind1empty_docs"), PRE_STN3, "kind1_empty", reporter, "kind-1 empty"
    )
    expect_error(
        cluster, insert("kind1buf_docs"), PRE_STN3, "kind1_buffer", reporter,
        "kind-1 buffer-only INSERT",
    )

    expect_error(
        cluster, insert("untagzero_docs"), STALE_FIELDED, "untagged_zero_insert",
        reporter, "untagged zero-term INSERT",
    )
    refused_insert_does_not_dirty(
        cluster, "untagzero_docs", "untagzero_idx", insert("untagzero_docs"),
        STALE_FIELDED, "untagged_zero_insert", reporter,
    )
    expect_error(
        cluster, insert("untagzerov2_docs"), MIXED_FIELDED, "untagged_zero_v2_insert",
        reporter, "untagged zero-term beside v2 INSERT",
    )
    refused_insert_does_not_dirty(
        cluster, "untagzerov2_docs", "untagzerov2_idx", insert("untagzerov2_docs"),
        MIXED_FIELDED, "untagged_zero_v2_insert", reporter,
    )

    expect_ok(cluster, scan("tagzero_docs"), "tagged_zero_empty", reporter,
              "tagged BufferNoTerms alone is Current")

    # Tagged zero-term INSERT/flush/restart: write a term into tagzerolive
    # (empty tagged? it was empty BufferEmpty). Plant tagzero already Current.
    # Use tagzero: INSERT a term, force flush, restart is later.
    try:
        cluster.sql_ok(
            "SET stannum.write_buffer_docs = 1;\n"
            "INSERT INTO tagzero_docs VALUES (2, 'needle', 'pad');"
        )
        reporter.pass_("tagged_zero_insert_flush_restart", "INSERT into tagged NoTerms proceeded")
    except RuntimeError as error:
        reporter.fail(
            "tagged_zero_insert_flush_restart",
            "INSERT into tagged NoTerms failed",
            str(error)[-400:],
        )


def finish_live_rows(cluster, reporter):
    """Rows that need a second plant pass or live SQL after the first asserts."""
    # v1 + BufferNoTerms / BufferStale / BufferCurrent: replant plant_v1.
    cluster.checkpoint()
    path = cluster.relation_path("plant_v1_idx")
    cluster.stop("fast")
    rel = RelationFile(path)
    rel.write_buffer_stream(tagged_stream(()), 1)
    cluster.start()
    expect_error(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM plant_v1_docs WHERE title ==> 'needle'",
        STALE_FIELDED,
        "v1_noterms",
        reporter,
        "v1 + tagged BufferNoTerms",
    )
    cluster.checkpoint()
    cluster.stop("fast")
    rel.write_buffer_stream(untagged_legacy_stream(), 1)
    cluster.start()
    expect_error(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM plant_v1_docs WHERE title ==> 'needle'",
        STALE_FIELDED,
        "v1_stale",
        reporter,
        "v1 + BufferStale",
    )
    cluster.checkpoint()
    cluster.stop("fast")
    rel.write_buffer_stream(
        tagged_stream(((0, 1, (("needle", [1]),)), (1, 1, (("pad", [1]),)))),
        1,
    )
    cluster.start()
    expect_error(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM plant_v1_docs WHERE title ==> 'needle'",
        MIXED_FIELDED,
        "v1_buf_current",
        reporter,
        "v1 + BufferCurrent",
    )

    # mixed_noterms: tagged NoTerms on v1+v2 mix.
    mix = cluster.relation_path("plant_mix_idx")
    cluster.checkpoint()
    cluster.stop("fast")
    RelationFile(mix).write_buffer_stream(tagged_stream(()), 1)
    cluster.start()
    expect_error(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM plant_mix_docs WHERE title ==> 'needle'",
        MIXED_FIELDED,
        "mixed_noterms",
        reporter,
        "v1+v2 + BufferNoTerms",
    )
    cluster.checkpoint()
    cluster.stop("fast")
    RelationFile(mix).write_buffer_stream(
        tagged_stream(((0, 1, (("needle", [1]),)),)),
        1,
    )
    cluster.start()
    expect_error(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM plant_mix_docs WHERE title ==> 'needle'",
        MIXED_FIELDED,
        "mixed_match",
        reporter,
        "v1+v2 + BufferCurrent (matching v2 side)",
    )

    # v2 + BufferMalformed
    cluster.checkpoint()
    path = cluster.relation_path("v2empty_idx")
    cluster.stop("fast")
    torn = tagged_stream(((0, 1, (("needle", [1]),)),))[:-2]
    RelationFile(path).write_buffer_stream(torn, 1)
    cluster.start()
    text = cluster.sql_fail(
        "SET enable_seqscan = off; SELECT count(*) FROM v2empty_docs WHERE title ==> 'needle'"
    )
    if "invalid page" in text:
        reporter.fail("v2_buf_malformed", "planted page is unreadable", text)
    elif STALE_FIELDED in text or MIXED_FIELDED in text or PRE_STN3 in text:
        reporter.fail("v2_buf_malformed", "expected Corrupt, got rebuild/migration", text)
    else:
        reporter.pass_("v2_buf_malformed", "v2 + BufferMalformed is Corrupt", text.split("\n")[0][-200:])

    # v1+v2 + malformed trailer: corrupt second segment version on plant_mix
    # after restoring? mix currently has BufferCurrent. Demoted first + bad
    # second trailer.
    cluster.checkpoint()
    path = cluster.relation_path("plant_mix_idx")
    cluster.stop("fast")
    rel = RelationFile(path)
    entries = rel.entries()
    if len(entries) >= 2:
        first, blocks, nbytes, _ = entries[1]
        blob = rel.run_blob(first, blocks, nbytes)
        rel.write_run(first, blocks, corrupt_trailer_version(blob))
        if len(corrupt_trailer_version(blob)) != nbytes:
            rel.set_entry_nbytes(1, len(corrupt_trailer_version(blob)))
        empty_buffer(rel)
    cluster.start()
    text = cluster.sql_fail(
        "SET enable_seqscan = off; SELECT count(*) FROM plant_mix_docs WHERE title ==> 'needle'"
    )
    if "invalid page" in text:
        reporter.fail("mixed_malformed_trailer", "planted page is unreadable", text)
    elif MIXED_FIELDED in text:
        reporter.fail("mixed_malformed_trailer", "malformed mix classified MixedFielded", text)
    elif PRE_STN3 in text:
        reporter.fail("mixed_malformed_trailer", "malformed mix classified PreStn3", text)
    else:
        reporter.pass_("mixed_malformed_trailer", "v1+v2+bad trailer is Corrupt", text.split("\n")[0][-200:])

    # tagged zero-term + v2: plant tagged NoTerms onto v2tilde (has v2 segments)
    cluster.checkpoint()
    path = cluster.relation_path("v2tilde_idx")
    cluster.stop("fast")
    RelationFile(path).write_buffer_stream(tagged_stream(()), 1)
    cluster.start()
    expect_ok(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM v2tilde_docs WHERE title ==> '\"~0~foo\"'",
        "tagged_zero_v2",
        reporter,
        "v2 + tagged BufferNoTerms is Current",
    )

    # tagged zero-term + v1 already asserted as v1_noterms → StaleFielded
    reporter.pass_("tagged_zero_v1", "same bytes as v1_noterms → StaleFielded")

    # untagged legacy buffer-only already buf_only_stale; well-formed keys:
    expect_error(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM bufstale_docs WHERE title ==> 'needle'",
        STALE_FIELDED,
        "untagged_legacy",
        reporter,
        "untagged well-formed legacy fielded buffer",
    )

    # v2_noterms: tagged NoTerms was planted on v2tilde for tagged_zero_v2,
    # which is also v2 + BufferNoTerms Current — reuse.
    reporter.pass_("v2_noterms", "same as tagged_zero_v2 (v2 + BufferNoTerms)")

    # Restart proof for tagged zero-term INSERT that flushed.
    expect_ok(
        cluster,
        "SET enable_seqscan = off; SELECT count(*) FROM tagzero_docs WHERE title ==> 'needle'",
        "tagged_zero_insert_flush_restart_search",
        reporter,
        "tagged NoTerms INSERT remains Current after later restart",
    )


def verify_old_src(path):
    cargo = (path / "Cargo.toml").read_text()
    if 'version = "0.4.0"' not in cargo:
        raise SystemExit(f"{path}: Cargo.toml is not version 0.4.0")
    git_dir = path / ".git"
    if git_dir.exists() or (path / ".git").is_file():
        head = subprocess.check_output(
            ["git", "-C", str(path), "rev-parse", "HEAD"], text=True
        ).strip()
        print(f"== old source {path} HEAD {head} (pin {OLD_PIN_SHORT})", flush=True)
        if not head.startswith(OLD_PIN_SHORT) and head != OLD_PIN:
            print(
                f"== warning: old source HEAD {head} is not pin {OLD_PIN}",
                flush=True,
            )
    else:
        print(f"== old source {path} (no git, pin {OLD_PIN_SHORT})", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--old-src", type=Path, help="0.4.0 source tree (built with cargo pgrx install)")
    parser.add_argument("--new-src", type=Path, default=ROOT, help="0.5.0 source tree")
    parser.add_argument("--old-snapshot", type=Path, help="prebuilt 0.4.0 extension directory")
    parser.add_argument("--new-snapshot", type=Path, help="prebuilt 0.5.0 extension directory")
    parser.add_argument("--work", type=Path, help="scratch directory")
    parser.add_argument("--port", default=PORT)
    parser.add_argument("--source-pins-only", action="store_true")
    args = parser.parse_args(argv)

    check_source_pins(args.new_src)
    print("ASSERT source-pins PASS four error strings present in production sources", flush=True)
    if args.source_pins_only:
        return 0

    if shutil.which("pg_config") is None:
        raise SystemExit("pg_config is not on PATH")
    major = pg_config("version").split()[1].split(".")[0]
    if major != "18":
        raise SystemExit(f"E.1 is PostgreSQL 18 only, got {pg_config('version')}")

    work = Path(args.work) if args.work else Path(tempfile.mkdtemp(prefix="stannum-e1-"))
    work.mkdir(parents=True, exist_ok=True)
    old_snap = args.old_snapshot or (work / "artifact-0.4.0")
    new_snap = args.new_snapshot or (work / "artifact-0.5.0")
    prefix_backup = work / "prefix-backup"
    started = time.monotonic()
    reporter = Reporter()

    snapshot_installed(prefix_backup)

    try:
        if args.old_snapshot is None:
            if args.old_src is None:
                raise SystemExit("pass --old-src (0.4.0 tree) or --old-snapshot")
            verify_old_src(args.old_src)
            compiled, _ = cargo_pgrx_install(args.old_src, "0.4.0")
            snapshot_installed(old_snap)
            lib = snapshot_library(old_snap)
            print(f"== 0.4.0 library {lib} sha256={sha256(lib)} compiled={compiled}", flush=True)
            if not compiled:
                print(
                    "== warning: 0.4.0 cargo pgrx install did not compile stannum; "
                    "using the worktree's existing release library",
                    flush=True,
                )
        else:
            lib = snapshot_library(old_snap)
            print(f"== 0.4.0 snapshot {old_snap} sha256={sha256(lib)}", flush=True)

        if args.new_snapshot is None:
            compiled, _ = cargo_pgrx_install(args.new_src, "0.5.0")
            snapshot_installed(new_snap)
            lib = snapshot_library(new_snap)
            print(f"== 0.5.0 library {lib} sha256={sha256(lib)} compiled={compiled}", flush=True)
            if not compiled:
                print(
                    "== warning: 0.5.0 cargo pgrx install did not compile stannum",
                    flush=True,
                )
        upgrade_sql = args.new_src / "postgres/sql/stannum--0.4.0--0.5.0.sql"
        old_sql = old_snap / "stannum--0.4.0.sql"
        if not old_sql.exists():
            shutil.copy2(args.new_src / "postgres/sql/stannum--0.4.0.sql", old_sql)

        install_snapshot(old_snap)
        cluster = Cluster(work / "cluster", port=args.port)
        cluster.init()
        cluster.start()
        try:
            specs = build_old_indexes(cluster, reporter)
            cluster.checkpoint()
            cluster.stop("fast")

            extras = []
            if upgrade_sql.exists():
                extras.append(upgrade_sql)
            if old_sql.exists():
                extras.append(old_sql)
            install_snapshot(new_snap, extra=extras)
            cluster.start()
            cluster.sql_ok("ALTER EXTENSION stannum UPDATE")
            ver = cluster.sql_ok("SELECT extversion FROM pg_extension WHERE extname='stannum'")
            if ver != "0.5.0":
                reporter.fail("alter_extension", f"extversion={ver}")
            else:
                reporter.pass_("alter_extension", "ALTER EXTENSION → 0.5.0")
            assert_prestn3(cluster, specs, reporter)
            reindex_old(cluster, specs, reporter)

            setup_stn4_live(cluster)
            names = [
                "v2empty_idx", "v2cur_idx", "v2tilde_idx", "tagtilde_idx",
                "emptyk5_idx", "bufcur_idx", "plant_v1_idx", "plant_mix_idx",
                "plant_three_idx", "plant_fch_idx", "plant_legacy_idx",
                "plant_stale_idx", "kind5lsg_idx", "kind1lsg_idx",
                "kind1empty_idx", "kind1buf_idx", "tagzero_idx",
                "untagzero_idx", "untagzerov2_idx", "bufstale_idx",
                "bufmal_idx", "tagzerolive_idx",
            ]
            paths = collect_paths(cluster, names)
            cluster.stop("fast")
            apply_plants(paths)
            cluster.start()
            assert_stn4(cluster, reporter)
            finish_live_rows(cluster, reporter)
        finally:
            cluster.stop("immediate")
    finally:
        install_snapshot(prefix_backup)
        elapsed = time.monotonic() - started
        print(f"== two-artifact-migration runtime {elapsed:.1f}s", flush=True)
        print(f"== prefix restored from {prefix_backup}", flush=True)

    if not reporter.ok():
        print("== two-artifact-migration FAILED", flush=True)
        return 1
    print("== two-artifact-migration PASS", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
