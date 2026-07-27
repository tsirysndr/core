use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, RwLock};

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hkdf::Hkdf;
use knot_runtime::{Entropy, K256Signer, MAX_SCALAR_ATTEMPTS, PublicKeyBytes, Signer};
use knot_types::KnotId;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

// why oh why didn't I just call this knot.sealed-key-store.v1. the
// wind changed and now we're stuck with this
const HKDF_INFO: &[u8] = b"knot2.sealed-key-store.v1";
const NONCE_LEN: usize = 12;
const SCALAR_LEN: usize = 32;
const VAULT_VERSION: u32 = 1;
const MIN_MASTER_KEY_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("sealed key store {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "sealed key store couldn't be decrypted. Master key may be wrong or file may be corrupt."
    )]
    Decrypt,
    #[error("sealed key store is malformed: {0}")]
    Malformed(String),
    #[error("no signing key sealed for {0}")]
    Missing(String),
    #[error("signing key is already sealed for {0}")]
    Occupied(String),
    #[error("{len}-byte master key is shorter than the {MIN_MASTER_KEY_LEN}-byte minimum")]
    WeakMasterKey { len: usize },
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey(Vec<u8>);

impl MasterKey {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, SecretsError> {
        let bytes = bytes.into();
        if bytes.len() < MIN_MASTER_KEY_LEN {
            return Err(SecretsError::WeakMasterKey { len: bytes.len() });
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SealedKeyId(String);

impl SealedKeyId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&KnotId> for SealedKeyId {
    fn from(did: &KnotId) -> Self {
        Self(did.as_str().to_string())
    }
}

impl<'de> serde::Deserialize<'de> for SealedKeyId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        KnotId::new(raw)
            .map(|did| Self::from(&did))
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
struct SecretScalar([u8; SCALAR_LEN]);

impl SecretScalar {
    fn generate(entropy: &dyn Entropy) -> Self {
        std::iter::repeat_with(|| {
            let mut bytes = [0u8; SCALAR_LEN];
            entropy.fill(&mut bytes);
            k256::ecdsa::SigningKey::from_slice(&bytes)
                .is_ok()
                .then_some(bytes)
        })
        .take(MAX_SCALAR_ATTEMPTS)
        .flatten()
        .next()
        .map(Self)
        .unwrap_or_else(|| {
            panic!(
                "entropy failed to yield valid secp256k1 scalar in {MAX_SCALAR_ATTEMPTS} attempts"
            )
        })
    }

    fn signer(&self) -> K256Signer {
        K256Signer::from_slice(&self.0).expect("stored scalar is always a valid signing key")
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct VaultKey([u8; 32]);

impl VaultKey {
    fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

pub struct KeyMaterial {
    scalar: SecretScalar,
}

impl KeyMaterial {
    pub fn signer(&self) -> K256Signer {
        self.scalar.signer()
    }

    pub fn public_key(&self) -> PublicKeyBytes {
        self.scalar.signer().public_key()
    }
}

#[derive(Zeroize, ZeroizeOnDrop, Serialize, Deserialize)]
#[serde(transparent)]
struct EncodedSecret(String);

impl EncodedSecret {
    fn new(value: String) -> Self {
        Self(value)
    }
}

#[derive(Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    entries: BTreeMap<SealedKeyId, EncodedSecret>,
}

pub struct SealedStore {
    path: PathBuf,
    enc_key: VaultKey,
    entropy: Box<dyn Entropy>,
    entries: RwLock<BTreeMap<SealedKeyId, SecretScalar>>,
    persist_lock: Mutex<()>,
}

impl SealedStore {
    pub fn open(
        path: impl Into<PathBuf>,
        master_key: &MasterKey,
        entropy: Box<dyn Entropy>,
    ) -> Result<Self, SecretsError> {
        let path = path.into();
        knot_resource::clear_staging(&path);
        sweep_pre_knot_tmp_vaults(&path);
        let enc_key = derive_enc_key(master_key);
        let entries = match std::fs::read(&path) {
            Ok(sealed) => decode_vault(&unseal(&enc_key, &sealed)?)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(source) => {
                return Err(SecretsError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        Ok(Self {
            path,
            enc_key,
            entropy,
            entries: RwLock::new(entries),
            persist_lock: Mutex::new(()),
        })
    }

    pub fn signer(&self, id: impl Into<SealedKeyId>) -> Result<K256Signer, SecretsError> {
        let id = id.into();
        self.entries
            .read()
            .expect("sealed store lock")
            .get(&id)
            .map(SecretScalar::signer)
            .ok_or(SecretsError::Missing(id.0))
    }

    pub fn public_key(&self, id: impl Into<SealedKeyId>) -> Result<PublicKeyBytes, SecretsError> {
        let id = id.into();
        self.entries
            .read()
            .expect("sealed store lock")
            .get(&id)
            .map(|scalar| scalar.signer().public_key())
            .ok_or(SecretsError::Missing(id.0))
    }

    pub fn generate(&self) -> KeyMaterial {
        KeyMaterial {
            scalar: SecretScalar::generate(&*self.entropy),
        }
    }

    pub fn store(
        &self,
        id: impl Into<SealedKeyId>,
        material: &KeyMaterial,
    ) -> Result<(), SecretsError> {
        let id = id.into();
        let guard = self.persist_guard();
        let mut staged = self.staged();
        if staged.contains_key(&id) {
            return Err(SecretsError::Occupied(id.0));
        }
        staged.insert(id, material.scalar.clone());
        self.commit_locked(&guard, staged)
    }

    pub fn ensure(&self, id: impl Into<SealedKeyId>) -> Result<PublicKeyBytes, SecretsError> {
        let id = id.into();
        let guard = self.persist_guard();
        if let Some(public) = self
            .entries
            .read()
            .expect("sealed store lock")
            .get(&id)
            .map(|scalar| scalar.signer().public_key())
        {
            return Ok(public);
        }
        let material = self.generate();
        let public = material.public_key();
        let mut staged = self.staged();
        staged.insert(id, material.scalar.clone());
        self.commit_locked(&guard, staged)?;
        Ok(public)
    }

    pub fn len(&self) -> usize {
        self.entries.read().expect("sealed store lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.read().expect("sealed store lock").is_empty()
    }

    pub fn remove(&self, id: impl Into<SealedKeyId>) -> Result<bool, SecretsError> {
        let id = id.into();
        let guard = self.persist_guard();
        let mut staged = self.staged();
        if staged.remove(&id).is_none() {
            return Ok(false);
        }
        self.commit_locked(&guard, staged)?;
        Ok(true)
    }

    fn persist_guard(&self) -> MutexGuard<'_, ()> {
        self.persist_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn staged(&self) -> BTreeMap<SealedKeyId, SecretScalar> {
        self.entries.read().expect("sealed store lock").clone()
    }

    fn commit_locked(
        &self,
        _guard: &MutexGuard<'_, ()>,
        staged: BTreeMap<SealedKeyId, SecretScalar>,
    ) -> Result<(), SecretsError> {
        let plaintext = encode_vault(&staged);
        let sealed = seal(&self.enc_key, &plaintext, &*self.entropy);
        atomic_write(&self.path, &sealed)?;
        *self.entries.write().expect("sealed store lock") = staged;
        Ok(())
    }
}

fn derive_enc_key(master_key: &MasterKey) -> VaultKey {
    let hk = Hkdf::<Sha256>::new(None, master_key.as_bytes());
    let mut okm = VaultKey([0u8; 32]);
    hk.expand(HKDF_INFO, &mut okm.0)
        .expect("32 bytes is valid HKDF-SHA256 output length");
    okm
}

fn seal(enc_key: &VaultKey, plaintext: &[u8], entropy: &dyn Entropy) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(enc_key.as_bytes()).expect("32-byte AES-256-GCM key");
    let mut nonce_bytes = [0u8; NONCE_LEN];
    entropy.fill(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(&nonce_bytes.into(), plaintext)
        .expect("AES-256-GCM encryption doesn't fail on valid inputs");
    [&nonce_bytes[..], &ciphertext[..]].concat()
}

fn unseal(enc_key: &VaultKey, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, SecretsError> {
    if sealed.len() < NONCE_LEN {
        return Err(SecretsError::Decrypt);
    }
    let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce_bytes
        .try_into()
        .expect("split_at(NONCE_LEN) yields exactly NONCE_LEN bytes");
    let cipher = Aes256Gcm::new_from_slice(enc_key.as_bytes()).expect("32-byte AES-256-GCM key");
    cipher
        .decrypt(&nonce.into(), ciphertext)
        .map(Zeroizing::new)
        .map_err(|_| SecretsError::Decrypt)
}

fn encode_vault(entries: &BTreeMap<SealedKeyId, SecretScalar>) -> Zeroizing<Vec<u8>> {
    let file = VaultFile {
        version: VAULT_VERSION,
        entries: entries
            .iter()
            .map(|(id, scalar)| {
                (
                    id.clone(),
                    EncodedSecret::new(STANDARD.encode(scalar.0.as_slice())),
                )
            })
            .collect(),
    };
    Zeroizing::new(serde_json::to_vec(&file).expect("vault always serializes"))
}

fn decode_vault(plaintext: &[u8]) -> Result<BTreeMap<SealedKeyId, SecretScalar>, SecretsError> {
    let VaultFile { version, entries } = serde_json::from_slice(plaintext)
        .map_err(|error| SecretsError::Malformed(error.to_string()))?;
    if version != VAULT_VERSION {
        return Err(SecretsError::Malformed(format!(
            "unsupported vault version {version}"
        )));
    }
    entries
        .into_iter()
        .map(|(id, mut encoded)| {
            let encoded = Zeroizing::new(std::mem::take(&mut encoded.0));
            let bytes = Zeroizing::new(
                STANDARD
                    .decode(encoded.as_bytes())
                    .map_err(|error| SecretsError::Malformed(error.to_string()))?,
            );
            let scalar: [u8; SCALAR_LEN] = bytes.as_slice().try_into().map_err(|_| {
                SecretsError::Malformed(format!("scalar for {} isn't 32 bytes", id.as_str()))
            })?;
            k256::ecdsa::SigningKey::from_slice(&scalar).map_err(|_| {
                SecretsError::Malformed(format!("scalar for {} isn't a valid key", id.as_str()))
            })?;
            Ok((id, SecretScalar(scalar)))
        })
        .collect()
}

fn effective_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn sweep_pre_knot_tmp_vaults(path: &Path) {
    let Some(stem) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Ok(listing) = std::fs::read_dir(effective_parent(path)) else {
        return;
    };
    let prefix = format!(".{stem}.");
    listing
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".tmp"))
        })
        .for_each(|path| {
            let _ = std::fs::remove_file(path);
        });
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SecretsError> {
    let parent = effective_parent(path);
    std::fs::create_dir_all(parent).map_err(|source| SecretsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    knot_resource::atomic_write_bytes(path, bytes, knot_resource::FileMode::Private)
        .map_err(Into::into)
}

impl From<knot_resource::FsError> for SecretsError {
    fn from(error: knot_resource::FsError) -> Self {
        SecretsError::Io {
            path: error.path,
            source: error.source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use knot_runtime::{SeededEntropy, Signer, verify};

    fn entropy(seed: u64) -> Box<dyn Entropy> {
        Box::new(SeededEntropy::new(seed))
    }

    fn master() -> MasterKey {
        MasterKey::new([7u8; 32]).unwrap()
    }

    fn kid(value: &str) -> KnotId {
        KnotId::new(value).unwrap()
    }

    fn store_at(path: &Path, seed: u64) -> SealedStore {
        SealedStore::open(path, &master(), entropy(seed)).unwrap()
    }

    fn store(seed: u64) -> (tempfile::TempDir, SealedStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir.path().join("keys.sealed"), seed);
        (dir, store)
    }

    #[test]
    fn an_absent_file_opens_as_an_empty_store() {
        let (_dir, store) = store(1);
        assert!(matches!(
            store.signer(&kid("did:web:nel.pet")),
            Err(SecretsError::Missing(_))
        ));
    }

    #[test]
    fn a_sealed_key_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.sealed");
        let public = store_at(&path, 1).ensure(&kid("did:web:nel.pet")).unwrap();
        let reopened = store_at(&path, 2);
        assert_eq!(
            reopened.public_key(&kid("did:web:nel.pet")).unwrap(),
            public
        );
    }

    #[test]
    fn ensure_is_idempotent() {
        let (_dir, store) = store(1);
        let first = store.ensure(&kid("did:plc:squid")).unwrap();
        let second = store.ensure(&kid("did:plc:squid")).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn a_generated_key_signs_verifiably_and_round_trips_through_storage() {
        let (_dir, store) = store(5);
        let material = store.generate();
        let public = material.public_key();
        store.store(&kid("did:plc:limpet"), &material).unwrap();

        let signer = store.signer(&kid("did:plc:limpet")).unwrap();
        let signature = signer.sign(b"a meta-repo cob change");
        assert!(verify(&public, b"a meta-repo cob change", &signature));
        assert_eq!(signer.public_key(), public);
    }

    #[test]
    fn storing_over_a_sealed_key_is_refused() {
        let (_dir, store) = store(1);
        let original = store.ensure(&kid("did:plc:limpet")).unwrap();
        let intruder = store.generate();
        assert!(matches!(
            store.store(&kid("did:plc:limpet"), &intruder),
            Err(SecretsError::Occupied(_))
        ));
        assert_eq!(
            store.public_key(&kid("did:plc:limpet")).unwrap(),
            original,
            "refused overwrite must leave the sealed key untouched"
        );
    }

    #[test]
    fn master_key_length_is_enforced_at_construction() {
        [(31usize, false), (32usize, true)]
            .iter()
            .for_each(|&(len, accepted)| {
                let result = MasterKey::new(vec![7u8; len]);
                assert_eq!(result.is_ok(), accepted, "{len}-byte master key acceptance");
                assert!(
                    accepted
                        || matches!(result, Err(SecretsError::WeakMasterKey { len: reported }) if reported == len),
                    "a refused master key reports its short length"
                );
            });
    }

    #[test]
    fn the_master_key_debug_redacts_its_bytes() {
        let rendered = format!("{:?}", MasterKey::new([7u8; 32]).unwrap());
        assert_eq!(rendered, "MasterKey(<redacted>)");
        assert!(!rendered.contains('7'));
    }

    #[test]
    fn a_wrong_master_key_fails_to_decrypt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        store_at(&path, 1).ensure(&kid("did:web:nel.pet")).unwrap();
        assert!(matches!(
            SealedStore::open(&path, &MasterKey::new([9u8; 32]).unwrap(), entropy(1)),
            Err(SecretsError::Decrypt)
        ));
    }

    #[test]
    fn a_removed_key_is_gone_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        let store = store_at(&path, 1);
        store.ensure(&kid("did:plc:squid")).unwrap();
        store.ensure(&kid("did:plc:clam")).unwrap();
        assert!(store.remove(&kid("did:plc:squid")).unwrap());
        assert!(!store.remove(&kid("did:plc:squid")).unwrap());

        let reopened = store_at(&path, 1);
        assert!(reopened.signer(&kid("did:plc:squid")).is_err());
        assert!(reopened.signer(&kid("did:plc:clam")).is_ok());
    }

    #[test]
    fn concurrent_writers_never_lose_a_sealed_key_or_error() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.sealed");
        let store = Arc::new(
            SealedStore::open(&path, &master(), Box::new(knot_runtime::OsEntropy)).unwrap(),
        );
        let dids: Vec<KnotId> = (0..64).map(|i| kid(&format!("did:plc:race{i}"))).collect();

        std::thread::scope(|scope| {
            dids.chunks(8).for_each(|chunk| {
                let store = Arc::clone(&store);
                let chunk = chunk.to_vec();
                scope.spawn(move || {
                    chunk.iter().for_each(|did| {
                        store.ensure(did).expect("concurrent seal mustn't error");
                    });
                });
            });
        });

        let reopened = SealedStore::open(&path, &master(), entropy(99)).unwrap();
        let missing: Vec<&KnotId> = dids
            .iter()
            .filter(|did| reopened.signer(*did).is_err())
            .collect();
        assert!(
            missing.is_empty(),
            "keys acknowledged in memory were lost from sealed file on disk: {missing:?}"
        );
    }

    #[test]
    fn concurrent_ensures_of_one_did_agree_on_a_single_key() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SealedStore::open(
                dir.path().join("keys.sealed"),
                &master(),
                Box::new(knot_runtime::OsEntropy),
            )
            .unwrap(),
        );
        let contested = kid("did:plc:whelk");

        let publics: Vec<PublicKeyBytes> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let store = Arc::clone(&store);
                    let did = contested.clone();
                    scope.spawn(move || store.ensure(&did).unwrap())
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });

        assert!(
            publics.windows(2).all(|pair| pair[0] == pair[1]),
            "every racing ensure must acknowledge same sealed key"
        );
        assert_eq!(store.public_key(&contested).unwrap(), publics[0]);
    }

    #[test]
    fn the_sealed_file_does_not_contain_raw_scalars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        let store = store_at(&path, 1);
        let material = store.generate();
        let scalar = material.scalar.0;
        store.store(&kid("did:plc:squid"), &material).unwrap();
        let sealed = std::fs::read(&path).unwrap();
        assert!(
            !sealed.windows(SCALAR_LEN).any(|window| window == scalar),
            "plaintext scalar must never appear in the sealed file"
        );
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn make_read_only(dir: &Path) -> bool {
        set_mode(dir, 0o555);
        let enforced = std::fs::write(dir.join("probe"), b"probe").is_err();
        if !enforced {
            set_mode(dir, 0o755);
            eprintln!("skipping permission fault injection, this user bypasses read-only modes");
        }
        enforced
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_persist_acknowledges_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.sealed");
        let store = store_at(&path, 1);
        store.ensure(&kid("did:plc:squid")).unwrap();

        if !make_read_only(dir.path()) {
            return;
        }
        let material = store.generate();
        assert!(matches!(
            store.store(&kid("did:plc:clam"), &material),
            Err(SecretsError::Io { .. })
        ));
        assert!(
            matches!(
                store.signer(&kid("did:plc:clam")),
                Err(SecretsError::Missing(_))
            ),
            "key whose persist failed mustn't be served from memory"
        );
        assert!(matches!(
            store.ensure(&kid("did:plc:clam")),
            Err(SecretsError::Io { .. })
        ));
        set_mode(dir.path(), 0o755);

        let public = store.ensure(&kid("did:plc:clam")).unwrap();
        let reopened = store_at(&path, 2);
        assert_eq!(reopened.public_key(&kid("did:plc:clam")).unwrap(), public);
        assert!(reopened.signer(&kid("did:plc:squid")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_removal_keeps_the_key_served_and_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.sealed");
        let store = store_at(&path, 1);
        store.ensure(&kid("did:plc:squid")).unwrap();

        if !make_read_only(dir.path()) {
            return;
        }
        assert!(matches!(
            store.remove(&kid("did:plc:squid")),
            Err(SecretsError::Io { .. })
        ));
        assert!(
            store.signer(&kid("did:plc:squid")).is_ok(),
            "removal that failed to persist must leave the key in service"
        );
        set_mode(dir.path(), 0o755);

        let reopened = store_at(&path, 2);
        assert!(reopened.signer(&kid("did:plc:squid")).is_ok());
    }

    struct BrokenEntropy;

    impl Entropy for BrokenEntropy {
        fn next_u64(&self) -> u64 {
            0
        }

        fn fill(&self, buffer: &mut [u8]) {
            buffer.fill(0);
        }

        fn derive(&self, _label: u64) -> Box<dyn Entropy> {
            Box::new(BrokenEntropy)
        }
    }

    #[test]
    #[should_panic(expected = "entropy failed to yield valid secp256k1 scalar")]
    fn broken_entropy_fails_stop_instead_of_spinning() {
        let _ = SecretScalar::generate(&BrokenEntropy);
    }

    #[test]
    fn stale_temp_files_are_swept_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.sealed");
        let stale = dir.path().join(".keys.sealed.knot-tmp.4242.7");
        let in_flight = dir.path().join(".keys.sealed.knot-tmp.4243.0");
        let pre_rename = dir.path().join(".keys.sealed.4242.7.tmp");
        std::fs::write(&stale, b"abandoned by a crashed run").unwrap();
        std::fs::write(&in_flight, b"another process is sealing right now").unwrap();
        std::fs::write(&pre_rename, b"abandoned before the staging rename").unwrap();
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(long_ago))
            .unwrap();

        store_at(&path, 1);
        assert!(!stale.exists(), "abandoned temp file must be swept at open");
        assert!(
            in_flight.exists(),
            "the sweep at open must keep the staging file a second process is filling"
        );
        assert!(
            !pre_rename.exists(),
            "a vault sealed by an older build still leaves temps this build has to reclaim"
        );
    }
}
