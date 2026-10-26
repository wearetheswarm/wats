//! A node's public identity (ADR 0055 §3–4): its node key, its name, and
//! the bindings that tie its agents to it.
//!
//! - **The node key** derives from the root secret under its own context
//!   ([`crate::identity::RootSecret::node_key`]). It is not the transport
//!   key, so a node's address stays unlinkable to its identity, and it
//!   never authors an event: it only signs bindings.
//! - **The name** is PascalCase, letters and digits only, always shown
//!   with `#` and the start of the node key: `AmberHive#4f2a`. Generated,
//!   it comes from the public node key alone ([`generated`]); chosen
//!   (`node.public_name`), it is checked ([`check_public_name`]) so that it
//!   holds neither the OS user, the host name, the home directory's name
//!   nor the account's full name.
//! - **A binding** is `announce data.node = {key, name, sig}`: the node's
//!   signature over the network, the agent's key, the node key and the
//!   name ([`binding_message`]). The announce itself is the agent's, so
//!   the proof goes both ways. Without a valid signature an agent has no
//!   node ([`verify`]).

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};

/// The `data` key of a binding.
pub const KEY: &str = "node";

/// The signed context of a binding.
pub const BINDING_CONTEXT: &str = "wats node binding v1";

/// Hex characters of the node key shown after `#`.
pub const SUFFIX: usize = 4;

/// Shortest and longest chosen name.
pub const MIN_NAME: usize = 3;
pub const MAX_NAME: usize = 24;

/// The first half of a generated name.
pub const ADJECTIVES: [&str; 48] = [
    "Amber", "Golden", "Busy", "Humming", "Waxen", "Honey", "Sunny", "Buzzing", "Royal", "Gentle",
    "Bright", "Merry", "Nimble", "Quiet", "Swift", "Brave", "Clever", "Lucky", "Mellow", "Rustic",
    "Velvet", "Copper", "Silver", "Misty", "Breezy", "Cozy", "Dappled", "Sturdy", "Lively",
    "Proud", "Sparkly", "Starry", "Mossy", "Clover", "Meadow", "Willow", "Thistle", "Bramble",
    "Linden", "Heather", "Saffron", "Cedar", "Maple", "Sage", "Tawny", "Dusky", "Sunlit", "Hazel",
];

/// The second half: words of a hive.
pub const HIVE_WORDS: [&str; 12] = [
    "Hive", "Comb", "Swarm", "Cell", "Colony", "Nest", "Brood", "Apiary", "Skep", "Cluster",
    "Hollow", "Burrow",
];

/// The generated name of the node whose public key is `key` (hex):
/// adjective and hive word, picked by BLAKE3 of the key alone.
pub fn generated(key: &str) -> String {
    let hash = blake3::derive_key("wats node name v1", key.as_bytes());
    let pick = |b: &[u8], len: usize| {
        usize::try_from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0) % len
    };
    format!(
        "{}{}",
        ADJECTIVES[pick(&hash[0..4], ADJECTIVES.len())],
        HIVE_WORDS[pick(&hash[4..8], HIVE_WORDS.len())]
    )
}

/// `name#4f2a`: the name and the start of the node key (`len` hex
/// characters, at least [`SUFFIX`]).
pub fn label(name: &str, key: &str, len: usize) -> String {
    let len = len.max(SUFFIX).min(key.len());
    format!("{name}#{}", &key[..len])
}

/// Whether `name` has the shape of a node name: PascalCase, letters and
/// digits, [`MIN_NAME`] to [`MAX_NAME`] characters, starting with a
/// capital and holding a lowercase letter.
pub fn well_formed(name: &str) -> bool {
    (MIN_NAME..=MAX_NAME).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric())
        && name.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && name.bytes().any(|b| b.is_ascii_lowercase())
}

/// The account's full name, when the OS gives it (macOS `RealName`, the
/// GECOS field elsewhere): the whole and each of its parts, lowercased
/// letters and digits, for [`check_public_name`].
fn full_name() -> Vec<String> {
    let Some(raw) = full_name_raw() else {
        return Vec::new();
    };
    let squeeze = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let mut words = vec![squeeze(&raw)];
    words.extend(raw.split_whitespace().map(squeeze));
    words.retain(|w| w.len() >= 3);
    words.dedup();
    words
}

fn full_name_raw() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    #[cfg(target_os = "macos")]
    let raw = std::process::Command::new("/usr/bin/id")
        .arg("-F")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())?;
    #[cfg(not(target_os = "macos"))]
    let raw = std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|text| {
            text.lines()
                .map(|l| l.split(':').collect::<Vec<_>>())
                .find(|f| f.first() == Some(&user.as_str()))
                .and_then(|f| {
                    f.get(4)
                        .map(|g| g.split(',').next().unwrap_or("").to_string())
                })
        })?;
    #[cfg(target_os = "macos")]
    let _ = &user;
    Some(raw.trim().to_string()).filter(|r| !r.is_empty())
}

