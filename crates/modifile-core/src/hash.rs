use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Context, Result};

/// SHA-256, lowercase hex. This is the store key, the lockfile pin, and the
/// subject digest an attestation is looked up by, so it is deliberately the
/// same algorithm GitHub uses for release asset digests.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).ctx(format!("hashing {}", path.display()))?;
    let mut reader = std::io::BufReader::with_capacity(128 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// GitHub reports asset digests as `sha256:abc...`; lockfiles store the bare
/// hex. Accept either spelling.
pub fn normalize_digest(raw: &str) -> String {
    raw.trim()
        .strip_prefix("sha256:")
        .unwrap_or(raw.trim())
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn strips_prefix_and_case() {
        assert_eq!(normalize_digest("sha256:ABC123"), "abc123");
        assert_eq!(normalize_digest(" abc123 "), "abc123");
    }
}
