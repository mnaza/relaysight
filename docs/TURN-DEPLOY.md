# Running the relay

`docs/TURN-COSTS.md` works out what TURN costs and concludes the hosting choice
decides it, not the relay itself. This is how to actually run one.

The code side was already finished: `services/api/src/turn.rs` mints ephemeral
credentials, `live.rs` hands them to the peer connection, and the browser uses
whatever the API returns. What was missing was the server.

## Why it has its own compose file

TURN wants bandwidth and nothing else, so it belongs on a cheap flat-rate box
rather than beside the API. On a hyperscaler the same traffic costs about a
hundred times more, which is the whole finding of the costs document. Keeping it
in `deploy/coturn/` rather than in the root compose makes that separation the
default rather than a thing you have to remember.

```
RTC_TURN_PUBLIC_IP=203.0.113.10 \
RTC_TURN_REALM=relay.example.com \
RTC_TURN_SECRET=$(openssl rand -hex 32) \
  docker compose -f deploy/coturn/docker-compose.yml up -d
```

All three are required and compose refuses to start without them. `external-ip`
in particular is the one people forget: without it coturn advertises the address
it sees, which behind any cloud NAT is a private one, and every candidate it
offers is unreachable.

Then give the API the other half:

```
RTC_TURN_URLS=turn:relay.example.com:3478?transport=udp,turn:relay.example.com:3478?transport=tcp
RTC_TURN_SECRET=<the same secret>
```

The secret is shared between the two and never leaves either. What reaches the
browser is a username that is an expiry and a password that is an HMAC of it,
good for ten minutes.

## Firewall

| Port | Protocol | For |
|---|---|---|
| 3478 | UDP and TCP | STUN and TURN |
| 5349 | TCP | TURN over TLS — see the gap below |
| 49160–49200 | UDP | the relay range |

The relay range is what caps concurrent relayed sessions, at 40 with the shipped
config. Raise `min-port`/`max-port` in `turnserver.conf` and the firewall rule
together, or sessions start failing once it fills, with nothing useful in the
log to say why.

## What is verified, and what is not

Verified against the shipped config, on 2026-09-04:

- coturn starts and applies the peer blacklist. Every private range is refused,
  so a credential cannot be used to reach the host's own network. That is the
  difference between a relay and an open proxy.
- **A credential minted by `services/api/src/turn.rs` is accepted.** Allocation
  succeeds. This is the join between the two halves and it now has evidence
  rather than an assumption.
- A credential signed with the wrong secret is refused.
- An expired username is refused, with `check_stun_auth: Cannot find
  credentials` in the log.

⚠️ **Not working: TURN over TLS.** `tls-listening-port=5349` is in the config
and the listener does not come up, because there is no certificate. Only 3478 is
listening.

That matters more than it looks. The sites that need a relay at all are the ones
behind restrictive egress, and the strictest of them permit nothing outbound but
TLS on 443. For those, plain TURN on 3478 is as unreachable as a direct path
was. **So the relay currently serves symmetric NAT and not hostile firewalls,
which is half the problem it exists for.**

To close it, point coturn at a real certificate for the realm and add:

```
cert=/etc/coturn/certs/fullchain.pem
pkey=/etc/coturn/certs/privkey.pem
alt-tls-listening-port=443
```

443 rather than only 5349, because 5349 is itself often blocked by the firewalls
this is meant to get through.

## Capacity

From the costs document, on a €97/month flat-rate box allowing 500 Mbit/s of a
1 Gbit uplink:

| Relaying monitoring sites, 4 cameras each | per site | sites per box |
|---|---|---|
| main stream | 16 Mbit/s | 31 |
| substream | 2.8 Mbit/s | 179 |

Live view already uses the substream where the camera publishes one, so the
second row is the one that applies. On-demand-only sites are not a constraint at
all.
