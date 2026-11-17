# One server, from nothing

The whole control plane on one rented Linux box, with a certificate, behind
one script. It comes up with a gateway of its own and a few test-pattern
cameras, so there is something to look at before any real site is connected.

`deploy/vps/` is the bundle, and `make check-vps` brings that same bundle up
on a laptop and checks it, so what is described here has run.

## What you need

- A Linux server with a public IPv4, Docker and the compose plugin, and about
  4 GB of memory: the first start compiles the API and the gateway, and the
  compiler is the hungriest thing that will ever run on it. Flat-rate
  bandwidth if live video will be relayed through it, for the reason in
  `docs/TURN-COSTS.md`.
- A DNS name with an A record pointing at that address, **before** the first
  start: Caddy asks Let's Encrypt for a certificate the moment it starts.
- The firewall open for:

| | |
| --- | --- |
| TCP 80 | Let's Encrypt's challenge, and a redirect to HTTPS |
| TCP 443, UDP 443 | the dashboard and the API, over HTTP/2 and HTTP/3 |
| UDP 1024–65535 | live video between browsers and the server's own gateway |

The UDP range is wide because the gateway takes an ephemeral port per
session and webrtc-rs has no setting to confine it. A browser on a network
that will not send UDP there needs TURN instead (`docs/TURN-DEPLOY.md`); that
is a second box, and the reason it is a separate document.

## Bringing it up

```bash
git clone https://github.com/mnaza/relaysight.git
cd relaysight/deploy/vps
cp .env.example .env
$EDITOR .env          # DOMAIN and PUBLIC_IP
./up.sh
```

`up.sh` generates the owner's password and the gateway token into `.env`
(mode 600) the first time, builds and starts the stack, waits for the
server's own gateway to report, and registers the demo cameras. It prints the
address and where the password is; it does not print the password.

Run it again after `git pull` to upgrade. It keeps what is in `.env`, and
registers only the demo cameras it does not have yet; `make check-vps` runs
it twice to hold it to that. An upgrade rebuilds both images from scratch:
the Dockerfiles copy the whole repository before compiling, so any change at
all invalidates the build cache. Expect the first start's wait every time,
until there are published images to pull instead.

## What is running

| | where | |
| --- | --- | --- |
| `caddy` | 80, 443 | TLS for `DOMAIN`, the dashboard out of `web/`, `/api` to the API |
| `api` | 127.0.0.1:8080 | the control plane; SQLite in the `api-data` volume |
| `gateway` | host network | the server's own site gateway; RTMP ingest on 127.0.0.1:1935, device tunnel off |
| `demo-cameras` | host network | `DEMO_CAMERAS` test patterns with a running clock, pushed over RTMP |

Only Caddy listens beyond loopback. The session cookie is `Secure`, because
nothing reaches the API except over TLS. `make check-vps` fails if either
stops being true.

The gateway runs on the host network rather than behind Docker's NAT because
WebRTC hands the browser the gateway's own addresses: behind the NAT those
would be bridge addresses nobody outside can reach. `RTC_UDP_ADDRS` is set to
`PUBLIC_IP`, so it offers that one address instead of every interface the
server has.

## Connecting a real site

A site runs its own gateway on its own LAN and enrols against this server:
create an enrolment in the dashboard and install the gateway with
`API_URL=https://DOMAIN` (`docs/INSTALL-GATEWAY.md`). It only ever connects
out, so the site needs no port forward.

Then `DEMO_CAMERAS=0` in `.env` and `./up.sh` stops the test patterns. The
demo sources stay on the roster until they are removed from the dashboard.

## Letting somebody look

Add a **viewer** in *People*: a viewer sees the fleet, incidents, alerts,
health, live video and recordings, and cannot change anything, so it is the
login to hand out for a demo. Each viewer watching is a session the gateway
carries and nothing caps how many (`docs/USERS.md`); on the test patterns that
costs this server CPU and bandwidth, on a real recorder it costs the recorder.
A technician login should not be handed to strangers: it can add sources, and
a source is an address this server's gateway will dial.

## The password

`ADMIN_PASSWORD` seeds the owner on the first start and is ignored after
that. To change it from the server rather than from the dashboard, set the
new one in `.env` together with `ADMIN_PASSWORD_RESET=true`, run `./up.sh`,
then take the reset line out again: it revokes every session on each start
while it is there.

## Backing it up

The state is one SQLite file in the `api-data` volume, and `.env`.
`docs/OPERATIONS.md` has the backup and restore, done rather than described;
on this layout the file is at
`/var/lib/docker/volumes/relaysight_api-data/_data/vms.db`.

## What has not been proved

`make check-vps` runs this bundle on one machine, so the browser reaches the
gateway over loopback. A browser on another network reaching it over
`PUBLIC_IP`, through a provider's firewall, has not been tried yet: that is
the first thing to look at on a real server, and the place to look if the
dashboard loads and the video does not.
