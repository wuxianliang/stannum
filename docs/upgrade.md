<!--
Copyright (C) 2026 Ben Weis <ben@springbird.app>

See LICENSE in the repository root for license terms.
-->

# Upgrading 0.4.0 to 0.5.0

Operators cut over with downtime, one `stannum.so`, `ALTER EXTENSION`, then
a non-concurrent `REINDEX` of every Stannum index. The catalog script is
SQL-only: it adds `capabilities()` and does not convert segments.
Development fielded-terms indexes left on the `stn3` branch are a documented
`REINDEX`, not a supported upgrade path and not a SQL migration.

PostgreSQL 18 is the engine this procedure was stated and tested on.

## What migrates, what rebuilds

Generation is decided by validated storage metadata and buffer tags, never
by whether a token resembles `~0~foo`.

| What you have | Class | Action |
| --- | --- | --- |
| Released 0.4.0 single-column or multi-column index (kind-1 meta; empty, buffer-only, or beside an LSG segment) | `PreStn3` | Supported cutover. Scans and inserts fail with the migration error until `REINDEX`. |
| Leftover STN4-development v1 fielded index (kind-5, STNF v1, or an untagged legacy fielded buffer) | `StaleFielded` | Rebuild error. `REINDEX` from the heap. No LSG migration, no SQL script. |
| Well-formed v1 beside well-formed v2, or a v2 index with an untagged legacy buffer | `MixedFielded` | Distinct rebuild error. Same heap `REINDEX`. |
| 0.1.0 or other STN3-development fielded-term leftovers | rebuild-only | Do not promise a direct migration. Reload or `REINDEX` from the heap. |
| Kind-5 envelope beside an LSG segment, a malformed trailer or buffer, a missing trailer, or any other unknown layout | `Corrupt` | Reject. Not a migration class. Restore or rebuild from a known-good heap. |

Pinned error texts:

- `PreStn3`: `stannum: index requires REINDEX to 0.5.0 (pre-STN3 segment)`
- `StaleFielded`: `stannum: index holds a pre-STN4 fielded write buffer; REINDEX the index`
- `MixedFielded`: `stannum: index mixes pre-STN4 and STN4 fielded write buffers; REINDEX the index`
- Kind-5 + LSG: `stannum: mixed-format index (KIND_ENVELOPE with a legacy LSG segment)` — corruption, not the `PreStn3` string

After 0.5.0 ships, `StaleFielded` and `MixedFielded` only concern development
indexes on `stn3`. They need no SQL script.

A 0.4.0 multi-column index is kind-1 with LSG segments, the same supported
`PreStn3` path as a single-column 0.4.0 index. It is not a fielded-terms
development leftover.

## Downtime, replicas, rollback

One cluster has one `stannum.so`. Replacing the library on a running
postmaster does not switch backends that already loaded it. Traffic stays
off until every Stannum index in the database has been rebuilt and checked.

**Replicas are rebuilt, not replayed.** Stop standbys before the primary
moves. After the primary has finished `ALTER EXTENSION`, `REINDEX`, and
verification, make new replicas from that upgraded primary (`pg_basebackup`
or an equivalent). Do not let a replica replay across the library swap.
0.4.0 removal-horizon WAL is not promised to redo under 0.5.0. The replay
boundary is a clean shutdown checkpoint, not resource-manager compatibility.
Skipping the checkpoint can refuse recovery.

**Rollback is restore.** Keep a 0.4.0 cluster (or a filesystem backup taken
before step 3) if you need to fail back. There is no downgrade: a 0.4.0
binary cannot read STN3, and 0.5.0 cannot read LSG1–LSG4. Once
non-concurrent `REINDEX` has swapped in a 0.5.0 index, reinstalling 0.4.0
does not bring the old segments back.

## Cutover

1. Backup. Confirm you can restore, or that a 0.4.0 cluster is still standing.
2. Quiesce writers. Drain sessions and workers that have loaded `stannum.so`.
3. `CHECKPOINT`, then a clean `pg_ctl stop`. This is the replay boundary.
4. With the server stopped, install the 0.5.0 package. Restart with the same
   `shared_preload_libraries` the cluster already required. The 0.5.0 binary
   still exports the 0.4.0 SQL entry points, so the pre-`ALTER` catalog
   resolves. `capabilities()` does not exist until the next step.
5. In each database: `ALTER EXTENSION stannum UPDATE TO '0.5.0';` then
   non-concurrent `REINDEX` of every Stannum index (see below).
6. `SELECT * FROM stannum.verify_index('<index>', true);` on each new index.
   Resume traffic only after every index is clean.
7. Rebuild replicas from this primary. Do not upgrade them in place by replay.

`ALTER EXTENSION` is catalog-only. Until `REINDEX` commits, scans and inserts
on a predecessor index raise the class error and dirty no index page.

## REINDEX runbook

Use ordinary `REINDEX INDEX` / `REINDEX TABLE` / `REINDEX DATABASE`, not
`REINDEX CONCURRENTLY`. Concurrent rebuild is not this procedure and is not
promised: the lifecycle harness exercises `CREATE INDEX CONCURRENTLY` and
plain `REINDEX INDEX`, not `REINDEX CONCURRENTLY`.

