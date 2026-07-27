use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use knot_types::KnotHostname;
use rustls::server::ResolvesServerCert;
use rustls_acme::caches::DirCache;
use rustls_acme::{AccountCache, AcmeConfig, CertCache};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    #[error("acme cache {path}: {source}")]
    Cache {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

struct RestrictedDirCache {
    dir: PathBuf,
    inner: DirCache<PathBuf>,
}

impl RestrictedDirCache {
    fn new(dir: PathBuf) -> Self {
        let inner = DirCache::new(dir.clone());
        Self { dir, inner }
    }
}

#[async_trait]
impl CertCache for RestrictedDirCache {
    type EC = std::io::Error;

    async fn load_cert(
        &self,
        domains: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EC> {
        self.inner.load_cert(domains, directory_url).await
    }

    async fn store_cert(
        &self,
        domains: &[String],
        directory_url: &str,
        cert: &[u8],
    ) -> Result<(), Self::EC> {
        self.inner.store_cert(domains, directory_url, cert).await?;
        restrict_cache_dir(&self.dir).map_err(std::io::Error::other)
    }
}

#[async_trait]
impl AccountCache for RestrictedDirCache {
    type EA = std::io::Error;

    async fn load_account(
        &self,
        contact: &[String],
        directory_url: &str,
    ) -> Result<Option<Vec<u8>>, Self::EA> {
        self.inner.load_account(contact, directory_url).await
    }

    async fn store_account(
        &self,
        contact: &[String],
        directory_url: &str,
        account: &[u8],
    ) -> Result<(), Self::EA> {
        self.inner
            .store_account(contact, directory_url, account)
            .await?;
        restrict_cache_dir(&self.dir).map_err(std::io::Error::other)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("acme contact {value:?} isn't a bare email address")]
pub struct AcmeContactError {
    value: String,
}

pub struct AcmeContact(String);

impl AcmeContact {
    // `mailto()` already puts on the scheme for us,
    // so someone that came with one
    // already went out to the ACME account as mailto:mailto:.
    // Hence no colons.
    pub fn new(contact: impl Into<String>) -> Result<Self, AcmeContactError> {
        let contact = contact.into();
        let mut halves = contact.split('@');
        let well_formed = matches!((halves.next(), halves.next(), halves.next()), (Some(local), Some(domain), None) if !local.is_empty() && domain.contains('.'))
            && !contact.contains(':')
            && !contact.chars().any(|c| c.is_whitespace() || c.is_control());
        match well_formed {
            true => Ok(Self(contact)),
            false => Err(AcmeContactError { value: contact }),
        }
    }

    pub fn mailto(&self) -> String {
        format!("mailto:{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcmeCacheDir(PathBuf);

impl AcmeCacheDir {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

pub struct AcmeParams {
    pub domains: Vec<KnotHostname>,
    pub contact: AcmeContact,
    pub cache_dir: AcmeCacheDir,
    pub production: bool,
}

pub fn start(
    params: AcmeParams,
    shutdown: CancellationToken,
) -> Result<Arc<dyn ResolvesServerCert>, AcmeError> {
    std::fs::create_dir_all(params.cache_dir.as_path()).map_err(|source| AcmeError::Cache {
        path: params.cache_dir.as_path().display().to_string(),
        source,
    })?;
    restrict_cache_dir(params.cache_dir.as_path())?;

    let mut state = AcmeConfig::<Infallible, Infallible>::new(params.domains)
        .contact_push(params.contact.mailto())
        .cache(RestrictedDirCache::new(params.cache_dir.0))
        .directory_lets_encrypt(params.production)
        .state();
    let resolver = state.resolver();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                event = state.next() => match event {
                    Some(Ok(ok)) => tracing::info!("acme: {ok:?}"),
                    Some(Err(error)) => tracing::warn!("acme: {error:?}"),
                    None => {
                        tracing::warn!("acme renewal stream ended, certificates will no longer renew");
                        break;
                    }
                },
            }
        }
    });

    Ok(resolver)
}

#[cfg(unix)]
fn restrict_cache_dir(path: &std::path::Path) -> Result<(), AcmeError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|source| {
        AcmeError::Cache {
            path: path.display().to_string(),
            source,
        }
    })?;
    std::fs::read_dir(path)
        .map_err(|source| AcmeError::Cache {
            path: path.display().to_string(),
            source,
        })?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        })
        .try_for_each(|entry| {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600)).map_err(
                |source| AcmeError::Cache {
                    path: entry.path().display().to_string(),
                    source,
                },
            )
        })
}

#[cfg(not(unix))]
fn restrict_cache_dir(_path: &std::path::Path) -> Result<(), AcmeError> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn acme_contact_accepts_a_bare_email_and_rejects_everything_else() {
        assert_eq!(
            AcmeContact::new("ops@oyster.cafe").unwrap().mailto(),
            "mailto:ops@oyster.cafe"
        );
        assert!(AcmeContact::new("").is_err());
        assert!(AcmeContact::new("ops").is_err());
        assert!(AcmeContact::new("ops@localhost").is_err());
        assert!(AcmeContact::new("mailto:ops@oyster.cafe").is_err());
        assert!(AcmeContact::new("ops@nel.pet@extra.dev").is_err());
        assert!(AcmeContact::new("ops @oyster.cafe").is_err());
        assert!(AcmeContact::new("@oyster.cafe").is_err());
    }

    #[test]
    fn the_cache_dir_and_its_files_are_tightened_to_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("account.key");
        std::fs::write(&key, b"private material").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();

        restrict_cache_dir(dir.path()).unwrap();

        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700,
            "the cache directory must be traversable only by its owner"
        );
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600,
            "a cached private key must be readable only by its owner"
        );
    }

    #[tokio::test]
    async fn a_cert_stored_after_boot_is_tightened_to_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let cache = RestrictedDirCache::new(dir.path().to_path_buf());
        cache
            .store_cert(
                &["anemone.knot".to_string()],
                "https://acme.test/directory",
                b"private cert material",
            )
            .await
            .unwrap();

        let modes: Vec<u32> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| {
                std::fs::metadata(entry.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            })
            .collect();
        assert_eq!(
            modes,
            vec![0o600],
            "a certificate written after boot must be the only entry and owner-only"
        );
    }
}
