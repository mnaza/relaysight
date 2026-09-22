//! `vms-gateway update`: take a newer signed release, and give it back if it
//! does not come up.
//!
//! Fetching and believing a release is [`fetch`]; putting it in place is
//! [`swap_and_confirm`], which only keeps the new binary if the gateway proves
//! itself afterwards — a heartbeat the API accepted, recorded by
//! [`record_heartbeat`]. An API that is down during an update therefore reads
//! as a bad release and is rolled back: the safe mistake, and the next run
//! tries again.
//! See docs/superpowers/specs/2026-09-22-gateway-installer-design.md.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Where releases come from unless `GATEWAY_UPDATE_URL` says otherwise.
pub const DEFAULT_CHANNEL: &str = "https://github.com/mnaza/relaysight/releases/latest/download";
/// Written to the state directory after every heartbeat the API accepts.
pub const HEARTBEAT_MARKER: &str = "last-heartbeat";

/// The release key compiled into this build (raw Ed25519, base64). Empty until a release
/// key exists, and a build without one refuses every release. `GATEWAY_RELEASE_PUBKEY` at
/// build time replaces it, which is how the installer check signs with a throwaway key.
const RELEASE_PUBKEY: &str = "";

pub fn release_public_key() -> Vec<u8> {
    use base64::Engine as _;
    let encoded = option_env!("GATEWAY_RELEASE_PUBKEY").unwrap_or(RELEASE_PUBKEY);
    // A malformed key is no key: verification refuses everything rather than guessing.
    base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .unwrap_or_default()
}

/// What putting a new binary in place came to.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Updated { from: String, to: String },
    RolledBack { from: String, to: String },
}

/// The service the binary belongs to, as far as an update needs it.
pub trait Service {
    fn restart(&mut self) -> anyhow::Result<()>;
    /// Whether the gateway has proven itself since `since`.
    fn came_up_since(&mut self, since: SystemTime) -> bool;
}

/// The newer release's version and binary, or `None` when this is already the latest.
pub async fn fetch(
    client: &reqwest::Client,
    channel: &str,
    public_key: &[u8],
    current: &str,
    arch: &str,
) -> anyhow::Result<Option<(String, Vec<u8>)>> {
    let base = channel.trim_end_matches('/');
    let manifest = get(client, &format!("{base}/release.json")).await?;
    let signature = get(client, &format!("{base}/release.json.sig")).await?;
    let signature = String::from_utf8(signature)
        .map_err(|_| anyhow::anyhow!("release signature is not text"))?;
    let manifest = crate::release::verify_manifest(&manifest, &signature, public_key)?;
    if !crate::release::is_newer(&manifest.version, current)? {
        return Ok(None);
    }
    let entry = crate::release::binary_for(&manifest, arch)?;
    let binary = get(client, &entry.url).await?;
    crate::release::check_sha256(&binary, &entry.sha256)?;
    Ok(Some((manifest.version, binary)))
}

async fn get(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let response = client.get(url).send().await?.error_for_status()?;
    Ok(response.bytes().await?.to_vec())
}

/// Put `new_binary` in place of `binary`, restart, and keep it only if it comes up. The
/// previous binary stays beside it as `<binary>.prev` either way.
pub fn swap_and_confirm(
    binary: &Path,
    new_binary: &[u8],
    service: &mut dyn Service,
    from: &str,
    to: &str,
) -> anyhow::Result<Outcome> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    let beside = |suffix: &str| {
        let mut name = binary.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        binary.with_file_name(name)
    };
    let (new, prev) = (beside(".new"), beside(".prev"));

    // Same directory, so both renames are atomic: at every instant there is a complete
    // binary at `binary`, and a crash leaves either the old one or the new one there.
    {
        let mut file = std::fs::File::create(&new)?;
        file.write_all(new_binary)?;
        file.sync_all()?;
        // Closed here, not at the end of the function: the kernel will not execute a
        // file that is still open for writing (ETXTBSY), and the restart below runs it.
    }
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(binary, &prev)?;
    std::fs::rename(&new, binary)?;

    let restarted = SystemTime::now();
    let came_up = match service.restart() {
        Ok(()) => service.came_up_since(restarted),
        Err(err) => {
            tracing::warn!(error = %err, "restart after update failed");
            false
        }
    };
    if came_up {
        return Ok(Outcome::Updated {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }

    // The same two steps as forward: a crash here leaves a whole binary in place.
    std::fs::copy(&prev, &new)?;
    std::fs::rename(&new, binary)?;
    if let Err(err) = service.restart() {
        tracing::error!(error = %err, "restart after rolling back failed");
    }
    Ok(Outcome::RolledBack {
        from: from.to_owned(),
        to: to.to_owned(),
    })
}

/// Note that the API just accepted a heartbeat. Failure is not worth stopping for: the
/// worst it costs is that an update in progress is rolled back.
pub fn record_heartbeat(state_dir: &Path) {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    if let Err(err) = std::fs::write(state_dir.join(HEARTBEAT_MARKER), now.to_string()) {
        tracing::debug!(error = %err, "could not record the heartbeat");
    }
}

/// When the marker says the last accepted heartbeat was, if it says anything.
fn last_heartbeat(marker: &Path) -> Option<SystemTime> {
    let millis: u64 = std::fs::read_to_string(marker).ok()?.trim().parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_millis(millis))
}

