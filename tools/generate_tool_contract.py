#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Generate the 0.4.0 tool-contract manifest and its readable summary.

Reads the extension SQL snapshot, reloption registrations, and GUC
registrations. Does not consult the design document. Re-running writes
byte-identical outputs.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SQL_PATH = Path("postgres/sql/stannum--0.4.0.sql")
OPTIONS_PATH = Path("postgres/src/options.rs")
GUC_PATHS = (
    Path("postgres/src/storage/mod.rs"),
    Path("postgres/src/storage/wal.rs"),
    Path("postgres/src/customscan.rs"),
)
CONST_PATHS = (
    Path("postgres/src/bm25.rs"),
    Path("postgres/src/udfs.rs"),
    Path("postgres/src/storage/layout.rs"),
    Path("tokenizer/src/spec.rs"),
)
CONTROL_PATH = Path("postgres/stannum.control")
CARGO_PATH = Path("postgres/Cargo.toml")
MANIFEST_PATH = Path("docs/tool-contract.manifest.json")
DOC_PATH = Path("docs/tool-contract.md")

PGRX_ENUM_NAME_RULE = (
    "pgrx PostgresGucEnum registers the variant identifier unless #[name] is set"
)
ATTR_WORDS = {
    "IMMUTABLE",
    "STABLE",
    "VOLATILE",
    "STRICT",
    "PARALLEL",
    "LEAKPROOF",
    "LANGUAGE",
    "CALLED",
    "RETURNS",
    "WINDOW",
    "COST",
    "ROWS",
    "SUPPORT",
    "AS",
}
CANONICAL_TYPES = {
    "text": "text",
    "int": "int4",
    "int2": "int2",
    "int4": "int4",
    "int8": "int8",
    "integer": "int4",
    "bigint": "int8",
    "smallint": "int2",
    "real": "float4",
    "float4": "float4",
    "float8": "float8",
    "bool": "bool",
    "boolean": "bool",
    "oid": "oid",
    "internal": "internal",
    "regclass": "regclass",
    "tid": "tid",
    "cstring": "cstring",
    "void": "void",
    "indexed_query": "indexed_query",
    "index_am_handler": "index_am_handler",
}


def read(root: Path, path: Path) -> str:
    return (root / path).read_text(encoding="utf-8")


def dumps(value) -> str:
    return json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def skip_string(text: str, index: int) -> int:
    quote = text[index]
    index += 1
    while index < len(text):
        if text[index] == quote:
            if quote == "'" and index + 1 < len(text) and text[index + 1] == "'":
                index += 2
                continue
            return index + 1
        if quote == '"' and text[index] == "\\":
            index += 2
            continue
        index += 1
    raise ValueError("unterminated string")


def skip_dollar(text: str, index: int) -> int | None:
    match = re.match(r"\$([A-Za-z_][A-Za-z0-9_]*)?\$", text[index:])
    if not match:
        return None
    tag = match.group(0)
    end = text.find(tag, index + len(tag))
    if end < 0:
        raise ValueError("unterminated dollar quote")
    return end + len(tag)


def strip_comments(text: str) -> str:
    out: list[str] = []
    index = 0
    while index < len(text):
        if text.startswith("--", index):
            end = text.find("\n", index)
            if end < 0:
                break
            out.append("\n")
            index = end
            continue
        if text.startswith("/*", index):
            end = text.find("*/", index + 2)
            if end < 0:
                raise ValueError("unterminated block comment")
            out.append(" ")
            index = end + 2
            continue
        if text[index] == "'":
            end = skip_string(text, index)
            out.append(text[index:end])
            index = end
            continue
        dollar = skip_dollar(text, index) if text[index] == "$" else None
        if dollar is not None:
            out.append(text[index:dollar])
            index = dollar
            continue
        out.append(text[index])
        index += 1
    return "".join(out)


def collapse_ws(text: str) -> str:
    out: list[str] = []
    index = 0
    pending = False
    while index < len(text):
        if text[index] == "'":
            if pending and out:
                out.append(" ")
            pending = False
            end = skip_string(text, index)
            out.append(text[index:end])
            index = end
            continue
        if text[index].isspace():
            pending = bool(out)
            index += 1
            continue
        if pending:
            out.append(" ")
            pending = False
        out.append(text[index])
        index += 1
    return "".join(out).strip()


def balanced(text: str, open_at: int, opener: str = "(", closer: str = ")") -> tuple[str, int]:
    if text[open_at] != opener:
        raise ValueError(f"expected {opener}")
    depth = 0
    index = open_at
    while index < len(text):
        if text[index] == "'":
            index = skip_string(text, index)
            continue
        if text[index] == '"':
            index = skip_rust_string(text, index)
            continue
        dollar = skip_dollar(text, index) if text[index] == "$" else None
        if dollar is not None:
            index = dollar
            continue
        if text[index] == opener:
            depth += 1
        elif text[index] == closer:
            depth -= 1
            if depth == 0:
                return text[open_at + 1 : index], index + 1
        index += 1
    raise ValueError(f"unbalanced {opener}")


def skip_rust_string(text: str, index: int) -> int:
    if text[index] != '"':
        raise ValueError("not a rust string")
    index += 1
    while index < len(text):
        if text[index] == "\\":
            index += 2
            continue
        if text[index] == '"':
            return index + 1
        index += 1
    raise ValueError("unterminated Rust string")


def split_top(text: str, separator: str = ",") -> list[str]:
    parts: list[str] = []
    start = 0
    depth = 0
    index = 0
    while index < len(text):
        if text[index] == "'":
            index = skip_string(text, index)
            continue
        if text[index] == '"':
            index = skip_rust_string(text, index)
            continue
        dollar = skip_dollar(text, index) if text[index] == "$" else None
        if dollar is not None:
            index = dollar
            continue
        if text[index] in "([{":
            depth += 1
        elif text[index] in ")]}":
            depth -= 1
        elif text[index] == separator and depth == 0:
            parts.append(text[start:index].strip())
            start = index + 1
        index += 1
    tail = text[start:].strip()
    if tail:
        parts.append(tail)
    return parts


def split_statements(text: str) -> list[tuple[int, str]]:
    statements: list[tuple[int, str]] = []
    start = 0
    depth = 0
    index = 0
    while index < len(text):
        if text.startswith("--", index):
            end = text.find("\n", index)
            index = len(text) if end < 0 else end + 1
            continue
        if text.startswith("/*", index):
            end = text.find("*/", index + 2)
            if end < 0:
                raise ValueError("unterminated block comment")
            index = end + 2
            continue
        if text[index] == "'":
            index = skip_string(text, index)
            continue
        dollar = skip_dollar(text, index) if text[index] == "$" else None
        if dollar is not None:
            index = dollar
            continue
        if text[index] == "(":
            depth += 1
        elif text[index] == ")":
            depth -= 1
            if depth < 0:
                raise ValueError("unbalanced parenthesis in SQL")
        elif text[index] == ";" and depth == 0:
            raw = text[start:index]
            code = strip_comments(raw).strip()
            if code:
                offset = _code_offset(raw)
                statements.append((text.count("\n", 0, start + offset) + 1, raw.strip()))
            start = index + 1
        index += 1
    if depth != 0:
        raise ValueError("unbalanced parenthesis at end of SQL")
    if strip_comments(text[start:]).strip():
        raise ValueError("trailing SQL was not consumed: " + collapse_ws(text[start:])[:240])
    return statements


def _code_offset(raw: str) -> int:
    stripped = strip_comments(raw)
    match = re.search(r"\S", stripped)
    if not match:
        return 0
    target = match.start()
    seen = 0
    index = 0
    while index < len(raw) and seen < target:
        if raw.startswith("--", index):
            end = raw.find("\n", index)
            index = len(raw) if end < 0 else end + 1
            continue
        if raw.startswith("/*", index):
            end = raw.find("*/", index + 2)
            index = end + 2
            continue
        seen += 1
        index += 1
    return index


def sql_name(token: str) -> tuple[str | None, str, bool]:
    token = token.strip().rstrip(")")
    quoted = token.startswith('"') and token.endswith('"')
    if quoted:
        return None, token[1:-1], True
    if "." in token:
        schema, name = token.rsplit(".", 1)
        if name.startswith('"') and name.endswith('"'):
            return schema, name[1:-1], True
        return schema, name, False
    return None, token, False


