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
`--check` has no recorded answers.

```sh
python3 contract/run.py --help
python3 contract/run.py --engine stannum --check contract/expected/stannum-0.4.0
```
