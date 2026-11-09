//! Signing and verifying repository indexes with ed25519.
//!
//! A repository's `index.toml` lists every archive with its SHA-256, so
//! authenticating the index authenticates every package in it. The signature
//! covers the exact bytes of the index, with a domain-separation prefix so it
//! can never be mistaken for a signature over anything else, and is stored
//! beside it as `index.toml.sig`.
//!
//! A signature counts only if its key id names a key in the *keyring*, a
//! directory of `*.pub` files the user controls. Verification is strict
//! (malleable signatures and weak keys are rejected). Nothing here decides
//! where the keyring comes from: that is the trust root and must arrive over a
//! channel you trust (for example baked into the installation image).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::id::{hex, sha256_hex};
use crate::{StoreError, io_ctx};

/// Prefix of everything that is signed. The version is part of it, so a
/// future format can never be confused with this one.
const DOMAIN: &[u8] = b"forge-index-v1\n";

fn bad(what: &'static str, value: impl Into<String>) -> StoreError {
    StoreError::Invalid { what, value: value.into() }
}

/// Decode lowercase hex of exactly `bytes` bytes.
fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let v = |c: u8| if c <= b'9' { c - b'0' } else { c - b'a' + 10 };
        out[i] = v(chunk[0]) * 16 + v(chunk[1]);
    }
    Some(out)
}

fn message(index_bytes: &[u8]) -> Vec<u8> {
    let mut m = DOMAIN.to_vec();
    m.extend_from_slice(index_bytes);
    m
}

// ---- keys -------------------------------------------------------------------

#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey {
    bytes: [u8; 32],
}

impl PublicKey {
    pub fn from_hex(s: &str) -> Result<Self, StoreError> {
        let bytes = unhex::<32>(s).ok_or_else(|| bad("public key (64 lowercase hex characters)", s))?;
        // Reject bytes that are not a valid curve point now, not at first use.
        VerifyingKey::from_bytes(&bytes).map_err(|e| bad("public key", e.to_string()))?;
        Ok(PublicKey { bytes })
    }

    pub fn to_hex(&self) -> String {
        hex(&self.bytes)
    }

    /// The key's id: SHA-256 of its 32 bytes. Signatures name their key by it.
    pub fn id(&self) -> String {
        sha256_hex(&self.bytes)
    }
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", self.id())
    }
}

/// A signing key. Never printed, and wiped from memory when dropped.
pub struct SecretKey {
    seed: [u8; 32],
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        // Best effort: volatile writes keep the compiler from optimizing this away.
        for b in self.seed.iter_mut() {
            // SAFETY: `b` is a valid, aligned, exclusive reference.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl SecretKey {
    /// A new random key, from the operating system's randomness.
    pub fn generate() -> Result<Self, StoreError> {
        let mut seed = [0u8; 32];
        fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut seed))
            .map_err(io_ctx("reading /dev/urandom"))?;
        Ok(SecretKey { seed })
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        SecretKey { seed }
    }

    pub fn public(&self) -> PublicKey {
        PublicKey { bytes: SigningKey::from_bytes(&self.seed).verifying_key().to_bytes() }
    }

    /// Sign the exact bytes of an index.
    pub fn sign_index(&self, index_bytes: &[u8]) -> SigFile {
        let sig = SigningKey::from_bytes(&self.seed).sign(&message(index_bytes));
        SigFile { version: 1, key_id: self.public().id(), signature: hex(&sig.to_bytes()) }
    }
}

// ---- files ------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PubFile {
    version: u32,
    public_key: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    version: u32,
    secret_seed: String,
    public_key: String,
}

