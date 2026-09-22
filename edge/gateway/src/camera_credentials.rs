//! Per-camera credentials, kept beside the gateway's identity and under the
//! same key. What a camera needs before it will say who it is, so the entries
//! are keyed by address rather than by camera id.
//! See docs/superpowers/specs/2026-09-21-per-camera-credentials-design.md.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const CREDENTIALS_FILE: &str = "camera-credentials.enc";
pub const CURRENT_VERSION: u32 = 1;

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CameraCredential {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for CameraCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraCredential")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StoredCredentials {
    version: u32,
    cameras: BTreeMap<String, CameraCredential>,
}

/// The file, its key, and the entries it holds.
pub struct CameraCredentials {
    path: PathBuf,
    key: [u8; 32],
    cameras: BTreeMap<String, CameraCredential>,
}

impl std::fmt::Debug for CameraCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraCredentials")
            .field("cameras", &self.cameras)
            .finish()
    }
}

impl CameraCredentials {
    /// An absent file is an empty store: a gateway that has never been given a
    /// per-camera password is not a gateway in trouble. A file that will not
    /// decrypt is an error — reading it as "no cameras" would silently send
    /// every camera the fallback password instead.
    pub fn load(dir: &Path, env_key: Option<&str>) -> anyhow::Result<Self> {
        let key = crate::identity::state_key(dir, env_key)?;
        let path = dir.join(CREDENTIALS_FILE);
        let cameras = match std::fs::read(&path) {
            Ok(blob) => {
                let plain = crate::identity::open_sealed(
                    &key,
                    &blob,
                    &format!("camera credentials at {}", path.display()),
                )?;
                let stored: StoredCredentials = serde_json::from_slice(&plain)?;
                if stored.version != CURRENT_VERSION {
                    anyhow::bail!(
                        "camera credentials version {} is newer than the {} this build understands",
                        stored.version,
                        CURRENT_VERSION
                    );
                }
                stored.cameras
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self { path, key, cameras })
    }

    /// A store with no file behind it, for a gateway that keeps no state and
    /// for tests. It holds nothing and refuses to be written.
    pub fn empty() -> Self {
        Self {
            path: PathBuf::new(),
            key: [0u8; 32],
            cameras: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, host: &str, username: &str, password: &str) -> anyhow::Result<()> {
        self.cameras.insert(
            normalise(host),
            CameraCredential {
                username: username.to_owned(),
                password: password.to_owned(),
            },
        );
        self.save()
    }

    /// True when there was something to remove.
    pub fn remove(&mut self, host: &str) -> anyhow::Result<bool> {
        let removed = self.cameras.remove(&normalise(host)).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// What is stored, for `credentials list`: hosts and usernames, never a
    /// password.
    pub fn hosts(&self) -> Vec<(String, String)> {
        self.cameras
            .iter()
            .map(|(host, credential)| (host.clone(), credential.username.clone()))
            .collect()
    }

    /// The credential for an address: the exact entry, else the one stored for
    /// the bare host. A camera answers ONVIF on one port and RTSP on another,
    /// and it is one camera with one password.
    pub fn get(&self, address: &str) -> Option<&CameraCredential> {
        let key = normalise(address);
        if let Some(found) = self.cameras.get(&key) {
            return Some(found);
        }
        let (bare, _) = split_host_port(&key);
        if let Some(found) = self.cameras.get(bare) {
            return Some(found);
        }
        // Stored with a port, asked for without one: still one camera, as long
        // as only one entry can answer.
        let mut matches = self
            .cameras
            .iter()
            .filter(|(host, _)| matches!(split_host_port(host), (h, Some(_)) if h == bare));
        match (matches.next(), matches.next()) {
            (Some((_, only)), None) => Some(only),
            _ => None,
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        if self.path.as_os_str().is_empty() {
            anyhow::bail!("this credential store has no file to write to");
        }
        let stored = StoredCredentials {
            version: CURRENT_VERSION,
            cameras: self.cameras.clone(),
        };
        let blob = crate::identity::seal(&self.key, &serde_json::to_vec(&stored)?)?;
        crate::identity::write_private(&self.path, &blob)
    }
}

/// Lowercased, trimmed, with any scheme and path removed, so the same camera
/// written three ways lands on one entry.
pub fn normalise(address: &str) -> String {
    let trimmed = address.trim();
    let without_scheme = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    let authority = without_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(without_scheme);
    // Credentials in a URL are the camera's, not an entry's key.
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    host.to_lowercase()
}

/// Split an address into host and port. An IPv6 literal keeps its brackets —
/// that is how `url` reports it and how it has to be written to be read back —
/// and its own colons are not a port separator.
fn split_host_port(address: &str) -> (&str, Option<&str>) {
    if let Some(end) = address.strip_prefix('[').and_then(|_| address.find(']')) {
        let (host, rest) = address.split_at(end + 1);
        return (host, rest.strip_prefix(':'));
    }
    match address.rsplit_once(':') {
        // A bare IPv6 address without brackets: every colon is the address.
        Some(_) if address.matches(':').count() > 1 => (address, None),
        Some((host, port)) => (host, Some(port)),
        None => (address, None),
    }
}

/// `vms-gateway credentials …`, run on the box that talks to the cameras.
/// Parsed by hand: the gateway is configured by environment variables and one
/// command does not earn an argument parser.
pub const USAGE: &str = "\
usage: vms-gateway credentials set <host> <username>   # password on stdin
       vms-gateway credentials remove <host>
       vms-gateway credentials list

The host is the camera's address, as in ONVIF_HOSTS. Credentials live in
GATEWAY_STATE_DIR, encrypted with the same key as the gateway's identity.";

/// Runs a `credentials` invocation. `args` are what follows the subcommand,
/// `read_password` is asked only by `set`, and `out` collects what a person
/// should see. Err means the command failed; the caller prints and exits.
pub fn run(
    dir: &Path,
    env_key: Option<&str>,
    args: &[String],
    read_password: impl FnOnce() -> anyhow::Result<String>,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let mut store = CameraCredentials::load(dir, env_key)?;
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["set", host, username] => {
            let password = read_password()?;
            let password = password.trim_end_matches(['\n', '\r']);
            if password.is_empty() {
                anyhow::bail!("an empty password is not a password; nothing was written");
            }
            store.set(host, username, password)?;
            writeln!(out, "{}: credentials for {username}", normalise(host))?;
            Ok(())
        }
        ["remove", host] => {
            if store.remove(host)? {
                writeln!(out, "{}: removed", normalise(host))?;
            } else {
                writeln!(out, "{}: nothing stored", normalise(host))?;
            }
            Ok(())
        }
        ["list"] => {
            let hosts = store.hosts();
            if hosts.is_empty() {
                writeln!(
                    out,
                    "no per-camera credentials; every camera uses CAMERA_USERNAME"
                )?;
            }
            for (host, username) in hosts {
                writeln!(out, "{host}\t{username}")?;
            }
            Ok(())
        }
        _ => anyhow::bail!("{USAGE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn credentials_survive_a_reload_and_never_hit_the_disk_in_the_clear() {
        let state = dir();
        let mut store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        store
            .set("192.168.1.50", "admin", "front-door-secret")
            .unwrap();
        store
            .set("192.168.1.51:8080", "operator", "back-door-secret")
            .unwrap();

        let reopened = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        assert_eq!(reopened.get("192.168.1.50").unwrap().username, "admin");
        assert_eq!(
            reopened.get("192.168.1.51:8080").unwrap().password,
            "back-door-secret"
        );

        let path = state.path().join(CREDENTIALS_FILE);
        let raw = std::fs::read(&path).unwrap();
        for secret in ["front-door-secret", "back-door-secret", "admin"] {
            assert!(
                !raw.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "{secret} is on disk in the clear"
            );
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the file is readable by others");
    }

    #[test]
    fn a_gateway_with_no_credential_file_has_an_empty_store() {
        let state = dir();
        let store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        assert!(store.hosts().is_empty());
        assert!(store.get("192.168.1.50").is_none());
    }

    #[test]
    fn a_file_that_does_not_decrypt_is_an_error_not_an_empty_store() {
        let state = dir();
        let mut store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        store.set("192.168.1.50", "admin", "secret").unwrap();
        let other = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let err = CameraCredentials::load(state.path(), Some(other))
            .expect_err("a wrong key must not read as no cameras");
        assert!(
            format!("{err}").contains("decrypt"),
            "the error must say why: {err}"
        );
    }

    #[test]
    fn an_address_is_found_however_it_is_written() {
        let state = dir();
        let mut store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        store.set("192.168.1.50:80", "admin", "secret").unwrap();
        for written in [
            "192.168.1.50:80",
            " 192.168.1.50:80 ",
            "HTTP://192.168.1.50:80/onvif/device_service",
            // No port at all: the only entry for that host answers.
            "192.168.1.50",
            "rtsp://192.168.1.50/stream",
        ] {
            assert!(
                store.get(written).is_some(),
                "{written} did not reach the stored entry"
            );
        }
        assert!(store.get("192.168.1.99").is_none(), "a different camera");
    }

    #[test]
    fn an_exact_entry_wins_over_the_bare_host() {
        let state = dir();
        let mut store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        store
            .set("192.168.1.50", "shared", "shared-secret")
            .unwrap();
        store.set("192.168.1.50:8080", "own", "own-secret").unwrap();
        assert_eq!(store.get("192.168.1.50:8080").unwrap().username, "own");
        assert_eq!(store.get("192.168.1.50:554").unwrap().username, "shared");
    }

    #[test]
    fn an_ipv6_camera_is_found_by_the_address_onvif_reports() {
        // url::Url keeps the brackets, so that is the form both sides use.
        let state = dir();
        let mut store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        store.set("[2001:db8::1]:8080", "admin", "secret").unwrap();
        for written in [
            "[2001:db8::1]:8080",
            "http://[2001:db8::1]:8080/onvif/device_service",
            // The same camera's stream, on its own port, and with no port.
            "rtsp://[2001:db8::1]:554/stream",
            "[2001:db8::1]",
        ] {
            assert!(
                store.get(written).is_some(),
                "{written} did not reach the stored entry"
            );
        }
        assert!(
            store.get("[2001:db8::2]").is_none(),
            "a different camera on the same network"
        );
    }

    #[test]
    fn a_password_in_a_url_is_not_part_of_the_key() {
        let state = dir();
        let mut store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        store.set("192.168.1.50", "admin", "secret").unwrap();
        assert!(
            store
                .get("rtsp://someone:something@192.168.1.50/stream")
                .is_some(),
            "userinfo in the URL must not hide the camera"
        );
    }

    #[test]
    fn a_damaged_file_is_an_error_and_a_future_version_says_so() {
        let state = dir();
        let mut store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        store.set("192.168.1.50", "admin", "secret").unwrap();
        let path = state.path().join(CREDENTIALS_FILE);

        let mut blob = std::fs::read(&path).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        std::fs::write(&path, &blob).unwrap();
        let err = CameraCredentials::load(state.path(), Some(KEY))
            .expect_err("a flipped byte must not read as no cameras");
        assert!(format!("{err}").contains("decrypt"), "{err}");

        // A file from a newer build says which version it is, rather than
        // being read with half its meaning missing.
        let key = crate::identity::state_key(state.path(), Some(KEY)).unwrap();
        let future = serde_json::json!({"version": CURRENT_VERSION + 1, "cameras": {}});
        let sealed = crate::identity::seal(&key, &serde_json::to_vec(&future).unwrap()).unwrap();
        crate::identity::write_private(&path, &sealed).unwrap();
        let err = CameraCredentials::load(state.path(), Some(KEY)).expect_err("newer version");
        assert!(
            format!("{err}").contains(&format!("version {}", CURRENT_VERSION + 1)),
            "the error must name the version: {err}"
        );
    }

    #[test]
    fn removing_an_entry_takes_it_away_for_good() {
        let state = dir();
        let mut store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        store.set("192.168.1.50", "admin", "secret").unwrap();
        assert!(store.remove("192.168.1.50").unwrap());
        assert!(
            !store.remove("192.168.1.50").unwrap(),
            "nothing left to remove"
        );
        assert!(
            CameraCredentials::load(state.path(), Some(KEY))
                .unwrap()
                .get("192.168.1.50")
                .is_none()
        );
    }

    #[test]
    fn listing_and_debugging_keep_the_passwords_in() {
        let state = dir();
        let mut store = CameraCredentials::load(state.path(), Some(KEY)).unwrap();
        store
            .set("192.168.1.50", "admin", "front-door-secret")
            .unwrap();
        let listed = store.hosts();
        assert_eq!(
            listed,
            vec![("192.168.1.50".to_string(), "admin".to_string())]
        );
        let printed = format!("{store:?}");
        assert!(
            !printed.contains("front-door-secret"),
            "a password reached a log line: {printed}"
        );
        assert!(printed.contains("admin"), "the username is not a secret");
    }

    fn run_args(dir: &Path, args: &[&str], password: &str) -> anyhow::Result<String> {
        let mut out = Vec::new();
        let owned: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        run(dir, Some(KEY), &owned, || Ok(password.to_owned()), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_subcommand_sets_lists_and_removes() {
        let state = dir();
        run_args(
            state.path(),
            &["set", "192.168.1.50", "admin"],
            "front-door-secret\n",
        )
        .unwrap();
        let listed = run_args(state.path(), &["list"], "").unwrap();
        assert!(
            listed.contains("192.168.1.50"),
            "the host is missing: {listed}"
        );
        assert!(
            listed.contains("admin"),
            "the username is missing: {listed}"
        );
        assert!(
            !listed.contains("front-door-secret"),
            "a password was printed: {listed}"
        );
        // The running gateway reads what the command wrote.
        assert_eq!(
            CameraCredentials::load(state.path(), Some(KEY))
                .unwrap()
                .get("192.168.1.50")
                .unwrap()
                .password,
            "front-door-secret",
            "the trailing newline of a piped password is not part of it"
        );

        assert!(
            run_args(state.path(), &["remove", "192.168.1.50"], "")
                .unwrap()
                .contains("removed")
        );
        assert!(
            run_args(state.path(), &["list"], "")
                .unwrap()
                .contains("CAMERA_USERNAME"),
            "an empty list should say what happens instead"
        );
    }

    #[test]
    fn the_subcommand_refuses_an_empty_password_and_an_unknown_verb() {
        let state = dir();
        let err = run_args(state.path(), &["set", "192.168.1.50", "admin"], "\n")
            .expect_err("an empty password must not be stored");
        assert!(format!("{err}").contains("empty password"), "{err}");
        assert!(
            CameraCredentials::load(state.path(), Some(KEY))
                .unwrap()
                .hosts()
                .is_empty(),
            "nothing should have been written"
        );

        let err = run_args(state.path(), &["rotate", "192.168.1.50"], "x")
            .expect_err("an unknown verb must not pass silently");
        assert!(format!("{err}").contains("usage:"), "{err}");
    }
}
