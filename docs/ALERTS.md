# Alerts: when something goes wrong, who hears about it

A camera goes offline, an incident opens, the dashboard shows it — and nobody
knows unless they happen to be looking at the dashboard. A fleet that only
reports to a screen nobody is watching reports to nobody.

Events now leave the building. The control plane raises them; a plugin decides
who hears about them and how.

## Why delivery is a plugin

Because the alternative is a database full of other people's credentials. A
Slack token, a webhook URL, an SMTP password — none of them belong in the
control plane, for the same reason a camera's password does not
(`docs/VIDEO-SOURCES.md`). The plugin holds its own configuration, and a
Community build does not grow a mail client it will never use.

`event_sink` has been a capability in the plugin protocol since the protocol
was written, with nothing behind it. This is what it means.

## What raises an event

| | |
| --- | --- |
| `camera_offline` / `camera_recovered` | a camera's incident opening or closing — one event per transition, not one per telemetry batch |
| `gateway_offline` / `gateway_recovered` | a gateway stopping or resuming reporting |
| `test` | somebody pressed *Send a test alert* |

A gateway going quiet is the one outage a site cannot report itself — it takes
every camera behind it with it — so the control plane watches for it directly,
comparing heartbeats against the same staleness the gateways screen uses.

Nothing is raised in the first minutes after the API starts: every camera
looks silent until its gateway re-reports, and a restart that announces an
outage it never saw start is worse than useless. The first sight of a gateway
says nothing either.

## Getting them somewhere

Run an `event_sink` plugin and register it in `plugins.d/`. The reference one
posts to a webhook, which covers Slack, Discord, Mattermost, a Telegram bridge
and anything internal:

```bash
docker compose --profile plugins up -d webhook-sink
```

```bash
WEBHOOK_URL=https://hooks.slack.com/services/...   # where to post
WEBHOOK_FORMAT=text                                # or json, for your own endpoint
WEBHOOK_MIN_SEVERITY=critical                      # only the bad ones
```

Filtering lives in the sink, not in the core: whoever is being woken up
decides what is worth waking up for. The plugin's own README in
[relaysight-plugins](https://github.com/mnaza/relaysight-plugins) has the rest
of its settings.

Writing your own sink is one endpoint — `POST /v1/events` — and
`docs/PLUGIN-SDK.md` has the shape.

## What happens when a sink is down

An alert that vanishes because a plugin was restarting is worse than no
alerts: silence then means "nobody was listening" while it reads as "nothing
happened". So an event is written down first and delivered afterwards.

- A sink that refuses is retried after 10 seconds, then a minute, then five,
  then half an hour.
- After that it is given up on, and the reason stays on the event where the
  *Alerts* panel shows it.
- A sink that answers `delivered: false` has decided the event is not for it.
  That is an answer, not a failure, and it is not retried.
- A sink registered this afternoon gets nothing from this morning. Alerts are
  about now, and a sink that opens by replaying the day is one nobody leaves
  switched on.

Events age out with incidents (`INCIDENT_RETENTION_DAYS`).

## What has never run

- **No real chat service has ever received one of these.** The sink is tested
  against its own decisions and the core against a sink in the test process.
  Nobody has pointed it at a live Slack workspace.
- **No pager, no SMS, no email.** A sink could do any of them; none exists.
- **Nothing deduplicates a camera that flaps.** A camera going up and down
  every minute raises an event every minute, and a sink that cannot cope with
  that will not enjoy it. That belongs in a rule engine, and there isn't one.
- **There are no alert rules and no quiet hours**, deliberately: the control
  plane sends everything and the sink decides. That is honest and simple, and
  it will not stay sufficient forever.