/// Check a chosen node name (`node.public_name`): its shape, and that it
/// holds nothing of the machine or the person (ADR 0055 §3.2).
pub fn check_public_name(name: &str) -> Result<(), String> {
    if !well_formed(name) {
        return Err(format!(
            "node.public_name {name:?}: PascalCase, letters and digits only, {MIN_NAME}-{MAX_NAME} \
             characters (e.g. AmberHive); the #id is added"
        ));
    }
    let mut words = crate::roster::private_words();
    // The whole and its parts: a first name alone is a leak.
    words.extend(
        full_name()
            .into_iter()
            .map(|w| ("the account's full name", w)),
    );
    match crate::roster::leak(name, &words) {
        Some(word) => Err(format!(
            "node.public_name {name:?} contains {word}: others see it (ADR 0055), choose another"
        )),
        None => Ok(()),
    }
}

/// What a binding's signature covers: BLAKE3 of the JCS form of the
/// context, network, agent key, node key and name.
pub fn binding_message(net: &str, author: &str, node: &str, name: &str) -> [u8; 32] {
    let value = json!({
        "ctx": BINDING_CONTEXT,
        "net": net,
        "author": author,
        "node": node,
        "name": name,
    });
    let bytes = serde_json_canonicalizer::to_vec(&value).unwrap_or_default();
    *blake3::hash(&bytes).as_bytes()
}

/// The `data.node` value binding `author` to the node of `key` on `net`.
pub fn bind(key: &SigningKey, net: &str, author: &str, name: &str) -> Value {
    let node = hex::encode(key.verifying_key().as_bytes());
    let sig = key.sign(&binding_message(net, author, &node, name));
    json!({"key": node, "name": name, "sig": hex::encode(sig.to_bytes())})
}

/// A binding that holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The node key, hex.
    pub key: String,
    pub name: String,
}

/// The node `data.node` binds `author` to on `net`, when its signature
/// holds and its name is well formed. Anything else binds nothing.
pub fn verify(value: &Value, net: &str, author: &str) -> Option<Binding> {
    let key = value.get("key")?.as_str()?;
    let name = value.get("name")?.as_str()?;
    let sig = value.get("sig")?.as_str()?;
    if !well_formed(name) {
        return None;
    }
    let mut key_bytes = [0u8; 32];
    hex::decode_to_slice(key, &mut key_bytes).ok()?;
    let mut sig_bytes = [0u8; 64];
    hex::decode_to_slice(sig, &mut sig_bytes).ok()?;
    let verifier = ed25519_zebra::VerificationKey::try_from(key_bytes).ok()?;
    verifier
        .verify(
            &ed25519_zebra::Signature::from_bytes(&sig_bytes),
            &binding_message(net, author, key, name),
        )
        .ok()?;
    Some(Binding {
        key: key.to_ascii_lowercase(),
        name: name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0055 §3.2: the generated name is PascalCase, from the key
    /// alone, and stable.
    #[test]
    fn a_generated_name_is_stable_pascal_case() {
        let a = generated("ab12");
        assert_eq!(a, generated("ab12"));
        assert!(well_formed(&a), "{a}");
        assert!(HIVE_WORDS.iter().any(|w| a.ends_with(w)), "{a}");
        for word in ADJECTIVES.iter().chain(HIVE_WORDS.iter()) {
            assert!(well_formed(&format!("{word}Hive")), "{word}");
        }
        assert_eq!(label("AmberHive", "4f2a9c00", 4), "AmberHive#4f2a");
        assert_eq!(label("AmberHive", "4f2a9c00", 6), "AmberHive#4f2a9c");
    }

    #[test]
    fn names_have_the_node_shape() {
        assert!(well_formed("AmberHive"));
        assert!(well_formed("Hive42"));
        assert!(!well_formed("amber-hive"));
        assert!(!well_formed("AMBER"));
        assert!(!well_formed("Ab"));
        assert!(!well_formed("Amber_Hive"));
        assert!(check_public_name("giddy_ant").is_err());
    }

    /// ADR 0055 §4: a binding holds for its author and network only, and
    /// a forged or altered one binds nothing.
    #[test]
    fn a_binding_holds_only_where_it_was_signed() {
        let node = SigningKey::from_bytes(&[7; 32]);
        let other = SigningKey::from_bytes(&[8; 32]);
        let value = bind(&node, "public", "agent1", "AmberHive");
        let found = verify(&value, "public", "agent1").unwrap();
        assert_eq!(found.key, hex::encode(node.verifying_key().as_bytes()));
        assert_eq!(found.name, "AmberHive");
        assert_eq!(
            verify(&value, "public", "agent2"),
            None,
            "copied to another agent"
        );
        assert_eq!(
            verify(&value, "private", "agent1"),
            None,
            "copied to another network"
        );
        let mut renamed = value.clone();
        renamed["name"] = json!("GoldenHive");
        assert_eq!(verify(&renamed, "public", "agent1"), None);
        let mut claimed = value.clone();
        claimed["key"] = json!(hex::encode(other.verifying_key().as_bytes()));
        assert_eq!(
            verify(&claimed, "public", "agent1"),
            None,
            "a node claimed without proof"
        );
        assert_eq!(verify(&json!({"key": "x"}), "public", "agent1"), None);
    }
}
