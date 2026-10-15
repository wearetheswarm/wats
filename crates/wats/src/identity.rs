//! Identity (SPEC §2.2).
//!
//! A node holds one 32-byte root secret. Every key it uses is derived from
//! it with BLAKE3 `derive_key`, under a distinct context string:
//!
//! - the transport key (the Iroh endpoint id), so that the network address
//!   of a node is not linkable to the authors it hosts;
//! - one author key per profile (SPEC §2.2 "Profiles"), the default profile
//!   being `"default"`.
//!
//! See `docs/adr/0003-key-derivation.md`.

use std::path::Path;

use ed25519_dalek::SigningKey;

const TRANSPORT_CONTEXT: &str = "wearetheswarm 2026-10-15 transport key v1";
const PROFILE_CONTEXT: &str = "wearetheswarm 2026-10-15 profile key v1";

/// Name of the profile used when none is given.
pub const DEFAULT_PROFILE: &str = "default";

/// The root secret of a node.
pub struct RootSecret([u8; 32]);

impl RootSecret {
    pub fn generate() -> std::io::Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Load an existing secret; fails if the file is missing.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("secret file {} not found", path.display()),
            ));
        }
        Self::load_or_create(path)
    }

    /// Load the secret from `path`, creating it (mode 0600 on Unix) if missing.
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut bytes = [0u8; 32];
                hex::decode_to_slice(text.trim(), &mut bytes).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{}: not a 32-byte hex secret: {e}", path.display()),
                    )
                })?;
                Ok(Self(bytes))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let secret = Self::generate()?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                write_private(path, format!("{}\n", hex::encode(secret.0)).as_bytes())?;
                Ok(secret)
            }
            Err(e) => Err(e),
        }
    }

    /// Secret bytes of the transport (Iroh endpoint) key.
    pub fn transport_key_bytes(&self) -> [u8; 32] {
        blake3::derive_key(TRANSPORT_CONTEXT, &self.0)
    }

    /// Author key of a profile.
    pub fn profile_key(&self, profile: &str) -> SigningKey {
        let mut material = self.0.to_vec();
        material.extend_from_slice(profile.as_bytes());
        SigningKey::from_bytes(&blake3::derive_key(PROFILE_CONTEXT, &material))
    }
}

impl Drop for RootSecret {
    fn drop(&mut self) {
        self.0 = [0; 32];
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)
}

/// Lowercase hex public key of an author key, as used in `Event::author`.
pub fn author_id(key: &SigningKey) -> String {
    hex::encode(key.verifying_key().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_keys_are_distinct_and_stable() {
        let root = RootSecret::from_bytes([3; 32]);
        let a = root.profile_key(DEFAULT_PROFILE);
        let b = root.profile_key("worker-2");
        assert_ne!(a.to_bytes(), b.to_bytes());
        assert_ne!(a.to_bytes(), root.transport_key_bytes());
        assert_eq!(
            a.to_bytes(),
            RootSecret::from_bytes([3; 32])
                .profile_key(DEFAULT_PROFILE)
                .to_bytes()
        );
    }

    #[test]
    fn load_or_create_persists() {
        let dir = std::env::temp_dir().join(format!("wats-identity-{}", std::process::id()));
        let path = dir.join("secret.key");
        let _ = std::fs::remove_dir_all(&dir);
        let first = RootSecret::load_or_create(&path).unwrap();
        let second = RootSecret::load_or_create(&path).unwrap();
        assert_eq!(first.transport_key_bytes(), second.transport_key_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
