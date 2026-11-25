#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

/// Generates a 256-bit CSPRNG session token encoded as 64 lowercase hexadecimal characters.
pub fn generate_session_token() -> anyhow::Result<String> {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw)
        .map_err(|err| anyhow::anyhow!("CSPRNG failure generating session token: {err}"))?;
    let mut hex = String::with_capacity(64);
    for byte in raw {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Constant-time byte comparison to prevent timing side-channels when validating bearer tokens.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn default_connection_file() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_STATE_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("ferrite-e2e").join("daemon-connection.json")
}

pub fn default_snapshots_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_STATE_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("ferrite-e2e").join("snapshots")
}

pub fn write_connection_file(path: &Path, info: &ConnectionInfo) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let payload = serde_json::to_vec_pretty(info)?;
    fs::write(path, payload)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn read_connection_file(path: &Path) -> anyhow::Result<ConnectionInfo> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_unique_tokens_and_compares_in_constant_time() {
        let t1 = generate_session_token().unwrap();
        let t2 = generate_session_token().unwrap();
        assert_eq!(t1.len(), 64);
        assert_ne!(t1, t2);
        assert!(constant_time_eq(t1.as_bytes(), t1.as_bytes()));
        assert!(!constant_time_eq(t1.as_bytes(), t2.as_bytes()));
        assert!(!constant_time_eq(b"short", b"longer"));
    }
}