def display_type(type_name: str) -> str:
    text = collapse_ws(type_name)
    array = text.endswith("[]")
    core = text[:-2] if array else text
    if core.lower().startswith("@extschema@."):
        core = core.split(".", 1)[1]
    if core.lower().startswith("pg_catalog."):
        core = core.split(".", 1)[1]
    core = core.lower()
    return core + ("[]" if array else "")


def canonical_type(type_name: str) -> str:
    displayed = display_type(type_name)
    array = displayed.endswith("[]")
    core = displayed[:-2] if array else displayed
    mapped = CANONICAL_TYPES.get(core, core)
    return mapped + ("[]" if array else "")


def decode_sql_string(literal: str) -> str:
    if len(literal) < 2 or literal[0] != "'" or literal[-1] != "'":
        raise ValueError(f"not a SQL string: {literal}")
    return literal[1:-1].replace("''", "'")


def parse_args(raw: str) -> list[dict]:
    arguments = []
    for part in split_top(strip_comments(raw)):
        if not part:
            continue
        has_default = False
        default = None
        match = re.search(r"\bDEFAULT\b", part, re.I)
        if match:
            has_default = True
            default = collapse_ws(part[match.end() :])
            part = part[: match.start()].strip()
        mode = None
        for prefix in ("INOUT", "VARIADIC", "IN", "OUT"):
            if re.match(rf"{prefix}\b", part, re.I):
                mode = prefix
                part = part[len(prefix) :].strip()
                break
        if part.startswith('"'):
            end = part.find('"', 1)
            name = part[1:end]
            quoted = True
            type_sql = collapse_ws(part[end + 1 :])
        else:
            name = None
            quoted = False
            type_sql = collapse_ws(part)
        if not type_sql:
            raise ValueError(f"argument has no type: {part}")
        arguments.append(
            {
                "default": default,
                "has_default": has_default,
                "mode": mode,
                "name": name,
                "quoted": quoted,
                "type": display_type(type_sql),
                "type_canonical": canonical_type(type_sql),
                "type_sql": type_sql,
            }
        )
    return arguments


def parse_returns(rest: str) -> tuple[dict, str]:
    rest = rest.lstrip()
    match = re.match(r"RETURNS\s+", rest, re.I)
    if not match:
        raise ValueError("function is missing RETURNS: " + rest[:80])
    body = rest[match.end() :]
    upper = body.lstrip().upper()
    if upper.startswith("TABLE"):
        open_at = body.upper().find("TABLE") + len("TABLE")
        open_at = body.find("(", open_at)
        columns_raw, end = balanced(body, open_at)
        columns = []
        for part in split_top(columns_raw):
            part = collapse_ws(part)
            if part.startswith('"'):
                name_end = part.find('"', 1)
                name = part[1:name_end]
                quoted = True
                type_sql = part[name_end + 1 :].strip()
            else:
                name, type_sql = part.split(None, 1)
                quoted = False
            columns.append(
                {
                    "name": name,
                    "quoted": quoted,
                    "type": display_type(type_sql),
                    "type_canonical": canonical_type(type_sql),
                    "type_sql": type_sql,
                }
            )
        return {"columns": columns, "kind": "table", "setof": False, "type": None}, body[end:]
    if upper.startswith("SETOF"):
        after = body.lstrip()[5:].lstrip()
        type_sql, remainder = take_type(after)
        return {
            "columns": None,
            "kind": "setof",
            "setof": True,
            "type": display_type(type_sql),
            "type_canonical": canonical_type(type_sql),
            "type_sql": type_sql,
        }, remainder
    type_sql, remainder = take_type(body.lstrip())
    return {
        "columns": None,
        "kind": "scalar",
        "setof": False,
        "type": display_type(type_sql),
        "type_canonical": canonical_type(type_sql),
        "type_sql": type_sql,
    }, remainder


def take_type(text: str) -> tuple[str, str]:
    match = re.match(r"(?:@extschema@\.|pg_catalog\.)?[A-Za-z_][A-Za-z0-9_]*\s*(?:\[\])?", text)
    if not match:
        raise ValueError(f"missing type near {text[:80]!r}")
    return collapse_ws(match.group(0)), text[match.end() :]


def parse_attributes(text: str) -> dict:
    tokens = re.findall(r"[A-Za-z_][A-Za-z0-9_]*|'[^']*'|\"[^\"]*\"", text)
    volatility = None
    strict = False
    parallel = None
    leakproof = False
    index = 0
    while index < len(tokens):
        token = tokens[index].upper()
        if token == "LANGUAGE":
            break
        if token in {"IMMUTABLE", "STABLE", "VOLATILE"}:
            if volatility:
                raise ValueError("multiple volatility markers")
            volatility = token
        elif token == "STRICT":
            strict = True
        elif token == "PARALLEL":
            index += 1
            parallel = tokens[index].upper()
            if parallel not in {"SAFE", "UNSAFE", "RESTRICTED"}:
                raise ValueError(f"bad PARALLEL qualifier {parallel}")
        elif token == "LEAKPROOF":
            leakproof = True
        elif token == "CALLED":
            index += 3
        elif token == "SUPPORT":
            raise ValueError("inline SUPPORT is not expected in this snapshot")
        else:
            raise ValueError(f"unrecognized function attribute {tokens[index]}")
        index += 1
    language = None
    symbol = None
    if index < len(tokens) and tokens[index].upper() == "LANGUAGE":
        language = tokens[index + 1]
    symbol_match = re.search(r"AS\s+'MODULE_PATHNAME'\s*,\s*'([^']+)'", text, re.I)
    if symbol_match:
        symbol = symbol_match.group(1)
    return {
        "language": language,
        "leakproof": leakproof,
        "parallel": parallel,
        "strict": strict,
        "symbol": symbol,
        "volatility": volatility,
    }


def function_identity(name: str, arguments: list[dict]) -> str:
    types = ",".join(argument["type_canonical"] for argument in arguments)
    return f"{name}({types})"


def signature_of(name: str, arguments: list[dict], returns: dict, quoted_name: bool = False) -> str:
    rendered_name = f'"{name}"' if quoted_name else name
    pieces = []
    for argument in arguments:
        if argument["name"] is None:
            piece = argument["type"]
        else:
            shown = f'"{argument["name"]}"' if argument["quoted"] else argument["name"]
            piece = f"{shown} {argument['type']}"
        if argument["mode"]:
            piece = f"{argument['mode']} {piece}"
        if argument["has_default"]:
            piece += f" = {argument['default']}"
        pieces.append(piece)
    if returns["kind"] == "table":
        columns = ", ".join(
            f"{column['name']} {column['type']}" for column in returns["columns"]
        )
        returned = f"TABLE({columns})"
    elif returns["kind"] == "setof":
        returned = f"SETOF {returns['type']}"
    else:
        returned = returns["type"]
    return f"{rendered_name}({', '.join(pieces)}) → {returned}"


def parse_function(raw: str, line: int) -> dict:
    text = collapse_ws(strip_comments(raw))
    match = re.match(r"CREATE\s+(OR\s+REPLACE\s+)?FUNCTION\s+(\S+)\s*\(", text, re.I)
    if not match:
        raise ValueError("not a function: " + text[:120])
    schema, name, quoted = sql_name(match.group(2))
    open_at = text.find("(", match.end() - 1)
    args_raw, after = balanced(text, open_at)
    arguments = parse_args(args_raw)
    returns, remainder = parse_returns(text[after:])
    attributes = parse_attributes(remainder)
    return {
        "acl": [],
        "arguments": arguments,
        "identity": function_identity(name, arguments),
        "language": attributes["language"],
        "leakproof": attributes["leakproof"],
        "line": line,
        "name": name,
        "or_replace": bool(match.group(1)),
        "parallel": attributes["parallel"],
        "quoted": quoted,
        "returns": returns,
        "schema": schema,
        "signature": signature_of(name, arguments, returns),
        "sql_schema": schema,
        "strict": attributes["strict"],
        "support": None,
        "symbol": attributes["symbol"],
        "volatility": attributes["volatility"],
    }


