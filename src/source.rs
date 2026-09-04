use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::config::{SourceConfig, SourceKind};

static STAGING_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct SourceMaterializer {
    data_dir: PathBuf,
    process_limit: Arc<Semaphore>,
}

#[derive(Debug, Clone)]
pub struct MaterializedSource {
    pub revision: String,
    pub snapshot: Option<PathBuf>,
}

impl SourceMaterializer {
    pub fn new(data_dir: PathBuf, jobs: u16) -> Self {
        Self {
            data_dir,
            process_limit: Arc::new(Semaphore::new(usize::from(jobs))),
        }
    }

    pub async fn resolve(
        &self,
        name: &str,
        source: &SourceConfig,
        fingerprint: &str,
        requested_revision: Option<&str>,
        paths: &[String],
        rebuild_repository: bool,
    ) -> Result<MaterializedSource> {
        let repository = self
            .ensure_repository(name, source, rebuild_repository)
            .await?;
        let revision = match requested_revision {
            Some(revision) => {
                self.fetch(&repository, source, Some(revision)).await?;
                self.git_output(
                    &repository,
                    ["rev-parse", &format!("{revision}^{{commit}}")],
                )
                .await?
            }
            None => {
                self.fetch(&repository, source, source.reference.as_deref())
                    .await?;
                self.git_output(&repository, ["rev-parse", "FETCH_HEAD^{commit}"])
                    .await?
            }
        };

        let selected = selected_paths(paths);
        let required: Vec<_> = paths
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let snapshot = if selected.is_empty() {
            None
        } else {
            Some(
                self.materialize_snapshot(
                    &repository,
                    fingerprint,
                    &revision,
                    &selected,
                    &required,
                )
                .await?,
            )
        };
        Ok(MaterializedSource { revision, snapshot })
    }

    pub fn snapshot_path(
        &self,
        fingerprint: &str,
        revision: &str,
        paths: &[String],
    ) -> Option<PathBuf> {
        let selected = selected_paths(paths);
        (!selected.is_empty()).then(|| {
            self.data_dir
                .join("snapshots")
                .join(snapshot_hash(fingerprint, revision, &selected))
        })
    }

    async fn ensure_repository(
        &self,
        name: &str,
        source: &SourceConfig,
        rebuild: bool,
    ) -> Result<PathBuf> {
        let repository = self.data_dir.join("repos").join(name);
        if !rebuild && tokio::fs::try_exists(repository.join(".git")).await? {
            return Ok(repository);
        }
        if tokio::fs::try_exists(&repository).await? {
            let quarantine = self.staging_path(&format!("repo-{name}"));
            tokio::fs::rename(&repository, &quarantine)
                .await
                .with_context(|| format!("failed to quarantine `{}`", repository.display()))?;
        }

        let (kind, configured) = source.kind_and_repository()?;
        if kind == SourceKind::Github && self.gh_authenticated().await {
            let extra = ["--filter=blob:none", "--no-checkout", "--depth=1"];
            let mut command = Command::new("gh");
            command
                .arg("repo")
                .arg("clone")
                .arg(configured)
                .arg(&repository);
            command.arg("--").args(extra);
            self.run(command, "gh repo clone").await?;
        } else {
            let remote = match kind {
                SourceKind::Git => configured.to_owned(),
                SourceKind::Github => format!("https://github.com/{configured}.git"),
            };
            let mut command = Command::new("git");
            command
                .arg("clone")
                .args(["--filter=blob:none", "--no-checkout", "--depth=1"])
                .arg(remote)
                .arg(&repository);
            self.run(command, "git clone").await?;
        }
        Ok(repository)
    }

    async fn fetch(
        &self,
        repository: &Path,
        source: &SourceConfig,
        reference: Option<&str>,
    ) -> Result<()> {
        let mut command = self.git(repository);
        command.arg("fetch").args(["--depth=1", "origin"]);
        if let Some(reference) = reference {
            command.arg(reference);
        }
        self.run(command, "git fetch").await.with_context(|| {
            format!(
                "failed to fetch {}",
                source
                    .reference
                    .as_deref()
                    .or(reference)
                    .unwrap_or("remote HEAD")
            )
        })
    }