/// The systemd unit a site box runs, and the marker it writes.
pub struct Systemd {
    pub unit: String,
    pub marker: PathBuf,
    pub wait: Duration,
}

impl Service for Systemd {
    fn restart(&mut self) -> anyhow::Result<()> {
        let status = std::process::Command::new("systemctl")
            .args(["restart", &self.unit])
            .status()?;
        anyhow::ensure!(
            status.success(),
            "systemctl restart {} failed: {status}",
            self.unit
        );
        Ok(())
    }

    fn came_up_since(&mut self, since: SystemTime) -> bool {
        let deadline = std::time::Instant::now() + self.wait;
        loop {
            if last_heartbeat(&self.marker).is_some_and(|at| at >= since) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Records what an update asked of the service, and says whether it came up.
    struct FakeService {
        restarts: u32,
        /// One answer per restart, in order; missing answers are "no".
        comes_up: Vec<bool>,
        fail_first_restart: bool,
    }

    impl Service for FakeService {
        fn restart(&mut self) -> anyhow::Result<()> {
            self.restarts += 1;
            if self.fail_first_restart && self.restarts == 1 {
                anyhow::bail!("systemctl said no");
            }
            Ok(())
        }
        fn came_up_since(&mut self, _since: SystemTime) -> bool {
            let index = self.restarts as usize - 1;
            self.comes_up.get(index).copied().unwrap_or(false)
        }
    }

    fn installed(dir: &tempfile::TempDir) -> PathBuf {
        let binary = dir.path().join("vms-gateway");
        std::fs::write(&binary, b"old binary").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        binary
    }

    #[test]
    fn a_release_that_comes_up_is_kept_and_the_old_one_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let binary = installed(&dir);
        let mut service = FakeService {
            restarts: 0,
            comes_up: vec![true],
            fail_first_restart: false,
        };
        let outcome = swap_and_confirm(&binary, b"new binary", &mut service, "0.1.0", "0.2.0")
            .expect("a clean update");
        assert_eq!(
            outcome,
            Outcome::Updated {
                from: "0.1.0".into(),
                to: "0.2.0".into()
            }
        );
        assert_eq!(std::fs::read(&binary).unwrap(), b"new binary");
        assert_eq!(
            std::fs::read(dir.path().join("vms-gateway.prev")).unwrap(),
            b"old binary",
            "the previous binary stays for a manual rollback"
        );
        assert!(!dir.path().join("vms-gateway.new").exists());
        let mode = std::fs::metadata(&binary).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the new binary has to be runnable");
        assert_eq!(service.restarts, 1);
    }

    #[test]
    fn a_release_that_never_comes_up_is_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        let binary = installed(&dir);
        let mut service = FakeService {
            restarts: 0,
            comes_up: vec![false, true],
            fail_first_restart: false,
        };
        let outcome = swap_and_confirm(&binary, b"broken binary", &mut service, "0.1.0", "0.2.0")
            .expect("a rollback is an outcome, not an error");
        assert_eq!(
            outcome,
            Outcome::RolledBack {
                from: "0.1.0".into(),
                to: "0.2.0".into()
            }
        );
        assert_eq!(
            std::fs::read(&binary).unwrap(),
            b"old binary",
            "the old binary is back"
        );
        assert_eq!(service.restarts, 2, "and restarted, so it is running again");
    }

    #[test]
    fn a_restart_that_fails_is_rolled_back_too() {
        let dir = tempfile::tempdir().unwrap();
        let binary = installed(&dir);
        let mut service = FakeService {
            restarts: 0,
            comes_up: vec![true, true],
            fail_first_restart: true,
        };
        let outcome =
            swap_and_confirm(&binary, b"new binary", &mut service, "0.1.0", "0.2.0").unwrap();
        assert!(matches!(outcome, Outcome::RolledBack { .. }), "{outcome:?}");
        assert_eq!(std::fs::read(&binary).unwrap(), b"old binary");
    }

    /// Runs the binary on restart, the way systemd would, and comes up if it ran.
    struct ExecService {
        binary: PathBuf,
    }

    impl Service for ExecService {
        fn restart(&mut self) -> anyhow::Result<()> {
            let status = std::process::Command::new(&self.binary).status()?;
            anyhow::ensure!(status.success(), "exited {status}");
            Ok(())
        }
        fn came_up_since(&mut self, _since: SystemTime) -> bool {
            true
        }
    }

