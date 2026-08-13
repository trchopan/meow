use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::Endpoint;
use iroh_blobs::{
    HashAndFormat,
    store::{
        GcConfig, ProtectOutcome,
        fs::{FsStore, options::Options},
    },
    ticket::BlobTicket,
};

pub(crate) struct BlobRuntime {
    endpoint: Option<Endpoint>,
    store: Option<FsStore>,
}

impl BlobRuntime {
    pub(crate) async fn start(
        endpoint: Endpoint,
        name: &str,
        registry: Option<crate::model::TransferRegistry>,
    ) -> Result<Arc<Self>> {
        let path = blob_store_path(name)?;
        let mut options = Options::new(&path);
        if let Some(registry) = registry {
            options.gc = Some(GcConfig {
                interval: Duration::from_secs(60 * 60),
                add_protected: Some(Arc::new(move |live: &mut HashSet<iroh_blobs::Hash>| {
                    let registry = registry.clone();
                    Box::pin(async move {
                        let now = crate::transfer::now_secs_for_gc();
                        let guard = registry.lock().expect("transfer registry poisoned");
                        for grant in guard.values().filter(|grant| grant.expires_at >= now) {
                            live.insert(grant.hash);
                        }
                        ProtectOutcome::Continue
                    })
                })),
            });
        }
        let store = FsStore::load_with_opts(path.join("blobs.db"), options).await?;
        Ok(Arc::new(Self {
            endpoint: Some(endpoint),
            store: Some(store),
        }))
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> Arc<Self> {
        Arc::new(Self {
            endpoint: None,
            store: None,
        })
    }

    pub(crate) async fn add_path(&self, path: &Path) -> Result<String> {
        let tag = self
            .store
            .as_ref()
            .context("blob runtime is disabled")?
            .add_path(path)
            .await?;
        Ok(HashAndFormat::new(tag.hash, tag.format).to_string())
    }

    pub(crate) fn ticket_for(&self, content: &str) -> Result<BlobTicket> {
        let content = HashAndFormat::from_str(content).context("invalid blob content id")?;
        let endpoint = self.endpoint.as_ref().context("blob runtime is disabled")?;
        Ok(BlobTicket::new(
            endpoint.addr(),
            content.hash,
            content.format,
        ))
    }

    pub(crate) fn transfer_protocol(
        &self,
        registry: crate::model::TransferRegistry,
    ) -> Result<crate::transfer::TransferProtocol> {
        let store = self
            .store
            .as_ref()
            .context("blob runtime is disabled")?
            .clone();
        Ok(crate::transfer::TransferProtocol::new(store, registry))
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        if let Some(store) = self.store.as_ref() {
            store.shutdown().await?;
        }
        Ok(())
    }
}

fn blob_store_path(name: &str) -> Result<PathBuf> {
    Ok(crate::state::app_data_dir()?.join("blobs").join(name))
}
