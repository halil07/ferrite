#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Normalized compact 16-hex-character SHA-256 fingerprint independent of CSS selectors or DOM node IDs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ElementFingerprint(pub String);

impl ElementFingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Normalizes accessible names by trimming and collapsing whitespace, preserving semantic meaning.
pub fn normalize_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Computes a deterministic CSS-independent fingerprint from `(frame_id, semantic_path, role, normalized_name, nth)`.
pub fn compute_fingerprint(
    frame_id: &str,
    semantic_path: &[String],
    role: &str,
    name: &str,
    nth: usize,
) -> ElementFingerprint {
    let mut hasher = Sha256::new();
    hasher.update(frame_id.as_bytes());
    hasher.update(b"|");
    hasher.update(semantic_path.join("/").as_bytes());
    hasher.update(b"|");
    hasher.update(role.to_ascii_lowercase().as_bytes());
    hasher.update(b"|");
    hasher.update(normalize_name(name).to_ascii_lowercase().as_bytes());
    hasher.update(b"|");
    hasher.update(nth.to_string().as_bytes());

    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    ElementFingerprint(hex)
}