def parse_signature_types(raw: str) -> tuple[str, list[str]]:
    text = collapse_ws(raw)
    open_at = text.find("(")
    schema, name, _quoted = sql_name(text[:open_at].strip())
    args_raw, _end = balanced(text, open_at)
    types = [canonical_type(part) for part in split_top(args_raw)] if args_raw.strip() else []
    if schema and schema not in {"@extschema@", "pg_catalog"}:
        raise ValueError(f"unexpected schema {schema}")
    return name, types


def parse_alter(raw: str, line: int) -> dict:
    text = collapse_ws(strip_comments(raw))
    match = re.match(r"ALTER\s+FUNCTION\s+(.+?)\s+SUPPORT\s+(\S+)\s*$", text, re.I)
    if not match:
        raise ValueError("unrecognized ALTER: " + text)
    name, types = parse_signature_types(match.group(1))
    _schema, support, _quoted = sql_name(match.group(2))
    return {
        "identity": f"{name}({','.join(types)})",
        "kind": "alter_support",
        "line": line,
        "name": name,
        "support": support,
    }


def parse_revoke_or_grant(raw: str, line: int) -> dict:
    text = collapse_ws(strip_comments(raw))
    match = re.match(
        r"(GRANT|REVOKE)\s+(\S+)\s+ON\s+(FUNCTION|TABLE)\s+(\S+?)(?:\((.*)\))?\s+FROM\s+(\S+)\s*$",
        text,
        re.I,
    )
    if not match:
        raise ValueError("unrecognized ACL statement: " + text)
    action, privilege, object_kind, target, args, grantee = match.groups()
    _schema, name, _quoted = sql_name(target)
    types = []
    if args is not None and args.strip():
        types = [canonical_type(part) for part in split_top(args)]
    return {
        "action": action.lower(),
        "grantee": grantee,
        "identity": f"{name}({','.join(types)})" if object_kind.upper() == "FUNCTION" else name,
        "kind": "acl",
        "line": line,
        "name": name,
        "object_kind": object_kind.lower(),
        "privilege": privilege,
    }


def parse_operator(raw: str, line: int) -> dict:
    text = collapse_ws(strip_comments(raw))
    match = re.match(r"CREATE\s+OPERATOR\s+(\S+)\s*\(", text, re.I)
    if not match:
        raise ValueError("not an operator")
    schema, name, _quoted = sql_name(match.group(1))
    body, _end = balanced(text, text.find("(", match.end() - 1))
    options = {}
    for part in split_top(body):
        key, value = (piece.strip() for piece in part.split("=", 1))
        options[key.upper()] = value
    left = display_type(options["LEFTARG"])
    right = display_type(options["RIGHTARG"])
    _pschema, procedure, _quoted = sql_name(options["PROCEDURE"])
    restrict = None
    if "RESTRICT" in options:
        _rschema, restrict, _quoted = sql_name(options["RESTRICT"])
    support = None
    if "SUPPORT" in options:
        _sschema, support, _quoted = sql_name(options["SUPPORT"])
    return {
        "identity": f"{name}({canonical_type(left)},{canonical_type(right)})",
        "left_type": left,
        "left_type_canonical": canonical_type(left),
        "line": line,
        "name": name,
        "procedure": procedure,
        "procedure_support": None,
        "restrict": restrict,
        "right_type": right,
        "right_type_canonical": canonical_type(right),
        "schema": schema,
        "support": support,
    }


def parse_opclass(raw: str, line: int) -> dict:
    text = collapse_ws(strip_comments(raw))
    match = re.match(
        r"CREATE\s+OPERATOR\s+CLASS\s+(\S+)\s+(DEFAULT\s+)?FOR\s+TYPE\s+(\S+)\s+USING\s+(\S+)\s+AS\s+(.*)$",
        text,
        re.I,
    )
    if not match:
        raise ValueError("not an operator class: " + text)
    schema, name, _quoted = sql_name(match.group(1))
    items = []
    for part in split_top(match.group(5)):
        if part.upper().startswith("OPERATOR"):
            item = re.match(
                r"OPERATOR\s+(\d+)\s+(\S+?)\((.*)\)\s*$",
                part,
                re.I,
            )
            if not item:
                raise ValueError("bad opclass operator: " + part)
            op_schema, op_name, _quoted = sql_name(item.group(2))
            arg_types = [display_type(piece) for piece in split_top(item.group(3))]
            items.append(
                {
                    "arguments": arg_types,
                    "arguments_canonical": [canonical_type(piece) for piece in arg_types],
                    "kind": "operator",
                    "name": op_name,
                    "schema": op_schema,
                    "strategy": int(item.group(1)),
                }
            )
        elif part.upper().startswith("STORAGE"):
            items.append({"kind": "storage", "type": display_type(part.split(None, 1)[1])})
        else:
            raise ValueError("unrecognized opclass item: " + part)
    return {
        "access_method": match.group(4),
        "default": bool(match.group(2)),
        "for_type": display_type(match.group(3)),
        "items": items,
        "line": line,
        "name": name,
        "schema": schema,
    }


def parse_type(raw: str, line: int) -> dict:
    text = collapse_ws(strip_comments(raw))
    match = re.match(r"CREATE\s+TYPE\s+(\S+)\s*(.*)$", text, re.I)
    if not match:
        raise ValueError("not a type")
    schema, name, _quoted = sql_name(match.group(1))
    rest = match.group(2).strip()
    options = {}
    shell = rest == ""
    if not shell:
        if not rest.startswith("("):
            raise ValueError("bad CREATE TYPE: " + text)
        body, _end = balanced(rest, 0)
        for part in split_top(body):
            key, value = (piece.strip() for piece in part.split("=", 1))
            value = re.sub(r"/\*.*", "", value).strip()
            options[key.upper()] = value
    return {
        "input": options.get("INPUT"),
        "internallength": options.get("INTERNALLENGTH"),
        "line": line,
        "name": name,
        "options": options,
        "output": options.get("OUTPUT"),
        "schema": schema,
        "shell": shell,
        "storage": options.get("STORAGE"),
    }


def parse_table(raw: str, line: int) -> dict:
    text = strip_comments(raw).strip()
    match = re.match(r"CREATE\s+TABLE\s+(\S+)\s*\(", text, re.I | re.S)
    if not match:
        raise ValueError("not a table")
    schema, name, _quoted = sql_name(match.group(1))
    body, _end = balanced(text, text.find("(", match.end() - 1))
    columns = []
    for part in split_top(body):
        part = collapse_ws(part)
        column_name, remainder = part.split(None, 1)
        constraints = []
        default = None
        check = None
        not_null = bool(re.search(r"\bNOT\s+NULL\b", remainder, re.I))
        primary_key = bool(re.search(r"\bPRIMARY\s+KEY\b", remainder, re.I))
        default_match = re.search(r"\bDEFAULT\b", remainder, re.I)
        type_sql = remainder
        if default_match:
            type_sql = remainder[: default_match.start()].strip()
            default = collapse_ws(remainder[default_match.end() :])
            for marker in ("CHECK", "NOT", "PRIMARY", "NULL", "UNIQUE"):
                found = re.search(rf"\b{marker}\b", default, re.I)
                if found:
                    default = default[: found.start()].strip()
                    break
        check_match = re.search(r"\bCHECK\s*\((.*)\)\s*$", remainder, re.I)
        if check_match:
            check = collapse_ws(check_match.group(1))
        type_sql = re.sub(
            r"\b(PRIMARY\s+KEY|NOT\s+NULL|NULL|UNIQUE)\b",
            "",
            type_sql,
            flags=re.I,
        )
        type_sql = collapse_ws(type_sql)
        if primary_key:
            constraints.append("PRIMARY KEY")
        if not_null:
            constraints.append("NOT NULL")
        if check:
            constraints.append(f"CHECK ({check})")
        columns.append(
            {
                "check": check,
                "constraints": constraints,
                "default": default,
                "name": column_name,
                "not_null": not_null,
                "primary_key": primary_key,
                "type": display_type(type_sql),
                "type_sql": type_sql,
            }
        )
    return {
        "acl": [],
        "columns": columns,
        "line": line,
        "name": name,
        "schema": schema,
    }


