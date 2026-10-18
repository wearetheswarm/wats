//! Sealed inbox (ADR 0015 §5): submissions only the holder of an inbox key
//! can read, carried as v2 `submit` events on the public network.
//!
//! - **Key.** HPKE base mode, X-Wing (KEM 0x647a), HKDF-SHA256,
//!   ChaCha20-Poly1305. The public key is 1,216 bytes; the secret is the
//!   32-byte X-Wing seed.
//! - **Inbox id.** `inbox:` + the first 32 hex characters of
//!   `BLAKE3-derive_key("wats inbox id v2", public key)`.
//! - **Seal.** `info = "wats inbox v2" || inbox id`, and
//!   `aad = "wats/submit/v2" || JCS({v, author, ts, net, kind, ext})` of the
//!   outer event, so a header changed in transit makes opening fail.
//! - **Plaintext.** A JSON header line, `\n`, then raw bytes: one chunk of
//!   the deflate-compressed payload. Parts are grouped by their shared
//!   one-off author and the `report` id of the header.
//!
//! Crash reports (ADR 0013) travel this way when an inbox key is known: the
//! compiled-in [`MAINTAINERS_INBOX`], or the `report.inbox` setting.

// Submissions are network input: they must never panic the reader.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ed25519_dalek::SigningKey;
use hpke::{Deserializable, Kem as _, OpModeR, OpModeS, Serializable};
use serde_json::{Map, Value, json};

use crate::event::MAX_EVENT_BYTES;
use crate::v2::{self, b64url_decode, b64url_encode};

type Kem = hpke::kem::XWing;
type Kdf = hpke::kdf::HkdfSha256;
type Aead = hpke::aead::ChaCha20Poly1305;
type PrivateKey = <Kem as hpke::Kem>::PrivateKey;
type PublicKey = <Kem as hpke::Kem>::PublicKey;
type EncappedKey = <Kem as hpke::Kem>::EncappedKey;

/// The maintainers' inbox public key (base64url), compiled in like the
/// bootstrap list, generated with `wats inbox keygen` (docs/release-signing.md,
/// "Inbox key"; the secret is kept in sops). Its id is
/// [`MAINTAINERS_INBOX_ID`]. `report.inbox` overrides it; with `None`,
/// crash reports would go to `report.collector` instead.
pub const MAINTAINERS_INBOX: Option<&str> = Some(
    "zbjHcnmWczc99LWhiOwnrkdKVhxue6MHa2YO87ak11oVCJpTPLUkm1Yx-5FhhXd6i8QwNqZlOEmRCDFQvFobFFFiD5QHcrhpzwoW7BKKShcDdjFtvmqshwa3UHB6U7LEKYLEVYeZmASBO1qIybGNzkpy-0lEO4mxvoh27Kl_zcGEaXp_Zkw1TBghXRKKR2Bv2gG7uxKclcgK4mOgQlEC_1Y9ytjDxHG2qafFlqyGe9ZnqBqAtda8Y4c3HhcCYiAoi5y7cEXKdtevLPEuiNBa64CsvIwlybt4W9QOs7pbl2zLxiYw0BAC6bkc-lGnTaJTtbJ0cfFzYsp8Jmp0oZIzcNMZR8WwClF14OdSjUOiUnhbrBu7zwi3_BTGECtUl3tw7ZRXd5VNtSJrgkwaySGoE3jKaCAm6aQGpwVgl_iXmjcFZqeIHIWpFtRrFiHHgtiWP3yKXDhAb1yOmJiYZ9s8lnCoupsV80iLZvBIWUMh0NZYX1SHpTuaVzYNYFQw4_LOBlIHN0LMPMlp8-ZOYoBm5mFITzlckICD_9WRnoIr3MJD9Vu4JEJWjgNVktOKBkKLxAkU51RLxyA5sRgVQyEPZGCRVCG5cZEEzjup-UTI9GZkhshQG1ZKM6w7jKls0_xsfMhVh9F1jHuNgMosNKuFcPRILLRZGME4C-yo5YmkOKaODZFi_tugEQeeKHI9SjE9k4JfVGQuVbJTKvYA-fly99y5ZTc9wilHVtqKASMwFBRKvDeaQmZ_oWE7N3vHnKZp78utU0C3ALqrFTNVvkmpdDgpn2pLuUwANTx6VUIYh-p3eBVprVYc87ZKMZWulkwz9UwSiUdgDBydVzm9xyLGIpOb4nmyf0Mqp4Rj4MkGIGjCcpIPbBCFECNYhzt-wLQ0fleqSWGsp6IJB4FLWnLE44cP8HuVShGa5Yg0eaxX5fIwDIcpKjpIJkaq5mU9D4kGyUlsewF6_DbDTUlnChpMRHxtaHVsSEt56Gl9Y8onXjqq6OQqb0qoAPZXXlQm01FaNmQXb6UQUKaBqwjLHPWpd2kUgQImpYW6_csHNOIbw_RIE0oiUXNCQPXE98CwuNBMfFFA2fBygwm84Fw0GnNRrFVpr1wxXwMXb1kMowJA4Lt-SwOofQMv_OG0C5R7gYBzwaCB9cJV4yMroUJhqjdJBHTBR0tcOeE3zHV6wdscr8y5hBA6kSanpRNe1rqw90y1G4aRmxFT8wZqS6VqqyMFiDeWV-vDZItzSGXIxvUjo6VeiEU7SapPkyelEygm9SQblmFio4hrwdVrI7ukJeZKJdxWElVNu7NIh1Gx4ltUQFsPJ6kn1lUk40RX2UysX3JOdIBZfWFDbWEeOYgUX0mbeGkb5ZwOfFR3rdZapjYn81BKICmgF8A1-pKYZ8K3XHdqywFbX1WrHvjKWsuQJTSLmhZ2rXVEA7OpwwJnm2mF9hqQBnI-RpEDnpRE5rWWniEFyAUXXkpRp5BBElt6q8SmaWDEN8GvdyE3sOtWPHu9cfwe99qcsiRwVHNhwdm1adTFhDmtV8kbfGpsLK0AsCCV1e0i8nizT74Apchr4fuEF3sNr1q9AMi1HXdE5rPUVPIf-tyaWAmPsCl1UyZzqTWzEw",
);
/// The inbox id of [`MAINTAINERS_INBOX`], as published with it.
pub const MAINTAINERS_INBOX_ID: &str = "inbox:6579c82d9bda63d4eea8adeeff05e383";

