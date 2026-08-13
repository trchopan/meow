use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointId};
use iroh_blobs::{BlobsProtocol, HashAndFormat, store::fs::FsStore, ticket::BlobTicket};

pub(crate) struct BlobRuntime {
    endpoint: Option<Endpoint>,
    store: Option<FsStore>,
}

impl BlobRuntime {
    pub(crate) async fn start(endpoint: Endpoint, name: &str) -> Result<Arc<Self>> {
        let store = FsStore::load(blob_store_path(name)?).await?;
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

    pub(crate) fn protocol(&self) -> BlobsProtocol {
        BlobsProtocol::new(
            self.store
                .as_ref()
                .expect("blob runtime is disabled")
                .as_ref(),
            None,
        )
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

    pub(crate) async fn download_to(
        &self,
        provider: &str,
        content: &str,
        destination: PathBuf,
    ) -> Result<()> {
        let provider = EndpointId::from_str(provider).context("invalid blob provider id")?;
        let content = HashAndFormat::from_str(content).context("invalid blob content id")?;
        let store = self.store.as_ref().context("blob runtime is disabled")?;
        let endpoint = self.endpoint.as_ref().context("blob runtime is disabled")?;
        let downloader = store.downloader(endpoint);
        downloader.download(content, vec![provider]).await?;
        store.export(content.hash, destination).await?;
        Ok(())
    }

    pub(crate) async fn download_ticket(
        &self,
        ticket: &BlobTicket,
        destination: PathBuf,
    ) -> Result<()> {
        self.download_to(
            &ticket.addr().id.to_string(),
            &ticket.hash_and_format().to_string(),
            destination,
        )
        .await
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