def parse_view(raw: str, line: int) -> dict:
    text = strip_comments(raw).strip()
    match = re.match(r"CREATE\s+VIEW\s+(\S+)\s+", text, re.I | re.S)
    if not match:
        raise ValueError("not a view")
    schema, name, _quoted = sql_name(match.group(1))
    rest = text[match.end() - 1 :].lstrip()
    options = {}
    if rest.upper().startswith("WITH"):
        open_at = rest.find("(")
        body, end = balanced(rest, open_at)
        for part in split_top(body):
            key, value = (piece.strip() for piece in part.split("=", 1))
            options[key.lower()] = json.loads(value.lower()) if value.lower() in {"true", "false"} else value
        rest = rest[end:].lstrip()
    if not rest.upper().startswith("AS"):
        raise ValueError("view is missing AS")
    query = collapse_ws(rest[2:])
    return {
        "column_comments": {},
        "line": line,
        "name": name,
        "options": options,
        "query": query,
        "schema": schema,
    }


def parse_comment(raw: str, line: int) -> dict:
    text = strip_comments(raw).strip()
    match = re.match(
        r"COMMENT\s+ON\s+COLUMN\s+(\S+)\s+IS\s+('(?:''|[^'])*')\s*$",
        collapse_ws(text),
        re.I,
    )
    if not match:
        raise ValueError("unrecognized COMMENT: " + collapse_ws(text)[:160])
    target = match.group(1)
    pieces = target.split(".")
    if len(pieces) == 3:
        schema, view, column = pieces
    elif len(pieces) == 2:
        schema, view, column = None, pieces[0], pieces[1]
    else:
        raise ValueError("COMMENT target is not schema.relation.column: " + target)
    return {
        "column": column,
        "kind": "comment",
        "line": line,
        "schema": schema,
        "text": decode_sql_string(match.group(2)),
        "view": view,
    }


def parse_access_method(raw: str, line: int) -> dict:
    text = collapse_ws(strip_comments(raw))
    match = re.match(
        r"CREATE\s+ACCESS\s+METHOD\s+(\S+)\s+TYPE\s+(\S+)\s+HANDLER\s+(\S+)\s*$",
        text,
        re.I,
    )
    if not match:
        raise ValueError("not an access method: " + text)
    _schema, handler, _quoted = sql_name(match.group(3))
    return {
        "handler": handler,
        "line": line,
        "name": match.group(1),
        "type": match.group(2).lower(),
    }


def classify_sql(text: str, path: Path) -> dict:
    catalog = {
        "access_method": None,
        "functions": [],
        "operator_classes": [],
        "operators": [],
        "statements": 0,
        "tables": [],
        "types": [],
        "views": [],
    }
    pending_support = []
    pending_acl = []
    pending_comments = []
    for line, raw in split_statements(text):
        catalog["statements"] += 1
        flat = collapse_ws(strip_comments(raw))
        kind = flat.split()[0].upper() if flat else ""
        head = " ".join(flat.split()[:4]).upper()
        if head.startswith("CREATE OR REPLACE FUNCTION") or head.startswith("CREATE FUNCTION"):
            catalog["functions"].append(parse_function(raw, line))
        elif head.startswith("CREATE ACCESS METHOD"):
            if catalog["access_method"]:
                raise ValueError("multiple access methods")
            catalog["access_method"] = parse_access_method(raw, line)
        elif head.startswith("CREATE OPERATOR CLASS"):
            catalog["operator_classes"].append(parse_opclass(raw, line))
        elif head.startswith("CREATE OPERATOR"):
            catalog["operators"].append(parse_operator(raw, line))
        elif head.startswith("CREATE TYPE"):
            catalog["types"].append(parse_type(raw, line))
        elif head.startswith("CREATE TABLE"):
            catalog["tables"].append(parse_table(raw, line))
        elif head.startswith("CREATE VIEW"):
            catalog["views"].append(parse_view(raw, line))
        elif head.startswith("ALTER FUNCTION"):
            pending_support.append(parse_alter(raw, line))
        elif kind in {"REVOKE", "GRANT"}:
            pending_acl.append(parse_revoke_or_grant(raw, line))
        elif kind == "COMMENT":
            pending_comments.append(parse_comment(raw, line))
        else:
            raise ValueError(f"{path}:{line}: unrecognized statement: {flat[:240]}")
    functions = {function["identity"]: function for function in catalog["functions"]}
    if len(functions) != len(catalog["functions"]):
        raise ValueError("duplicate function identity in snapshot")
    for alter in pending_support:
        function = functions.get(alter["identity"])
        if function is None:
            raise ValueError(f"SUPPORT target not found: {alter['identity']}")
        if function["support"]:
            raise ValueError(f"duplicate SUPPORT on {alter['identity']}")
        function["support"] = alter["support"]
        function["support_line"] = alter["line"]
    for acl in pending_acl:
        entry = {
            "action": acl["action"],
            "grantee": acl["grantee"],
            "line": acl["line"],
            "privilege": acl["privilege"],
        }
        if acl["object_kind"] == "function":
            function = functions.get(acl["identity"])
            if function is None:
                raise ValueError(f"ACL target not found: {acl['identity']}")
            function["acl"].append(entry)
        elif acl["object_kind"] == "table":
            matches = [table for table in catalog["tables"] if table["name"] == acl["name"]]
            if len(matches) != 1:
                raise ValueError(f"ACL table not found: {acl['name']}")
            matches[0]["acl"].append(entry)
        else:
            raise ValueError(f"unhandled ACL object {acl['object_kind']}")
    views = {view["name"]: view for view in catalog["views"]}
    for comment in pending_comments:
        view = views.get(comment["view"])
        if view is None:
            raise ValueError(f"comment view not found: {comment['view']}")
        view["column_comments"][comment["column"]] = comment["text"]
    for operator in catalog["operators"]:
        identity = function_identity(
            operator["procedure"],
            [
                {"type_canonical": operator["left_type_canonical"]},
                {"type_canonical": operator["right_type_canonical"]},
            ],
        )
        procedure = functions.get(identity)
        if procedure is None:
            raise ValueError(f"operator procedure not found: {identity}")
        operator["procedure_support"] = procedure["support"]
        operator["procedure_volatility"] = procedure["volatility"]
        operator["procedure_strict"] = procedure["strict"]
        operator["procedure_parallel"] = procedure["parallel"]
    merged: dict[str, dict] = {}
    order: list[str] = []
    for item in catalog["types"]:
        current = merged.get(item["name"])
        if current is None:
            merged[item["name"]] = item
            order.append(item["name"])
            continue
        if item["shell"]:
            current["shell"] = True
            continue
        item["shell"] = current["shell"] or item["shell"]
        item["shell_line"] = current["line"]
        merged[item["name"]] = item
    catalog["types"] = [merged[name] for name in order]
    catalog["source"] = path.as_posix()
    return catalog


def rust_cstring(text: str) -> str | None:
    stripped = text.strip()
    if not stripped.startswith('c"'):
        return None
    index = 2
    chars: list[str] = []
    escapes = {"n": "\n", "t": "\t", "r": "\r", "\\": "\\", '"': '"', "'": "'", "0": "\0"}
    while index < len(stripped):
        char = stripped[index]
        if char == "\\":
            chars.append(escapes.get(stripped[index + 1], stripped[index + 1]))
            index += 2
            continue
        if char == '"':
            return "".join(chars)
        chars.append(char)
        index += 1
    raise ValueError("unterminated Rust c-string")


def eval_numeric(expr: str, env: dict[str, tuple[str, object]]) -> object:
    text = collapse_ws(expr)
    if text in {"true", "false"}:
        return text == "true"
    from_match = re.fullmatch(r"f64::from\((.+)\)", text)
    if from_match:
        inner = from_match.group(1)
        _typ, value = lookup_const(inner, env)
        return float(struct_f32(value))
    cast = re.fullmatch(r"(.+)\s+as\s+[A-Za-z0-9_:]+", text)
    if cast:
        return eval_numeric(cast.group(1), env)
    if text == "i32::MAX":
        return 2_147_483_647
    if text in env:
        return env[text][1]
    if re.fullmatch(r"\d[\d_]*", text):
        return int(text.replace("_", ""))
    if re.fullmatch(r"\d[\d_]*\.\d+", text) or re.fullmatch(r"\d[\d_]*\.\d+e[+-]?\d+", text, re.I):
        return float(text.replace("_", ""))
    if re.fullmatch(r"[A-Za-z0-9_:.+\-*/() ]+", text) and re.search(r"[+*/-]", text):
        return eval_arith(text, env)
    found = lookup_const(text, env)
    return found[1]


