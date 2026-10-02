use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    #[default]
    Yahoo,
    Fmp,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Yahoo => "Yahoo Finance",
            Self::Fmp => "FMP",
        }
    }
    pub fn index(self) -> i32 {
        if self == Self::Fmp {
            1
        } else {
            0
        }
    }
}

// Deliberately does not implement Debug: API keys must never enter logs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackendSettings {
    pub provider: Provider,
    pub enabled: bool,
    pub api_keys: BTreeMap<String, String>,
    pub symbol_aliases: BTreeMap<String, String>,
}

impl Default for BackendSettings {
    fn default() -> Self {
        Self {
            provider: Provider::Yahoo,
            enabled: true,
            api_keys: BTreeMap::new(),
            symbol_aliases: BTreeMap::new(),
        }
    }
}

pub async fn load(path: &Path) -> Result<BackendSettings> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BackendSettings::default())
        }
        Err(error) => return Err(error.into()),
    };
    let plain = tokio::task::spawn_blocking(move || protect(&bytes, false)).await??;
    serde_json::from_slice(&plain).context("Cannot read backend settings")
}

pub async fn save(path: &Path, settings: &BackendSettings) -> Result<()> {
    let plain = serde_json::to_vec(settings)?;
    let encrypted = tokio::task::spawn_blocking(move || protect(&plain, true)).await??;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = path.with_extension("tmp");
    tokio::fs::write(&temporary, encrypted).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

#[cfg(windows)]
fn protect(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into()?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // DPAPI protects data for the current Windows user; UI prompts are forbidden.
    unsafe {
        let ok = if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        anyhow::ensure!(
            ok != 0,
            "Windows could not {} backend settings",
            if encrypt { "protect" } else { "unlock" }
        );
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData as *mut std::ffi::c_void);
        Ok(result)
    }
}

#[cfg(not(windows))]
fn protect(bytes: &[u8], _encrypt: bool) -> Result<Vec<u8>> {
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_round_trip() {
        let data = br#"{"api_keys":{"fmp":"test-key"}}"#;
        let encrypted = protect(data, true).unwrap();
        #[cfg(windows)]
        assert!(!encrypted.windows(8).any(|part| part == b"test-key"));
        assert_eq!(protect(&encrypted, false).unwrap(), data);
    }
}
