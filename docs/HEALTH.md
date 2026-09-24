# Health history: how often has this been down?

The dashboard shows now. An incident says a camera is down. Neither answers
the question that decides whether somebody drives out to a site: has this been
down four times this week, or once for four hours?

Every telemetry report the fleet already sends is folded into an hourly
rollup, and the dashboard shows what those rollups say.

## What is counted

The time **between** reports, attributed to the state the camera was in over
it. A gateway reporting every twenty seconds and one reporting every five give
the same answer, because the answer is seconds rather than samples.

Each hour, per camera: seconds healthy, seconds warning, seconds offline,
reconnects, average fps and bitrate, worst packet loss.

Rollups rather than samples, deliberately. Five hundred cameras reporting
every twenty seconds is two million rows a day, and nobody has ever needed to
know what one second looked like.

## Gaps are not uptime

A gateway that was itself offline for three hours and came back saying
"healthy" does not get three hours of healthy. One interval is counted — at
most 150 seconds — and the rest is a gap nobody was watching.

That is why every answer carries two numbers:

| | |
| --- | --- |
| `uptime_percent` | healthy time as a share of the time anything was **known** |
| `covered_percent` | how much of the window was known at all |

A camera nobody heard from for six days of a week is not 100% up. It is 100%
of a day, and the second number is what stops the first from lying. A camera
with no history at all reports `null` rather than a perfect week.

## Reading it

```text
GET /api/v1/health?days=7                  the fleet
GET /api/v1/cameras/{camera_id}/health     one camera
```

`days` is clamped to 90, and history is kept for `HEALTH_RETENTION_DAYS`
(default 30), after which the rollups are pruned with everything else in the
retention pass.

On screen: an *Uptime, 7 days* figure on the overview, and a line on each
camera's telemetry card with its own week and how many times it came back.
Both are computed — there are no constants on that screen, and there have not
been since five of them were taken out.

## What this is not

- **Not an SLA.** It measures what the gateways reported, which is not the
  same as what the cameras did. A gateway that lies, or a clock that jumps,
  lands here unfiltered.
- **Not a replacement for incidents.** An incident is the outage with a start
  and an end; this is the shape of the month around it.
- **Nothing has run for a month.** The fold is tested with minutes and hours
  of synthetic telemetry. Nobody has watched a real fleet write thirty days of
  this, and the first person to will find out what a year of clock drift does
  to an hourly bucket.
