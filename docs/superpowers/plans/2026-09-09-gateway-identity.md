# Encrypted Gateway Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The gateway's enrolled identity (token, customer/site, entitlement limit) survives restarts: persisted on the edge, encrypted at rest, with a loud recovery path.

**Architecture:** One new module `edge/gateway/src/identity.rs` owns file layout, crypto (XChaCha20-Poly1305, key file or env key) and the boot decision table; `main.rs` calls a single `establish_identity` step where it used to call `enroll_if_requested` directly. The fake control plane learns to answer `/gateways/enroll` claim-exactly-once so restart behavior is testable end to end.

**Tech Stack:** Rust; new dep `chacha20poly1305 = "0.10"` (gateway only); `rand 0.8` (already a gateway dep) for key/nonce bytes; `tempfile` added to gateway dev-dependencies.

**Spec:** `docs/superpowers/specs/2026-09-09-gateway-identity-design.md`

## Global Constraints

- No production code without a failing test first; run the test and watch it fail before implementing.
- The repo-root `Cargo.toml` carries a local uncommitted `[patch.crates-io]` retina patch. **In any fresh worktree, append it FIRST, before any cargo command** (path `/home/andrey/work/retina-fork`), and never stage `Cargo.toml` or run `git add .` — stage files by explicit path. `Cargo.lock` WILL change in Task 1 (new deps); commit it only after `git diff Cargo.lock | grep -i retina` prints nothing.
- Tests: `cargo test -p vms-gateway` (currently 72 passing: 71 + 1 ignored). Web tests unaffected but run once in Task 4.
- File names and modes are exact: state dir mode 0700; `identity.enc` and `identity.key` mode 0600; nonce is 24 bytes prepended to the ciphertext; `GATEWAY_STATE_KEY` is 64 hex chars; state-dir env `GATEWAY_STATE_DIR` defaults to `data/gateway`; re-enroll env `GATEWAY_REENROLL` compares `== "true"`.
- Boot rules exactly as the spec orders them; undecryptable state is a refuse-to-start whose message names both fixes.
- The persisted JSON carries `"version": 1`; an unknown version is an error, never a guess.

## File Structure

- `edge/gateway/src/identity.rs` — create: `GatewayIdentity`, `IdentityStore` (open/load/save/wipe), `boot_plan`, unit tests.
- `edge/gateway/src/main.rs` — modify: `mod identity;`, `establish_identity`, wiring at the old enroll call site, tests.
- `edge/gateway/Cargo.toml` — modify: add `chacha20poly1305`; add `tempfile` dev-dep.
- `edge/gateway/src/fake_control_plane.rs` — modify: `/gateways/enroll` handling + `Seen.enrolls`/claimed tokens.
- `docker-compose.yml`, `docs/RUNNING-LOCALLY.md`, `docs/BACKLOG.md` — modify (Task 4).

---

### Task 1: The identity store

**Files:**
- Modify: `edge/gateway/Cargo.toml`
- Create: `edge/gateway/src/identity.rs`
- Modify: `edge/gateway/src/main.rs` (add `mod identity;` next to the other mods at the top)

**Interfaces:**
- Produces (Task 2/3 call these exact signatures):
  - `identity::GatewayIdentity { version: u32, gateway_token, gateway_id, customer_id, customer_name, site_id, site_name, city: String, camera_limit: usize }` (Serialize/Deserialize/Debug/Clone/PartialEq)
  - `identity::CURRENT_VERSION: u32` (= 1)
  - `IdentityStore::open(dir: &Path, env_key: Option<&str>) -> anyhow::Result<Self>`
  - `IdentityStore::load(&self) -> anyhow::Result<Option<GatewayIdentity>>`
  - `IdentityStore::save(&self, identity: &GatewayIdentity) -> anyhow::Result<()>`
  - `IdentityStore::wipe(&self) -> anyhow::Result<()>`

- [ ] **Step 1: Add the dependencies** — in `edge/gateway/Cargo.toml`, under `[dependencies]` after `bytes`:

