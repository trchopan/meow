use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use iroh_blobs::ticket::BlobTicket;
use serde::{Deserialize, Serialize};

use crate::{blob::BlobRuntime, file_transfer};

const REFERENCE_PREFIX: &str = "meow1-";
const REFERENCE_MAX_BYTES: usize = 16 * 1024;
const REFERENCE_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransferReference {
    ticket: String,
    expires_at: u64,
    filename: String,
}

pub(crate) fn create_reference(ticket: &BlobTicket, filename: &str) -> Result<String> {
    let reference = TransferReference {
        ticket: ticket.to_string(),
        expires_at: now_secs()?.saturating_add(REFERENCE_TTL_SECS),
        filename: file_transfer::sanitize_filename(filename),
    };
    let bytes = serde_json::to_vec(&reference)?;
    Ok(format!("{REFERENCE_PREFIX}{}", hex_encode(&bytes)))
}

pub(crate) fn parse_reference(value: &str) -> Result<(BlobTicket, String)> {
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
    Ok((
        ticket,
        file_transfer::sanitize_filename(&reference.filename),
    ))
}

pub(crate) fn receive_command(reference: &str) -> Result<String> {
    if reference.is_empty() || reference.contains('\'') || reference.contains('\n') {
        bail!("transfer reference cannot be safely placed in a shell command");
    }
    Ok(format!("meow receive '{reference}'"))
}

pub(crate) async fn receive_file(
    blob_runtime: &BlobRuntime,
    reference: &str,
    destination: Option<&Path>,
) -> Result<PathBuf> {
    let (ticket, source_filename) = parse_reference(reference)?;
    let destination = resolve_destination(destination, &source_filename)?;
    let temporary = file_transfer::ReceiveTarget::new(&destination)?;
    blob_runtime
        .download_ticket(&ticket, temporary.path().to_path_buf())
        .await
        .context("failed to download file")?;
    temporary.finish()
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
        let reference = create_reference(&ticket, "../hello.txt").unwrap();
        let (decoded, filename) = parse_reference(&reference).unwrap();
        assert_eq!(decoded, ticket);
        assert_eq!(filename, "hello.txt");
    }
}
