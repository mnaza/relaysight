# Backing it up, restoring it, upgrading it

Written after doing it rather than from memory. `make check-restore` does the
whole of it again — back up a running control plane, destroy it, restore it,
and open a database from before users existed with today's binary.

## What the state actually is

| | |
| --- | --- |
| **The database** | one SQLite file, `DATABASE_URL`, default `sqlite:data/vms.db`. Customers, sites, gateways and their token hashes, cameras, users, sessions, recording policies, incidents, alerts, health history and the audit chain |
| **The plugin directory** | `PLUGIN_DIR`, default `plugins.d`. Each plugin's manifest and, where one is used, its `token_file` |
| **The environment** | `.env` or whatever sets `ADMIN_PASSWORD`, `GATEWAY_TOKEN`, the OIDC settings and the TURN secret. Not in the database, and not in a backup of it |

**Video is not here.** Recordings live wherever the storage plugin puts them,
and the ring lives on the gateway's own disk. A control plane restored from
yesterday points at the same bucket and the same gateways, and the video is
where it was. A gateway that is rebuilt loses whatever its ring held.

## Backing up

```bash
sqlite3 /var/lib/relaysight/vms.db ".backup '/backups/vms-$(date +%F).db'"
```

This works while the API is serving. It goes through SQLite's own backup API,
which takes the lock it needs; `cp` does not, and a copy taken in the middle
of a write can catch a transaction half-applied without saying so. `cp` is
for a control plane that has been stopped.

Copy the plugin directory alongside it if any plugin has a `token_file`: those
are credentials, and they are not in the database.

Nothing here schedules that for you. It is one line in cron, and a backup
nobody has ever restored is not a backup — which is what `make check-restore`
is for.

## Restoring

1. Stop the API.
2. Put the file back where `DATABASE_URL` points, removing any `-wal` and
   `-shm` beside it.
3. Start the API.

What comes back: users and their passwords, the gateway roster and the token
hashes that authenticate them, cameras, policies, incidents, alerts, health
history, and an audit chain that still verifies end to end. **Sessions come
back too** — a cookie issued before the backup still works after the restore,
because sessions are rows like everything else. Somebody who should no longer
be in is no longer in only if they were disabled before the backup was taken.

Gateways reconnect on their own; they poll. Cameras reappear in the fleet as
their gateways report, and a camera that had been seen before is on the
roster immediately rather than vanishing until it reports.

## Upgrading

Migrations run at startup, in order, once each. There are no down migrations
and no way back, so take a backup first — the upgrade path is forward, and
the restore path is the file you took before it.

An installation from before users existed comes up with its single password
carried into one owner account, whose email is `ADMIN_EMAIL` (default
`admin@localhost`). Nobody's password changes. This is exercised against a
real file in `make check-restore`, by building a database with the schema of
that era and opening it with today's binary.

## What has not been tried

- A restore onto a different machine or architecture. SQLite files are
  portable and nothing here should care; nobody has done it.
- A backup taken while a migration was running.
- Postgres. There is no Postgres in this system; when there is, none of the
  above transfers.
- A control plane that has been running long enough for the database to be
  large. Everything above was done with a database of a few hundred
  kilobytes, and `.backup` on a database of gigabytes is a different
  conversation about how long it holds its lock.
