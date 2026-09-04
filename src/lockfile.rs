use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{SourceConfig, SourceKind};

pub const LOCK_VERSION: u32 = 1;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    #[serde(default = "lock_version")]
    pub version: u32,
    #[serde(default)]
    pub sources: BTreeMap<String, LockedSource>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedSource {
    pub fingerprint: String,
    pub revision: String,
}

const fn lock_version() -> u32 {
    LOCK_VERSION
}

impl Lockfile {
    pub async fn load_if_exists(path: &Path) -> Result<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(input) => {
                let lock: Self = toml::from_str(&input)
                    .with_context(|| format!("failed to parse lockfile {}", path.display()))?;
                if lock.version != LOCK_VERSION {
                    bail!(
                        "unsupported lockfile version {}, expected {}",
                        lock.version,
                        LOCK_VERSION
                    );
                }
                Ok(lock)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                version: LOCK_VERSION,
                sources: BTreeMap::new(),
            }),
            Err(error) => {
                Err(error).with_context(|| format!("failed to read lockfile {}", path.display()))
            }
        }
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("failed to serialize lockfile")
    }
}

pub fn fingerprint(source: &SourceConfig) -> Result<String> {
    let (kind, repository) = source.kind_and_repository()?;
    let kind = match kind {
        SourceKind::Git => b"git".as_slice(),
        SourceKind::Github => b"github".as_slice(),
    };

    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"yaasm-source-fingerprint-v1");
    hash_field(&mut hasher, kind);
    hash_field(&mut hasher, repository.as_bytes());
    match source.reference.as_deref() {
        Some(reference) => {
            hash_field(&mut hasher, b"ref-present");
            hash_field(&mut hasher, reference.as_bytes());
        }
        None => hash_field(&mut hasher, b"ref-absent"),
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(reference: Option<&str>) -> SourceConfig {
        SourceConfig {
            github: Some("owner/repo".into()),
            git: None,
            reference: reference.map(str::to_owned),
        }
    }

    #[test]
    fn fingerprint_distinguishes_absent_and_present_ref() {
        assert_ne!(
            fingerprint(&source(None)).unwrap(),
            fingerprint(&source(Some("main"))).unwrap()
        );
    }

    #[test]
    fn lock_output_is_sorted() {
        let mut lock = Lockfile {
            version: LOCK_VERSION,
            sources: BTreeMap::new(),
        };
        lock.sources.insert(
            "z".into(),
            LockedSource {
                fingerprint: "z".into(),
                revision: "1".into(),
            },
        );
        lock.sources.insert(
            "a".into(),
            LockedSource {
                fingerprint: "a".into(),
                revision: "2".into(),
            },
        );
        let output = lock.to_toml().unwrap();
        assert!(output.find("[sources.a]").unwrap() < output.find("[sources.z]").unwrap());
    }
}