/// Size of an X-Wing public key.
pub const PUBLIC_KEY_BYTES: usize = 1216;
/// Size of an X-Wing secret (its seed).
pub const SECRET_BYTES: usize = 32;
/// Context of the inbox id derivation.
const ID_CONTEXT: &str = "wats inbox id v2";
/// HPKE `info` prefix, followed by the inbox id.
const INFO_PREFIX: &[u8] = b"wats inbox v2";
/// AAD prefix, followed by the JCS of the outer header.
const AAD_PREFIX: &[u8] = b"wats/submit/v2";
/// The outer header keys bound by the AAD.
const AAD_KEYS: [&str; 6] = ["v", "author", "ts", "net", "kind", "ext"];
/// Payload types of a submission (ADR 0015 §5).
pub const TYPES: [&str; 4] = ["crash", "security", "join", "request"];
/// Largest payload once decompressed: a bound against zip bombs.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// Most parts a payload may be split into.
pub const MAX_PARTS: u32 = 128;
/// Room left under the 16 KiB canonical cap when sizing chunks.
const SIZE_MARGIN: usize = 256;
/// A `pow` value at least as long as any `b3:<u64>` nonce.
const POW_PLACEHOLDER: &str = "b3:99999999999999999999";

/// The public half of an inbox: what reporters seal to.
#[derive(Clone)]
pub struct InboxPublic {
    key: PublicKey,
    bytes: Vec<u8>,
    id: String,
}

impl std::fmt::Debug for InboxPublic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxPublic").field("id", &self.id).finish()
    }
}

impl InboxPublic {
    fn from_key(key: PublicKey) -> Self {
        let bytes = key.to_bytes().to_vec();
        let id = inbox_id(&bytes);
        Self { key, bytes, id }
    }

    /// Parse a base64url public key (1,216 bytes).
    pub fn from_b64(s: &str) -> Result<Self> {
        let bytes = b64url_decode(s.trim()).context("an inbox key is canonical base64url")?;
        ensure!(
            bytes.len() == PUBLIC_KEY_BYTES,
            "an inbox key is {PUBLIC_KEY_BYTES} bytes, not {}",
            bytes.len()
        );
        let key = PublicKey::from_bytes(&bytes).map_err(|e| anyhow!("not an X-Wing key: {e}"))?;
        Ok(Self::from_key(key))
    }

