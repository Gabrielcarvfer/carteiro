//! Updates from GitHub releases. Every release carries `latest.json` (the
//! version and the SHA-256 of each package) and `latest.json.sig`, an Ed25519
//! signature of that file made in CI with the release key. A package is used
//! only if the manifest's signature verifies against the public key built
//! into the app, the version is newer than the running one (no downgrades),
//! and the download matches the manifest's hash. GitHub's TLS alone is not
//! trusted for this.

use std::collections::BTreeMap;

use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::http::{self, Request};

/// Installers with the bundled virus scanner run to a few hundred MB.
const MAX_PACKAGE: usize = 512 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Package kind ("windows-msi", "linux-appimage", "android-apk") -> file.
    pub files: BTreeMap<String, Asset>,
}

/// The package kind this build updates itself with.
pub fn platform_kind() -> Option<&'static str> {
    if cfg!(target_os = "android") {
        Some("android-apk")
    } else if cfg!(windows) {
        Some("windows-msi")
    } else if cfg!(target_os = "linux") {
        Some("linux-appimage")
    } else {
        None
    }
}

/// "1.2.3" (optionally "v1.2.3") -> (1, 2, 3). Pre-release suffixes aren't
/// offered as updates.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
    let r = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(r)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

/// Checks the signature over the exact manifest bytes, then parses them.
pub fn verify_manifest(json: &[u8], sig_b64: &str, public_key: &[u8]) -> Result<Manifest> {
    let sig = crate::b64::decode(sig_b64.trim()).ok_or_else(|| Error::Other("update signature isn't valid base64".into()))?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(json, &sig)
        .map_err(|_| Error::Other("the update's signature doesn't match Carteiro's release key; ignoring it".into()))?;
    let m: Manifest = serde_json::from_slice(json)?;
    if parse_version(&m.version).is_none() {
        return Err(Error::Other(format!("the update has an invalid version: {}", m.version)));
    }
    // File names go into URLs and onto disk.
    let safe = |n: &str| !n.is_empty() && n.len() < 200 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b)) && !n.starts_with('.');
    if let Some(bad) = m.files.values().find(|a| !safe(&a.name) || a.sha256.len() != 64) {
        return Err(Error::Other(format!("the update lists an invalid file: {}", bad.name)));
    }
    Ok(m)
}

pub fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Release key and repository ("owner/name"), baked in at build time.
#[derive(Debug, Clone)]
pub struct Channel {
    pub repo: String,
    pub public_key: Vec<u8>,
}