    async fn materialize_snapshot(
        &self,
        repository: &Path,
        fingerprint: &str,
        revision: &str,
        selected_paths: &[String],
        required_paths: &[String],
    ) -> Result<PathBuf> {
        let destination = self.data_dir.join("snapshots").join(snapshot_hash(
            fingerprint,
            revision,
            selected_paths,
        ));
        if tokio::fs::try_exists(&destination).await? {
            self.validate_snapshot(&destination, required_paths).await?;
            return Ok(destination);
        }

        let staging = self.staging_path("snapshot");
        tokio::fs::create_dir_all(&staging).await?;
        self.validate_git_tree(repository, revision, selected_paths, required_paths)
            .await?;

        let mut command = self.git(repository);
        command
            .arg(format!("--work-tree={}", staging.display()))
            .args(["checkout", "--force", revision, "--"])
            .args(selected_paths);
        if let Err(error) = self.run(command, "git checkout").await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(error);
        }
        self.validate_snapshot(&staging, required_paths).await?;
        match tokio::fs::rename(&staging, &destination).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                tokio::fs::remove_dir_all(&staging).await?;
            }
            Err(error) => return Err(error).context("failed to publish snapshot"),
        }
        Ok(destination)
    }

    async fn validate_git_tree(
        &self,
        repository: &Path,
        revision: &str,
        selected_paths: &[String],
        required_paths: &[String],
    ) -> Result<()> {
        let mut arguments = vec!["ls-tree", "-r", revision, "--"];
        arguments.extend(selected_paths.iter().map(String::as_str));
        let tree = self.git_output(repository, arguments).await?;
        for line in tree.lines() {
            if line.starts_with("160000 ") {
                bail!("selected skill tree contains an unsupported Git submodule");
            }
        }
        for path in required_paths {
            let manifest = format!("{path}/SKILL.md");
            let found = tree.lines().any(|line| {
                line.starts_with("100")
                    && line
                        .split_once('\t')
                        .is_some_and(|(_, item)| item == manifest)
            });
            if !found {
                bail!("`{path}` does not contain a regular tracked SKILL.md");
            }
        }
        Ok(())
    }

    async fn validate_snapshot(&self, root: &Path, paths: &[String]) -> Result<()> {
        for path in paths {
            let metadata = tokio::fs::symlink_metadata(root.join(path).join("SKILL.md"))
                .await
                .with_context(|| format!("skill `{path}` has no SKILL.md"))?;
            if !metadata.file_type().is_file() {
                bail!("skill `{path}` has no regular SKILL.md");
            }
        }
        Ok(())
    }

    async fn gh_authenticated(&self) -> bool {
        let mut command = Command::new("gh");
        command.args(["auth", "status"]);
        self.run(command, "gh auth status").await.is_ok()
    }

    async fn git_output<I, S>(&self, repository: &Path, arguments: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = self.git(repository);
        command.args(arguments);
        let output = self.output(command, "git").await?;
        Ok(output.trim().to_owned())
    }

    fn git(&self, repository: &Path) -> Command {
        let mut command = Command::new("git");
        command.arg("-C").arg(repository);
        command
    }

    async fn run(&self, command: Command, label: &str) -> Result<()> {
        self.output(command, label).await.map(|_| ())
    }

    async fn output(&self, mut command: Command, label: &str) -> Result<String> {
        let _permit = self
            .process_limit
            .acquire()
            .await
            .context("process limit closed")?;
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to start {label}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("{label} failed: {}", stderr.trim());
        }
        String::from_utf8(output.stdout)
            .with_context(|| format!("{label} returned non-UTF-8 output"))
    }

    fn staging_path(&self, prefix: &str) -> PathBuf {
        let id = STAGING_ID.fetch_add(1, Ordering::Relaxed);
        self.data_dir
            .join(".staging")
            .join(format!("{prefix}-{}-{id}", std::process::id()))
    }
}

pub fn selected_paths(paths: &[String]) -> Vec<String> {
    let unique: BTreeSet<_> = paths.iter().cloned().collect();
    let mut selected = Vec::<String>::new();
    for path in unique {
        if !selected
            .iter()
            .any(|parent| Path::new(&path).starts_with(parent))
        {
            selected.push(path);
        }
    }
    selected
}

pub fn snapshot_hash(fingerprint: &str, revision: &str, paths: &[String]) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"yaasm-snapshot-v1");
    hash_field(&mut hasher, fingerprint.as_bytes());
    hash_field(&mut hasher, revision.as_bytes());
    for path in paths {
        hash_field(&mut hasher, path.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_paths_remove_duplicates_and_covered_children() {
        let paths = vec![
            "skills/b".into(),
            "skills/a/child".into(),
            "skills/a".into(),
            "skills/b".into(),
        ];
        assert_eq!(selected_paths(&paths), ["skills/a", "skills/b"]);
    }

    #[test]
    fn snapshot_identity_includes_paths() {
        assert_ne!(
            snapshot_hash("f", "r", &["a".into()]),
            snapshot_hash("f", "r", &["b".into()])
        );
    }
}