    /// The public key as base64url.
    pub fn to_b64(&self) -> String {
        b64url_encode(&self.bytes)
    }

    /// `inbox:<32 hex>`.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// An inbox key pair: who holds it reads the submissions.
pub struct InboxKey {
    secret: PrivateKey,
    public: InboxPublic,
}

impl InboxKey {
    /// A fresh key pair.
    pub fn generate() -> Result<Self> {
        let mut seed = [0u8; SECRET_BYTES];
        getrandom::fill(&mut seed).map_err(|e| anyhow!("no randomness: {e}"))?;
        Self::from_seed(&seed)
    }

    fn from_seed(seed: &[u8]) -> Result<Self> {
        let secret = PrivateKey::from_bytes(seed).map_err(|e| anyhow!("bad inbox secret: {e}"))?;
        let public = InboxPublic::from_key(Kem::sk_to_pk(&secret));
        Ok(Self { secret, public })
    }

    /// Parse the secret as written by [`InboxKey::secret_hex`]: the 32-byte
    /// seed in hex, surrounding whitespace ignored.
    pub fn from_secret_str(s: &str) -> Result<Self> {
        let mut seed = hex::decode(s.trim()).context("an inbox secret is hex")?;
        let key = if seed.len() == SECRET_BYTES {
            Self::from_seed(&seed)
        } else {
            Err(anyhow::anyhow!(
                "an inbox secret is {SECRET_BYTES} bytes, not {}",
                seed.len()
            ))
        };
        seed.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&seed);
        key
    }

    /// Read a secret file, or standard input when `path` is `-` (e.g.
    /// `sops decrypt inbox.key.enc | wats inbox read --key -`).
    pub fn load(path: &Path) -> Result<Self> {
        let secret = crate::identity::read_secret(path, "inbox secret")?;
        Self::from_secret_str(secret.as_str()?)
    }

    /// The secret (the X-Wing seed) in hex.
    pub fn secret_hex(&self) -> String {
        hex::encode(self.secret.to_bytes())
    }

    pub fn public(&self) -> &InboxPublic {
        &self.public
    }
}

/// `inbox:` + 32 hex of `BLAKE3-derive_key("wats inbox id v2", key)`.
pub fn inbox_id(public_key: &[u8]) -> String {
    let digest = blake3::derive_key(ID_CONTEXT, public_key);
    let mut id = String::from(v2::INBOX_PREFIX);
    id.extend(hex::encode(digest).chars().take(32));
    id
}

fn info(inbox_id: &str) -> Vec<u8> {
    [INFO_PREFIX, inbox_id.as_bytes()].concat()
}

/// `"wats/submit/v2" || JCS({v, author, ts, net, kind, ext})` of the outer
/// event (`wire` holds at least those keys).
pub fn aad(wire: &Map<String, Value>) -> Result<Vec<u8>> {
    let mut header = Map::new();
    for key in AAD_KEYS {
        let value = wire
            .get(key)
            .with_context(|| format!("submission header has no {key:?}"))?;
        header.insert(key.into(), value.clone());
    }
    let jcs = serde_json_canonicalizer::to_vec(&Value::Object(header))?;
    Ok([AAD_PREFIX, &jcs].concat())
}

/// Seal `plaintext` to `inbox` for the outer header `wire`. Returns `enc`
/// and `ct`, base64url.
pub fn seal(
    inbox: &InboxPublic,
    wire: &Map<String, Value>,
    plaintext: &[u8],
) -> Result<(String, String)> {
    let (enc, ct) = hpke::single_shot_seal::<Aead, Kdf, Kem>(
        &OpModeS::Base,
        &inbox.key,
        &info(&inbox.id),
        plaintext,
        &aad(wire)?,
    )
    .map_err(|e| anyhow!("seal: {e}"))?;
    Ok((b64url_encode(&enc.to_bytes()), b64url_encode(&ct)))
}

