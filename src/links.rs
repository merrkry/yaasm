use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

static LINK_ID: AtomicU64 = AtomicU64::new(0);

pub async fn establish_owned_link(link: &Path, destination: &Path) -> Result<()> {
    if let Ok(metadata) = tokio::fs::symlink_metadata(link).await
        && !is_link(&metadata.file_type())
    {
        bail!("YAASM data entry `{}` is not a link", link.display());
    }
    replace_link(link, destination).await
}

pub async fn establish_placement(link: &Path, destination: &Path, data_dir: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(link).await {
        Ok(metadata) => {
            if !is_link(&metadata.file_type()) || !is_managed(link, data_dir).await? {
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
    Ok(target.is_absolute() && target.starts_with(data_dir))
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

#[cfg(unix)]
async fn read_link(link: &Path) -> Result<Option<PathBuf>> {
    match tokio::fs::read_link(link).await {
        Ok(target) => Ok(Some(target)),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed to read symbolic link"),
    }
}

#[cfg(windows)]
async fn read_link(link: &Path) -> Result<Option<PathBuf>> {
    let owned = link.to_owned();
    match tokio::task::spawn_blocking(move || junction::get_target(owned)).await? {
        Ok(target) => Ok(Some(target)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).context("failed to read junction"),
    }
}

#[cfg(unix)]
async fn remove_link(link: &Path) -> Result<()> {
    tokio::fs::remove_file(link)
        .await
        .with_context(|| format!("failed to remove link `{}`", link.display()))
}

#[cfg(windows)]
async fn remove_link(link: &Path) -> Result<()> {
    let owned = link.to_owned();
    tokio::task::spawn_blocking(move || junction::delete(owned))
        .await
        .context("junction task failed")??;
    Ok(())
}

fn is_link(file_type: &std::fs::FileType) -> bool {
    #[cfg(unix)]
    return file_type.is_symlink();
    #[cfg(windows)]
    return file_type.is_symlink() || file_type.is_dir();
}