Each rebuild takes `AccessExclusiveLock`. The relfilenode swap is visible at
that index's own commit. It is not an atomic swap of every index in the
cluster.

`REINDEX` calls `ambuild`. That path writes a fresh empty meta page and
scans the heap. It does not successfully open the rejected predecessor as a
reader: `ambuild` / `ambuildempty` are exempt from the migration and rebuild
fences so the error texts that say `REINDEX` can actually clear them.
Guarded callbacks (`aminsert`, vacuum, scans, `view`) still fence first and
allocate no index page.

PostgreSQL keeps the index definition. The rebuild therefore preserves:

- indexed columns and their order
- index expressions and partial-index predicates
- tokenizer and dictionary configuration (`tokenizer`, `case_folding`,
  `accent_folding`, `long_tokens`, `max_token_bytes`, `graphemes`,
  `position_gaps`, jieba analysis)
- `field_weights` (a reloption; `ALTER INDEX … SET/RESET (field_weights)` is
  refused — change weights with `REINDEX`)
- other reloptions (`k1`, `b`, `score_stop_words`, storage options)

After a supported 0.4.0 rebuild, confirm at least:

- membership (`==>` / `search` / `search_count` on terms that were indexed)
- ranking (`score` / `full_score` order on a known query)
- field scopes on a multi-column index (`title:(…)` stays in title)
- positional behavior (a phrase or `NEAR`/`THEN` does not match across fields)
- `stannum.verify_index(index, true)` is empty

For a `StaleFielded` or `MixedFielded` development index the same `REINDEX`
writes STNF v2 (or a single-column STN3 segment) from the heap. Search after
the rebuild must answer; an insert after the rebuild must write.

### Interrupted rebuild

A non-concurrent `REINDEX` publishes the new relfilenode at commit. An
error, cancel, or crash before that commit leaves the previous index in
place and does not rewrite heap tuples. A `CREATE INDEX` terminated at its
first merge checkpoint leaves no index behind (`to_regclass` is NULL) and
leaves the heap untouched. Do not treat an in-progress rebuild as published.

A crash after a fold, merge, or VACUUM has written run pages but before the
meta page publishes them leaves the old directory; unreferenced pages are
orphans that `VACUUM` reclaims. That is publication of maintenance, not of
`REINDEX`, and it does not drop heap rows.

## The six rebuild guarantees

| Guarantee | Status |
| --- | --- |
| Which formats migrate vs rebuild, with the pinned actionable errors | **Tested.** `postgres/tests/two_artifact_migration.py` (0.4.0 kind-1/LSG → `PreStn3`; STN4-dev matrix → `StaleFielded` / `MixedFielded` / `Corrupt`); `test_two_artifact_migration.py` pins the strings; `postgres/src/lib.rs` `all_valid_v1_is_stalefielded_for_every_non_live_buffer` and the VACUUM fence tests. |
| Supported rebuild DDL rebuilds from the heap without first successfully scanning the rejected format | **Tested.** `ambuild` in `postgres/src/am.rs` writes an empty meta then `table_index_build_scan`s the heap; `reindex_clears_a_planted_stale_class` plants STNF v1, `REINDEX`s, and asserts v2 trailers plus answers; `two_artifact_migration.py` `reindex_old` after `PreStn3`. `MixedFielded` has no separate `REINDEX` test; it uses the same exempt `ambuild`. |
| Full preservation of the index definition | **Documented; only partly tested.** PostgreSQL `REINDEX` keeps `pg_index` and reloptions. E.1 rebuilds a jieba index and a two-column index and checks a term count. There is no post-rebuild dump of expressions, predicates, `field_weights`, or the full reloption list. |
| Extension-update / restart / `REINDEX` ordering on PG18 | **Tested.** `two_artifact_migration.py`: 0.4.0 writes → clean stop → install 0.5.0 → start → `ALTER EXTENSION` → `PreStn3` → `REINDEX`. `extension_upgrade.py` fingerprints the catalog after every retained `ALTER EXTENSION` path. |
| Post-rebuild membership, ranking, field scopes, positional behavior, and integrity | **Partly tested.** E.1 and `reindex_clears_a_planted_stale_class` check membership counts. `ranked_fuzz.py` keeps comparing answers across live `REINDEX INDEX` steps. Field scopes, positional queries, and `verify_index` after a 0.4.0→0.5.0 rebuild are not asserted by that cutover job. |
| Interrupted rebuild publishes no partial index and loses no heap data | **Partly tested.** `crash_before_publication.py` `terminate`: a `CREATE INDEX` stopped at the first merge checkpoint leaves no index and does not touch the heap. Fold/merge/VACUUM crashes leave the previous directory plus reclaimable orphans. There is no dedicated test that interrupts `REINDEX INDEX` itself. |

## Related

- [Compatibility with TIN](compatibility.md) — the 0.5.0 catalog GAPs
- [Releasing](RELEASING.md) — versioning and the release checklist
- [Recovery and replicas](architecture/recovery-and-parallel.md) — standby reads once both sides run the same 0.5.0 binary
