use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEME: &str = "windows-dpapi-current-user";
const CURRENT_VERSION: u8 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProtectedBlob {
    pub version: u8,
    pub scheme: String,
    pub blob: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnprotectError {
    Malformed,
    Unavailable,
}

impl std::fmt::Display for UnprotectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("protected data is malformed"),
            Self::Unavailable => f.write_str("protected data is unavailable for this Windows user"),
        }
    }
}

impl std::error::Error for UnprotectError {}

fn entropy_blob(purpose: &[u8]) -> windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
    windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
        cbData: purpose.len() as u32,
        pbData: purpose.as_ptr() as *mut u8,
    }
}

fn dpapi(input: &[u8], purpose: &[u8], protect: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let entropy = entropy_blob(purpose);
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        if protect {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(anyhow!(
            "Windows DPAPI operation failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as _) };
    Ok(bytes)
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn validate_checksum(
    value: &ProtectedBlob,
    ciphertext: &[u8],
) -> std::result::Result<(), UnprotectError> {
    let Some(expected) = &value.sha256 else {
        return if value.version == 1 {
            Ok(())
        } else {
            Err(UnprotectError::Malformed)
        };
    };
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UnprotectError::Malformed);
    }
    if !expected.eq_ignore_ascii_case(&checksum(ciphertext)) {
        return Err(UnprotectError::Malformed);
    }
    Ok(())
}

pub fn protect(purpose: &[u8], plaintext: &[u8]) -> Result<ProtectedBlob> {
    let protected = dpapi(plaintext, purpose, true)?;
    Ok(ProtectedBlob {
        version: CURRENT_VERSION,
        scheme: SCHEME.to_string(),
        sha256: Some(checksum(&protected)),
        blob: STANDARD.encode(protected),
    })
}

pub fn unprotect(
    purpose: &[u8],
    value: &ProtectedBlob,
) -> std::result::Result<Vec<u8>, UnprotectError> {
    if !matches!(value.version, 1 | CURRENT_VERSION) || value.scheme != SCHEME {
        return Err(UnprotectError::Malformed);
    }
    let ciphertext = STANDARD
        .decode(&value.blob)
        .map_err(|_| UnprotectError::Malformed)?;
    validate_checksum(value, &ciphertext)?;
    dpapi(&ciphertext, purpose, false).map_err(|_| UnprotectError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_envelope_versions() {
        let value = ProtectedBlob {
            version: 3,
            scheme: SCHEME.to_string(),
            blob: String::new(),
            sha256: None,
        };
        assert_eq!(unprotect(b"test", &value), Err(UnprotectError::Malformed));
    }

    #[test]
    fn rejects_tampered_ciphertext_checksum() {
        let mut value = protect(b"test", b"value").unwrap();
        value.sha256 = Some("0".repeat(64));
        assert_eq!(unprotect(b"test", &value), Err(UnprotectError::Malformed));
    }
}