def lookup_const(expr: str, env: dict[str, tuple[str, object]]) -> tuple[str, object]:
    if expr in env:
        return env[expr]
    tail = expr.split("::")[-1]
    if tail in env:
        return env[tail]
    raise ValueError(f"unresolved Rust expression: {expr}")


def struct_f32(value: object) -> float:
    import struct

    return struct.unpack(">f", struct.pack(">f", float(value)))[0]


def eval_arith(expr: str, env: dict[str, tuple[str, object]]) -> int | float:
    def replace(match: re.Match[str]) -> str:
        token = match.group(0)
        if token in env:
            return repr(env[token][1])
        if re.fullmatch(r"\d[\d_]*", token):
            return token.replace("_", "")
        return token

    substituted = re.sub(r"[A-Za-z_][A-Za-z0-9_:]*|\d[\d_]*", replace, expr)
    if not re.fullmatch(r"[\d\s.+\-*/()]+", substituted):
        raise ValueError(f"cannot evaluate {expr} -> {substituted}")
    value = _arith(substituted.replace(" ", ""))
    if isinstance(value, float) and value.is_integer():
        return int(value)
    return value


def _arith(expr: str) -> int | float:
    index = 0

    def parse_expr() -> int | float:
        nonlocal index
        value = parse_term()
        while index < len(expr) and expr[index] in "+-":
            op = expr[index]
            index += 1
            rhs = parse_term()
            value = value + rhs if op == "+" else value - rhs
        return value

    def parse_term() -> int | float:
        nonlocal index
        value = parse_factor()
        while index < len(expr) and expr[index] in "*/":
            op = expr[index]
            index += 1
            rhs = parse_factor()
            value = value * rhs if op == "*" else value / rhs
        return value

    def parse_factor() -> int | float:
        nonlocal index
        if expr[index] == "(":
            index += 1
            value = parse_expr()
            index += 1
            return value
        start = index
        while index < len(expr) and expr[index] not in "+-*/()":
            index += 1
        token = expr[start:index]
        return float(token) if "." in token else int(token)

    value = parse_expr()
    if index != len(expr):
        raise ValueError(f"trailing arithmetic in {expr}")
    return value


def harvest_consts(text: str, env: dict[str, tuple[str, object]]) -> None:
    impl: str | None = None
    depth = 0
    for match in re.finditer(
        r"\bimpl\s+([A-Za-z0-9_]+)\s*\{|\b(?:pub(?:\([^)]*\))?\s+)?const\s+([A-Z0-9_]+)\s*:\s*([A-Za-z0-9_:]+)\s*=\s*([^;]+);",
        text,
    ):
        if match.group(1):
            impl = match.group(1)
            continue
        name, typ, expr = match.group(2), match.group(3), match.group(4).strip()
        if typ in {"i32", "u32", "usize", "u8", "f32", "f64"} or typ.startswith("f"):
            try:
                value = eval_numeric(expr, env)
            except ValueError:
                value = expr
            env[name] = (typ, value)
            if impl:
                env[f"{impl}::{name}"] = (typ, value)
        else:
            env[name] = (typ, expr)
    if depth:
        return


def parse_enum_members(text: str) -> dict[str, list[tuple[str, str]]]:
    tables: dict[str, list[tuple[str, str]]] = {}
    for match in re.finditer(r"enum_members!\s*\(", text):
        body, _end = balanced(text, match.end() - 1)
        parts = split_top(body)
        name = parts[0].strip()
        if not re.fullmatch(r"[A-Z][A-Z0-9_]*", name):
            continue
        members = []
        for part in parts[1:]:
            item = re.match(r"\(\s*\"([^\"]+)\"\s*,\s*([A-Z0-9_]+)\s*\)", collapse_ws(part))
            if not item:
                raise ValueError("bad enum_members entry: " + part)
            members.append((item.group(1), item.group(2)))
        tables[name] = members
    return tables


def parse_reloptions(text: str, path: Path, env: dict[str, tuple[str, object]]) -> list[dict]:
    init = extract_fn(text, "init")
    lock_match = re.search(r"let\s+lock\s*=\s*pg_sys::(\w+)", init)
    lock = lock_match.group(1) if lock_match else None
    tables = parse_enum_members(text)
    ignored = ignored_reloptions(text)
    warning_match = re.search(r'pgrx::warning!\s*\(\s*"([^"]+)"', text, re.S)
    if not warning_match:
        raise ValueError("TIN ignore warning was not found")
    warning = warning_match.group(1)
    error_match = re.search(r'pgrx::error!\(\s*"([^"]+)"\s*\)', text)
    alter_error = error_match.group(1) if error_match else None
    alter_subtypes = list(dict.fromkeys(re.findall(r"AT_(?:Set|Replace|Reset)RelOptions", text)))
    options: list[dict] = []
    consumed = mask_loops(init, options, tables, env, lock, path, warning, ignored)
    for match in re.finditer(r"pg_sys::(add_(?:int|real|enum|string)_reloption)\s*\(", consumed):
        body, _end = balanced(consumed, match.end() - 1)
        args = split_top(body)
        kind = match.group(1).split("_")[1]
        name = rust_cstring(args[1])
        if name is None:
            raise ValueError("reloption call was not expanded: " + args[1])
        options.append(
            reloption_from_call(
                kind,
                name,
                args,
                tables,
                env,
                lock,
                path,
                line_of(text, match.start()),
                warning,
                ignored,
                alter_error,
                alter_subtypes,
            )
        )
    parse_names = re.findall(r'c"([A-Za-z0-9_]+)"\.as_ptr\(\),\s*pg_sys::relopt_type', text)
    if set(parse_names) != {option["name"] for option in options}:
        raise ValueError(
            "reloption registration does not match amoptions parse entries: "
            f"{sorted(set(parse_names) ^ {option['name'] for option in options})}"
        )
    if len(options) != len({option["name"] for option in options}):
        raise ValueError("duplicate reloption name")
    if not ignored <= {option["name"] for option in options}:
        raise ValueError(f"ignored reloptions are not registered: {sorted(ignored)}")
    options.sort(key=lambda option: text.find(f'c"{option["name"]}"'))
    for option in options:
        option["line"] = line_of(text, text.find(f'c"{option["name"]}"'))
    return options


def ignored_reloptions(text: str) -> set[str]:
    for match in re.finditer(r"matches!\s*\(", text):
        body, _end = balanced(text, match.end() - 1)
        if "name.as_ref()" not in body:
            continue
        names = re.findall(r'"([A-Za-z0-9_]+)"', body)
        if not names:
            raise ValueError("ignored-reloption match list is empty")
        return set(names)
    raise ValueError("ignored-reloption match list not found")


def extract_fn(text: str, name: str) -> str:
    match = re.search(rf"\bfn\s+{name}\s*\(", text)
    if not match:
        raise ValueError(f"fn {name} not found")
    brace = text.find("{", match.end())
    body, _end = balanced(text, brace, "{", "}")
    return body


def mask_loops(init: str, options: list[dict], tables, env, lock, path, warning, ignored) -> str:
    pattern = re.compile(
        r"for\s*\(([^)]*)\)\s+in\s*\[(.*?)\]\s*\{(.*?)\n\s*\}",
        re.S,
    )
    pieces = []
    cursor = 0
    for match in pattern.finditer(init):
        pieces.append(init[cursor : match.start()])
        pieces.append(" " * (match.end() - match.start()))
        cursor = match.end()
        bindings = [part.strip() for part in match.group(1).split(",")]
        body = match.group(3)
        call = re.search(r"pg_sys::(add_(?:int|real)_reloption)\s*\(", body)
        if not call:
            raise ValueError("reloption loop has no registration call")
        call_body, _end = balanced(body, call.end() - 1)
        template = split_top(call_body)
        kind = call.group(1).split("_")[1]
        for tuple_text in split_top(match.group(2)):
            if not tuple_text.strip():
                continue
            values = split_top(tuple_text.strip()[1:-1])
            bound = dict(zip(bindings, values, strict=True))
            args = [substitute_binding(arg, bound) for arg in template]
            name = rust_cstring(args[1])
            if not name:
                raise ValueError("loop reloption has no name")
            options.append(
                reloption_from_call(
                    kind,
                    name,
                    args,
                    tables,
                    env,
                    lock,
                    path,
                    line_of(init, match.start()),
                    warning,
                    ignored,
                    None,
                    [],
                )
            )
    pieces.append(init[cursor:])
    return "".join(pieces)


