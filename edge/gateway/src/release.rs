//! What a release is, and whether to believe one.
//!
//! A release is a manifest — its version and, per architecture, a binary's URL
//! and SHA-256 — plus an Ed25519 signature over the manifest's exact bytes. The
//! signature pins the manifest and the manifest pins the binaries, so nothing
//! downloaded is trusted until both check out.
//! See docs/superpowers/specs/2026-09-22-gateway-installer-design.md.

use std::collections::BTreeMap;

use anyhow::{Context, anyhow, bail};

/// One architecture's binary.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Binary {
    pub url: String,
    pub sha256: String,
}

/// `release.json`.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Manifest {
    pub version: String,
    pub binaries: BTreeMap<String, Binary>,
}

/// The manifest, if `signature` (base64, as in `release.json.sig`) is `public_key`'s
/// signature over exactly these bytes. Nothing in the manifest is looked at before that.
pub fn verify_manifest(
    manifest: &[u8],
    signature: &str,
    public_key: &[u8],
) -> anyhow::Result<Manifest> {
    use base64::Engine as _;
    if public_key.is_empty() {
        bail!("this build carries no release key, so it cannot believe any release");
    }
    let signature = base64::engine::general_purpose::STANDARD
        .decode(signature.trim())
        .context("release signature is not base64")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(manifest, &signature)
        .map_err(|_| anyhow!("release manifest is not signed by the release key"))?;
    serde_json::from_slice(manifest).context("signed release manifest does not parse")
}

/// True when `candidate` is a strictly later `X.Y.Z` than `current`. An update never goes
/// sideways or back; a rollback is the updater's own act, never the channel's.
pub fn is_newer(candidate: &str, current: &str) -> anyhow::Result<bool> {
    Ok(parse_version(candidate)? > parse_version(current)?)
}

fn parse_version(version: &str) -> anyhow::Result<(u64, u64, u64)> {
    let parts: Vec<&str> = version.split('.').collect();
    let [major, minor, patch] = parts.as_slice() else {
        bail!("{version:?} is not a release version (X.Y.Z)");
    };
    let number = |part: &str| {
        part.parse::<u64>()
            .with_context(|| format!("{version:?} is not a release version (X.Y.Z)"))
    };
    Ok((number(major)?, number(minor)?, number(patch)?))
}

/// The binary for this architecture (`std::env::consts::ARCH`'s spelling).
pub fn binary_for<'a>(manifest: &'a Manifest, arch: &str) -> anyhow::Result<&'a Binary> {
    manifest
        .binaries
        .get(arch)
        .ok_or_else(|| anyhow!("release {} has no binary for {arch}", manifest.version))
}

/// Whether `bytes` hash to `expected`, a hex SHA-256.
pub fn check_sha256(bytes: &[u8], expected: &str) -> anyhow::Result<()> {
    let actual: String = ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected.trim().to_ascii_lowercase() {
        bail!("downloaded binary hashes to {actual}, the release says {expected}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn keypair() -> Ed25519KeyPair {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    const MANIFEST: &str = r#"{"version":"0.2.0","binaries":{
        "x86_64":{"url":"https://example.test/vms-gateway-x86_64-linux","sha256":"aa"},
        "aarch64":{"url":"https://example.test/vms-gateway-aarch64-linux","sha256":"bb"}}}"#;

    fn signed(key: &Ed25519KeyPair, manifest: &str) -> String {
        STANDARD.encode(key.sign(manifest.as_bytes()).as_ref())
    }

    #[test]
    fn a_manifest_signed_by_the_release_key_is_believed() {
        let key = keypair();
        let manifest = verify_manifest(
            MANIFEST.as_bytes(),
            &signed(&key, MANIFEST),
            key.public_key().as_ref(),
        )
        .expect("a good signature");
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.binaries.len(), 2);
    }

    #[test]
    fn a_manifest_changed_after_signing_is_refused() {
        let key = keypair();
        let signature = signed(&key, MANIFEST);
        let tampered = MANIFEST.replace("0.2.0", "9.9.9");
        assert!(
            verify_manifest(tampered.as_bytes(), &signature, key.public_key().as_ref()).is_err(),
            "a version bumped by whoever controls the download must not pass"
        );
    }

    #[test]
    fn a_signature_from_another_key_is_refused() {
        let ours = keypair();
        let theirs = keypair();
        assert!(
            verify_manifest(
                MANIFEST.as_bytes(),
                &signed(&theirs, MANIFEST),
                ours.public_key().as_ref()
            )
            .is_err()
        );
    }

    #[test]
    fn a_missing_or_garbled_signature_is_refused() {
        let key = keypair();
        for signature in ["", "not base64 at all!", &STANDARD.encode([0u8; 12])] {
            assert!(
                verify_manifest(MANIFEST.as_bytes(), signature, key.public_key().as_ref()).is_err(),
                "signature {signature:?} was accepted"
            );
        }
    }

    #[test]
    fn a_build_without_a_release_key_believes_nothing() {
        // What a gateway built before a release key exists carries: nothing to check with.
        let key = keypair();
        assert!(verify_manifest(MANIFEST.as_bytes(), &signed(&key, MANIFEST), &[]).is_err());
    }

    #[test]
    fn a_well_signed_manifest_that_is_not_a_manifest_is_refused() {
        let key = keypair();
        let junk = r#"{"version": 2}"#;
        assert!(
            verify_manifest(
                junk.as_bytes(),
                &signed(&key, junk),
                key.public_key().as_ref()
            )
            .is_err()
        );
    }

    #[test]
    fn only_a_strictly_later_version_is_newer() {
        assert!(is_newer("0.2.0", "0.1.0").unwrap());
        assert!(is_newer("0.10.0", "0.9.9").unwrap(), "numbers, not strings");
        assert!(is_newer("1.0.0", "0.99.99").unwrap());
        assert!(
            !is_newer("0.2.0", "0.2.0").unwrap(),
            "the same version is not an update"
        );
        assert!(!is_newer("0.1.9", "0.2.0").unwrap(), "never a downgrade");
        for bad in ["0.2", "v0.2.0", "0.2.0-rc.1", "0.2.x", ""] {
            assert!(
                is_newer(bad, "0.1.0").is_err(),
                "{bad:?} was read as a version"
            );
        }
    }

    #[test]
    fn each_architecture_gets_its_own_binary() {
        let manifest: Manifest = serde_json::from_str(MANIFEST).unwrap();
        assert!(
            binary_for(&manifest, "x86_64")
                .unwrap()
                .url
                .ends_with("x86_64-linux")
        );
        assert!(
            binary_for(&manifest, "aarch64")
                .unwrap()
                .url
                .ends_with("aarch64-linux")
        );
        assert!(
            binary_for(&manifest, "riscv64").is_err(),
            "a release without this architecture has nothing for it"
        );
    }

    #[test]
    fn a_binary_must_hash_to_what_the_manifest_says() {
        // SHA-256 of "abc".
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        check_sha256(b"abc", abc).expect("the right bytes");
        check_sha256(b"abc", &abc.to_uppercase()).expect("hex case does not matter");
        assert!(check_sha256(b"abd", abc).is_err(), "one byte off");
        assert!(check_sha256(b"abc", "").is_err(), "no hash is not a match");
    }
}
