use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::paths::simplified;
use anyhow::{Context, Result, bail};

static LINK_ID: AtomicU64 = AtomicU64::new(0);

pub async fn establish_owned_link(link: &Path, destination: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(link).await {
        Ok(_) if read_link(link).await?.is_none() => {
            bail!("YAASM data entry `{}` is not a link", link.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect data entry"),
    }
    replace_link(link, destination).await
}

pub async fn establish_placement(link: &Path, destination: &Path, data_dir: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(link).await {
        Ok(_) => {
            if !is_managed(link, data_dir).await? {
                bail!("target entry `{}` is not managed by YAASM", link.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect target entry"),
    }
    replace_link(link, destination).await
}

pub async fn clean_target(
    target: &Path,
    desired: &BTreeSet<String>,
    data_dir: &Path,
) -> Result<()> {
    let mut entries = tokio::fs::read_dir(target)
        .await
        .with_context(|| format!("failed to scan target `{}`", target.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if !desired.contains(name_text.as_ref()) && is_managed(&entry.path(), data_dir).await? {
            remove_link(&entry.path()).await?;
        }
    }
    Ok(())
}

pub async fn is_managed(link: &Path, data_dir: &Path) -> Result<bool> {
    let Some(target) = read_link(link).await? else {
        return Ok(false);
    };
    Ok(target.is_absolute() && simplified(&target).starts_with(simplified(data_dir)))
}

pub(crate) async fn link_destination(link: &Path) -> Result<Option<PathBuf>> {
    read_link(link).await
}

async fn replace_link(link: &Path, destination: &Path) -> Result<()> {
    let parent = link.parent().context("link has no parent directory")?;
    tokio::fs::create_dir_all(parent).await?;
    let id = LINK_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".yaasm-link-{}-{id}", std::process::id()));
    create_link(destination.to_path_buf(), temporary.clone()).await?;
    #[cfg(windows)]
    if tokio::fs::symlink_metadata(link).await.is_ok() {
        remove_link(link).await?;
    }
    if let Err(error) = tokio::fs::rename(&temporary, link).await {
        let _ = remove_link(&temporary).await;
        return Err(error).with_context(|| format!("failed to publish link `{}`", link.display()));
    }
    Ok(())
}

#[cfg(unix)]
async fn create_link(destination: PathBuf, link: PathBuf) -> Result<()> {
    tokio::fs::symlink(destination, link)
        .await
        .context("failed to create symbolic link")
}

#[cfg(windows)]
async fn create_link(destination: PathBuf, link: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || junction::create(destination, link))
        .await
        .context("junction task failed")??;
    Ok(())
}

async fn read_link(link: &Path) -> Result<Option<PathBuf>> {
    #[cfg(windows)]
    const ERROR_NOT_A_REPARSE_POINT: i32 = 4390;

    match tokio::fs::read_link(link).await {
        Ok(target) => Ok(Some(target)),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        #[cfg(windows)]
        Err(error) if error.raw_os_error() == Some(ERROR_NOT_A_REPARSE_POINT) => Ok(None),
        Err(error) => Err(error).context("failed to read link"),
    }
}

#[cfg(unix)]
pub(crate) async fn remove_link(link: &Path) -> Result<()> {
    tokio::fs::remove_file(link)
        .await
        .with_context(|| format!("failed to remove link `{}`", link.display()))
}

#[cfg(windows)]
pub(crate) async fn remove_link(link: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    let metadata = tokio::fs::symlink_metadata(link).await?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
        // RemoveDirectory removes the junction itself without changing its target.
        tokio::fs::remove_dir(link).await?;
    } else {
        tokio::fs::remove_file(link).await?;
    }
    Ok(())
}
