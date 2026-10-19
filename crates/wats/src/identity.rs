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

/// Largest secret file (or standard input) [`read_secret`] reads.
const MAX_SECRET_BYTES: u64 = 64 * 1024;

/// A secret read by [`read_secret`]: its text, overwritten with zeros when
/// dropped (best effort: copies made while parsing are not tracked).
pub struct SecretText(Vec<u8>);

impl SecretText {
    /// The secret without surrounding whitespace (the trailing newline).
    pub fn as_str(&self) -> anyhow::Result<&str> {
        Ok(std::str::from_utf8(&self.0)
            .map_err(|_| anyhow::anyhow!("a secret is text (hex)"))?
            .trim())
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        self.0.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.0);
    }
}

/// Read a secret key from `path`, or from standard input when `path` is
/// `-`, so that a key kept encrypted never touches the disk in clear:
/// `age -d guardian.key.age | wats guardian halt --key - …`.
pub fn read_secret(path: &Path, what: &str) -> anyhow::Result<SecretText> {
    use anyhow::Context;
    use std::io::Read;
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        std::io::stdin()
            .take(MAX_SECRET_BYTES)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read the {what} from standard input"))?;
    } else {
        std::fs::File::open(path)
            .and_then(|f| f.take(MAX_SECRET_BYTES).read_to_end(&mut bytes))
            .with_context(|| format!("read the {what} {}", path.display()))?;
    }
    let secret = SecretText(bytes);
    anyhow::ensure!(!secret.as_str()?.is_empty(), "the {what} is empty");
    Ok(secret)
}

/// Lowercase hex public key of an author key, as used in `Event::author`.
pub fn author_id(key: &SigningKey) -> String {
    hex::encode(key.verifying_key().as_bytes())
}

/// The RFC 4648 base32 alphabet, lowercase.
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// RFC 4648 base32 of `bytes`, lowercase, without padding.
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8 | u32::from(b)) & 0xfff;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[(acc >> bits) as usize & 31] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[(acc << (5 - bits)) as usize & 31] as char);
    }
    out
}

/// The bytes of RFC 4648 base32 text without padding, either case;
/// `None` unless it is the canonical encoding of some bytes (a valid
/// length, and zero bits after the last byte).
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = match c.to_ascii_lowercase() {
            c @ b'a'..=b'z' => c - b'a',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        acc = (acc << 5 | u32::from(v)) & 0xfff;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    (bits < 5 && acc & ((1 << bits) - 1) == 0).then_some(out)
}

/// The base32 form of an author id (64 hex characters): 52 lowercase
/// characters, the form a `to:` tag carries (ADR 0022 §1). `None` when
/// `id` is not an author id.
pub fn author_id_base32(id: &str) -> Option<String> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(id, &mut bytes).ok()?;
    Some(base32_encode(&bytes))
}

/// The author id (lowercase hex) of its base32 form, either case; `None`
/// unless `text` is 52 base32 characters that encode 32 bytes.
pub fn author_id_from_base32(text: &str) -> Option<String> {
    if text.len() != 52 {
        return None;
    }
    let bytes = base32_decode(text)?;
    (bytes.len() == 32).then(|| hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc_4648() {
        // RFC 4648 §10, lowercase and without padding.
        for (plain, encoded) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(base32_encode(plain.as_bytes()), encoded);
            assert_eq!(base32_decode(encoded).unwrap(), plain.as_bytes());
            assert_eq!(
                base32_decode(&encoded.to_ascii_uppercase()).unwrap(),
                plain.as_bytes()
            );
        }
        // Padding, other alphabets, impossible lengths, trailing bits.
        for bad in ["my======", "m", "mzx", "mzxw6y", "mz", "01", "my!"] {
            assert!(base32_decode(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn author_ids_round_trip_through_base32() {
        for seed in [0u8, 7, 255] {
            let id = author_id(&SigningKey::from_bytes(&[seed; 32]));
            let b32 = author_id_base32(&id).unwrap();
            assert_eq!(b32.len(), 52);
            assert_eq!(b32, b32.to_ascii_lowercase());
            assert_eq!(author_id_from_base32(&b32).unwrap(), id);
            assert_eq!(
                author_id_from_base32(&b32.to_ascii_uppercase()).unwrap(),
                id
            );
        }
        let b32 = author_id_base32(&"00".repeat(32)).unwrap();
        assert_eq!(b32, "a".repeat(52));
        assert!(author_id_base32("abc").is_none());
        assert!(author_id_from_base32(&b32[..51]).is_none());
        // The 4 bits after the key are zero: one form per key.
        assert!(author_id_from_base32(&format!("{}b", &b32[..51])).is_none());
    }

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
