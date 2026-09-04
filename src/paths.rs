use std::env;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::reject_parent_components;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve(config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> Result<Self> {
        let current_dir = env::current_dir().context("failed to get current directory")?;
        let home = home_dir()?;
        let (config_dir, data_dir) = match (config_dir, data_dir) {
            (Some(config), Some(data)) => (
                absolute_without_parent(&current_dir, &config)?,
                absolute_without_parent(&current_dir, &data)?,
            ),
            (None, None) => {
                let config_base = env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".config"));
                let data_base = env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/share"));
                (config_base.join("yaasm"), data_base.join("yaasm"))
            }
            _ => bail!("--config-dir and --data-dir must be provided together"),
        };
        reject_overlap(&config_dir, &data_dir)?;
        Ok(Self {
            config_dir,
            data_dir,
        })
    }
}

pub fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

pub fn absolute_without_parent(base: &Path, path: &Path) -> Result<PathBuf> {
    reject_parent_components(path)?;
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Ok(normalize(&joined))
}

pub fn reject_overlap(left: &Path, right: &Path) -> Result<()> {
    if left == right || left.starts_with(right) || right.starts_with(left) {
        bail!(
            "paths must not be equal or nested: `{}` and `{}`",
            left.display(),
            right.display()
        );
    }
    Ok(())
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_nested_roots() {
        assert!(reject_overlap(Path::new("/a"), Path::new("/a/b")).is_err());
    }

    #[test]
    fn resolves_relative_override() {
        let path = absolute_without_parent(Path::new("/work"), Path::new("state")).unwrap();
        assert_eq!(path, Path::new("/work/state"));
    }
}