impl Channel {
    /// None when this build wasn't made with an update channel (local builds).
    pub fn from_build(repo: Option<&str>, public_key_b64: Option<&str>) -> Option<Channel> {
        let repo = repo?.trim();
        let key = crate::b64::decode(public_key_b64?.trim())?;
        let valid_repo = repo.split('/').count() == 2 && repo.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b)) && !repo.contains("..");
        (valid_repo && key.len() == 32).then(|| Channel { repo: repo.to_string(), public_key: key })
    }

    fn url(&self, path: &str) -> String {
        format!("https://github.com/{}/releases/{path}", self.repo)
    }

    /// The release page, for platforms that install updates by hand.
    pub fn release_page(&self, version: &str) -> String {
        self.url(&format!("tag/v{version}"))
    }

    /// The latest signed manifest, if it's newer than `current`.
    pub async fn check(&self, current: &str) -> Result<Option<Manifest>> {
        let get = |path: String| async move { http::send(Request::new("GET", &self.url(&path))).await?.ok() };
        let json = get("latest/download/latest.json".into()).await?.body;
        let sig = get("latest/download/latest.json.sig".into()).await?.text();
        let m = verify_manifest(&json, &sig, &self.public_key)?;
        Ok(is_newer(&m.version, current).then_some(m))
    }

    /// Downloads this platform's package and checks it against the manifest.
    pub async fn download(&self, m: &Manifest, kind: &str) -> Result<(Asset, Vec<u8>)> {
        let asset = m.files.get(kind).ok_or_else(|| Error::Other(format!("version {} has no {kind} package", m.version)))?;
        let url = self.url(&format!("download/v{}/{}", m.version, asset.name));
        let data = http::send(Request::new("GET", &url).max_body(MAX_PACKAGE)).await?.ok()?.body;
        if !sha256_hex(&data).eq_ignore_ascii_case(&asset.sha256) {
            return Err(Error::Other("the downloaded update doesn't match its signed checksum; discarded".into()));
        }
        Ok((asset.clone(), data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn keypair() -> Ed25519KeyPair {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    const MANIFEST: &str = r#"{"version":"0.2.0","files":{"windows-msi":{"name":"Carteiro_0.2.0_x64_en-US.msi","sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}}}"#;

    #[test]
    fn versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"), "no downgrades");
        assert!(!is_newer("0.2.0-rc1", "0.1.0"));
        assert!(!is_newer("0.2", "0.1.0"));
    }

    #[test]
    fn manifest_needs_a_valid_signature() {
        let key = keypair();
        let sig = crate::b64::encode(key.sign(MANIFEST.as_bytes()).as_ref());
        let m = verify_manifest(MANIFEST.as_bytes(), &sig, key.public_key().as_ref()).unwrap();
        assert_eq!(m.version, "0.2.0");

        let tampered = MANIFEST.replace("0.2.0", "9.9.9");
        assert!(verify_manifest(tampered.as_bytes(), &sig, key.public_key().as_ref()).is_err(), "edited manifest");
        let other = keypair();
        assert!(verify_manifest(MANIFEST.as_bytes(), &sig, other.public_key().as_ref()).is_err(), "someone else's key");
        assert!(verify_manifest(MANIFEST.as_bytes(), "not base64!", key.public_key().as_ref()).is_err());
    }

    /// Made by the release workflow's commands (openssl pkeyutl over the
    /// jq-written latest.json) with a throwaway key.
    #[test]
    fn verifies_what_ci_signs() {
        let json = br#"{"version":"0.2.0","files":{"windows-msi":{"name":"Carteiro_0.2.0_x64_en-US.msi","sha256":"8b25505cb8d6a75a790f7aee75930a62dc03dfabdab4d43acc40468485509f4b"},"linux-appimage":{"name":"Carteiro_0.2.0_amd64.AppImage","sha256":"8a8f60ecb09b7e64c6d5214a8043865e608507db8c3f61f995eae6d078875901"},"android-apk":{"name":"Carteiro_0.2.0_android.apk","sha256":"eb38e8b5a96b6e83fe9cb1a84113fee8bce4171088378628672105554ce86565"}}}
"#;
        let sig = "0FDdGjuZY2cnC+gxC4zgiMMWd6Vt6ytZmId/scTuQHmhlwSaKCHpmBtCxzjmqJhvr1YbteY0pfozldlROjVMAw==";
        let ch = Channel::from_build(Some("me/carteiro"), Some("SU4U4qj8nx1R/vN6Zks23hfl4n7uhBnP1Yj8Cky1Akk=")).unwrap();
        let m = verify_manifest(json, sig, &ch.public_key).unwrap();
        assert_eq!(m.files["linux-appimage"].name, "Carteiro_0.2.0_amd64.AppImage");
        assert_eq!(m.files.len(), 3);
    }

    #[test]
    fn manifest_file_names_are_checked() {
        let key = keypair();
        for bad in ["../../evil.msi", "a/b.msi", ".hidden", "x y.msi"] {
            let json = MANIFEST.replace("Carteiro_0.2.0_x64_en-US.msi", bad);
            let sig = crate::b64::encode(key.sign(json.as_bytes()).as_ref());
            assert!(verify_manifest(json.as_bytes(), &sig, key.public_key().as_ref()).is_err(), "{bad}");
        }
    }

    #[test]
    fn sha256_and_channel() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let key = crate::b64::encode([7u8; 32]);
        assert!(Channel::from_build(Some("me/carteiro"), Some(&key)).is_some());
        assert!(Channel::from_build(None, Some(&key)).is_none());
        assert!(Channel::from_build(Some("me/carteiro"), None).is_none());
        assert!(Channel::from_build(Some("../x"), Some(&key)).is_none());
        assert!(Channel::from_build(Some("me/carteiro"), Some(&crate::b64::encode([7u8; 16]))).is_none());
    }
}
