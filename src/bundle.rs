use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::{Config, LOCAL_SOURCE};
use crate::lockfile::{LockedSource, Lockfile};
use crate::source::{MaterializedSource, SourceMaterializer};

const VERSION: u32 = 1;
static STAGING_ID: AtomicU64 = AtomicU64::new(0);

struct BundleStaging(std::path::PathBuf);

impl Drop for BundleStaging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    version: u32,
    sources: BTreeMap<String, BundleSource>,
    skills: BTreeMap<String, RemoteSkill>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BundleSource {
    locked: LockedSource,
    files: BTreeMap<String, BundleFile>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BundleFile {
    kind: FileKind,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum FileKind {
    File,
    Executable,
    Symlink,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RemoteSkill {
    source: String,
    path: String,
}

fn remote_skills(config: &Config) -> Result<BTreeMap<String, RemoteSkill>> {
    let mut skills = BTreeMap::new();
    for (name, skill) in &config.skills {
        if skill.source == LOCAL_SOURCE {
            continue;
        }
        if !config.sources.contains_key(&skill.source) {
            bail!(
                "skill `{name}` references unknown source `{}`",
                skill.source
            );
        }
        let path = skill
            .path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect::<Vec<_>>()
            .join("/");
        if !path.is_empty() {
            validate_path(&path)?;
        }
        skills.insert(
            name.clone(),
            RemoteSkill {
                source: skill.source.clone(),
                path,
            },
        );
    }
    Ok(skills)
}

impl Bundle {
    pub async fn build(
        output: &Path,
        config: &Config,
        lock: &Lockfile,
        materializer: &SourceMaterializer,
    ) -> Result<()> {
        match tokio::fs::symlink_metadata(output).await {
            Ok(_) => bail!("bundle output already exists: `{}`", output.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to inspect bundle output"),
        }
        let mut bundle = Self {
            version: VERSION,
            sources: BTreeMap::new(),
            skills: remote_skills(config)?,
        };
        let parent = output.parent().context("bundle output has no parent")?;
        // A sibling staging directory keeps publication on the same filesystem.
        let id = STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(".yaasm-bundle-{}-{id}", std::process::id()));
        tokio::fs::create_dir(&staging)
            .await
            .context("failed to create bundle staging directory")?;
        let staging = BundleStaging(staging);
        bundle
            .build_contents(&staging.0, config, lock, materializer)
            .await?;
        tokio::fs::rename(&staging.0, output)
            .await
            .context("failed to publish bundle")?;
        Ok(())
    }

    async fn build_contents(
        &mut self,
        staging: &Path,
        config: &Config,
        lock: &Lockfile,
        materializer: &SourceMaterializer,
    ) -> Result<()> {
        tokio::fs::create_dir(staging.join("objects")).await?;
        #[cfg(unix)]
        {
            set_mode(staging, 0o755).await?;
            set_mode(&staging.join("objects"), 0o755).await?;
        }
        for (name, source) in &config.sources {
            let locked = lock.sources[name].clone();
            let paths: Vec<_> = self
                .skills
                .values()
                .filter(|skill| &skill.source == name)
                .map(|skill| {
                    if skill.path.is_empty() {
                        ".".into()
                    } else {
                        skill.path.clone()
                    }
                })
                .collect();
            let mut files = BTreeMap::new();
            if !paths.is_empty() {
                let tracked = materializer
                    .export_locked_tree(
                        name,
                        source,
                        &locked.revision,
                        &paths,
                        &staging.join("objects"),
                    )
                    .await
                    .with_context(|| format!("source `{name}`"))?;
                for file in tracked {
                    validate_path(&file.path)?;
                    let kind = match file.mode.as_str() {
                        "100644" => FileKind::File,
                        "100755" => FileKind::Executable,
                        "120000" => FileKind::Symlink,
                        _ => bail!("unsupported Git file mode {}", file.mode),
                    };
                    let sha256 = file.sha256;
                    #[cfg(unix)]
                    set_mode(&staging.join("objects").join(&sha256), 0o644).await?;
                    files.insert(file.path, BundleFile { kind, sha256 });
                }
            }
            let source = BundleSource { locked, files };
            self.validate_source(name, &source)?;
            self.sources.insert(name.clone(), source);
        }
        tokio::fs::write(staging.join("bundle.toml"), toml::to_string_pretty(self)?).await?;
        #[cfg(unix)]
        set_mode(&staging.join("bundle.toml"), 0o644).await?;
        Ok(())
    }

    pub async fn load(root: &Path, config: &Config, lock: &Lockfile) -> Result<Self> {
        let input = tokio::fs::read_to_string(root.join("bundle.toml"))
            .await
            .context("failed to read bundle.toml")?;
        let bundle: Self = toml::from_str(&input).context("invalid bundle manifest")?;
        if bundle.version != VERSION {
            bail!("unsupported bundle version {}", bundle.version);
        }
        let identities: BTreeMap<_, _> = bundle
            .sources
            .iter()
            .map(|(name, source)| (name, &source.locked))
            .collect();
        let expected: BTreeMap<_, _> = lock.sources.iter().collect();
        if identities != expected || bundle.skills != remote_skills(config)? {
            bail!("bundle does not match remote configuration and lockfile");
        }
        Ok(bundle)
    }

    fn validate_source(&self, name: &str, source: &BundleSource) -> Result<()> {
        let mut folded = BTreeMap::new();
        for (path, file) in &source.files {
            validate_path(path)?;
            if file.sha256.len() != 64
                || !file
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                bail!("invalid bundle object digest for `{path}`");
            }
            let mut prefix = String::new();
            for part in path.split('/') {
                if !prefix.is_empty() {
                    if source.files.contains_key(&prefix) {
                        bail!("bundle file is also a parent directory: `{prefix}`");
                    }
                    prefix.push('/');
                }
                prefix.push_str(part);
                if let Some(previous) = folded.insert(prefix.to_ascii_lowercase(), prefix.clone())
                    && previous != prefix
                {
                    bail!(
                        "bundle paths conflict under ASCII case folding: `{previous}` and `{prefix}`"
                    );
                }
            }
        }
        for skill in self.skills.values().filter(|skill| skill.source == name) {
            let manifest = if skill.path.is_empty() {
                "SKILL.md".into()
            } else {
                format!("{}/SKILL.md", skill.path)
            };
            match source.files.get(&manifest) {
                Some(file) if file.kind != FileKind::Symlink => {}
                _ => bail!("bundle skill `{}` has no regular SKILL.md", skill.path),
            }
        }
        Ok(())
    }

    pub fn source_names(&self) -> impl Iterator<Item = &String> {
        self.sources.keys()
    }

    pub async fn import_source(
        &self,
        root: &Path,
        data: &Path,
        name: &str,
    ) -> Result<MaterializedSource> {
        let source = &self.sources[name];
        self.validate_source(name, source)?;
        #[cfg(windows)]
        if let Some((path, _)) = source
            .files
            .iter()
            .find(|(_, file)| file.kind == FileKind::Symlink)
        {
            bail!(
                "upstream symlink `{path}` cannot be restored on Windows; bundle source was not imported"
            );
        }
        if source.files.is_empty() {
            return Ok(MaterializedSource {
                revision: source.locked.revision.clone(),
                snapshot: None,
            });
        }
        let identity = digest(toml::to_string(source)?.as_bytes());
        let destination = data.join("snapshots").join(format!("bundle-{identity}"));
        let staging = data.join(".staging").join(format!("bundle-{name}"));
        let imported = tokio::fs::try_exists(&destination).await?;
        if !imported {
            tokio::fs::create_dir(&staging).await?;
        }
        // Read and hash every referenced object even when a previous import exists.
        // A damaged bundle must not be hidden by a warm data directory.
        let mut buffer = vec![0; 64 * 1024];
        for (path, file) in &source.files {
            let object = root.join("objects").join(&file.sha256);
            let metadata = tokio::fs::symlink_metadata(&object)
                .await
                .with_context(|| format!("missing bundle object for `{path}`"))?;
            if !metadata.file_type().is_file() {
                bail!("bundle object for `{path}` is not a regular file");
            }
            let output = if imported {
                None
            } else {
                Some(prepare_file_path(&staging, path).await?)
            };
            let mut restored = match &output {
                Some(output) if file.kind != FileKind::Symlink => Some(
                    tokio::fs::File::create_new(output)
                        .await
                        .with_context(|| format!("failed to restore bundle file `{path}`"))?,
                ),
                _ => None,
            };
            let mut contents = Vec::new();
            let mut input = tokio::fs::File::open(&object).await?;
            let mut hasher = Sha256::new();
            loop {
                let count = input.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                let chunk = &buffer[..count];
                hasher.update(chunk);
                if let Some(restored) = &mut restored {
                    restored.write_all(chunk).await?;
                } else if !imported && file.kind == FileKind::Symlink {
                    contents.extend_from_slice(chunk);
                }
            }
            if hex::encode(hasher.finalize()) != file.sha256 {
                bail!("bundle integrity check failed for `{path}`");
            }
            if let Some(mut restored) = restored {
                restored.flush().await?;
            }
            let Some(output) = output else {
                continue;
            };
            match file.kind {
                FileKind::Symlink => restore_symlink(&contents, &output).await?,
                FileKind::File | FileKind::Executable => {
                    #[cfg(unix)]
                    {
                        let mode = if file.kind == FileKind::Executable {
                            0o755
                        } else {
                            0o644
                        };
                        set_mode(&output, mode).await?;
                    }
                }
            }
        }
        if !imported {
            tokio::fs::rename(&staging, &destination).await?;
        }
        Ok(MaterializedSource {
            revision: source.locked.revision.clone(),
            snapshot: Some(destination),
        })
    }
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

async fn prepare_file_path(root: &Path, path: &str) -> Result<PathBuf> {
    let relative = Path::new(path);
    let mut parent = root.to_owned();
    for component in relative
        .parent()
        .context("bundle file has no parent")?
        .components()
    {
        parent.push(component);
        match tokio::fs::symlink_metadata(&parent).await {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => bail!("bundle parent is not a directory: `{}`", parent.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&parent).await?;
            }
            Err(error) => return Err(error).context("failed to inspect bundle parent"),
        }
    }
    Ok(root.join(relative))
}

#[cfg(unix)]
async fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    for part in path.split('/') {
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'));
        if part.is_empty()
            || matches!(part, "." | "..")
            || part.ends_with(['.', ' '])
            || reserved
            || part
                .chars()
                .any(|character| character.is_control() || "<>:\"\\|?*".contains(character))
        {
            bail!("bundle path is not portable: `{path}`");
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn restore_symlink(contents: &[u8], output: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStringExt;
    let target = std::ffi::OsString::from_vec(contents.to_vec());
    tokio::fs::symlink(PathBuf::from(target), output)
        .await
        .context("failed to restore upstream symlink")
}

#[cfg(windows)]
async fn restore_symlink(_contents: &[u8], output: &Path) -> Result<()> {
    bail!(
        "upstream symlink `{}` cannot be restored on Windows; bundle source was not imported",
        output.display()
    )
}
