// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    Standard,
    Full,
}

impl VerifyMode {
    pub fn from_env() -> Self {
        Self::from_knobs(&oj_env::get().knobs)
    }

    pub fn from_knobs(knobs: &oj_env::Knobs) -> Self {
        if knobs.cache_verify_full {
            VerifyMode::Full
        } else {
            VerifyMode::Standard
        }
    }
}

/// Serialized as `{"size":..,"hash":..}`, the on-disk manifest record shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedFile {
    pub size: u64,
    pub hash: String,
}

impl ExpectedFile {
    pub(crate) fn of(bytes: &[u8]) -> Self {
        Self {
            size: bytes.len() as u64,
            hash: blake3::hash(bytes).to_hex().to_string(),
        }
    }
}

#[derive(Debug)]
pub enum VerifyError {
    Io(io::Error),
    WrongSize { expected: u64, actual: u64 },
    WrongHash { expected: String, actual: String },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::Io(e) => write!(f, "io: {e}"),
            VerifyError::WrongSize { expected, actual } => {
                write!(f, "size mismatch: expected {expected} B, found {actual} B")
            }
            VerifyError::WrongHash { expected, actual } => {
                write!(
                    f,
                    "hash mismatch: expected {}…, found {}…",
                    expected.get(..12).unwrap_or(expected),
                    actual.get(..12).unwrap_or(actual)
                )
            }
        }
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic_write needs a parent dir",
        )
    })?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    write_via_tmp(&tmp, path, bytes)
}

/// Writes `bytes` to `tmp`, then renames it over `path`, so readers see the
/// old file or the new one, never a partial write. `tmp` is removed on failure.
pub(crate) fn write_via_tmp(tmp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let result = fs::write(tmp, bytes).and_then(|()| fs::rename(tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// Parses a JSON cache file. An unparseable file is removed, so a corrupt
/// entry is a miss once and then gone, never served.
pub(crate) fn read_json_or_remove<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    parse_json_or_remove(path, &bytes)
}

pub(crate) fn parse_json_or_remove<T: DeserializeOwned>(path: &Path, bytes: &[u8]) -> Option<T> {
    match serde_json::from_slice(bytes) {
        Ok(value) => Some(value),
        Err(_) => {
            let _ = fs::remove_file(path);
            None
        }
    }
}

pub fn verify_file(
    path: &Path,
    expected: &ExpectedFile,
    mode: VerifyMode,
) -> Result<(), VerifyError> {
    match mode {
        VerifyMode::Standard => {
            let meta = fs::metadata(path).map_err(VerifyError::Io)?;
            if meta.len() != expected.size {
                return Err(VerifyError::WrongSize {
                    expected: expected.size,
                    actual: meta.len(),
                });
            }
            Ok(())
        }
        VerifyMode::Full => verified_read(path, expected, mode).map(|_| ()),
    }
}

pub fn verified_read(
    path: &Path,
    expected: &ExpectedFile,
    mode: VerifyMode,
) -> Result<Vec<u8>, VerifyError> {
    let bytes = fs::read(path).map_err(VerifyError::Io)?;
    if bytes.len() as u64 != expected.size {
        return Err(VerifyError::WrongSize {
            expected: expected.size,
            actual: bytes.len() as u64,
        });
    }
    if mode == VerifyMode::Full {
        let actual = blake3::hash(&bytes).to_hex().to_string();
        if actual != expected.hash {
            return Err(VerifyError::WrongHash {
                expected: expected.hash.clone(),
                actual,
            });
        }
    }
    Ok(bytes)
}

pub fn write_self_verified(path: &Path, payload: &[u8]) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(payload.len() + 65);
    bytes.extend_from_slice(blake3::hash(payload).to_hex().as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(payload);
    atomic_write(path, &bytes)
}

pub fn read_self_verified(path: &Path) -> Result<Vec<u8>, VerifyError> {
    let bytes = fs::read(path).map_err(VerifyError::Io)?;
    let Some((header, payload)) = bytes.split_at_checked(65).filter(|(h, _)| h[64] == b'\n') else {
        return Err(VerifyError::WrongSize {
            expected: 65,
            actual: bytes.len() as u64,
        });
    };
    let expected = String::from_utf8_lossy(&header[..64]).into_owned();
    let actual = blake3::hash(payload).to_hex().to_string();
    if actual != expected {
        return Err(VerifyError::WrongHash { expected, actual });
    }
    Ok(payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oj-integrity-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn expected_for(bytes: &[u8]) -> ExpectedFile {
        ExpectedFile::of(bytes)
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let d = tmp("atomic");
        let p = d.join("out.json");
        atomic_write(&p, b"{}").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"{}");
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn verified_read_returns_bytes_on_match() {
        let d = tmp("ok");
        let p = d.join("blob");
        fs::write(&p, b"hello").unwrap();
        let exp = expected_for(b"hello");
        assert_eq!(
            verified_read(&p, &exp, VerifyMode::Standard).unwrap(),
            b"hello"
        );
        assert_eq!(verified_read(&p, &exp, VerifyMode::Full).unwrap(), b"hello");
    }

    #[test]
    fn wrong_size_fails_in_both_modes() {
        let d = tmp("size");
        let p = d.join("blob");
        fs::write(&p, b"hello!").unwrap();
        let exp = expected_for(b"hello");
        assert!(matches!(
            verified_read(&p, &exp, VerifyMode::Standard),
            Err(VerifyError::WrongSize {
                expected: 5,
                actual: 6
            })
        ));
        assert!(verify_file(&p, &exp, VerifyMode::Standard).is_err());
    }

    #[test]
    fn same_size_corruption_needs_full_mode() {
        let d = tmp("flip");
        let p = d.join("blob");
        fs::write(&p, b"hellX").unwrap();
        let exp = expected_for(b"hello");
        assert!(verified_read(&p, &exp, VerifyMode::Standard).is_ok());
        assert!(verify_file(&p, &exp, VerifyMode::Standard).is_ok());
        assert!(matches!(
            verified_read(&p, &exp, VerifyMode::Full),
            Err(VerifyError::WrongHash { .. })
        ));
        assert!(verify_file(&p, &exp, VerifyMode::Full).is_err());
    }

    #[test]
    fn missing_file_is_io_error() {
        let d = tmp("missing");
        let exp = expected_for(b"hello");
        assert!(matches!(
            verify_file(&d.join("nope"), &exp, VerifyMode::Standard),
            Err(VerifyError::Io(_))
        ));
    }

    #[test]
    fn self_verified_roundtrip_and_flip_detection() {
        let d = tmp("selfv");
        let p = d.join("entry.json");
        write_self_verified(&p, b"{\"a\":1}").unwrap();
        assert_eq!(read_self_verified(&p).unwrap(), b"{\"a\":1}");

        // Same-size, single-byte flip in the payload must be caught.
        let mut bytes = fs::read(&p).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&p, &bytes).unwrap();
        assert!(matches!(
            read_self_verified(&p),
            Err(VerifyError::WrongHash { .. })
        ));

        // Truncation below the header is a framing failure, not a panic.
        fs::write(&p, b"short").unwrap();
        assert!(read_self_verified(&p).is_err());
    }
}
