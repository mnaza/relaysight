# Encrypted persistent gateway identity

**Goal:** The gateway's enrolled token and identity live only in process
memory — `enroll_if_requested` mutates `config` and nothing writes it down.
The enrollment token is one-shot, so a container restart loses the identity
and cannot repeat the enrollment: today's real deployments survive restarts
only via the shared bootstrap `GATEWAY_TOKEN`, which defeats per-gateway
tokens. Persist the enrolled identity on the edge, encrypted at rest.

**Decisions taken with the user (2026-09-09):**

- **Key source: auto key file + env override.** First boot generates a random
  32-byte key file (mode 0600) in the state dir; `GATEWAY_STATE_KEY` (64 hex
  chars) overrides it for operators with a secret manager.
- **Boot rule: state wins; the token is for fresh state.** A valid persisted
  identity is used and `ENROLLMENT_TOKEN` ignored (it is one-shot and already
  burned). `GATEWAY_REENROLL=true` wipes state and enrolls fresh — the
  recovery path for revoked or moved gateways.

## What exists today

- `edge/gateway/src/main.rs`: `Config::from_env` reads `GATEWAY_TOKEN`
  (default `demo-local-token`) and `ENROLLMENT_TOKEN`;
  `enroll_if_requested` (main.rs:167) posts to `/api/v1/gateways/enroll`,
  then overwrites `config.token`, customer/site fields and `camera_limit`
  in memory only.
- The API rotates the gateway's token on re-enrollment and burns enrollment
  tokens on first claim (`claim_enrollment` is claim-exactly-once).
- The compose `gateway` service has no volume; nothing on the edge persists.
- `FakeControlPlane` (edge/gateway/src/fake_control_plane.rs) already fakes
  the API for command-loop tests and records bearer tokens per request.

## Architecture

One new module, `edge/gateway/src/identity.rs`, owns persistence and crypto.
`main.rs` calls three things: load at boot, save after enrollment, and wipe
when `GATEWAY_REENROLL=true`. Nothing else in the gateway sees file paths,
keys or nonces.

## What persists, and where

```json
{ "version": 1,
  "gateway_token": "...", "gateway_id": "...",
  "customer_id": "...", "customer_name": "...",
  "site_id": "...", "site_name": "...", "city": "...",
  "camera_limit": 0 }
```

serialized and encrypted to `<GATEWAY_STATE_DIR>/identity.enc`.
`GATEWAY_STATE_DIR` defaults to `data/gateway` (relative, like the API's
`data/vms.db`, so `cargo run` needs no root); the directory is created with
mode 0700. Compose gains a `gateway-data:/data` volume and
`GATEWAY_STATE_DIR: /data`. An unknown `version` is an error, not a guess.

## Encryption

XChaCha20-Poly1305 (RustCrypto `chacha20poly1305`, the gateway's one new
dependency). A random 24-byte nonce is generated per write and prepended to
the ciphertext. The 32-byte key comes from `GATEWAY_STATE_KEY` (64 hex
chars) when set; otherwise from `<state dir>/identity.key`, generated on
first boot with mode 0600.

The docs state plainly what this buys: the state file alone — in a backup,
on a copied SD card, in a world-readable mistake — is useless without the
key file or env key. It is not protection against root on the box.

## Boot rules

Evaluated in `main` before the enrollment step, as a pure decision over
(state present, state decryptable, `ENROLLMENT_TOKEN` set, `GATEWAY_REENROLL`):

1. `GATEWAY_REENROLL=true` → delete `identity.enc`, log loudly, proceed as
   if no state. (Refuse to start if the flag is set with no
   `ENROLLMENT_TOKEN` — a wipe with nothing to re-enroll from would brick
   the gateway's identity for no gain.)
2. Valid persisted identity → apply it to `config` (token, customer/site
   fields, camera_limit) and ignore `ENROLLMENT_TOKEN`.
3. No state + `ENROLLMENT_TOKEN` → enroll exactly as today, then persist
   the result.
4. No state, no token → today's behavior: env `GATEWAY_TOKEN` bootstrap,
   nothing persisted.
5. State present but undecryptable or corrupt → **refuse to start**, with a
   message naming the two fixes: restore the matching key
   (`identity.key` / `GATEWAY_STATE_KEY`), or set `GATEWAY_REENROLL=true`
   with a fresh enrollment token. Silently discarding an identity would be
   worse than a crash loop.

## Testing

TDD as always.

- Identity module: encrypt→decrypt roundtrip; a wrong key fails to decrypt;
  a truncated/corrupt file fails; the key file is created with mode 0600 and
  reused on the next load; `GATEWAY_STATE_KEY` overrides the key file; an
  unknown JSON version is an error.
- Boot decision: a pure function unit-tested for all five arms above.
- End-to-end against `FakeControlPlane` (extended to answer
  `POST /api/v1/gateways/enroll` once and refuse a second claim, mirroring
  the real API): first boot enrolls and persists into a tempdir; a second
  boot from the same dir uses the persisted token without calling `/enroll`
  again (the fake counts enroll requests); `GATEWAY_REENROLL=true` wipes and
  re-enrolls.
- Docs and compose: RUNNING-LOCALLY gains a "Gateway identity" section
  (where the state lives, what the encryption protects against, the
  recovery flag); compose gets the volume; the backlog line is checked off.

## Out of scope

Per-camera credential store encrypted at rest (its own backlog item),
automatic re-enrollment on 401, key rotation, TPM/HSM integration, gateway
revocation (its own backlog item).