/// Open a submission (its wire JSON) with `key`. Fails on another inbox,
/// a wrong key, or any change to the bound header or body.
pub fn open(key: &InboxKey, event: &Value) -> Result<Vec<u8>> {
    let wire = event.as_object().context("a submission is an object")?;
    ensure!(
        wire.get("ext")
            .and_then(|e| e.get("to"))
            .and_then(Value::as_str)
            == Some(key.public.id()),
        "not addressed to {}",
        key.public.id()
    );
    let field = |name: &str| -> Result<Vec<u8>> {
        wire.get(name)
            .and_then(Value::as_str)
            .and_then(b64url_decode)
            .with_context(|| format!("submission has no base64url {name:?}"))
    };
    let enc = EncappedKey::from_bytes(&field("enc")?).map_err(|e| anyhow!("bad enc: {e}"))?;
    hpke::single_shot_open::<Aead, Kdf, Kem>(
        &OpModeR::Base,
        &key.secret,
        &enc,
        &info(&key.public.id),
        &field("ct")?,
        &aad(wire)?,
    )
    .map_err(|e| anyhow!("open: {e}"))
}

/// The header line of a part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartHeader {
    pub kind: String,
    pub report: String,
    pub part: u32,
    pub parts: u32,
}

impl PartHeader {
    fn line(&self) -> Vec<u8> {
        json!({"type": self.kind, "report": self.report, "part": self.part,
               "parts": self.parts, "z": "deflate"})
        .to_string()
        .into_bytes()
    }
}

/// `header line || \n || chunk`.
pub fn frame(header: &PartHeader, chunk: &[u8]) -> Vec<u8> {
    let mut out = header.line();
    out.push(b'\n');
    out.extend_from_slice(chunk);
    out
}

/// Split a plaintext into its header and chunk, checking the header.
pub fn unframe(plain: &[u8]) -> Result<(PartHeader, &[u8])> {
    let newline = plain
        .iter()
        .position(|&b| b == b'\n')
        .context("part has no header line")?;
    let (line, rest) = plain.split_at(newline);
    let chunk = rest.get(1..).unwrap_or_default();
    let header: Map<String, Value> =
        serde_json::from_slice(line).context("part header is not a JSON object")?;
    let text = |key: &str| header.get(key).and_then(Value::as_str).unwrap_or_default();
    let number = |key: &str| {
        header
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    let parsed = PartHeader {
        kind: text("type").to_string(),
        report: text("report").to_string(),
        part: number("part"),
        parts: number("parts"),
    };
    ensure!(
        TYPES.contains(&parsed.kind.as_str()),
        "unknown part type {:?}",
        parsed.kind
    );
    ensure!(
        text("z") == "deflate",
        "unknown compression {:?}",
        text("z")
    );
    ensure!(
        parsed.report.len() == 16
            && parsed
                .report
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "report id is not 16 lowercase hex"
    );
    ensure!(
        (1..=MAX_PARTS).contains(&parsed.parts) && (1..=parsed.parts).contains(&parsed.part),
        "part {} of {} is out of range",
        parsed.part,
        parsed.parts
    );
    Ok((parsed, chunk))
}

/// Raw deflate, best compression.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Inflate, refusing more than [`MAX_PAYLOAD_BYTES`].
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let limit = u64::try_from(MAX_PAYLOAD_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data)
        .take(limit)
        .read_to_end(&mut out)
        .context("payload does not inflate")?;
    ensure!(
        out.len() <= MAX_PAYLOAD_BYTES,
        "payload inflates above {MAX_PAYLOAD_BYTES} bytes"
    );
    Ok(out)
}

/// The unsigned outer header of a submission to `inbox`.
fn outer_header(inbox: &InboxPublic, author: &SigningKey, ts: u64) -> Map<String, Value> {
    let Value::Object(header) = json!({
        "v": v2::VERSION, "ts": ts, "net": crate::node::PUBLIC_NET, "thread": null,
        "parent": null, "refs": [], "kind": v2::KIND_SUBMIT, "ext": {"to": inbox.id},
        "author": hex::encode(author.verifying_key().as_bytes()),
    }) else {
        return Map::new();
    };
    header
}

/// Canonical size of `wire` once `pow`, `id` and `sig` are filled in, at
/// their longest.
fn final_size(wire: &Map<String, Value>) -> Result<usize> {
    let mut full = wire.clone();
    full.insert("pow".into(), POW_PLACEHOLDER.into());
    full.insert("id".into(), "0".repeat(64).into());
    full.insert("sig".into(), "0".repeat(v2::SUBMIT_SIG_HEX).into());
    Ok(serde_json_canonicalizer::to_vec(&Value::Object(full))?.len())
}