fn valid_key_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Write `<name>.key` (mode 0600) and `<name>.pub` into `dir`. Never overwrites.
pub fn write_key_pair(dir: &Path, name: &str, key: &SecretKey) -> Result<(PathBuf, PathBuf), StoreError> {
    if !valid_key_name(name) {
        return Err(bad("key name (letters, digits, - and _)", name));
    }
    fs::create_dir_all(dir).map_err(io_ctx(format!("creating {}", dir.display())))?;
    let (secret_path, public_path) = (dir.join(format!("{name}.key")), dir.join(format!("{name}.pub")));
    let public = key.public();

    let secret_text = toml::to_string(&KeyFile {
        version: 1,
        secret_seed: hex(&key.seed),
        public_key: public.to_hex(),
    })
    .expect("serializes");
    let open = |path: &Path, mode: u32| {
        fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(path)
    };
    // The secret is created with its final permissions: there is no moment
    // when it exists readable by others.
    open(&secret_path, 0o600)
        .and_then(|mut f| f.write_all(secret_text.as_bytes()))
        .map_err(io_ctx(format!("creating {} (it must not already exist)", secret_path.display())))?;
    let public_text = toml::to_string(&PubFile { version: 1, public_key: public.to_hex() }).expect("serializes");
    if let Err(e) = open(&public_path, 0o644).and_then(|mut f| f.write_all(public_text.as_bytes())) {
        let _ = fs::remove_file(&secret_path);
        return Err(io_ctx(format!("creating {} (it must not already exist)", public_path.display()))(e));
    }
    Ok((secret_path, public_path))
}

/// Read a secret key file. Like `ssh`, refuse one that others can read.
pub fn load_secret_key(path: &Path) -> Result<SecretKey, StoreError> {
    let meta = fs::metadata(path).map_err(io_ctx(format!("reading {}", path.display())))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(bad(
            "secret key file permissions (others can read it; run chmod 600)",
            format!("{} has mode {mode:o}", path.display()),
        ));
    }
    let text = fs::read_to_string(path).map_err(io_ctx(format!("reading {}", path.display())))?;
    let file: KeyFile = toml::from_str(&text).map_err(|e| bad("secret key file", format!("{}: {e}", path.display())))?;
    if file.version != 1 {
        return Err(bad("secret key file version", file.version.to_string()));
    }
    let seed = unhex::<32>(&file.secret_seed).ok_or_else(|| bad("secret key", path.display().to_string()))?;
    let key = SecretKey { seed };
    // The file names its public half; they must agree, or the file was edited.
    if key.public().to_hex() != file.public_key {
        return Err(bad("secret key file (its public key does not match its secret)", path.display().to_string()));
    }
    Ok(key)
}

pub fn load_public_key(path: &Path) -> Result<PublicKey, StoreError> {
    let text = fs::read_to_string(path).map_err(io_ctx(format!("reading {}", path.display())))?;
    let file: PubFile = toml::from_str(&text).map_err(|e| bad("public key file", format!("{}: {e}", path.display())))?;
    if file.version != 1 {
        return Err(bad("public key file version", file.version.to_string()));
    }
    PublicKey::from_hex(&file.public_key)
}

// ---- signatures -------------------------------------------------------------

/// The contents of `index.toml.sig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigFile {
    pub version: u32,
    /// Id of the signing key (see [`PublicKey::id`]).
    pub key_id: String,
    /// 64-byte ed25519 signature, lowercase hex.
    pub signature: String,
}

impl SigFile {
    pub fn parse(text: &str) -> Result<Self, String> {
        let f: SigFile = toml::from_str(text).map_err(|e| e.to_string())?;
        if f.version != 1 {
            return Err(format!("unsupported signature version {}", f.version));
        }
        Ok(f)
    }

    pub fn render(&self) -> String {
        toml::to_string(self).expect("serializes")
    }
}

/// Why a signature was not accepted.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// Signed by a key that is not in the keyring.
    UnknownKey(String),
    Malformed(String),
    /// The signature does not match the index bytes.
    Invalid,
}

/// A directory of trusted public keys.
#[derive(Debug, Clone, Default)]
pub struct Keyring {
    keys: BTreeMap<String, PublicKey>,
}

impl Keyring {
    pub fn empty() -> Self {
        Keyring::default()
    }

