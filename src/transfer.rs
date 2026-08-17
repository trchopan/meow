use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, endpoint::Connection, protocol::ProtocolHandler};
use iroh_blobs::{Hash, store::fs::FsStore, ticket::BlobTicket};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::file_transfer;
use rand::RngCore;

const REFERENCE_PREFIX: &str = "meow1-";
const REFERENCE_MAX_BYTES: usize = 16 * 1024;
const REFERENCE_TTL_SECS: u64 = 24 * 60 * 60;
pub(crate) const TRANSFER_ALPN: &[u8] = b"meow/file-transfer/1";
const MAX_TRANSFER_SIZE: u64 = 100 * 1024 * 1024;
const TRANSFER_TIMEOUT_SECS: u64 = 10 * 60;
const MAX_ACTIVE_GRANTS: usize = 256;
const MAX_ATTEMPTS: u8 = 3;
const RETRY_WINDOW_SECS: u64 = 2 * 60;

#[derive(Debug, Clone)]
pub(crate) struct TransferGrant {
    pub(crate) hash: Hash,
    pub(crate) size: u64,
    pub(crate) digest: [u8; 32],
    pub(crate) expires_at: u64,
    pub(crate) attempts: u8,
    pub(crate) in_flight: bool,
    pub(crate) retry_until: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransferReference {
    ticket: String,
    capability: String,
    expires_at: u64,
    filename: String,
    size: u64,
    digest: [u8; 32],
}

pub(crate) fn create_reference(
    ticket: &BlobTicket,
    filename: &str,
    size: u64,
    digest: [u8; 32],
) -> Result<(String, String)> {
    if size > MAX_TRANSFER_SIZE {
        bail!(
            "file exceeds the {} MiB limit",
            MAX_TRANSFER_SIZE / (1024 * 1024)
        );
    }
    let capability = random_capability();
    let expires_at = now_secs()?.saturating_add(REFERENCE_TTL_SECS);
    let reference = TransferReference {
        ticket: ticket.to_string(),
        capability: capability.clone(),
        expires_at,
        filename: file_transfer::sanitize_filename(filename),
        size,
        digest,
    };
    let bytes = serde_json::to_vec(&reference)?;
    Ok((
        format!("{REFERENCE_PREFIX}{}", hex_encode(&bytes)),
        capability,
    ))
}

pub(crate) fn reference_expiry() -> Result<u64> {
    Ok(now_secs()?.saturating_add(REFERENCE_TTL_SECS))
}

pub(crate) fn retain_grant(
    registry: &mut HashMap<String, TransferGrant>,
    capability: String,
    grant: TransferGrant,
) {
    let now = now_secs().unwrap_or(u64::MAX);
    registry.retain(|_, grant| grant.expires_at >= now);
    if registry.len() >= MAX_ACTIVE_GRANTS
        && let Some(oldest) = registry
            .iter()
            .min_by_key(|(_, grant)| grant.expires_at)
            .map(|(capability, _)| capability.clone())
    {
        registry.remove(&oldest);
    }
    registry.insert(
        capability,
        TransferGrant {
            attempts: 0,
            in_flight: false,
            retry_until: 0,
            ..grant
        },
    );
}

pub(crate) fn acquire_grant(
    registry: &mut HashMap<String, TransferGrant>,
    capability: &str,
) -> Option<TransferGrant> {
    let now = now_secs().ok()?;
    registry.retain(|_, grant| grant.expires_at >= now);
    let grant = registry.get_mut(capability)?;
    if grant.in_flight
        || grant.attempts >= MAX_ATTEMPTS
        || (grant.retry_until != 0 && grant.retry_until < now)
    {
        return None;
    }
    grant.attempts = grant.attempts.saturating_add(1);
    grant.in_flight = true;
    grant.retry_until = now.saturating_add(RETRY_WINDOW_SECS);
    Some(grant.clone())
}

pub(crate) fn complete_grant(
    registry: &mut HashMap<String, TransferGrant>,
    capability: &str,
    size: u64,
    digest: [u8; 32],
) -> bool {
    let Some(grant) = registry.get(capability) else {
        return false;
    };
    if !grant.in_flight || grant.size != size || grant.digest != digest {
        return false;
    }
    registry.remove(capability).is_some()
}

pub(crate) fn fail_grant(registry: &mut HashMap<String, TransferGrant>, capability: &str) {
    let now = now_secs().unwrap_or(u64::MAX);
    let should_remove = registry.get_mut(capability).is_some_and(|grant| {
        grant.in_flight = false;
        grant.attempts >= MAX_ATTEMPTS || grant.retry_until < now
    });
    if should_remove {
        registry.remove(capability);
    }
}

pub(crate) fn parse_reference(value: &str) -> Result<(BlobTicket, String, TransferGrant, String)> {
    if value.len() > REFERENCE_MAX_BYTES {
        bail!("transfer reference is too large");
    }
    let encoded = value
        .strip_prefix(REFERENCE_PREFIX)
        .context("invalid transfer reference prefix")?;
    let bytes = hex_decode(encoded).context("invalid transfer reference encoding")?;
    let reference: TransferReference =
        serde_json::from_slice(&bytes).context("invalid transfer reference payload")?;
    if reference.expires_at < now_secs()? {
        bail!("transfer reference has expired");
    }
    let ticket = reference
        .ticket
        .parse::<BlobTicket>()
        .context("invalid blob ticket in transfer reference")?;
    if reference.size > MAX_TRANSFER_SIZE {
        bail!(
            "transfer exceeds the {} MiB limit",
            MAX_TRANSFER_SIZE / (1024 * 1024)
        );
    }
    Ok((
        ticket.clone(),
        file_transfer::sanitize_filename(&reference.filename),
        TransferGrant {
            hash: ticket.hash(),
            size: reference.size,
            digest: reference.digest,
            expires_at: reference.expires_at,
            attempts: 0,
            in_flight: false,
            retry_until: 0,
        },
        reference.capability,
    ))
}

pub(crate) fn receive_command(reference: &str) -> Result<String> {
    if reference.is_empty() || reference.contains('\'') || reference.contains('\n') {
        bail!("transfer reference cannot be safely placed in a shell command");
    }
    Ok(format!("meow receive '{reference}'"))
}

pub(crate) async fn receive_file(
    endpoint: &Endpoint,
    reference: &str,
    destination: Option<&Path>,
) -> Result<PathBuf> {
    let (ticket, source_filename, grant, capability) = parse_reference(reference)?;
    let destination = resolve_destination(destination, &source_filename)?;
    let temporary = file_transfer::ReceiveTarget::new(&destination)?;
    let connection = tokio::time::timeout(
        Duration::from_secs(TRANSFER_TIMEOUT_SECS),
        endpoint.connect(ticket.addr().clone(), TRANSFER_ALPN),
    )
    .await
    .context("transfer connection timed out")??;
    let (mut send, mut recv) = connection.open_bi().await?;
    write_frame(
        &mut send,
        &TransferRequest {
            capability: capability.clone(),
        },
    )
    .await?;
    let response: TransferResponse = tokio::time::timeout(
        Duration::from_secs(TRANSFER_TIMEOUT_SECS),
        read_frame(&mut recv),
    )
    .await
    .context("transfer response timed out")??;
    if !response.ok {
        bail!("source rejected transfer");
    }
    if response.size != grant.size
        || response.size > MAX_TRANSFER_SIZE
        || response.digest != grant.digest
    {
        bail!("source reported an invalid transfer size");
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(temporary.path())?;
    let mut hasher = blake3::Hasher::new();
    let mut remaining = response.size;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let chunk = tokio::time::timeout(
            Duration::from_secs(TRANSFER_TIMEOUT_SECS),
            recv.read(&mut buffer),
        )
        .await
        .context("transfer read timed out")??
        .unwrap_or(0);
        if chunk == 0 {
            bail!("source ended transfer early");
        }
        let chunk = chunk.min(remaining as usize);
        std::io::Write::write_all(&mut file, &buffer[..chunk])?;
        hasher.update(&buffer[..chunk]);
        remaining -= chunk as u64;
    }
    if hasher.finalize().as_bytes() != &grant.digest {
        bail!("downloaded file failed integrity verification");
    }
    let path = temporary.finish()?;
    write_frame(
        &mut send,
        &TransferComplete {
            capability,
            size: grant.size,
            digest: grant.digest,
        },
    )
    .await?;
    Ok(path)
}

#[derive(Debug, Serialize, Deserialize)]
struct TransferRequest {
    capability: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct TransferResponse {
    ok: bool,
    size: u64,
    digest: [u8; 32],
}

#[derive(Debug)]
pub(crate) struct TransferProtocol {
    store: FsStore,
    registry: Arc<Mutex<HashMap<String, TransferGrant>>>,
}

impl TransferProtocol {
    pub(crate) fn new(
        store: FsStore,
        registry: Arc<Mutex<HashMap<String, TransferGrant>>>,
    ) -> Self {
        Self { store, registry }
    }
}

impl ProtocolHandler for TransferProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let capability = Arc::new(Mutex::new(None::<String>));
        let capability_for_error = capability.clone();
        let result = async {
            let (mut send, mut recv) = connection.accept_bi().await?;
            let request: TransferRequest = read_frame(&mut recv).await?;
            *capability
                .lock()
                .expect("transfer capability mutex poisoned") = Some(request.capability.clone());
            let grant = acquire_grant(
                &mut self.registry.lock().expect("transfer registry poisoned"),
                &request.capability,
            );
            let Some(grant) = grant else {
                bail!("unknown transfer capability");
            };
            if grant.expires_at < now_secs()? || grant.size > MAX_TRANSFER_SIZE {
                bail!("expired or invalid transfer capability");
            }
            let status = self.store.blobs().status(grant.hash).await?;
            let iroh_blobs::api::blobs::BlobStatus::Complete { size } = status else {
                bail!("transfer blob is incomplete");
            };
            if size != grant.size {
                bail!("transfer blob size changed");
            }
            write_frame(
                &mut send,
                &TransferResponse {
                    ok: true,
                    size,
                    digest: grant.digest,
                },
            )
            .await?;
            let reader = self.store.reader(grant.hash);
            tokio::time::timeout(
                Duration::from_secs(TRANSFER_TIMEOUT_SECS),
                tokio::io::copy(&mut reader.take(grant.size), &mut send),
            )
            .await
            .context("transfer timed out")??;
            let completion: TransferComplete = tokio::time::timeout(
                Duration::from_secs(TRANSFER_TIMEOUT_SECS),
                read_frame(&mut recv),
            )
            .await
            .context("transfer completion timed out")??;
            if completion.capability != request.capability
                || !complete_grant(
                    &mut self.registry.lock().expect("transfer registry poisoned"),
                    &request.capability,
                    completion.size,
                    completion.digest,
                )
            {
                bail!("invalid transfer completion");
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(err) = result {
            if let Some(capability) = capability_for_error
                .lock()
                .expect("transfer capability mutex poisoned")
                .as_deref()
            {
                fail_grant(
                    &mut self.registry.lock().expect("transfer registry poisoned"),
                    capability,
                );
            }
            connection.close(1u32.into(), err.to_string().as_bytes());
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TransferComplete {
    capability: String,
    size: u64,
    digest: [u8; 32],
}

pub(crate) fn digest_path(path: &Path) -> Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn random_capability() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

async fn write_frame<T: Serialize>(send: &mut iroh::endpoint::SendStream, value: &T) -> Result<()> {
    let bytes = bincode::serialize(value)?;
    if bytes.len() > 16 * 1024 {
        bail!("transfer frame too large");
    }
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read_frame<T: for<'de> Deserialize<'de>>(
    recv: &mut iroh::endpoint::RecvStream,
) -> Result<T> {
    let mut length = [0u8; 4];
    recv.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > 16 * 1024 {
        bail!("transfer frame too large");
    }
    let mut bytes = vec![0u8; length];
    recv.read_exact(&mut bytes).await?;
    Ok(bincode::deserialize(&bytes)?)
}

fn resolve_destination(destination: Option<&Path>, filename: &str) -> Result<PathBuf> {
    let Some(destination) = destination else {
        return Ok(std::env::current_dir()?.join(filename));
    };
    if destination.is_dir() {
        Ok(destination.join(filename))
    } else {
        Ok(destination.to_path_buf())
    }
}

fn now_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

pub(crate) fn now_secs_for_gc() -> u64 {
    now_secs().unwrap_or(u64::MAX)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    if value.is_empty() || !value.len().is_multiple_of(2) {
        bail!("hex payload has invalid length");
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).context("invalid hex byte"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{EndpointId, SecretKey};
    use iroh_blobs::{BlobFormat, Hash};

    #[test]
    fn hex_round_trips() {
        let bytes = b"hello";
        assert_eq!(hex_decode(&hex_encode(bytes)).unwrap(), bytes);
    }

    #[test]
    fn destination_defaults_to_current_directory() {
        let path = resolve_destination(None, "hello.txt").unwrap();
        assert_eq!(path, std::env::current_dir().unwrap().join("hello.txt"));
    }

    #[test]
    fn receive_command_is_shell_safe() {
        assert_eq!(
            receive_command("meow1-abcd").unwrap(),
            "meow receive 'meow1-abcd'"
        );
        assert!(receive_command("bad'input").is_err());
    }

    #[test]
    fn reference_round_trips() {
        let ticket = BlobTicket::new(
            EndpointId::from(SecretKey::generate().public()).into(),
            Hash::from_bytes([7; 32]),
            BlobFormat::Raw,
        );
        let (reference, capability) =
            create_reference(&ticket, "../hello.txt", 3, [9; 32]).unwrap();
        let (decoded, filename, grant, parsed_capability) = parse_reference(&reference).unwrap();
        assert_eq!(decoded, ticket);
        assert_eq!(filename, "hello.txt");
        assert_eq!(parsed_capability, capability);
        assert_eq!(grant.size, 3);
        assert_eq!(grant.digest, [9; 32]);
    }

    #[test]
    fn grants_are_bounded_and_single_use() {
        let mut registry = HashMap::new();
        let grant = TransferGrant {
            hash: Hash::from_bytes([1; 32]),
            size: 1,
            digest: [2; 32],
            expires_at: u64::MAX,
            attempts: 0,
            in_flight: false,
            retry_until: 0,
        };
        retain_grant(&mut registry, "one".to_string(), grant.clone());
        assert!(registry.contains_key("one"));
        let acquired = acquire_grant(&mut registry, "one").unwrap();
        assert_eq!(acquired.size, grant.size);
        assert!(acquired.in_flight);
        assert!(acquire_grant(&mut registry, "one").is_none());
        fail_grant(&mut registry, "one");
        assert!(registry.get("one").is_some_and(|grant| !grant.in_flight));
        assert!(acquire_grant(&mut registry, "one").is_some());
        assert!(complete_grant(&mut registry, "one", 1, [2; 32]));
        assert!(registry.is_empty());
    }

    #[test]
    fn oversized_reference_is_rejected() {
        let ticket = BlobTicket::new(
            EndpointId::from(SecretKey::generate().public()).into(),
            Hash::from_bytes([7; 32]),
            BlobFormat::Raw,
        );
        assert!(create_reference(&ticket, "file", MAX_TRANSFER_SIZE + 1, [0; 32]).is_err());
    }
}
