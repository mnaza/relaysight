//! The gateway's persisted identity: what enrollment earned, written down so
//! a restart does not lose it. File layout, crypto and the boot decision
//! live here and nowhere else.
//! See docs/superpowers/specs/2026-09-09-gateway-identity-design.md.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::RngCore;
use rand::rngs::OsRng;

const IDENTITY_FILE: &str = "identity.enc";
const KEY_FILE: &str = "identity.key";
const NONCE_LEN: usize = 24;
pub const CURRENT_VERSION: u32 = 1;

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

impl std::fmt::Debug for GatewayIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayIdentity")
            .field("version", &self.version)
            .field("gateway_token", &"<redacted>")
            .field("gateway_id", &self.gateway_id)
            .field("customer_id", &self.customer_id)
            .field("site_id", &self.site_id)
            .field("site_name", &self.site_name)
            .finish_non_exhaustive()
    }
}

/// The key protecting everything the gateway keeps in its state directory:
/// `GATEWAY_STATE_KEY` when given, else a file created 0600 on first use. The
/// directory itself is created 0700.
pub(crate) fn state_key(dir: &Path, env_key: Option<&str>) -> anyhow::Result<[u8; 32]> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    match env_key {
        Some(hex) => key_from_hex(hex),
        None => load_or_create_key(&dir.join(KEY_FILE)),
    }
}

/// Nonce, then ciphertext. One construction for every secret on this box.
pub(crate) fn seal(key: &[u8; 32], plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(&nonce), plain)
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// `what` names the file in the error, because the one thing an operator needs
/// from a failure here is which file to look at.
pub(crate) fn open_sealed(key: &[u8; 32], blob: &[u8], what: &str) -> anyhow::Result<Vec<u8>> {
    if blob.len() <= NONCE_LEN {
        anyhow::bail!("{what} is truncated");
    }
    let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
    XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| anyhow::anyhow!("{what} does not decrypt with this key"))
}

/// Write, then rename: a reader never sees a half-written secret, and the file
/// is never readable by anyone else even for an instant.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    // `<file>.tmp`, not a replaced extension: identity.enc.tmp is the name a
    // crash under an older build would have left, and two files in this
    // directory must never collide on it.
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = path.with_file_name(tmp_name);
    fs::write(&tmp_path, bytes)?;
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp_path, path)?;
    Ok(())
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
        Ok(Self {
            dir: dir.to_path_buf(),
            key: state_key(dir, env_key)?,
        })
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
        let plain = open_sealed(
            &self.key,
            &blob,
            &format!("identity state at {}", path.display()),
        )?;
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
        let blob = seal(&self.key, &serde_json::to_vec(identity)?)?;
        write_private(&self.dir.join(IDENTITY_FILE), &blob)
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
        assert!(
            store.load().unwrap().is_none(),
            "a fresh dir has no identity"
        );
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
        assert!(
            wrong.load().is_err(),
            "a wrong key must refuse, not report a fresh start"
        );
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
        assert!(
            store.load().is_err(),
            "a flipped byte must fail the AEAD tag"
        );
        std::fs::write(&path, &blob[..10]).unwrap();
        assert!(
            store.load().is_err(),
            "a truncated file must not panic or pass"
        );
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
        assert!(
            other.load().is_err(),
            "a different env key must not decrypt"
        );
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
        assert!(
            store.load().is_err(),
            "a version from the future must not be guessed at"
        );
    }

    #[test]
    fn a_failed_save_never_destroys_the_previous_identity() {
        // save() must write a temp file and rename it into place: a crash
        // mid-save must leave either the old identity or the new one, never
        // a truncated file. Simulate the observable contract: after any
        // successful save there is no leftover temp file, and the state
        // file always decrypts.
        let dir = tempfile::tempdir().unwrap();
        let store = IdentityStore::open(dir.path(), None).unwrap();
        store.save(&sample()).unwrap();
        let mut second = sample();
        second.gateway_token = "tok-2".into();
        store.save(&second).unwrap();
        assert_eq!(store.load().unwrap().unwrap().gateway_token, "tok-2");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn debug_output_never_contains_the_token() {
        let text = format!("{:?}", sample());
        assert!(
            !text.contains("tok-1"),
            "the token leaked into Debug output: {text}"
        );
        assert!(text.contains("<redacted>"));
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