    /// Found by `make check-installer`: a binary still open for writing cannot be executed
    /// (ETXTBSY), so the new binary has to be closed before anything tries to run it.
    #[test]
    fn the_new_binary_can_be_executed_the_moment_it_is_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let binary = installed(&dir);
        let mut service = ExecService {
            binary: binary.clone(),
        };
        let outcome = swap_and_confirm(
            &binary,
            b"#!/bin/sh\nexit 0\n",
            &mut service,
            "0.1.0",
            "0.2.0",
        )
        .unwrap();
        assert!(
            matches!(outcome, Outcome::Updated { .. }),
            "the new binary could not be run: {outcome:?}"
        );
    }

    #[test]
    fn a_heartbeat_newer_than_the_restart_is_proof_and_an_older_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut service = Systemd {
            unit: "relaysight-gateway".into(),
            marker: dir.path().join(HEARTBEAT_MARKER),
            wait: Duration::ZERO,
        };
        let before = SystemTime::now() - Duration::from_secs(5);
        assert!(!service.came_up_since(before), "no heartbeat at all");

        record_heartbeat(dir.path());
        assert!(
            service.came_up_since(before),
            "a heartbeat after the restart"
        );
        let after = SystemTime::now() + Duration::from_secs(5);
        assert!(
            !service.came_up_since(after),
            "a heartbeat from before the restart proves nothing about the new binary"
        );
    }

    // --- fetch, against a small static server ---

    /// Serve `files` by path on `listener`; anything else is a 404.
    fn serve(listener: tokio::net::TcpListener, files: HashMap<String, Vec<u8>>) {
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let files = files.clone();
                tokio::spawn(async move {
                    let mut request = vec![0u8; 4096];
                    let n = stream.read(&mut request).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&request[..n]);
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let response = match files.get(&path) {
                        Some(body) => [
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes(),
                            body.clone(),
                        ]
                        .concat(),
                        None => b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_vec(),
                    };
                    let _ = stream.write_all(&response).await;
                });
            }
        });
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        ring::digest::digest(&ring::digest::SHA256, bytes)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn keypair() -> Ed25519KeyPair {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    /// A channel publishing `version`, signed with `key`, whose x86_64 binary is `binary`
    /// and whose manifest claims `claimed` as its hash (the real one when `None`).
    async fn channel(
        key: &Ed25519KeyPair,
        version: &str,
        binary: &[u8],
        claimed: Option<&str>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hash = claimed
            .map(str::to_owned)
            .unwrap_or_else(|| sha256_hex(binary));
        let manifest = format!(
            r#"{{"version":"{version}","binaries":{{"x86_64":{{"url":"{url}/vms-gateway-x86_64-linux","sha256":"{hash}"}}}}}}"#
        );
        let signature =
            base64::engine::general_purpose::STANDARD.encode(key.sign(manifest.as_bytes()));
        serve(
            listener,
            HashMap::from([
                ("/release.json".to_owned(), manifest.into_bytes()),
                ("/release.json.sig".to_owned(), signature.into_bytes()),
                ("/vms-gateway-x86_64-linux".to_owned(), binary.to_vec()),
            ]),
        );
        url
    }

    #[tokio::test]
    async fn a_newer_signed_release_is_fetched_with_its_binary() {
        let key = keypair();
        let url = channel(&key, "0.2.0", b"new binary", None).await;
        let fetched = fetch(
            &reqwest::Client::new(),
            &url,
            key.public_key().as_ref(),
            "0.1.0",
            "x86_64",
        )
        .await
        .expect("a good release");
        assert_eq!(fetched, Some(("0.2.0".to_owned(), b"new binary".to_vec())));
    }

    #[tokio::test]
    async fn the_version_already_running_is_nothing_to_fetch() {
        let key = keypair();
        let url = channel(&key, "0.2.0", b"new binary", None).await;
        let fetched = fetch(
            &reqwest::Client::new(),
            &url,
            key.public_key().as_ref(),
            "0.2.0",
            "x86_64",
        )
        .await
        .unwrap();
        assert_eq!(fetched, None);
    }

    #[tokio::test]
    async fn a_release_signed_by_someone_else_is_refused() {
        let url = channel(&keypair(), "0.2.0", b"new binary", None).await;
        let ours = keypair();
        assert!(
            fetch(
                &reqwest::Client::new(),
                &url,
                ours.public_key().as_ref(),
                "0.1.0",
                "x86_64"
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_binary_that_is_not_the_one_the_manifest_names_is_refused() {
        let key = keypair();
        let url = channel(
            &key,
            "0.2.0",
            b"swapped binary",
            Some(&sha256_hex(b"new binary")),
        )
        .await;
        assert!(
            fetch(
                &reqwest::Client::new(),
                &url,
                key.public_key().as_ref(),
                "0.1.0",
                "x86_64"
            )
            .await
            .is_err(),
            "the signature pins the manifest, and the manifest pins the binary"
        );
    }

    #[tokio::test]
    async fn a_channel_with_nothing_on_it_is_an_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        serve(listener, HashMap::new());
        assert!(
            fetch(&reqwest::Client::new(), &url, &[1; 32], "0.1.0", "x86_64")
                .await
                .is_err()
        );
    }
}