    /// Load every `*.pub` file in `dir`. A missing directory is an empty
    /// keyring (nothing is trusted). A `*.pub` file that does not parse is an
    /// error: silently skipping it would quietly shrink what is trusted.
    pub fn load_dir(dir: &Path) -> Result<Self, StoreError> {
        let mut ring = Keyring::empty();
        let rd = match fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ring),
            Err(e) => return Err(io_ctx(format!("reading {}", dir.display()))(e)),
        };
        let mut paths: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "pub"))
            .collect();
        paths.sort();
        for p in paths {
            ring.insert(load_public_key(&p)?);
        }
        Ok(ring)
    }

    pub fn insert(&mut self, key: PublicKey) {
        self.keys.insert(key.id(), key);
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn ids(&self) -> Vec<String> {
        self.keys.keys().cloned().collect()
    }

    /// Check that `sig` is a valid signature over `index_bytes` by a key in
    /// the ring; returns the signing key's id.
    pub fn verify_index(&self, index_bytes: &[u8], sig: &SigFile) -> Result<String, VerifyError> {
        let key = self.keys.get(&sig.key_id).ok_or_else(|| VerifyError::UnknownKey(sig.key_id.clone()))?;
        let sig_bytes = unhex::<64>(&sig.signature)
            .ok_or_else(|| VerifyError::Malformed("signature is not 128 lowercase hex characters".into()))?;
        let verifying = VerifyingKey::from_bytes(&key.bytes).map_err(|e| VerifyError::Malformed(e.to_string()))?;
        verifying
            .verify_strict(&message(index_bytes), &Signature::from_bytes(&sig_bytes))
            .map_err(|_| VerifyError::Invalid)?;
        Ok(sig.key_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> SecretKey {
        SecretKey::from_seed([n; 32])
    }

    fn ring_of(keys: &[&SecretKey]) -> Keyring {
        let mut r = Keyring::empty();
        for k in keys {
            r.insert(k.public());
        }
        r
    }

    #[test]
    fn a_signature_verifies_with_the_matching_key() {
        let k = key(1);
        let sig = k.sign_index(b"index bytes");
        assert_eq!(ring_of(&[&k]).verify_index(b"index bytes", &sig), Ok(k.public().id()));
    }

    #[test]
    fn any_change_to_the_index_invalidates_the_signature() {
        let k = key(1);
        let sig = k.sign_index(b"index bytes");
        let ring = ring_of(&[&k]);
        for tampered in [&b"index byteS"[..], b"index bytes ", b"", b"index byte"] {
            assert_eq!(ring.verify_index(tampered, &sig), Err(VerifyError::Invalid), "{tampered:?}");
        }
    }

    #[test]
    fn a_key_that_is_not_in_the_keyring_is_not_trusted() {
        let (signer, other) = (key(1), key(2));
        let sig = signer.sign_index(b"x");
        assert!(matches!(ring_of(&[&other]).verify_index(b"x", &sig), Err(VerifyError::UnknownKey(_))));
        assert!(matches!(Keyring::empty().verify_index(b"x", &sig), Err(VerifyError::UnknownKey(_))));
    }

    #[test]
    fn a_signature_cannot_be_moved_to_another_key_id() {
        // Claiming a trusted key's id does not help: the signature is checked
        // against THAT key's bytes.
        let (attacker, trusted) = (key(1), key(2));
        let mut sig = attacker.sign_index(b"x");
        sig.key_id = trusted.public().id();
        assert_eq!(ring_of(&[&trusted]).verify_index(b"x", &sig), Err(VerifyError::Invalid));
    }

    #[test]
    fn malformed_signatures_are_rejected_not_panicked_on() {
        let k = key(1);
        let ring = ring_of(&[&k]);
        let good = k.sign_index(b"x");
        for bad_sig in ["", "zz", &"a".repeat(127), &"a".repeat(129), &"A".repeat(128), &"g".repeat(128)] {
            let sig = SigFile { signature: bad_sig.to_string(), ..good.clone() };
            assert!(matches!(ring.verify_index(b"x", &sig), Err(VerifyError::Malformed(_))), "{bad_sig:?}");
        }
        // Right shape, wrong content.
        let sig = SigFile { signature: "0".repeat(128), ..good };
        assert_eq!(ring.verify_index(b"x", &sig), Err(VerifyError::Invalid));
    }

    #[test]
    fn the_signed_message_is_domain_separated() {
        // A signature over the raw bytes (no prefix) must not verify as an index signature.
        let k = key(1);
        let raw = SigningKey::from_bytes(&[1; 32]).sign(b"index bytes");
        let sig = SigFile { version: 1, key_id: k.public().id(), signature: hex(&raw.to_bytes()) };
        assert_eq!(ring_of(&[&k]).verify_index(b"index bytes", &sig), Err(VerifyError::Invalid));
    }

    #[test]
    fn signing_is_deterministic_and_keys_differ() {
        assert_eq!(key(1).sign_index(b"a"), key(1).sign_index(b"a"));
        assert_ne!(key(1).sign_index(b"a"), key(2).sign_index(b"a"));
        assert_ne!(key(1).public().id(), key(2).public().id());
        assert_eq!(key(1).public().id().len(), 64);
    }

    #[test]
    fn generated_keys_are_random() {
        let (a, b) = (SecretKey::generate().unwrap(), SecretKey::generate().unwrap());
        assert_ne!(a.public(), b.public());
    }

    #[test]
    fn secrets_are_never_printed() {
        let k = key(7);
        assert!(!format!("{k:?}").contains("07"));
        assert_eq!(format!("{k:?}"), "SecretKey(<redacted>)");
    }

    #[test]
    fn signature_files_round_trip_and_reject_unknown_fields() {
        let sig = key(1).sign_index(b"x");
        assert_eq!(SigFile::parse(&sig.render()).unwrap(), sig);
        assert!(SigFile::parse("version = 2\nkey_id = \"a\"\nsignature = \"b\"\n").is_err());
        assert!(SigFile::parse(&format!("{}extra = 1\n", sig.render())).is_err());
        assert!(SigFile::parse("garbage").is_err());
    }

    #[test]
    fn key_files_are_written_private_and_load_back() {
        let d = tempfile::tempdir().unwrap();
        let k = key(3);
        let (secret, public) = write_key_pair(d.path(), "release", &k).unwrap();
        assert_eq!(fs::metadata(&secret).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&public).unwrap().permissions().mode() & 0o777, 0o644);
        assert_eq!(load_secret_key(&secret).unwrap().public(), k.public());
        assert_eq!(load_public_key(&public).unwrap(), k.public());
    }

    #[test]
    fn existing_key_files_are_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        write_key_pair(d.path(), "k", &key(1)).unwrap();
        assert!(write_key_pair(d.path(), "k", &key(2)).is_err());
        // The first key survived.
        assert_eq!(load_secret_key(&d.path().join("k.key")).unwrap().public(), key(1).public());
    }

    #[test]
    fn a_failed_second_file_does_not_leave_a_lone_secret_behind() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("k.pub"), "squatter").unwrap();
        assert!(write_key_pair(d.path(), "k", &key(1)).is_err());
        assert!(!d.path().join("k.key").exists());
    }

    #[test]
    fn bad_key_names_are_refused() {
        let d = tempfile::tempdir().unwrap();
        for n in ["", "../x", "a/b", "a b", ".hidden"] {
            assert!(write_key_pair(d.path(), n, &key(1)).is_err(), "{n:?}");
        }
        assert!(fs::read_dir(d.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_secret_key_readable_by_others_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (secret, _) = write_key_pair(d.path(), "k", &key(1)).unwrap();
        for mode in [0o640, 0o604, 0o644, 0o660, 0o666] {
            fs::set_permissions(&secret, fs::Permissions::from_mode(mode)).unwrap();
            let err = load_secret_key(&secret).unwrap_err().to_string();
            assert!(err.contains("chmod 600"), "{mode:o}: {err}");
        }
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(load_secret_key(&secret).is_ok(), "read-only for the owner is fine");
    }

    #[test]
    fn an_edited_secret_key_file_is_detected() {
        let d = tempfile::tempdir().unwrap();
        let (secret, _) = write_key_pair(d.path(), "k", &key(1)).unwrap();
        let other = key(2);
        let text = fs::read_to_string(&secret).unwrap().replace(&key(1).public().to_hex(), &other.public().to_hex());
        fs::write(&secret, text).unwrap();
        assert!(load_secret_key(&secret).unwrap_err().to_string().contains("does not match"));
    }

    #[test]
    fn public_keys_must_be_valid_curve_points() {
        // Roughly half of all y-coordinates are not on the curve. Find some by
        // search, and check that every one is rejected and every valid one works.
        let (mut rejected, mut accepted) = (0, 0);
        for first in 0u8..=255 {
            let mut bytes = [0u8; 32];
            bytes[0] = first;
            let hex_key = hex(&bytes);
            match PublicKey::from_hex(&hex_key) {
                Ok(k) => {
                    accepted += 1;
                    assert_eq!(k.to_hex(), hex_key);
                }
                Err(_) => rejected += 1,
            }
        }
        assert!(rejected > 50 && accepted > 50, "rejected {rejected}, accepted {accepted}");
        assert!(PublicKey::from_hex("abc").is_err());
        assert!(PublicKey::from_hex(&"AB".repeat(32)).is_err(), "uppercase is not canonical");
        assert!(PublicKey::from_hex(&key(1).public().to_hex()).is_ok());
    }

    #[test]
    fn a_weak_key_cannot_be_used_to_forge_a_signature() {
        // The identity point is a valid public key, and (R = identity, S = 0) is
        // the classic signature that a non-strict verifier accepts for ANY message.
        // Strict verification must refuse it.
        let weak_hex = format!("01{}", "00".repeat(31));
        let weak = PublicKey::from_hex(&weak_hex).expect("the identity point is a valid encoding");
        let mut ring = Keyring::empty();
        ring.insert(weak.clone());
        let forged = SigFile {
            version: 1,
            key_id: weak.id(),
            signature: format!("01{}{}", "00".repeat(31), "00".repeat(32)),
        };
        for msg in [&b"anything"[..], b"", b"index bytes"] {
            assert_eq!(ring.verify_index(msg, &forged), Err(VerifyError::Invalid), "{msg:?}");
        }
    }

    #[test]
    fn the_keyring_loads_every_pub_file_and_ignores_other_files() {
        let d = tempfile::tempdir().unwrap();
        for (n, k) in [("a", key(1)), ("b", key(2))] {
            write_key_pair(d.path(), n, &k).unwrap();
        }
        fs::write(d.path().join("README"), "not a key").unwrap();
        let ring = Keyring::load_dir(d.path()).unwrap();
        assert_eq!(ring.ids().len(), 2, "the .key files and README are not loaded as keys");
        assert!(ring.verify_index(b"x", &key(2).sign_index(b"x")).is_ok());
    }

    #[test]
    fn a_missing_keyring_directory_trusts_nothing() {
        let d = tempfile::tempdir().unwrap();
        let ring = Keyring::load_dir(&d.path().join("nope")).unwrap();
        assert!(ring.is_empty());
        assert!(matches!(ring.verify_index(b"x", &key(1).sign_index(b"x")), Err(VerifyError::UnknownKey(_))));
    }

    #[test]
    fn a_broken_key_file_is_an_error_not_a_quietly_smaller_keyring() {
        let d = tempfile::tempdir().unwrap();
        write_key_pair(d.path(), "good", &key(1)).unwrap();
        fs::write(d.path().join("broken.pub"), "this is not a key file").unwrap();
        assert!(Keyring::load_dir(d.path()).is_err());
    }
}