def substitute_binding(arg: str, bound: dict[str, str]) -> str:
    stripped = arg.strip()
    for name, value in bound.items():
        if stripped == f"{name}.as_ptr()" or stripped == name:
            return value
    return arg


def reloption_from_call(
    kind, name, args, tables, env, lock, path, line, warning, ignored, alter_error, alter_subtypes
) -> dict:
    description = rust_cstring(args[2])
    option = {
        "description": description,
        "ignored": name in ignored,
        "kind": kind,
        "line": line,
        "lock": lock,
        "name": name,
        "source": path.as_posix(),
    }
    if name in ignored:
        option["tin_ignore_warning"] = warning
    if kind in {"int", "real"}:
        default_expr = collapse_ws(args[3])
        min_expr = collapse_ws(args[4])
        max_expr = collapse_ws(args[5])
        option.update(
            {
                "default": eval_numeric(default_expr, env),
                "default_expression": default_expr,
                "maximum": eval_numeric(max_expr, env),
                "maximum_expression": max_expr,
                "minimum": eval_numeric(min_expr, env),
                "minimum_expression": min_expr,
            }
        )
        if kind == "real" and default_expr.startswith("f64::from("):
            literal = lookup_const(re.fullmatch(r"f64::from\((.+)\)", default_expr).group(1), env)
            option["source_literal"] = literal[1]
            option["registered_value"] = option["default"]
    elif kind == "enum":
        table_match = re.search(r"([A-Z0-9_]+)", args[3])
        table = table_match.group(1)
        default_const = collapse_ws(args[4])
        members = []
        default_name = None
        default_ordinal = None
        default_value = env[default_const][1]
        for ordinal, (label, const_name) in enumerate(tables[table]):
            value = env[const_name][1]
            members.append({"ordinal": ordinal, "symbol": const_name, "value": label, "symbol_value": value})
            if value == default_value:
                default_name = label
                default_ordinal = ordinal
        if default_name is None:
            raise ValueError(f"enum default {default_const} is not in {table}")
        option.update(
            {
                "default": default_name,
                "default_expression": default_const,
                "default_ordinal": default_ordinal,
                "domain": [member["value"] for member in members],
                "enum_table": table,
                "members": members,
            }
        )
    elif kind == "string":
        option.update(
            {
                "default": None,
                "default_expression": collapse_ws(args[3]),
                "validator": None if collapse_ws(args[4]) == "None" else collapse_ws(args[4]),
            }
        )
        if name == "field_weights":
            option["alter_error"] = alter_error
            option["alter_rejected"] = True
            option["alter_subtypes"] = alter_subtypes
    else:
        raise ValueError(f"unknown reloption kind {kind}")
    return option


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def parse_gucs(files: list[tuple[Path, str]], env: dict[str, tuple[str, object]]) -> list[dict]:
    gucs = []
    for path, text in files:
        enums = parse_rust_enums(text)
        settings = parse_guc_settings(text, env, enums)
        for match in re.finditer(r"GucRegistry::define_(bool|int|enum)_guc\s*\(", text):
            body, _end = balanced(text, match.end() - 1)
            args = split_top(body)
            kind = match.group(1)
            name = rust_cstring(args[0])
            setting_name = args[3].strip().lstrip("&").strip()
            setting = settings[setting_name]
            context = args[-2].strip().split("::")[-1]
            flags = collapse_ws(args[-1])
            guc = {
                "context": context,
                "default": setting["default"],
                "default_expression": setting["default_expression"],
                "flags": flags,
                "long_description": rust_cstring(args[2]),
                "name": name,
                "registered_in": path.as_posix(),
                "registered_in_function": "init",
                "registered_when": registration_condition(text, match.start()),
                "setting": setting_name,
                "short_description": rust_cstring(args[1]),
                "source_line": line_of(text, match.start()),
                "type": kind,
            }
            if kind == "int":
                guc["minimum_expression"] = collapse_ws(args[4])
                guc["maximum_expression"] = collapse_ws(args[5])
                guc["minimum"] = eval_numeric(guc["minimum_expression"], env)
                guc["maximum"] = eval_numeric(guc["maximum_expression"], env)
            elif kind == "enum":
                enum_name = setting["rust_type"]
                enum = enums[enum_name]
                guc["enum"] = enum_name
                guc["enum_name_rule"] = PGRX_ENUM_NAME_RULE
                guc["values"] = [variant["config_name"] for variant in enum if not variant["hidden"]]
                guc["variants"] = enum
                guc["default"] = next(
                    variant["config_name"]
                    for variant in enum
                    if variant["name"] == setting["default"]
                )
            gucs.append(guc)
    if len(gucs) != len({guc["name"] for guc in gucs}):
        raise ValueError("duplicate GUC name")
    return gucs


def registration_condition(text: str, offset: int) -> str:
    fn_at = text.rfind("fn init()", 0, offset)
    window = text[fn_at:offset]
    if "process_shared_preload_libraries_in_progress" in window and re.search(
        r"\breturn\b", window
    ):
        return "shared_preload_libraries"
    return "init"


def parse_guc_settings(text: str, env, enums) -> dict[str, dict]:
    settings = {}
    pattern = re.compile(
        r"static\s+([A-Z0-9_]+)\s*:\s*GucSetting\s*<\s*([^>]+)\s*>\s*=\s*GucSetting\s*::\s*<\s*[^>]+>\s*::\s*new\s*\((.*?)\)\s*;",
        re.S,
    )
    for match in pattern.finditer(text):
        name, typ, expr = match.group(1), collapse_ws(match.group(2)), collapse_ws(match.group(3))
        if typ in enums or "::" in expr and expr.split("::")[0] in enums:
            default = expr.split("::")[-1]
        else:
            default = eval_numeric(expr, env)
        settings[name] = {
            "default": default,
            "default_expression": expr,
            "rust_type": typ,
        }
    return settings


def parse_rust_enums(text: str) -> dict[str, list[dict]]:
    enums = {}
    for match in re.finditer(r"enum\s+([A-Za-z0-9_]+)\s*\{", text):
        if "PostgresGucEnum" not in text[max(0, match.start() - 200) : match.start()]:
            continue
        body, _end = balanced(text, match.end() - 1, "{", "}")
        variants = []
        for ordinal, part in enumerate(split_top(body)):
            if not part.strip():
                continue
            name_attr = re.search(r'#\[\s*name\s*=\s*c"([^"]+)"\s*\]', part)
            hidden = bool(re.search(r"#\[\s*hidden\s*=\s*true\s*\]", part))
            variant = re.findall(r"\b([A-Z][A-Za-z0-9_]*)\b", part)
            if not variant:
                raise ValueError("enum variant has no name: " + part)
            variant_name = variant[-1]
            variants.append(
                {
                    "config_name": name_attr.group(1) if name_attr else variant_name,
                    "hidden": hidden,
                    "name": variant_name,
                    "ordinal": ordinal,
                }
            )
        enums[match.group(1)] = variants
    return enums


def validation_notes(files: list[tuple[Path, str]]) -> list[dict]:
    notes = []
    for path, text in files:
        for match in re.finditer(r'pgrx::error!\(\s*"([^"]+)"', text):
            message = match.group(1)
            if any(
                needle in message
                for needle in ("field_weights", "key columns", "expression keys")
            ):
                notes.append(
                    {
                        "message": message,
                        "source": path.as_posix(),
                        "source_line": line_of(text, match.start()),
                    }
                )
    return notes


def parse_control(text: str) -> dict:
    values = {}
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        if not line or "=" not in line:
            continue
        key, value = (piece.strip() for piece in line.split("=", 1))
        if value.startswith("'") and value.endswith("'"):
            value = value[1:-1]
        elif value in {"true", "false"}:
            value = value == "true"
        values[key] = value
    return values


def pg_floor(cargo: str) -> int:
    majors = [int(item) for item in re.findall(r"(?m)^pg(\d+)\s*=", cargo)]
    if not majors:
        raise ValueError("postgres/Cargo.toml has no pgNN features")
    return min(majors)


def pgrx_version(cargo: str) -> str:
    match = re.search(r'(?m)^pgrx\s*=\s*"=?([^"]+)"', cargo)
    if not match:
        raise ValueError("pgrx version not found")
    return match.group(1)


