use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::collections::{CandidateDir, discover_remote_skills_from_bundle, normalize_remote_rule};
use crate::config::{CollectionConfig, Config, LOCAL_SOURCE, validate_named_collection};
use crate::lockfile::{LockedSource, Lockfile};
use crate::source::{MaterializedSource, SourceMaterializer};

const VERSION: u32 = 2;
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
    selection_fingerprint: String,
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteSkill {
    pub source: String,
    pub path: String,
}

fn explicit_remote_skills(config: &Config) -> Result<BTreeMap<String, RemoteSkill>> {
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

fn selection_fingerprint(
    config: &Config,
    explicit_skills: &BTreeMap<String, RemoteSkill>,
) -> Result<String> {
    let mut canonical_collections = Vec::new();
    for collection in &config.collections {
        if collection.source == LOCAL_SOURCE {
            continue;
        }
        if !config.sources.contains_key(&collection.source) {
            bail!(
                "collection references unknown source `{}`",
                collection.source
            );
        }
        // Rule order inside `paths` is significant because `!` negations apply in order.
        let mut rules = Vec::with_capacity(collection.paths.len());
        for raw in &collection.paths {
            let rule = normalize_remote_rule(raw)?;
            rules.push((rule.negated, rule.pattern));
        }
        canonical_collections.push((collection.source.clone(), rules));
    }
    canonical_collections.sort();

    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"yaasm-bundle-selection-v1");
    hash_len(&mut hasher, explicit_skills.len());
    for (name, skill) in explicit_skills {
        hash_field(&mut hasher, name.as_bytes());
        hash_field(&mut hasher, skill.source.as_bytes());
        hash_field(&mut hasher, skill.path.as_bytes());
    }
    hash_len(&mut hasher, canonical_collections.len());
    for (source, rules) in &canonical_collections {
        hash_field(&mut hasher, source.as_bytes());
        hash_len(&mut hasher, rules.len());
        for (negated, pattern) in rules {
            hash_field(&mut hasher, if *negated { b"negated" } else { b"positive" });
            hash_field(&mut hasher, pattern.as_bytes());
        }
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
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
        let explicit = explicit_remote_skills(config)?;
        let fingerprint = selection_fingerprint(config, &explicit)?;
        let mut bundle = Self {
            version: VERSION,
            selection_fingerprint: fingerprint,
            sources: BTreeMap::new(),
            skills: explicit,
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
        let explicit_folded: BTreeMap<String, String> = self
            .skills
            .keys()
            .map(|name| (name.to_ascii_lowercase(), name.clone()))
            .collect();
        let mut all_folded = explicit_folded.clone();

        for (name, source) in &config.sources {
            let locked = lock.sources[name].clone();
            let explicit_paths: Vec<_> = self
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
            let source_collections: Vec<CollectionConfig> = config
                .collections
                .iter()
                .filter(|collection| &collection.source == name)
                .cloned()
                .collect();

            let (discovered, tracked) = materializer
                .export_locked_source(
                    name,
                    source,
                    &locked.revision,
                    &explicit_paths,
                    &source_collections,
                    &explicit_folded,
                    &staging.join("objects"),
                )
                .await
                .with_context(|| format!("source `{name}`"))?;

            for (skill_name, skill_path) in discovered {
                validate_path(&skill_path)?;
                let lower = skill_name.to_ascii_lowercase();
                if let Some(existing) = all_folded.insert(lower, skill_name.clone()) {
                    bail!(
                        "source `{name}` discovered skill `{skill_name}` which conflicts with `{existing}`"
                    );
                }
                self.skills.insert(
                    skill_name,
                    RemoteSkill {
                        source: name.clone(),
                        path: skill_path,
                    },
                );
            }

            let mut files = BTreeMap::new();
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
            let source = BundleSource { locked, files };
            self.validate_source(name, &source, config)?;
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
        let explicit = explicit_remote_skills(config)?;
        let expected_selection = selection_fingerprint(config, &explicit)?;
        if identities != expected || bundle.selection_fingerprint != expected_selection {
            bail!("bundle does not match remote configuration and lockfile");
        }
        let collection_sources: BTreeSet<&str> = config
            .collections
            .iter()
            .filter(|collection| collection.source != LOCAL_SOURCE)
            .map(|collection| collection.source.as_str())
            .collect();
        for (name, skill) in &bundle.skills {
            if !bundle.sources.contains_key(&skill.source) {
                bail!(
                    "bundle skill `{name}` references unknown source `{}`",
                    skill.source
                );
            }
            match explicit.get(name) {
                Some(expected_skill) if expected_skill != skill => {
                    bail!("bundle does not match remote configuration and lockfile");
                }
                None if !collection_sources.contains(skill.source.as_str()) => {
                    bail!("bundle does not match remote configuration and lockfile");
                }
                _ => {}
            }
        }
        for (name, expected_skill) in &explicit {
            if bundle.skills.get(name) != Some(expected_skill) {
                bail!("bundle does not match remote configuration and lockfile");
            }
        }
        Ok(bundle)
    }

    pub fn skills(&self) -> &BTreeMap<String, RemoteSkill> {
        &self.skills
    }

    fn validate_source(&self, name: &str, source: &BundleSource, config: &Config) -> Result<()> {
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
        validate_named_collection(
            "skill",
            self.skills
                .iter()
                .filter(|(_, skill)| skill.source == name)
                .map(|(skill_name, _)| skill_name),
        )?;
        for skill in self.skills.values().filter(|skill| skill.source == name) {
            if !skill.path.is_empty() {
                validate_path(&skill.path)?;
            }
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
        let source_collections: Vec<&CollectionConfig> = config
            .collections
            .iter()
            .filter(|collection| collection.source == name)
            .collect();
        if !source_collections.is_empty() {
            let explicit = explicit_remote_skills(config)?;
            let explicit_folded: BTreeMap<String, String> = explicit
                .keys()
                .map(|skill_name| (skill_name.to_ascii_lowercase(), skill_name.clone()))
                .collect();
            let candidate_dirs: Vec<CandidateDir> = source
                .files
                .iter()
                .filter(|(_, file)| file.kind != FileKind::Symlink)
                .filter_map(|(path, _)| path.strip_suffix("/SKILL.md"))
                .filter(|dir| !dir.is_empty())
                .map(|dir| CandidateDir::utf8(dir.to_owned()))
                .collect();
            let expected_discovered = discover_remote_skills_from_bundle(
                name,
                &source_collections,
                &candidate_dirs,
                &explicit_folded,
            )
            .context("bundle does not match remote configuration and lockfile")?;
            let actual_discovered: BTreeMap<String, String> = self
                .skills
                .iter()
                .filter(|(skill_name, skill)| {
                    skill.source == name && !explicit.contains_key(*skill_name)
                })
                .map(|(skill_name, skill)| (skill_name.clone(), skill.path.clone()))
                .collect();
            if actual_discovered != expected_discovered {
                bail!("bundle does not match remote configuration and lockfile");
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
        config: &Config,
    ) -> Result<MaterializedSource> {
        let source = &self.sources[name];
        self.validate_source(name, source, config)?;
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
                discovered_skills: BTreeMap::new(),
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
            discovered_skills: BTreeMap::new(),
        })
    }
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hash_len(hasher, value.len());
    hasher.update(value);
}

fn hash_len(hasher: &mut Sha256, len: usize) {
    hasher.update((len as u64).to_be_bytes());
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