/// The largest chunk whose part stays within [`MAX_EVENT_BYTES`] (less a
/// margin), computed from the encodings: everything but `ct` is fixed, and
/// `ct` is the base64url of header line + `\n` + chunk + the 16-byte tag.
pub fn max_chunk(inbox: &InboxPublic) -> Result<usize> {
    let author = SigningKey::from_bytes(&[1; 32]);
    let mut wire = outer_header(inbox, &author, u64::from(u32::MAX));
    wire.insert("enc".into(), b64url_encode(&[0; v2::ENC_BYTES]).into());
    wire.insert("ct".into(), "".into());
    let fixed = final_size(&wire)?;
    let header = PartHeader {
        kind: "security".into(),
        report: "0".repeat(16),
        part: MAX_PARTS,
        parts: MAX_PARTS,
    };
    let room = MAX_EVENT_BYTES
        .checked_sub(fixed)
        .and_then(|r| r.checked_sub(SIZE_MARGIN))
        .context("no room for a chunk")?;
    // base64url without padding: n bytes take ceil(4n / 3) characters.
    let ct_bytes = room.saturating_mul(3) / 4;
    ct_bytes
        .min(v2::MAX_CT_BYTES)
        .checked_sub(16)
        .and_then(|r| r.checked_sub(header.line().len()))
        .and_then(|r| r.checked_sub(1))
        .context("no room for a chunk")
}

/// A payload sealed into submissions, ready to publish.
#[derive(Debug)]
pub struct Sealed {
    pub report: String,
    pub events: Vec<Value>,
    /// Proof of work mined into each event.
    pub bits: Vec<u32>,
}

/// Seal `payload` (of type `kind`) to `inbox`: compress it, split it into
/// balanced chunks, and build one signed, mined `submit` event per chunk,
/// all under one fresh one-off author. `pow_bits(size)` gives the bits to
/// mine for an event of `size` canonical bytes. CPU-bound: call it from a
/// blocking thread.
pub fn seal_payload(
    inbox: &InboxPublic,
    kind: &str,
    payload: &[u8],
    ts: u64,
    pow_bits: impl Fn(usize) -> u32,
) -> Result<Sealed> {
    ensure!(TYPES.contains(&kind), "unknown submission type {kind:?}");
    ensure!(
        payload.len() <= MAX_PAYLOAD_BYTES,
        "payload is {} bytes, above {MAX_PAYLOAD_BYTES}",
        payload.len()
    );
    let compressed = compress(payload)?;
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow!("no randomness: {e}"))?;
    let author = SigningKey::from_bytes(&seed);
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).map_err(|e| anyhow!("no randomness: {e}"))?;
    let report = hex::encode(id);

    let mut chunk = max_chunk(inbox)?;
    let parts = loop {
        // Balanced chunks: as many parts as `chunk` needs, evenly sized.
        let count = compressed.len().div_ceil(chunk).max(1);
        let size = compressed.len().div_ceil(count).max(1);
        let n = u32::try_from(count).unwrap_or(u32::MAX);
        ensure!(
            n <= MAX_PARTS,
            "payload needs {count} parts, above {MAX_PARTS}"
        );
        let mut parts = Vec::with_capacity(count);
        let mut fits = true;
        let chunks: Vec<&[u8]> = if compressed.is_empty() {
            vec![&[]]
        } else {
            compressed.chunks(size).collect()
        };
        for (i, piece) in (1u32..).zip(chunks) {
            let header = PartHeader {
                kind: kind.into(),
                report: report.clone(),
                part: i,
                parts: n,
            };
            let mut wire = outer_header(inbox, &author, ts);
            let (enc, ct) = seal(inbox, &wire, &frame(&header, piece))?;
            wire.insert("enc".into(), enc.into());
            wire.insert("ct".into(), ct.into());
            let size = final_size(&wire)?;
            if size > MAX_EVENT_BYTES {
                fits = false;
                break;
            }
            parts.push((wire, size));
        }
        if fits {
            break parts;
        }
        // The estimate was off: shrink and try again.
        chunk = chunk
            .checked_sub(512)
            .filter(|c| *c > 0)
            .context("cannot size the chunks")?;
    };

    let mut events = Vec::with_capacity(parts.len());
    let mut bits = Vec::with_capacity(parts.len());
    for (mut wire, size) in parts {
        let required = pow_bits(size);
        let id = crate::wire::mine(&mut wire, required)?;
        wire.insert("id".into(), hex::encode(id).into());
        wire.insert("sig".into(), v2::v2_sig(&author, None, &id).into());
        events.push(Value::Object(wire));
        bits.push(required);
    }
    Ok(Sealed {
        report,
        events,
        bits,
    })
}