def find_rmgr_header() -> Path:
    candidates = []
    try:
        included = subprocess.check_output(
            ["pg_config", "--includedir-server"], text=True, stderr=subprocess.DEVNULL
        ).strip()
        if included:
            candidates.append(Path(included) / "access/rmgr.h")
    except (OSError, subprocess.CalledProcessError):
        pass
    candidates.extend(
        [
            Path("/opt/homebrew/include/postgresql@17/server/access/rmgr.h"),
            Path("/usr/include/postgresql/17/server/access/rmgr.h"),
            Path("/usr/include/postgresql/server/access/rmgr.h"),
        ]
    )
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise ValueError("access/rmgr.h not found; cannot resolve RM_MIN_CUSTOM_ID")


def pg_macros(header: Path) -> dict[str, int]:
    defined = {"UINT8_MAX": "255", "INT32_MAX": "2147483647"}
    for line in header.read_text(encoding="utf-8").splitlines():
        match = re.match(r"\s*#define\s+(RM_[A-Z0-9_]+)\s+(.+?)(?:\s*/\*.*)?$", line)
        if match:
            defined[match.group(1)] = match.group(2).strip()
    needed = {}
    for name in ("RM_MIN_CUSTOM_ID", "RM_MAX_CUSTOM_ID"):
        needed[name] = resolve_c(defined[name], defined, set())
    return needed


def resolve_c(expr: str, defined: dict[str, str], stack: set[str]) -> int:
    expr = expr.strip()
    if re.fullmatch(r"\d+", expr):
        return int(expr)
    if expr in defined and expr not in stack:
        return resolve_c(defined[expr], defined, stack | {expr})
    if re.fullmatch(r"[A-Z0-9_+\-*/() ]+", expr):
        substituted = re.sub(
            r"[A-Z_][A-Z0-9_]*",
            lambda item: str(resolve_c(item.group(0), defined, stack)),
            expr,
        )
        return int(eval_arith(substituted, {}))
    raise ValueError(f"unresolved C macro expression: {expr}")


def qualify(schema: str | None, extension_schema: str) -> str:
    if schema in {None, "@extschema@"}:
        return extension_schema
    if schema == "pg_catalog":
        return "pg_catalog"
    return schema


def build_manifest(root: Path) -> dict:
    cargo = read(root, CARGO_PATH)
    control = parse_control(read(root, CONTROL_PATH))
    extension_schema = control["schema"]
    env: dict[str, tuple[str, object]] = {}
    for path in CONST_PATHS:
        harvest_consts(read(root, path), env)
    harvest_consts(read(root, OPTIONS_PATH), env)
    for path in GUC_PATHS:
        harvest_consts(read(root, path), env)
    header = find_rmgr_header()
    macros = pg_macros(header)
    env["pg_sys::RM_MIN_CUSTOM_ID"] = ("i32", macros["RM_MIN_CUSTOM_ID"])
    env["pg_sys::RM_MAX_CUSTOM_ID"] = ("i32", macros["RM_MAX_CUSTOM_ID"])
    sql = classify_sql(read(root, SQL_PATH), SQL_PATH)
    for function in sql["functions"]:
        function["schema"] = qualify(function["schema"], extension_schema)
        function["source"] = SQL_PATH.as_posix()
    for table in sql["tables"]:
        table["schema"] = qualify(table["schema"], extension_schema)
        table["source"] = SQL_PATH.as_posix()
    for view in sql["views"]:
        view["schema"] = qualify(view["schema"], extension_schema)
        view["source"] = SQL_PATH.as_posix()
    for item in sql["types"]:
        item["schema"] = qualify(item["schema"], extension_schema)
        item["source"] = SQL_PATH.as_posix()
    for item in sql["operator_classes"]:
        item["schema"] = qualify(item["schema"], extension_schema)
        item["source"] = SQL_PATH.as_posix()
    for item in sql["operators"]:
        item["source"] = SQL_PATH.as_posix()
    if sql["access_method"]:
        sql["access_method"]["source"] = SQL_PATH.as_posix()
    guc_files = [(path, read(root, path)) for path in GUC_PATHS]
    options_text = read(root, OPTIONS_PATH)
    reloptions = parse_reloptions(options_text, OPTIONS_PATH, env)
    for option in reloptions:
        if option["name"] == "field_weights":
            option["validation_errors"] = [
                note
                for note in validation_notes(guc_files)
                if "field_weights" in note["message"]
            ]
    manifest = {
        "access_method": sql["access_method"],
        "extension": {
            "control": CONTROL_PATH.as_posix(),
            "default_version": control.get("default_version"),
            "name": "stannum",
            "relocatable": control.get("relocatable"),
            "schema": extension_schema,
            "superuser": control.get("superuser"),
        },
        "functions": sql["functions"],
        "gucs": parse_gucs(guc_files, env),
        "index_limit_errors": [
            note
            for note in validation_notes(guc_files)
            if "field_weights" not in note["message"]
        ],
        "operator_classes": sql["operator_classes"],
        "operators": sql["operators"],
        "pg_version_floor": {
            "evidence": "lowest pgNN feature in postgres/Cargo.toml",
            "major": pg_floor(cargo),
        },
        "pgrx": {"enum_name_rule": PGRX_ENUM_NAME_RULE, "version": pgrx_version(cargo)},
        "reloptions": reloptions,
        "sources": {
            "constants": [path.as_posix() for path in CONST_PATHS],
            "control": CONTROL_PATH.as_posix(),
            "gucs": [path.as_posix() for path in GUC_PATHS],
            "pg_macros": "access/rmgr.h",
            "reloptions": OPTIONS_PATH.as_posix(),
            "sql": SQL_PATH.as_posix(),
        },
        "sql_statements_consumed": sql["statements"],
        "tables": sql["tables"],
        "types": sql["types"],
        "views": sql["views"],
    }
    return json.loads(dumps(manifest))


def yn(value) -> str:
    if value is None:
        return "not specified"
    if value is True:
        return "yes"
    if value is False:
        return "no"
    return str(value)


def show(value) -> str:
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    return str(value)


