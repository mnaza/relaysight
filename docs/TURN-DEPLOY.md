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

Put the certificate in place — see "The certificate" below — before running
this. A relay started without one comes up with no TLS listener, and
installing a certificate into it afterwards does not open one either; only a
restart does, as below.

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
RTC_TURN_URLS=turn:relay.example.com:3478?transport=udp,turn:relay.example.com:3478?transport=tcp,turns:relay.example.com:443?transport=tcp
RTC_TURN_SECRET=<the same secret>
```

The secret is shared between the two and never leaves either. What reaches the
browser is a username that is an expiry and a password that is an HMAC of it,
good for ten minutes.

## The certificate

TURN over TLS needs a certificate for the realm. coturn reads it from
`deploy/coturn/certs/` (or wherever `RTC_TURN_CERT_DIR` points). The hook,
`deploy/coturn/install-cert.sh`, does not read that variable — it writes to its
own `CERT_DIR` (default: `certs/` beside the script) in group `CERT_GROUP`
(default: 65534). certbot's renewal timer runs the hook with an empty
environment, so neither `RTC_TURN_CERT_DIR` nor a compose `.env` file reaches
it. If you moved the mount with `RTC_TURN_CERT_DIR`, point the hook at the same
place by running it through a small wrapper that sets `CERT_DIR` (and
`CERT_GROUP`, if that also differs from the default) before calling
`install-cert.sh`, and register the wrapper as the deploy hook instead. certbot
runs the hook after the first issuance and after every renewal:

```
certbot certonly --standalone -d relay.example.com \
  --deploy-hook /path/to/relaysight/deploy/coturn/install-cert.sh