```toml
chacha20poly1305 = "0.10"
```

and add a dev-dependencies section (or extend it if one exists):

```toml
[dev-dependencies]
tempfile = "3"
```

Then `cargo check -p vms-gateway` and verify `git diff Cargo.lock | grep -i retina` prints nothing.

- [ ] **Step 2: Write the failing tests** — create `edge/gateway/src/identity.rs` containing the module doc, and (for now) only the tests; add `mod identity;` in `main.rs` next to the other `mod` lines:

```rust
//! The gateway's persisted identity: what enrollment earned, written down so
//! a restart does not lose it. File layout, crypto and the boot decision
//! live here and nowhere else.
//! See docs/superpowers/specs/2026-09-09-gateway-identity-design.md.

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GatewayIdentity {
        GatewayIdentity {
            version: CURRENT_VERSION,
            gateway_token: "tok-1".into(),
            gateway_id: "gw-1".into(),
            customer_id: "cust-1".into(),
            customer_name: "Customer".into(),
            site_id: "site-1".into(),
            site_name: "Site".into(),
            city: "Barcelona".into(),
            camera_limit: 3,
        }
    }

    #[test]
    fn a_saved_identity_loads_back_and_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityStore::open(dir.path(), None).unwrap();
        assert!(store.load().unwrap().is_none(), "a fresh dir has no identity");
        store.save(&sample()).unwrap();
        assert_eq!(store.load().unwrap().unwrap(), sample());
        // The restart: a second open reuses the generated key file.
        let reopened = IdentityStore::open(dir.path(), None).unwrap();
        assert_eq!(reopened.load().unwrap().unwrap(), sample());
    }

    #[test]
    fn the_state_file_is_ciphertext_and_a_wrong_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityStore::open(dir.path(), None).unwrap();
        store.save(&sample()).unwrap();
        let blob = std::fs::read(dir.path().join("identity.enc")).unwrap();
        assert!(
            !String::from_utf8_lossy(&blob).contains("tok-1"),
            "the token is readable in the state file"
        );

        let wrong = IdentityStore::open(dir.path(), Some(&"ab".repeat(32))).unwrap();
        assert!(wrong.load().is_err(), "a wrong key must refuse, not report a fresh start");
    }

    #[test]
    fn corrupt_or_truncated_state_is_an_error_not_a_fresh_start() {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityStore::open(dir.path(), None).unwrap();
        store.save(&sample()).unwrap();
        let path = dir.path().join("identity.enc");
        let mut blob = std::fs::read(&path).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        std::fs::write(&path, &blob).unwrap();
        assert!(store.load().is_err(), "a flipped byte must fail the AEAD tag");
        std::fs::write(&path, &blob[..10]).unwrap();
        assert!(store.load().is_err(), "a truncated file must not panic or pass");
    }

    #[test]
    fn key_file_and_state_files_have_tight_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("state");
        let store = IdentityStore::open(&dir, None).unwrap();
        store.save(&sample()).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("identity.key")), 0o600);
        assert_eq!(mode(&dir.join("identity.enc")), 0o600);
    }

    #[test]
    fn an_env_key_overrides_the_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let key = "0f".repeat(32);
        let store = IdentityStore::open(dir.path(), Some(&key)).unwrap();
        store.save(&sample()).unwrap();
        assert!(
            !dir.path().join("identity.key").exists(),
            "an env key must not generate a key file"
        );
        let again = IdentityStore::open(dir.path(), Some(&key)).unwrap();
        assert_eq!(again.load().unwrap().unwrap(), sample());
        let other = IdentityStore::open(dir.path(), Some(&"11".repeat(32))).unwrap();
        assert!(other.load().is_err(), "a different env key must not decrypt");
        assert!(
            IdentityStore::open(dir.path(), Some("short")).is_err(),
            "a malformed key is refused at open, not at first use"
        );
    }

    #[test]
    fn an_unknown_version_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityStore::open(dir.path(), None).unwrap();
        let mut future = sample();
        future.version = 99;
        store.save(&future).unwrap();
        assert!(store.load().is_err(), "a version from the future must not be guessed at");
    }

    #[test]
    fn wipe_removes_state_and_wiping_nothing_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityStore::open(dir.path(), None).unwrap();
        store.wipe().unwrap();
        store.save(&sample()).unwrap();
        store.wipe().unwrap();
        assert!(store.load().unwrap().is_none());
    }
}
```