def render_markdown(manifest: dict) -> str:
    lines = [
        "<!-- Generated by tools/generate_tool_contract.py. Do not edit. -->",
        "",
        "# Stannum tool contract",
        "",
        "Generated from the extension SQL snapshot, reloption registrations, and GUC",
        "registrations listed in `docs/tool-contract.manifest.json`. This file explains",
        "that manifest. The snapshot and manifest are authoritative; change the sources",
        "and regenerate. Nothing here is copied from the design summary.",
        "",
        f"PostgreSQL major floor: **{manifest['pg_version_floor']['major']}**",
        f"({manifest['pg_version_floor']['evidence']}).",
        f"Extension `{manifest['extension']['name']}` schema `{manifest['extension']['schema']}`,",
        f"default version `{manifest['extension']['default_version']}`.",
        f"SQL statements consumed: {manifest['sql_statements_consumed']}.",
        "",
        "## Access method",
        "",
    ]
    access = manifest["access_method"]
    lines.append(
        f"`{access['name']}` TYPE {access['type'].upper()} HANDLER `{access['handler']}`"
        f" ({access['source']}:{access['line']})."
    )
    lines.extend(["", "## Types", ""])
    for item in manifest["types"]:
        lines.append(f"### `{item['schema']}.{item['name']}`")
        lines.append("")
        lines.append(f"- internallength: {item.get('internallength')}")
        lines.append(f"- storage: {item.get('storage')}")
        lines.append(f"- input: `{item.get('input')}`")
        lines.append(f"- output: `{item.get('output')}`")
        lines.append(f"- shell declaration: {yn(item.get('shell'))}")
        lines.append(f"- source: {item['source']}:{item['line']}")
        lines.append("")
    lines.extend(["## Operators", ""])
    for item in manifest["operators"]:
        lines.append(
            f"### `{item['schema']}.{item['name']}` ({item['left_type']}, {item['right_type']})"
        )
        lines.append("")
        lines.append(f"- procedure: `{item['procedure']}`")
        lines.append(f"- restrict: `{item['restrict']}`")
        lines.append(f"- operator SUPPORT clause: {item['support'] or 'none'}")
        lines.append(f"- procedure SUPPORT: `{item['procedure_support'] or 'none'}`")
        lines.append(
            f"- procedure volatility/strict/parallel: {item['procedure_volatility'] or 'not specified'}"
            f" / {yn(item['procedure_strict'])} / {item['procedure_parallel'] or 'not specified'}"
        )
        lines.append(f"- source: {item['source']}:{item['line']}")
        lines.append("")
    lines.extend(["## Operator classes", ""])
    for item in manifest["operator_classes"]:
        lines.append(f"### `{item['schema']}.{item['name']}`")
        lines.append("")
        lines.append(
            f"DEFAULT={yn(item['default'])} FOR TYPE `{item['for_type']}` USING `{item['access_method']}`."
        )
        for entry in item["items"]:
            if entry["kind"] == "operator":
                args = ", ".join(entry["arguments"])
                lines.append(
                    f"- operator {entry['strategy']}: `{entry['schema']}.{entry['name']}({args})`"
                )
            else:
                lines.append(f"- storage: `{entry['type']}`")
        lines.append(f"- source: {item['source']}:{item['line']}")
        lines.append("")
    lines.extend(["## Functions", ""])
    lines.append("Properties are those written in the snapshot. An absent volatility or")
    lines.append("parallel marker is not filled in from PostgreSQL defaults. `acl` lists")
    lines.append("only GRANT/REVOKE statements in the snapshot.")
    lines.append("")
    for function in manifest["functions"]:
        lines.append(f"### `{function['signature']}`")
        lines.append("")
        lines.append(f"- schema: `{function['schema']}`")
        lines.append(f"- volatility: {function['volatility'] or 'not specified'}")
        lines.append(f"- strict: {yn(function['strict'])}")
        lines.append(f"- parallel: {function['parallel'] or 'not specified'}")
        lines.append(f"- support: `{function['support']}`" if function["support"] else "- support: none")
        if function["acl"]:
            for acl in function["acl"]:
                lines.append(
                    f"- acl: {acl['action'].upper()} {acl['privilege']} FROM {acl['grantee']}"
                )
        else:
            lines.append("- acl: none in snapshot")
        lines.append(f"- language: {function['language']}")
        lines.append(f"- symbol: `{function['symbol']}`")
        lines.append(f"- source: {function['source']}:{function['line']}")
        lines.append("")
    lines.extend(["## Tables", ""])
    for table in manifest["tables"]:
        lines.append(f"### `{table['schema']}.{table['name']}`")
        lines.append("")
        for column in table["columns"]:
            extra = []
            if column["primary_key"]:
                extra.append("PRIMARY KEY")
            if column["not_null"]:
                extra.append("NOT NULL")
            if column["default"] is not None:
                extra.append(f"DEFAULT {column['default']}")
            if column["check"]:
                extra.append(f"CHECK ({column['check']})")
            suffix = f" {' '.join(extra)}" if extra else ""
            lines.append(f"- `{column['name']}` {column['type']}{suffix}")
        if table["acl"]:
            for acl in table["acl"]:
                lines.append(
                    f"- acl: {acl['action'].upper()} {acl['privilege']} FROM {acl['grantee']}"
                )
        lines.append(f"- source: {table['source']}:{table['line']}")
        lines.append("")
    lines.extend(["## Views", ""])
    for view in manifest["views"]:
        lines.append(f"### `{view['schema']}.{view['name']}`")
        lines.append("")
        lines.append(f"- options: `{json.dumps(view['options'], sort_keys=True)}`")
        lines.append(f"- query: `{view['query']}`")
        for column, comment in view["column_comments"].items():
            lines.append(f"- comment on `{column}`: {comment}")
        lines.append(f"- source: {view['source']}:{view['line']}")
        lines.append("")
    lines.extend(["## Reloptions", ""])
    lines.append(f"{len(manifest['reloptions'])} reloptions, in registration order.")
    lines.append("")
    for option in manifest["reloptions"]:
        role = "accepted and ignored" if option["ignored"] else "behavior"
        lines.append(f"### `{option['name']}` ({option['kind']}, {role})")
        lines.append("")
        lines.append(f"- description: {option['description']}")
        if option["kind"] == "enum":
            lines.append(f"- domain: {' | '.join(option['domain'])}")
            lines.append(f"- default: `{option['default']}`")
        elif option["kind"] == "string":
            lines.append(f"- default: {show(option['default'])}")
            lines.append(f"- validator: {show(option['validator'])}")
        else:
            lines.append(
                f"- default: {option['default']} (`{option['default_expression']}`)"
            )
            lines.append(
                f"- range: {option['minimum']} .. {option['maximum']}"
                f" (`{option['minimum_expression']}` .. `{option['maximum_expression']}`)"
            )
            if "registered_value" in option:
                lines.append(f"- registered float8: {option['registered_value']}")
                lines.append(f"- source literal: {option['source_literal']}")
        if option["ignored"]:
            lines.append(f"- warning: `{option['tin_ignore_warning']}`")
        if option.get("alter_rejected"):
            lines.append(f"- alter rejected: `{option['alter_error']}`")
            lines.append(f"- alter subtypes: {', '.join(option['alter_subtypes'])}")
        for error in option.get("validation_errors") or []:
            lines.append(f"- validation: `{error['message']}` ({error['source']}:{error['source_line']})")
        lines.append(f"- lock: {option['lock']}")
        lines.append(f"- source: {option['source']}:{option['line']}")
        lines.append("")
    lines.extend(["## GUCs", ""])
    lines.append(f"{len(manifest['gucs'])} GUCs, in registration order across the source files.")
    lines.append("")
    for guc in manifest["gucs"]:
        lines.append(f"### `{guc['name']}` ({guc['type']})")
        lines.append("")
        lines.append(f"- context: {guc['context']}")
        lines.append(f"- flags: `{guc['flags']}`")
        lines.append(f"- default: {show(guc['default'])} (`{guc['default_expression']}`)")
        if guc["type"] == "int":
            lines.append(
                f"- range: {guc['minimum']} .. {guc['maximum']}"
                f" (`{guc['minimum_expression']}` .. `{guc['maximum_expression']}`)"
            )
        if guc["type"] == "enum":
            lines.append(f"- values: {' | '.join(guc['values'])}")
            lines.append(f"- name rule: {guc['enum_name_rule']}")
        lines.append(f"- registered when: {guc['registered_when']}")
        lines.append(f"- short: {guc['short_description']}")
        lines.append(f"- long: {guc['long_description']}")
        lines.append(f"- source: {guc['registered_in']}:{guc['source_line']}")
        lines.append("")
    if manifest["index_limit_errors"]:
        lines.extend(["## Index-limit errors in the parsed sources", ""])
        for note in manifest["index_limit_errors"]:
            lines.append(f"- `{note['message']}` ({note['source']}:{note['source_line']})")
        lines.append("")
    lines.append(
        f"pgrx {manifest['pgrx']['version']}: {manifest['pgrx']['enum_name_rule']}."
    )
    lines.append("")
    return "\n".join(lines)


def write_outputs(root: Path, json_text: str, markdown: str) -> None:
    manifest_path = root / MANIFEST_PATH
    doc_path = root / DOC_PATH
    manifest_path.parent.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(json_text, encoding="utf-8")
    doc_path.write_text(markdown, encoding="utf-8")


def generate(root: Path) -> tuple[str, str]:
    manifest = build_manifest(root)
    json_text = dumps(manifest)
    markdown = render_markdown(json.loads(json_text))
    return json_text, markdown


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    json_text, markdown = generate(args.root)
    manifest_path = args.root / MANIFEST_PATH
    doc_path = args.root / DOC_PATH
    if args.check:
        current_json = manifest_path.read_text(encoding="utf-8") if manifest_path.exists() else ""
        current_md = doc_path.read_text(encoding="utf-8") if doc_path.exists() else ""
        if current_json != json_text or current_md != markdown:
            raise SystemExit("generated tool contract differs from outputs on disk")
        return
    write_outputs(args.root, json_text, markdown)
    manifest = json.loads(json_text)
    print(
        f"wrote {MANIFEST_PATH} and {DOC_PATH}: "
        f"functions={len(manifest['functions'])} "
        f"reloptions={len(manifest['reloptions'])} "
        f"gucs={len(manifest['gucs'])} "
        f"statements={manifest['sql_statements_consumed']}"
    )


if __name__ == "__main__":
    main()