/// What [`read_submissions`] found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReadSummary {
    /// Payloads complete and written (or already there), by path.
    pub written: Vec<PathBuf>,
    /// Complete payloads whose file already existed.
    pub existing: usize,
    /// Payloads with parts still missing.
    pub incomplete: usize,
    /// Submissions that did not open (other key, tampered, malformed).
    pub undecryptable: usize,
    /// Payloads rejected as a whole (inconsistent or duplicate parts, bad
    /// compression, too large).
    pub rejected: usize,
    /// Complete payloads of a type not handled yet.
    pub skipped: usize,
}

/// The parts of one payload: same one-off author, same report id.
#[derive(Default)]
struct Group {
    kind: String,
    parts: u32,
    chunks: BTreeMap<u32, Vec<u8>>,
    bad: bool,
}

/// Open `events` (wire JSON of v2 `submit` events) with `key`, group the
/// parts by (author, report id), and write each complete crash report to
/// `<out>/<report id>.json`, leaving existing files alone.
pub fn read_submissions(key: &InboxKey, events: &[Value], out: &Path) -> Result<ReadSummary> {
    let mut summary = ReadSummary::default();
    let mut groups: HashMap<(String, String), Group> = HashMap::new();
    for event in events {
        let opened = (|| -> Result<(String, PartHeader, Vec<u8>)> {
            crate::wire::AnyEvent::from_value(event.clone())?;
            let author = event
                .get("author")
                .and_then(Value::as_str)
                .context("no author")?
                .to_string();
            let plain = open(key, event)?;
            let (header, chunk) = unframe(&plain)?;
            Ok((author, header, chunk.to_vec()))
        })();
        let Ok((author, header, chunk)) = opened else {
            summary.undecryptable = summary.undecryptable.saturating_add(1);
            continue;
        };
        let group = groups
            .entry((author, header.report.clone()))
            .or_insert_with(|| Group {
                kind: header.kind.clone(),
                parts: header.parts,
                ..Group::default()
            });
        if group.kind != header.kind
            || group.parts != header.parts
            || group.chunks.insert(header.part, chunk).is_some()
        {
            group.bad = true;
        }
    }
    let mut complete = Vec::new();
    for ((_, report), group) in groups {
        if group.bad {
            summary.rejected = summary.rejected.saturating_add(1);
        } else if group.chunks.len() < usize::try_from(group.parts).unwrap_or(usize::MAX) {
            summary.incomplete = summary.incomplete.saturating_add(1);
        } else if group.kind != "crash" {
            summary.skipped = summary.skipped.saturating_add(1);
        } else {
            complete.push((report, group));
        }
    }
    complete.sort_by(|a, b| a.0.cmp(&b.0));
    if !complete.is_empty() {
        std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    }
    for (report, group) in complete {
        let path = out.join(format!("{report}.json"));
        if path.exists() {
            summary.existing = summary.existing.saturating_add(1);
            continue;
        }
        let compressed: Vec<u8> = group.chunks.into_values().flatten().collect();
        let Ok(payload) = decompress(&compressed) else {
            summary.rejected = summary.rejected.saturating_add(1);
            continue;
        };
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                summary.existing = summary.existing.saturating_add(1);
                continue;
            }
            Err(e) => bail!("write {}: {e}", path.display()),
        };
        file.write_all(&payload)?;
        summary.written.push(path);
    }
    Ok(summary)
}

