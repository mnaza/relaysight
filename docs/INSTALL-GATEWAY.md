# Installing a gateway on a site box

The gateway runs as a systemd service on a small Linux box on the camera
network, from a signed release, and keeps itself current. This is what
`install.sh` sets up, where everything lives, and what happens when an update
goes wrong.

## Installing

The dashboard's *Add gateway* step gives you the command, with this site's
enrollment token filled in:

```bash
curl -fsSL https://github.com/mnaza/relaysight/releases/latest/download/install.sh \
  | sudo sh -s -- --api-url 'https://api.example.com' \
    --enrollment-token 'TOKEN' --gateway-id 'gw-main-street-1a2b3c4d'
```

It needs root, systemd, `curl` and `openssl`, and an x86_64 or aarch64 box
with glibc 2.36 or newer (Debian 12, Ubuntu 24.04 and later; Ubuntu 22.04
has 2.35, one short).

Before it installs anything, it checks the release manifest's signature
against the release key it carries, and the binary against the hash in that
signed manifest. If either check fails, it stops with nothing changed.

Any other gateway setting goes in with `--env KEY=VALUE`, as many times as
needed. For example, a camera network that multicast discovery cannot reach:

```bash
  … --env ONVIF_DISCOVERY_SECONDS=0 --env ONVIF_HOSTS=192.168.1.50,192.168.1.51
```

Running it again brings the install up to the latest release. It keeps the
configuration, changing only the keys it's given.

## Where things live

| | |
| --- | --- |
| `/usr/local/bin/vms-gateway` | the gateway; `vms-gateway --version` says which |
| `/usr/local/bin/vms-gateway.prev` | the binary before the last update |
| `/etc/relaysight/gateway.env` | its configuration, 0640, `root:relaysight-gateway` |
| `/var/lib/relaysight-gateway/` | its state, 0700: the encrypted identity and camera credentials |
| `relaysight-gateway.service` | the gateway, as the unprivileged user `relaysight-gateway` |
| `relaysight-gateway-update.timer` | runs the update once a day, at a random point in the hour |

The service can write only its state directory. It has no capabilities, and
sees a read-only system, a private `/tmp`, and no home directories.

```bash
systemctl status relaysight-gateway
journalctl -u relaysight-gateway -f
```

## Camera passwords

No camera password goes on the install command line. `CAMERA_USERNAME` and
`CAMERA_PASSWORD` in `gateway.env` are an optional fallback for a site where
every camera shares one. A camera with its own password gets an entry on the
box. The password is read from stdin, so it never shows up in `ps` or in a
shell history:

```bash
printf '%s\n' 'the-camera-password' \
  | sudo relaysight-gateway-credentials set 192.168.1.50 admin
sudo relaysight-gateway-credentials list
sudo systemctl restart relaysight-gateway
```

The wrapper runs `vms-gateway credentials` as the service's user, with the
service's state directory and key, so the file it writes is the one the
service reads. The gateway loads credentials at startup, so restart it after
a change.

## Updates

Once a day, `relaysight-gateway-update.service` runs `vms-gateway update` as
root:

1. It fetches the channel's `release.json` and `release.json.sig`, and
   believes them only if the release key compiled into the gateway signed
   them.
2. It moves only to a strictly later version. It never downgrades.
3. It downloads this architecture's binary, checks the hash, and swaps it in
   with two renames. At every moment there is a whole binary in place.
4. It restarts the service and waits up to 90 s for the gateway to prove
   itself: a heartbeat the API accepted, which the gateway records in its
   state directory.
5. If that proof doesn't come, the previous binary goes back, the service is
   restarted on it, and the update exits non-zero, so the journal says so.

An API that is unreachable during an update therefore looks like a bad
release and is rolled back. That's the safe mistake, and the next day tries
again.

```bash
sudo systemctl start relaysight-gateway-update     # update now
journalctl -u relaysight-gateway-update            # what the last one did
```

To roll back by hand, `vms-gateway.prev` is the previous binary:

```bash
sudo mv /usr/local/bin/vms-gateway.prev /usr/local/bin/vms-gateway
sudo systemctl restart relaysight-gateway
```

## Removing it

```bash
sudo systemctl disable --now relaysight-gateway relaysight-gateway-update.timer
sudo rm /etc/systemd/system/relaysight-gateway*.{service,timer}
sudo rm /usr/local/bin/vms-gateway* /usr/local/bin/relaysight-gateway-credentials
sudo rm -r /etc/relaysight /var/lib/relaysight-gateway
sudo userdel relaysight-gateway
sudo systemctl daemon-reload
```

Removing the state directory throws away the gateway's identity. To bring the
gateway back afterwards, revoke it in the dashboard and enrol it again with a
fresh token.

## Releasing

A `v*` tag on the public repository runs `.github/workflows/release.yml`,
which does four things:

- builds both architectures on Debian 12's glibc, with the tag's version
  compiled in;
- signs the manifest with the `RELEASE_SIGNING_KEY` secret;
- publishes the binaries, `release.json`, its signature and `install.sh` as a
  GitHub release;
- pushes an amd64 image to `ghcr.io/<owner>/relaysight-gateway`.

The key has to exist before the first release:

```bash
scripts/release-key.sh generate release.key      # keep it somewhere safe, offline
scripts/release-key.sh public release.key        # the two public forms
```

The raw form goes in `RELEASE_PUBKEY` in `edge/gateway/src/update.rs`, and
the PEM form in `PUBKEY_PEM` in `deploy/gateway/install.sh`. The private key
goes in the repository secret. The publish job refuses to run while either
public key is missing, since a release no gateway can verify is worse than
none. It also refuses if the two public keys differ: `install.sh` would then
accept a release the gateway refuses, or the other way round.

## What has been checked, and what has not

Checked on 2026-09-22 with `make check-installer`, which boots Debian 13 with
systemd in a container and runs the real `install.sh` against a local release
channel signed with a throwaway key:

- The gateway installs, enrols against a fake API and heartbeats, under the
  hardened unit.
- An update takes a newer release and keeps the gateway's identity: it does
  not enrol again.
- A release whose binary exits at once is rolled back, and the previous
  version heartbeats again.
- A release signed by another key is refused, saying why.
- The credentials wrapper writes a file the service can read and start with.
- Re-running the installer leaves a working install alone.

That check found two bugs before any site did. Every update would have rolled
back, because the new binary was still open for writing when systemd tried to
run it. And a re-run with nothing to change stopped silently halfway.

Not checked yet:

- **A real release.** No release key exists yet. The release workflow has never
  run, and no binary has been downloaded from GitHub.
- **aarch64.** Every run so far has been x86_64.
- **Real site hardware**, and distributions other than Debian.
- **Updates driven or staged by the control plane.** They don't exist: every
  gateway updates on its own timer.
