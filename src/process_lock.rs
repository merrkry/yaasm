use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub struct ProcessLocks {
    _files: Vec<File>,
}

impl ProcessLocks {
    pub fn acquire(config_dir: &Path, data_dir: &Path) -> Result<Self> {
        Self::acquire_paths(vec![
            config_dir.join(".process.lock"),
            data_dir.join(".process.lock"),
        ])
    }

    pub fn acquire_data(data_dir: &Path) -> Result<Self> {
        Self::acquire_paths(vec![data_dir.join(".process.lock")])
    }

    fn acquire_paths(mut paths: Vec<PathBuf>) -> Result<Self> {
        paths.sort();

        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let file = open_lock(&path)?;
            match file.try_lock() {
                Ok(()) => files.push(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    bail!("another YAASM process holds `{}`", path.display());
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(error)
                        .with_context(|| format!("failed to lock `{}`", path.display()));
                }
            }
        }
        Ok(Self { _files: files })
    }
}

fn open_lock(path: &PathBuf) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open process lock `{}`", path.display()))
}