- [ ] **Step 3: Run and watch them fail**

Run: `cargo test -p vms-gateway identity::` — expected: compile error, `GatewayIdentity`/`IdentityStore` not found.

- [ ] **Step 4: Implement** — above the tests module in `identity.rs`:

```rust
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::rngs::OsRng;
use rand::RngCore;

const IDENTITY_FILE: &str = "identity.enc";
const KEY_FILE: &str = "identity.key";
const NONCE_LEN: usize = 24;
pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GatewayIdentity {
    pub version: u32,
    pub gateway_token: String,
    pub gateway_id: String,
    pub customer_id: String,
    pub customer_name: String,
    pub site_id: String,
    pub site_name: String,
    pub city: String,
    pub camera_limit: usize,
}

pub struct IdentityStore {
    dir: PathBuf,
    key: [u8; 32],
}

impl IdentityStore {
    /// Open the state dir (created 0700 if absent). The key comes from
    /// `env_key` (64 hex chars) when given, else from the key file,
    /// created with mode 0600 on first use.
    pub fn open(dir: &Path, env_key: Option<&str>) -> anyhow::Result<Self> {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let key = match env_key {
            Some(hex) => key_from_hex(hex)?,
            None => load_or_create_key(&dir.join(KEY_FILE))?,
        };
        Ok(Self { dir: dir.to_path_buf(), key })
    }

    /// Ok(None): never enrolled. Err: state exists but cannot be trusted —
    /// wrong key, corruption, or a version this build does not know.
    pub fn load(&self) -> anyhow::Result<Option<GatewayIdentity>> {
        let path = self.dir.join(IDENTITY_FILE);
        let blob = match fs::read(&path) {
            Ok(blob) => blob,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        if blob.len() <= NONCE_LEN {
            anyhow::bail!("identity state at {} is truncated", path.display());
        }
        let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let plain = cipher.decrypt(XNonce::from_slice(nonce), ciphertext).map_err(|_| {
            anyhow::anyhow!("identity state at {} does not decrypt with this key", path.display())
        })?;
        let identity: GatewayIdentity = serde_json::from_slice(&plain)?;
        if identity.version != CURRENT_VERSION {
            anyhow::bail!(
                "identity state version {} is newer than the {} this build understands",
                identity.version,
                CURRENT_VERSION
            );
        }
        Ok(Some(identity))
    }

    pub fn save(&self, identity: &GatewayIdentity) -> anyhow::Result<()> {
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), serde_json::to_vec(identity)?.as_slice())
            .map_err(|_| anyhow::anyhow!("identity encryption failed"))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);
        let path = self.dir.join(IDENTITY_FILE);
        fs::write(&path, blob)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// Removing nothing is fine — the reenroll path wipes unconditionally.
    pub fn wipe(&self) -> anyhow::Result<()> {
        match fs::remove_file(self.dir.join(IDENTITY_FILE)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

fn load_or_create_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    match fs::read(path) {
        Ok(bytes) => bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("key file {} is not 32 bytes", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0u8; 32];
            OsRng.fill_bytes(&mut key);
            fs::write(path, key)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(key)
        }
        Err(err) => Err(err.into()),
    }
}

fn key_from_hex(hex: &str) -> anyhow::Result<[u8; 32]> {
    anyhow::ensure!(
        hex.len() == 64,
        "GATEWAY_STATE_KEY must be 64 hex characters, got {}",
        hex.len()
    );
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .map_err(|_| anyhow::anyhow!("GATEWAY_STATE_KEY is not valid hex"))?;
    }
    Ok(key)
}
```

