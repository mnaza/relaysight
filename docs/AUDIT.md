# The audit log, and how much it is worth

Every consequential act by a person or a gateway leaves a row: who, when,
what, and to what. Enrolments, revocations, retirements, logins and failed
logins, password changes, sources added and removed, recording policies,
plugins connected, users created and turned off.

`GET /api/v1/audit` reads it, and only an owner may.

## Why the rows are chained

A log in a table is something anybody with database access can edit without
leaving a mark — including the person whose action it records. Each row
therefore carries the hash of the row before it, over every field it holds:

```
hash = sha256(prev_hash ‖ id ‖ at ‖ actor ‖ action ‖ subject ‖ detail)
```

Each field is length-prefixed, so two different sets of fields cannot hash to
the same thing by running together.

```bash
curl -s .../api/v1/audit/verify
{"checked": 1284, "unchained": 0, "broken_at": null, "head": "9f3c…"}
```

Editing a row, deleting one, or reordering them breaks every hash after the
change, and `broken_at` names the first row that no longer adds up.

## What that does and does not prove

**It catches an edit.** Changing one row without recomputing the whole chain
is detected, exactly and by position.

**It does not catch a rewrite by itself.** Somebody with database access and
this repository can recompute the entire chain, and the result verifies
against itself. What makes that detectable is having written the head hash
down somewhere else: the API logs it on every verification, and the export
carries it. A head that does not match the one in yesterday's journal means
the log was rebuilt, whatever it says about itself now.

**Rows written before the chain existed are not claimed to be sound.** They
come back as `unchained` rather than as verified. An install that predates
this has a log that is still a log; it simply is not evidence.

## Taking it away

```bash
curl -s .../api/v1/audit/export -o audit.csv
```

CSV, oldest first, quoted so a spreadsheet reads it back as what it was. An
auditor checking the chain outside this system is the point: a system that
verifies its own log and asks to be believed is not much of a control.

## Retention

`AUDIT_RETENTION_DAYS` prunes old rows. Pruning removes the oldest rows, which
leaves the chain intact from the first surviving row onwards — the first one
then links to something that is gone, and verification starts there rather
than calling it a break.

## What this is not

- Not tamper-*proof*. Nothing in one database is. It is tamper-*evident*, and
  only as far as the head hash is recorded somewhere this system does not
  control.
- Not an append-only store. That is a different product, usually with a
  different price.
- Not a compliance certification. It is a log with hashes, which is what most
  frameworks actually ask for, and no framework has been run against it.