```

`--standalone` answers the challenge itself on port 80, so 80 has to be open to
the box while certbot runs.

The hook copies instead of pointing coturn at `/etc/letsencrypt/live`, because
the files there are symlinks into `archive/` and the key is readable by root
only, while coturn runs as `nobody`. It installs the key 0640 in group 65534,
which is `nobody`'s, refuses a key that does not belong to its certificate, and
sends coturn `SIGUSR2`, which reloads the pair without a restart.

Put the first certificate in place before starting the relay. Started without
one, coturn does not fail: it runs with no TLS listener and says so once in its
log. That state shows up as `unhealthy` in `docker compose ps` instead of as
sites that quietly cannot connect.

The healthcheck is `deploy/coturn/healthcheck.sh`: it completes a TLS handshake
on 443, then reads the certificate the relay just served and requires that it
has not expired and that it is for `RTC_TURN_REALM`. A handshake on its own
would pass an expired certificate, or one for another name, and a relay whose
renewal broke months ago would report healthy while every `turns:` URL failed
in the browser. It runs every ten seconds, which is worth knowing because
coturn logs each handshake.

If the relay was already started without a certificate, installing one does
not repair it: verified against this compose file, installing the first
certificate and sending `SIGUSR2` left 443 closed, and only
`docker compose -f deploy/coturn/docker-compose.yml restart coturn` opened it.
A reload replaces the certificate an already-open listener presents; it does
not create a listener that never came up. The hook checks for that itself —
after signalling, it looks for something answering TLS on 443, and if nothing
does it says to restart the relay and exits non-zero rather than reporting a
reload that changed nothing.

certbot runs this hook as root. It also runs it from wherever the deploy hook
path points, which is typically a git checkout owned by whoever deployed it —
so anyone who can write to that checkout can run as root at the next renewal.
Either have the relay box's checkout owned by root, or copy `install-cert.sh`
to `/etc/letsencrypt/renewal-hooks/deploy/` and set `CERT_DIR` for it there.

## Firewall

| Port | Protocol | For |
|---|---|---|
| 3478 | UDP and TCP | STUN and TURN |
| 443 | TCP | TURN over TLS |
| 80 | TCP | certbot, during issuance and renewal only |
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

Verified on 2026-09-20 by `make check-relay`, which runs this compose file with throwaway certificates:

- 443 presents the installed certificate and a TLS handshake completes.
- A TURN allocation over TLS on 443 authenticates with the shared secret; a
  wrong secret cannot allocate.
- `install-cert.sh` refuses a key that does not belong to its certificate, and
  leaves the installed pair alone.
- Installing a new certificate changes what 443 presents within seconds, with
  no restart.
- An unreadable key turns the relay `unhealthy`.
- So does a certificate for another name, and an expired one — the second only
  where openssl can date a certificate in the past (3.5 and newer); older ones
  skip that step and say so.
- Installing a certificate into a relay that came up without one leaves 443
  closed, and the hook says to restart instead of reporting a reload. The
  restart opens it.

The advice this file used to give, `alt-tls-listening-port=443`, does not work:
that option is coturn's alternative port for NAT behaviour discovery and never
opens 443. `tls-listening-port=443` does. It replaces 5349, because 5349 is
blocked by the same firewalls 443 is meant to get through.

coturn runs as the image's `nobody` with every capability dropped except
`NET_BIND_SERVICE`, which is what lets it bind 443 under host networking.

Not verified:

- A real domain with a real certbot issuance and renewal. Nothing here has run
  against a public relay yet.
- Container runtimes that do not honour a declared `NET_BIND_SERVICE` for a
  non-root user. Docker 29 does. Where it is not honoured, 443 never listens and
  the healthcheck says so.
- certbot installed as a snap. Its hooks run with a confined PATH and may not
  find `docker`, which the deploy hook needs to signal a running relay.

## How the gateway reaches the relay

webrtc-rs, which the gateway uses for live video, only speaks TURN over UDP: as
of 0.20.5 it skips every `turns:` URL and every `turn:` URL over TCP
(https://github.com/webrtc-rs/webrtc/issues/848). The gateway works around that
outside the library, in `edge/gateway/src/turn_bridge/`:

1. Before a live session it sends a STUN Binding request to the relay's UDP
   TURN URL, and resends it twice a quarter-second apart so that one lost
   datagram does not decide the matter. If the relay answers within a second,
   the session uses plain UDP TURN and nothing else changes. The answer is
   remembered for ten minutes, because whether UDP gets out is a property of
   the site. A change in the site's firewall can therefore take up to ten
   minutes to be noticed. Resolving the relay's name gets the same second, so
   a resolver that never answers costs a session two seconds, not a session.
2. If the relay does not answer, the gateway drops the UDP URL and bridges each
   `turns:` (and `turn:…?transport=tcp`) URL: webrtc-rs talks UDP TURN to a
   socket on 127.0.0.1, and the gateway carries every message to the relay and
   back, over TLS for a `turns:` URL and over plain TCP for a
   `turn:…?transport=tcp` URL. If the relay cannot be reached that way either,
   the bridge answers webrtc-rs's requests with an error as soon as the relay
   connection fails — which can take up to five seconds when a firewall drops
   the connection rather than refusing it — so the session starts without a
   relay instead of waiting out retries.

The "WebRTC peer connected" log line says which way a relayed session went:
`relay_transport=udp`, `tcp` or `tls`.

The bridge verifies the relay's certificate against the public roots, with the
URL's host as the server name. A relay whose certificate comes from a private
authority needs `GATEWAY_TURN_CA_FILE` on the gateway, pointing at that
authority's PEM certificate. A file that cannot be read or parsed is logged
and ignored.

Verified on 2026-09-14 by `make check-gateway-relay`: with the gateway's UDP TURN
URL pointing at a closed port and the gateway limited to relay candidates, a live
session from a fake camera to a fake browser carried its video through the TLS
bridge to a local coturn and back.

Not verified: a real site behind TLS-only egress, and a relay on the public
internet reached this way. Also not verified: the fail-fast path against a
relay that refuses the bridge's connection, which is covered by a unit test
and by reading webrtc-rs's source but has not been run end to end; and plain
TCP bridging (`turn:…?transport=tcp`) against a real coturn, which has only
met an in-process test server.

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