/// Write a new secret to `path` (mode 0600, never over an existing file)
/// and return the key.
pub fn keygen(path: &Path) -> Result<InboxKey> {
    let key = InboxKey::generate()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).with_context(|| {
        format!(
            "create {} (an existing file is never replaced)",
            path.display()
        )
    })?;
    writeln!(file, "{}", key.secret_hex())?;
    Ok(key)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use crate::wire::AnyEvent;

    fn sizes_ok(sealed: &Sealed) {
        for event in &sealed.events {
            let size = serde_json_canonicalizer::to_vec(event).unwrap().len();
            assert!(size <= MAX_EVENT_BYTES, "{size}");
            AnyEvent::from_value(event.clone()).unwrap();
            assert_eq!(event["refs"], json!([]));
            assert_eq!(event["kind"], "submit");
        }
    }

    /// Text that compresses about as well as a crash report's.
    fn report(len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut n = 0u64;
        while out.len() < len {
            let hash = blake3::hash(&n.to_le_bytes());
            out.extend_from_slice(
                format!(
                    "{{\"line\": {n}, \"msg\": \"node {}\"}}\n",
                    &hash.to_hex()[..24]
                )
                .as_bytes(),
            );
            n += 1;
        }
        out.truncate(len);
        out
    }

    #[test]
    fn keys_round_trip() {
        let key = InboxKey::generate().unwrap();
        let again = InboxKey::from_secret_str(&format!("{}\n", key.secret_hex())).unwrap();
        assert_eq!(again.public().to_b64(), key.public().to_b64());
        let public = InboxPublic::from_b64(&key.public().to_b64()).unwrap();
        assert_eq!(public.id(), key.public().id());
        assert_eq!(public.bytes.len(), PUBLIC_KEY_BYTES);
        assert!(v2::is_inbox_id(public.id()), "{}", public.id());
        assert!(InboxPublic::from_b64("AAAA").is_err());
        assert!(InboxKey::from_secret_str("abcd").is_err());
        // The compiled-in key, once set, must parse.
        if let Some(maintainers) = MAINTAINERS_INBOX {
            let public = InboxPublic::from_b64(maintainers).unwrap();
            assert_eq!(public.id(), MAINTAINERS_INBOX_ID, "the published inbox id");
        }
    }

    #[test]
    fn seal_open_wrong_key_and_tampering() {
        let key = InboxKey::generate().unwrap();
        let sealed = seal_payload(
            key.public(),
            "crash",
            b"{\"kind\":\"panic\"}",
            1_760_800_000,
            |_| 0,
        )
        .unwrap();
        assert_eq!(sealed.events.len(), 1);
        sizes_ok(&sealed);
        let event = &sealed.events[0];
        assert_eq!(
            v2::b64url_decode(event["enc"].as_str().unwrap())
                .unwrap()
                .len(),
            v2::ENC_BYTES
        );
        let plain = open(&key, event).unwrap();
        let (header, chunk) = unframe(&plain).unwrap();
        assert_eq!((header.part, header.parts), (1, 1));
        assert_eq!(header.report, sealed.report);
        assert_eq!(decompress(chunk).unwrap(), b"{\"kind\":\"panic\"}");

        // Another key with the same inbox id cannot open it.
        let other = InboxKey::generate().unwrap();
        let mut forged = event.clone();
        forged["ext"]["to"] = other.public().id().into();
        assert!(open(&other, &forged).is_err());
        assert!(open(&other, event).is_err());

        // Any bound header key changed: opening fails.
        for (key_name, value) in [
            ("author", json!("11".repeat(32))),
            ("ts", json!(1_760_800_001u64)),
            ("net", json!("other")),
            ("kind", json!("sealed")),
            ("v", json!(3)),
            ("ext", json!({"to": key.public().id(), "_grease": 1})),
        ] {
            let mut tampered = event.clone();
            tampered[key_name] = value;
            assert!(open(&key, &tampered).is_err(), "{key_name}");
        }
        let mut tampered = event.clone();
        let ct = event["ct"].as_str().unwrap();
        let flipped = if ct.starts_with('A') { "B" } else { "A" };
        tampered["ct"] = format!("{flipped}{}", &ct[1..]).into();
        assert!(open(&key, &tampered).is_err());
    }

    #[test]
    fn chunking_and_reassembly() {
        let key = InboxKey::generate().unwrap();
        let payload = report(64 * 1024);
        let sealed = seal_payload(key.public(), "crash", &payload, 1_760_800_000, |_| 1).unwrap();
        assert!(sealed.events.len() >= 2, "{}", sealed.events.len());
        sizes_ok(&sealed);
        // Every part shares the one-off author.
        let author = &sealed.events[0]["author"];
        assert!(sealed.events.iter().all(|e| &e["author"] == author));

        let dir = tempfile::tempdir().unwrap();
        // Shuffled order, plus noise from another inbox.
        let mut events = sealed.events.clone();
        events.reverse();
        let other = InboxKey::generate().unwrap();
        events.extend(
            seal_payload(other.public(), "crash", b"x", 1_760_800_000, |_| 0)
                .unwrap()
                .events,
        );
        let summary = read_submissions(&key, &events, dir.path()).unwrap();
        assert_eq!(summary.written.len(), 1, "{summary:?}");
        assert_eq!(summary.undecryptable, 1);
        let path = dir.path().join(format!("{}.json", sealed.report));
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        // Idempotent.
        let again = read_submissions(&key, &events, dir.path()).unwrap();
        assert_eq!((again.written.len(), again.existing), (0, 1));

        // A missing part: incomplete, nothing written.
        let dir = tempfile::tempdir().unwrap();
        let summary = read_submissions(&key, &sealed.events[1..], dir.path()).unwrap();
        assert_eq!((summary.incomplete, summary.written.len()), (1, 0));

        // Parts of two one-off authors never merge, even with the same
        // report id.
        let second = seal_payload(key.public(), "crash", &payload, 1_760_800_000, |_| 1).unwrap();
        let mut mixed = vec![sealed.events[0].clone()];
        mixed.extend(second.events[1..].iter().cloned());
        let summary = read_submissions(&key, &mixed, dir.path()).unwrap();
        assert_eq!(summary.incomplete, 2, "{summary:?}");
    }

    /// A part built by hand, for the malformed cases.
    fn part(key: &InboxKey, author: &SigningKey, header: &PartHeader, chunk: &[u8]) -> Value {
        let mut wire = outer_header(key.public(), author, 1_760_800_000);
        let (enc, ct) = seal(key.public(), &wire, &frame(header, chunk)).unwrap();
        wire.insert("enc".into(), enc.into());
        wire.insert("ct".into(), ct.into());
        let id = crate::wire::mine(&mut wire, 0).unwrap();
        wire.insert("id".into(), hex::encode(id).into());
        wire.insert("sig".into(), v2::v2_sig(author, None, &id).into());
        Value::Object(wire)
    }

    #[test]
    fn duplicate_out_of_range_and_bombs() {
        let key = InboxKey::generate().unwrap();
        let author = SigningKey::from_bytes(&[5; 32]);
        let header = |part, parts| PartHeader {
            kind: "crash".into(),
            report: "0123456789abcdef".into(),
            part,
            parts,
        };
        let z = compress(b"hello").unwrap();
        let dir = tempfile::tempdir().unwrap();

        // The same part number twice: the payload is rejected.
        let events = [
            part(&key, &author, &header(1, 2), &z),
            part(&key, &author, &header(1, 2), b"other"),
            part(&key, &author, &header(2, 2), b""),
        ];
        let summary = read_submissions(&key, &events, dir.path()).unwrap();
        assert_eq!((summary.rejected, summary.written.len()), (1, 0));

        // Out of range part numbers do not open as parts.
        for (p, n) in [(0, 1), (3, 2), (1, MAX_PARTS + 1)] {
            let summary =
                read_submissions(&key, &[part(&key, &author, &header(p, n), &z)], dir.path())
                    .unwrap();
            assert_eq!(summary.undecryptable, 1, "{p}/{n}");
        }

        // A zip bomb: tiny compressed, too large inflated.
        let bomb = compress(&vec![0u8; MAX_PAYLOAD_BYTES + 1]).unwrap();
        assert!(bomb.len() < 4096);
        assert!(decompress(&bomb).is_err());
        let summary = read_submissions(
            &key,
            &[part(&key, &author, &header(1, 1), &bomb)],
            dir.path(),
        )
        .unwrap();
        assert_eq!((summary.rejected, summary.written.len()), (1, 0));

        // Too large to send.
        assert!(
            seal_payload(
                key.public(),
                "crash",
                &vec![0; MAX_PAYLOAD_BYTES + 1],
                0,
                |_| 0
            )
            .is_err()
        );
    }

    #[test]
    fn keygen_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.key");
        let key = keygen(&path).unwrap();
        assert_eq!(
            InboxKey::load(&path).unwrap().public().id(),
            key.public().id()
        );
        assert!(keygen(&path).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
