# Stannum tool-contract suite

Engine-agnostic runner for the stannum tool contract (design
`docs/designs/stn3-tool-layer-2026-09-29.md` §7). Adapted from
`conformance/`. The runner imports nothing from this repository: only the
Python standard library, plus `psycopg` and `PyYAML`.

```
contract/
  run.py
  cases/            declarative cases (empty until step 1.2)
  expected/         recorded answers (empty until step 1.2)
  divergences/      intentional differences only
```

The connection string comes only from the environment variable named by
`--dsn-env` (default `CONFORMANCE_DSN`), never from the command line.

Every run creates a scratch schema `contract_<random>`, uses it, and drops
it at the end, including when the run fails.

An empty case list is a successful run. `--record` and `--check` both report
0 cases and 0 failures and exit 0, including a missing or empty expected
directory. A non-empty suite still fails when filters match nothing or when
`--check` has no recorded answers. In a non-empty suite, check mode FAILs a
case with no recording, a recording with no case, and a capture-name set that
does not match the case. A SKIP in check mode fails the run; an exclusion is
EXCLUDED and does not. Record mode fails on CRASH, ERROR, or LOST. Duplicate case
ids fail at load. `contract/exclusions.yaml` lists case ids deliberately not
recorded (`excluded: [{id, reason}, ...]`); an excluded case skips. An entry
may set `engines: ["0.4.0"]`: that skip applies only when `--check` or
`--record` runs against an engine whose `extversion` is in the list. Other
engines still execute the case. A case with `shape: true` embeds its answer
in the capture (`expect`) and needs no recording; `--check` compares that
spec when the case is not excluded and has no recording file. `--shape` runs
those cases without an expected directory. The same
file's `objects` list waives manifest coverage. Every function, reloption,
GUC, operator, opclass, SUPPORT attachment, the `jieba_words` ACL, and
`index_health.security_invoker` needs a case `covers` entry or an objects
line.

`index_options` values are spliced verbatim into `WITH (key = value)`. Enum
values are bare (`tokenizer: unicode`). String values are already quoted in
the YAML (`field_weights: "'title:3,body:1'"`).

Corpora insert every declared column. Rows are lists in column order or
dicts keyed by column name. `pad` is only for the default `(id, body)`
shape. Multi-column corpora set `index_sql`.

The default `ranked` capture orders by score descending, then `ctid`
ascending, and records `f32` bits (`float4send` hex) beside each id. A case
with its own `sql` must use that tie-break itself.

ERROR text must match. A same-SQLSTATE message change FAILs unless the
capture sets `message_prefix` or `normalize: message`, or a
`contract/divergences/<engine>.yaml` entry lists the capture and, when it
carries `expected`, the live error text matches that entry. `normalize:
version` compares `stannum.version()` as present-but-unpinned. `normalize:
explain_parallel` stores only whether the plan has a parallel worker.
Page counts, root blocks, and schema-qualified names are omitted from the
capture SQL rather than compared raw.

`--record --merge` (and a partial `--area`/`--case` record, which merges)
refuses when the existing file's `engine` or `extension_version` differs.
The source header has no timestamp. `suite_commit` is the last commit that
touched the suite sources, ignoring `contract/expected`, so a recording
commit does not rewrite the stamp.

Script steps may insert into `stannum.jieba_words`. The runner snapshots
that table after `LOAD` and restores it after every case and on setup
failure. Schema setup sits in the same cleanup scope as the run, so a
failure after `CREATE SCHEMA` still drops the scratch schema.

```sh
python3 contract/run.py --help
python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0
```