API note for the implementer: this targets chacha20poly1305 0.10 (`XChaCha20Poly1305::new(key.into())`, `Aead::encrypt/decrypt` with `XNonce::from_slice`). If the shipped signatures differ, adapt inside this file only — the tests pin behavior. `rand 0.8`'s `OsRng.fill_bytes` supplies key and nonce bytes; do not add a `getrandom`/`rand` feature juggle.

- [ ] **Step 5: Run the tests and make sure they pass**

Run: `cargo test -p vms-gateway identity::` — expected: 7 passed. Then the full `cargo test -p vms-gateway` — nothing else broke (the module is unused by production code until Task 2; expect `dead_code` warnings, do not silence).

- [ ] **Step 6: Commit**

```bash
git add edge/gateway/Cargo.toml edge/gateway/src/identity.rs edge/gateway/src/main.rs Cargo.lock
git commit -m "The gateway can write its identity down, encrypted"
```

---

### Task 2: The boot decision and main wiring

**Files:**
- Modify: `edge/gateway/src/identity.rs` (append `BootPlan` + `boot_plan`)
- Modify: `edge/gateway/src/main.rs` (`establish_identity`, call-site rewiring, tests)

**Interfaces:**
- Consumes: Task 1's `IdentityStore`/`GatewayIdentity`.
- Produces:
  - `identity::BootPlan { UsePersisted(GatewayIdentity), Enroll, Bootstrap }` (Debug, PartialEq)
  - `identity::boot_plan(loaded: anyhow::Result<Option<GatewayIdentity>>, has_enrollment_token: bool, reenroll: bool) -> anyhow::Result<BootPlan>`
  - `async fn establish_identity(config: &mut Config, client: &reqwest::Client, hostname: &str, store: &identity::IdentityStore, reenroll: bool) -> anyhow::Result<()>` in main.rs (Task 3's tests call it)

- [ ] **Step 1: Write the failing test** — in the main.rs tests module (it has `use super::*;`), add:

```rust
    fn sample_identity() -> identity::GatewayIdentity {
        identity::GatewayIdentity {
            version: identity::CURRENT_VERSION,
            gateway_token: "tok-1".into(),
            gateway_id: "gw-1".into(),
            customer_id: "cust-1".into(),
            customer_name: "Customer".into(),
            site_id: "site-1".into(),
            site_name: "Site".into(),
            city: "Barcelona".into(),
            camera_limit: 3,
        }
    }

    #[test]
    fn the_boot_decision_covers_every_arm() {
        use identity::{boot_plan, BootPlan};
        // Reenroll without a token refuses rather than wiping.
        assert!(boot_plan(Ok(None), false, true).is_err());
        // Reenroll with a token enrolls, even over state that cannot be read.
        assert!(matches!(
            boot_plan(Err(anyhow::anyhow!("corrupt")), true, true),
            Ok(BootPlan::Enroll)
        ));
        // Unreadable state without the reenroll escape hatch refuses to start,
        // and the message names both fixes.
        let err = boot_plan(Err(anyhow::anyhow!("bad key")), true, false).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("GATEWAY_REENROLL"), "no recovery hint in: {text}");
        assert!(text.contains("GATEWAY_STATE_KEY"), "no key hint in: {text}");
        // A persisted identity wins and the burned env token is ignored.
        assert!(matches!(
            boot_plan(Ok(Some(sample_identity())), true, false),
            Ok(BootPlan::UsePersisted(_))
        ));
        // Fresh state: a token enrolls; no token is today's bootstrap.
        assert!(matches!(boot_plan(Ok(None), true, false), Ok(BootPlan::Enroll)));
        assert!(matches!(boot_plan(Ok(None), false, false), Ok(BootPlan::Bootstrap)));
    }
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p vms-gateway the_boot_decision` — expected: compile error, `boot_plan` not found.

- [ ] **Step 3: Implement `boot_plan`** — append to `identity.rs` (above the tests module):

```rust
#[derive(Debug, PartialEq)]
pub enum BootPlan {
    UsePersisted(GatewayIdentity),
    Enroll,
    Bootstrap,
}

/// The identity decision at boot. Callers pass `Ok(None)` as `loaded` when
/// `reenroll` is set — the plan ignores state entirely on that path, which
/// is exactly what makes the flag the cure for undecryptable state.
pub fn boot_plan(
    loaded: anyhow::Result<Option<GatewayIdentity>>,
    has_enrollment_token: bool,
    reenroll: bool,
) -> anyhow::Result<BootPlan> {
    if reenroll {
        anyhow::ensure!(
            has_enrollment_token,
            "GATEWAY_REENROLL=true without ENROLLMENT_TOKEN would wipe the identity \
             with nothing to re-enroll from"
        );
        return Ok(BootPlan::Enroll);
    }
    match loaded {
        Err(err) => Err(err.context(
            "persisted gateway identity cannot be read; restore the matching key \
             (identity.key or GATEWAY_STATE_KEY), or set GATEWAY_REENROLL=true with \
             a fresh ENROLLMENT_TOKEN",
        )),
        Ok(Some(identity)) => Ok(BootPlan::UsePersisted(identity)),
        Ok(None) if has_enrollment_token => Ok(BootPlan::Enroll),
        Ok(None) => Ok(BootPlan::Bootstrap),
    }
}
```

- [ ] **Step 4: Run the test and make sure it passes**

Run: `cargo test -p vms-gateway the_boot_decision` — expected: PASS.

- [ ] **Step 5: Wire main** — in `main.rs`, replace the line
`enroll_if_requested(&mut config, &client, &hostname).await?;` (main.rs:143) with:

```rust
    let state_dir = env::var("GATEWAY_STATE_DIR").unwrap_or_else(|_| "data/gateway".into());
    let store = identity::IdentityStore::open(
        std::path::Path::new(&state_dir),
        env::var("GATEWAY_STATE_KEY").ok().filter(|k| !k.is_empty()).as_deref(),
    )?;
    let reenroll = env::var("GATEWAY_REENROLL").is_ok_and(|v| v == "true");
    establish_identity(&mut config, &client, &hostname, &store, reenroll).await?;
```

and add, next to `enroll_if_requested`:

```rust
/// Boot-time identity: use what enrollment earned last time, or earn it now
/// and write it down. See identity::boot_plan for the decision table.
async fn establish_identity(
    config: &mut Config,
    client: &reqwest::Client,
    hostname: &str,
    store: &identity::IdentityStore,
    reenroll: bool,
) -> anyhow::Result<()> {
    let loaded = if reenroll { Ok(None) } else { store.load() };
    match identity::boot_plan(loaded, config.enrollment_token.is_some(), reenroll)? {
        identity::BootPlan::UsePersisted(id) => {
            info!(gateway_id = %id.gateway_id, site = %id.site_name, "using persisted gateway identity");
            config.token = id.gateway_token;
            config.gateway_id = id.gateway_id;
            config.customer_id = id.customer_id;
            config.customer_name = id.customer_name;
            config.site_id = id.site_id;
            config.site_name = id.site_name;
            config.city = id.city;
            config.camera_limit = id.camera_limit;
            // The env token is one-shot and already burned; never spend it again.
            config.enrollment_token = None;
        }
        identity::BootPlan::Enroll => {
            if reenroll {
                store.wipe()?;
                warn!("GATEWAY_REENROLL: wiped persisted identity, enrolling fresh");
            }
            enroll_if_requested(config, client, hostname).await?;
            store.save(&identity::GatewayIdentity {
                version: identity::CURRENT_VERSION,
                gateway_token: config.token.clone(),
                gateway_id: config.gateway_id.clone(),
                customer_id: config.customer_id.clone(),
                customer_name: config.customer_name.clone(),
                site_id: config.site_id.clone(),
                site_name: config.site_name.clone(),
                city: config.city.clone(),
                camera_limit: config.camera_limit,
            })?;
            info!("gateway identity persisted");
        }
        identity::BootPlan::Bootstrap => {}
    }
    Ok(())
}
```

- [ ] **Step 6: Run the full suite**

Run: `cargo test -p vms-gateway` — expected: all pass (79 + the new one). The existing tests never touch `establish_identity`, so nothing else moves.

- [ ] **Step 7: Commit**

```bash
git add edge/gateway/src/identity.rs edge/gateway/src/main.rs
git commit -m "Boot rules: persisted identity wins, a token earns one, reenroll starts over"
```

---

### Task 3: Restart survival, end to end

**Files:**
- Modify: `edge/gateway/src/fake_control_plane.rs` (`/gateways/enroll` + `Seen` fields)
- Modify: `edge/gateway/src/main.rs` (tests)

**Interfaces:**
- Consumes: `establish_identity` (Task 2), `IdentityStore` (Task 1), existing test helpers `config(&api.url)` (note: its locals must be named `cfg`, the helper is called `config`).
- Produces: `Seen.enrolls: u32` and claim-exactly-once fake enrollment.

- [ ] **Step 1: Write the failing tests** — the two tests in Step 2 below reference `Seen.enrolls`, which does not exist yet: write the TESTS first (Step 2's code), run
`cargo test -p vms-gateway -- enrollment_is_persisted reenroll_wipes` and watch the compile error (`no field enrolls on Seen`) — that is the RED. Then implement this step's fake extension as the GREEN. In `fake_control_plane.rs`, add to `Seen`:

```rust
    /// Enrollment attempts, and the enrollment tokens already claimed —
    /// the real API burns each token on first claim, so the fake does too.
    pub enrolls: u32,
```

and add a `claimed` set next to `queue` in `start`:

```rust
        let claimed = Arc::new(RwLock::new(std::collections::HashSet::<String>::new()));
```

then a branch before the `/storage/uploads` one:

```rust
                } else if first.starts_with("POST") && first.contains("/gateways/enroll") {
                    recorder.write().await.enrolls += 1;
                    let token = request
                        .split_once("\r\n\r\n")
                        .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
                        .and_then(|v| v["enrollment_token"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    let mut claimed = claimed.write().await;
                    if token.is_empty() || claimed.contains(&token) {
                        // The real API answers Gone for a burned token.
                        "HTTP/1.1 410 Gone\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
                    } else {
                        claimed.insert(token.clone());
                        json_response(
                            &serde_json::json!({
                                "gateway_token": format!("enrolled-{}", claimed.len()),
                                "entitlement": {
                                    "edition": "community", "plan": "community",
                                    "self_hosted": true, "managed": false,
                                    "camera_limit": null, "capabilities": [],
                                },
                                "customer_id": "cust-1", "customer_name": "Customer",
                                "site_id": "site-1", "site_name": "Site", "city": "Barcelona",
                            })
                            .to_string(),
                        )
                    }
```

(`claimed` must be cloned into the spawned task alongside `recorder`/`queue`.)

- [ ] **Step 2: The tests** (written before Step 1's implementation, per Step 1) — in the main.rs tests module:

```rust
    #[tokio::test]
    async fn enrollment_is_persisted_and_a_restart_reuses_it_without_the_api() {
        let api = FakeControlPlane::start(Vec::new(), 0).await;
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();

        let store = identity::IdentityStore::open(dir.path(), None).unwrap();
        let mut cfg = config(&api.url);
        cfg.enrollment_token = Some("ENROLL-1".into());
        establish_identity(&mut cfg, &client, "edge-1", &store, false).await.unwrap();
        assert_eq!(cfg.token, "enrolled-1");
        assert_eq!(cfg.customer_id, "cust-1");

        // The restart: fresh env-derived config, same state dir, and the same
        // burned token still sitting in the environment.
        let store2 = identity::IdentityStore::open(dir.path(), None).unwrap();
        let mut cfg2 = config(&api.url);
        cfg2.enrollment_token = Some("ENROLL-1".into());
        establish_identity(&mut cfg2, &client, "edge-1", &store2, false).await.unwrap();
        assert_eq!(cfg2.token, "enrolled-1", "the restart must reuse the persisted token");
        assert_eq!(cfg2.site_name, "Site", "the persisted identity carries the site");
        assert_eq!(
            api.seen.read().await.enrolls,
            1,
            "the burned enrollment token must not be spent again"
        );
    }

    #[tokio::test]
    async fn reenroll_wipes_state_and_spends_a_fresh_token() {
        let api = FakeControlPlane::start(Vec::new(), 0).await;
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();

        let store = identity::IdentityStore::open(dir.path(), None).unwrap();
        let mut cfg = config(&api.url);
        cfg.enrollment_token = Some("ENROLL-1".into());
        establish_identity(&mut cfg, &client, "edge-1", &store, false).await.unwrap();

        let mut cfg2 = config(&api.url);
        cfg2.enrollment_token = Some("ENROLL-2".into());
        establish_identity(&mut cfg2, &client, "edge-1", &store, true).await.unwrap();
        assert_eq!(cfg2.token, "enrolled-2", "reenroll must earn a fresh token");
        assert_eq!(
            store.load().unwrap().unwrap().gateway_token,
            "enrolled-2",
            "the fresh identity must be the one persisted"
        );

        // And the flag alone, with no token to re-enroll from, refuses.
        let mut cfg3 = config(&api.url);
        assert!(establish_identity(&mut cfg3, &client, "edge-1", &store, true).await.is_err());
    }
```

- [ ] **Step 3: Run RED, then implement the fake, then GREEN**

RED: with the tests in place and the fake untouched, `cargo test -p vms-gateway -- enrollment_is_persisted reenroll_wipes` fails to compile (`no field enrolls`). Implement Step 1's fake extension. GREEN: the two tests now pass — no production changes should be needed beyond Tasks 1–2; these tests pin the integration. If they expose a real gap, fix it in `establish_identity`/`identity.rs` and note it in the report.

- [ ] **Step 4: Run the full suite**

Run: `cargo test -p vms-gateway` — expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add edge/gateway/src/fake_control_plane.rs edge/gateway/src/main.rs
git commit -m "A gateway restart keeps its identity; the fake API burns tokens like the real one"
```

---

### Task 4: Compose, docs, backlog, verification

**Files:**
- Modify: `docker-compose.yml`
- Modify: `docs/RUNNING-LOCALLY.md`
- Modify: `docs/BACKLOG.md`

- [ ] **Step 1: Compose** — in the `gateway` service: add to its `environment:` block

```yaml
      GATEWAY_STATE_DIR: /data
```

and to the service:

```yaml
    volumes:
      - gateway-data:/data
```

and `gateway-data:` to the top-level `volumes:` section. Validate with `docker compose config -q` (uses the gitignored override if present; that is fine — it must still parse).

- [ ] **Step 2: Docs** — in `docs/RUNNING-LOCALLY.md`, after the "Incidents" section, add a "Gateway identity" section: after a successful enrollment the gateway writes its identity (token, customer/site, camera limit) to `GATEWAY_STATE_DIR` (default `data/gateway`; `/data` under compose, on the `gateway-data` volume), encrypted with a key file generated beside it — or with `GATEWAY_STATE_KEY` (64 hex chars) when set. State without its key is useless, which is what the encryption buys: safe backups and copies, not protection from root. On later boots the persisted identity wins and `ENROLLMENT_TOKEN` is ignored; `GATEWAY_REENROLL=true` plus a fresh enrollment token wipes and re-enrolls (also the fix for undecryptable state, which otherwise refuses to start).

- [ ] **Step 3: Backlog** — check off `- [ ] Encrypted persistent gateway token / identity` in `docs/BACKLOG.md`.

- [ ] **Step 4: Full verification**

Run: `cargo test --workspace` and `cd web && npm test` — everything green; report exact counts. Verify `git diff Cargo.lock | grep -i retina` prints nothing and `git status --short` shows only expected files.

- [ ] **Step 5: Commit**

```bash
git add docker-compose.yml docs/RUNNING-LOCALLY.md docs/BACKLOG.md
git commit -m "The gateway keeps its identity across restarts; compose and docs say how"
```
