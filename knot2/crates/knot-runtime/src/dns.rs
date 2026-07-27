use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::OnceCell;

use crate::http::NetworkError;

pub type DnsFuture = Pin<Box<dyn Future<Output = Result<Vec<String>, NetworkError>> + Send>>;

pub trait DnsTxtResolver: Send + Sync + 'static {
    fn lookup_txt(&self, name: String) -> DnsFuture;
}

pub struct SystemDns {
    resolver: Arc<OnceCell<hickory_resolver::TokioAsyncResolver>>,
}

impl SystemDns {
    pub fn new() -> Self {
        Self {
            resolver: Arc::new(OnceCell::new()),
        }
    }
}

impl Default for SystemDns {
    fn default() -> Self {
        Self::new()
    }
}

impl DnsTxtResolver for SystemDns {
    fn lookup_txt(&self, name: String) -> DnsFuture {
        let cell = self.resolver.clone();
        Box::pin(async move {
            let resolver = cell
                .get_or_try_init(|| async {
                    hickory_resolver::TokioAsyncResolver::tokio_from_system_conf()
                        .map_err(|error| NetworkError::Build(error.to_string()))
                })
                .await?;
            match resolver.txt_lookup(name).await {
                Ok(lookup) => Ok(lookup.iter().map(render_txt).collect()),
                Err(error) => match error.kind() {
                    hickory_resolver::error::ResolveErrorKind::NoRecordsFound { .. } => {
                        Ok(Vec::new())
                    }
                    _ => Err(NetworkError::Request(error.to_string())),
                },
            }
        })
    }
}

fn render_txt(record: &hickory_resolver::proto::rr::rdata::TXT) -> String {
    let bytes: Vec<u8> = record
        .txt_data()
        .iter()
        .flat_map(|chunk| chunk.iter().copied())
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub struct FakeDns<F> {
    responder: F,
}

impl<F> FakeDns<F>
where
    F: Fn(&str) -> Result<Vec<String>, NetworkError> + Send + Sync + 'static,
{
    pub fn new(responder: F) -> Self {
        Self { responder }
    }
}

impl<F> DnsTxtResolver for FakeDns<F>
where
    F: Fn(&str) -> Result<Vec<String>, NetworkError> + Send + Sync + 'static,
{
    fn lookup_txt(&self, name: String) -> DnsFuture {
        let result = (self.responder)(&name);
        Box::pin(async move { result })
    }
}
