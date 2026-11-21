use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::collections::{
    CandidateDir, discover_remote_skills, discover_remote_skills_lenient,
    remote_collection_prefixes,
};
use crate::config::{CollectionConfig, SourceConfig, SourceKind};
use crate::paths::{canonicalize_with_missing, simplified};

static STAGING_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct SourceMaterializer {
    data_dir: PathBuf,
    process_limit: Arc<Semaphore>,
    ssh_command: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MaterializedSource {
    pub revision: String,
    pub snapshot: Option<PathBuf>,
    pub discovered_skills: BTreeMap<String, String>,
}

pub struct TrackedFile {
    pub path: String,
    pub mode: String,
    pub sha256: String,
}

struct RawTreeEntry {
    mode: String,
    object_id: String,
    path: Vec<u8>,
}

impl SourceMaterializer {
    pub fn new(data_dir: PathBuf, jobs: u16) -> Self {
        let ssh_command = multiplexed_ssh_command();
        Self {
            data_dir,
            process_limit: Arc::new(Semaphore::new(usize::from(jobs))),
            ssh_command,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn resolve(
        &self,
        name: &str,
        source: &SourceConfig,
        fingerprint: &str,
        requested_revision: Option<&str>,
        explicit_paths: &[String],
        collections: &[CollectionConfig],
        explicit_folded: &BTreeMap<String, String>,
        rebuild_repository: bool,
    ) -> Result<MaterializedSource> {
        let (repository, revision) = self
            .resolve_repository(name, source, requested_revision, rebuild_repository)
            .await?;

        let discovered_skills = if collections.is_empty() {
            BTreeMap::new()
        } else {
            self.discover_in_repository(name, &repository, &revision, collections, explicit_folded)
                .await?
        };

        let mut all_paths = explicit_paths.to_vec();
        all_paths.extend(discovered_skills.values().cloned());

        let selected = selected_paths(&all_paths);
        let required: Vec<_> = all_paths
            .into_iter()
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
        Ok(MaterializedSource {
            revision,
            snapshot,
            discovered_skills,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn try_cached_source(
        &self,
        name: &str,
        source: &SourceConfig,
        fingerprint: &str,
        revision: &str,
        explicit_paths: &[String],
        collections: &[CollectionConfig],
        explicit_folded: &BTreeMap<String, String>,
    ) -> Result<Option<MaterializedSource>> {
        let discovered_skills = if collections.is_empty() {
            BTreeMap::new()
        } else {
            let Some(repository) = self.matching_local_repository(name, source).await? else {
                return Ok(None);
            };
            if !self.has_commit(&repository, revision).await {
                return Ok(None);
            }
            match self
                .discover_in_repository(name, &repository, revision, collections, explicit_folded)
                .await
            {
                Ok(skills) => skills,
                Err(_) => return Ok(None),
            }
        };

        let mut all_paths = explicit_paths.to_vec();
        all_paths.extend(discovered_skills.values().cloned());
        let selected = selected_paths(&all_paths);
        if selected.is_empty() {
            return Ok(Some(MaterializedSource {
                revision: revision.to_owned(),
                snapshot: None,
                discovered_skills,
            }));
        }
        let Some(snapshot) = self.snapshot_path(fingerprint, revision, &all_paths) else {
            return Ok(None);
        };
        if tokio::fs::try_exists(&snapshot).await? {
            let required: Vec<_> = all_paths
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            if self.validate_snapshot(&snapshot, &required).await.is_err() {
                return Ok(None);
            }
            Ok(Some(MaterializedSource {
                revision: revision.to_owned(),
                snapshot: Some(snapshot),
                discovered_skills,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn discover_skills_in_cached_repo(
        &self,
        name: &str,
        revision: &str,
        collections: &[CollectionConfig],
    ) -> Result<Option<BTreeMap<String, String>>> {
        if collections.is_empty() {
            return Ok(Some(BTreeMap::new()));
        }
        let repository = self.data_dir.join("repos").join(name);
        if !tokio::fs::try_exists(repository.join(".git")).await?
            || !self.has_commit(&repository, revision).await
        {
            return Ok(None);
        }
        let refs: Vec<_> = collections.iter().collect();
        let prefixes = selected_paths(&remote_collection_prefixes(&refs)?);
        let candidates = self
            .list_candidate_dirs(&repository, revision, &prefixes)
            .await?;
        Ok(Some(discover_remote_skills_lenient(
            name,
            &refs,
            &candidates,
        )?))
    }

    pub async fn discover_skills_for_diff(
        &self,
        name: &str,
        revision: &str,
        collections: &[CollectionConfig],
    ) -> Result<BTreeMap<String, String>> {
        if collections.is_empty() {
            return Ok(BTreeMap::new());
        }
        let repository = self.data_dir.join("repos").join(name);
        if !self.has_commit(&repository, revision).await {
            self.fetch(&repository, Some(revision)).await?;
        }
        let refs: Vec<_> = collections.iter().collect();
        let prefixes = selected_paths(&remote_collection_prefixes(&refs)?);
        let candidates = self
            .list_candidate_dirs(&repository, revision, &prefixes)
            .await?;
        discover_remote_skills_lenient(name, &refs, &candidates)
    }

    async fn discover_in_repository(
        &self,
        name: &str,
        repository: &Path,
        revision: &str,
        collections: &[CollectionConfig],
        explicit_folded: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let refs: Vec<_> = collections.iter().collect();
        let prefixes = selected_paths(&remote_collection_prefixes(&refs)?);
        let candidates = self
            .list_candidate_dirs(repository, revision, &prefixes)
            .await?;
        discover_remote_skills(name, &refs, &candidates, explicit_folded)
    }

    async fn list_candidate_dirs(
        &self,
        repository: &Path,
        revision: &str,
        prefixes: &[String],
    ) -> Result<Vec<CandidateDir>> {
        let entries = self
            .list_tree_entries(repository, revision, prefixes)
            .await?;
        let mut dirs = BTreeMap::<Vec<u8>, CandidateDir>::new();
        for entry in entries {
            if matches!(entry.mode.as_str(), "100644" | "100755")
                && let Some(dir_bytes) = entry.path.strip_suffix(b"/SKILL.md")
                && !dir_bytes.is_empty()
            {
                dirs.entry(dir_bytes.to_vec())
                    .or_insert_with(|| CandidateDir::from_bytes(dir_bytes));
            }
        }
        Ok(dirs.into_values().collect())
    }

    async fn has_commit(&self, repository: &Path, revision: &str) -> bool {
        let mut command = self.git(repository);
        command.args(["cat-file", "-e", &format!("{revision}^{{commit}}")]);
        self.run(command, "git cat-file").await.is_ok()
    }

    async fn resolve_repository(
        &self,
        name: &str,
        source: &SourceConfig,
        requested_revision: Option<&str>,
        rebuild_repository: bool,
    ) -> Result<(PathBuf, String)> {
        let clone_revision = requested_revision.or(source.reference.as_deref());
        let (repository, cloned) = self
            .ensure_repository(name, source, clone_revision, rebuild_repository)
            .await?;
        let revision = if cloned {
            self.git_output(&repository, ["rev-parse", "HEAD^{commit}"])
                .await?
        } else if let Some(revision) = requested_revision {
            self.fetch(&repository, Some(revision)).await?;
            self.git_output(
                &repository,
                ["rev-parse", &format!("{revision}^{{commit}}")],
            )
            .await?
        } else {
            self.fetch(&repository, source.reference.as_deref()).await?;
            self.git_output(&repository, ["rev-parse", "FETCH_HEAD^{commit}"])
                .await?
        };

        Ok((repository, revision))
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

    /// Resolve a locked source, expand any collections, and export its tracked blobs
    /// as SHA-256 objects in an existing writable directory.
    /// Contents and recorded modes are independent of Git checkout settings.
    /// This operation may access the network.
    #[allow(clippy::too_many_arguments)]
    pub async fn export_locked_source(
        &self,
        name: &str,
        source: &SourceConfig,
        revision: &str,
        explicit_paths: &[String],
        collections: &[CollectionConfig],
        explicit_folded: &BTreeMap<String, String>,
        objects_dir: &Path,
    ) -> Result<(BTreeMap<String, String>, Vec<TrackedFile>)> {
        if explicit_paths.is_empty() && collections.is_empty() {
            return Ok((BTreeMap::new(), Vec::new()));
        }
        let (repository, resolved) = self
            .resolve_repository(name, source, Some(revision), false)
            .await?;
        if resolved != revision {
            bail!("resolved revision does not match lockfile");
        }
        let discovered_skills = if collections.is_empty() {
            BTreeMap::new()
        } else {
            self.discover_in_repository(name, &repository, revision, collections, explicit_folded)
                .await?
        };
        let mut all_paths = explicit_paths.to_vec();
        all_paths.extend(discovered_skills.values().cloned());
        if all_paths.is_empty() {
            return Ok((discovered_skills, Vec::new()));
        }

        let selected = selected_paths(&all_paths);
        self.prefetch_missing_objects(&repository, revision, &selected)
            .await?;
        let tree_entries = self
            .list_tree_entries(&repository, revision, &selected)
            .await?;
        let mut entries = Vec::with_capacity(tree_entries.len());
        let mut objects = String::new();
        for entry in tree_entries {
            let path = String::from_utf8(entry.path).context("bundle paths must be UTF-8")?;
            if !matches!(entry.mode.as_str(), "100644" | "100755" | "120000") {
                bail!("unsupported Git tree entry `{path}`");
            }
            objects.push_str(&entry.object_id);
            objects.push('\n');
            entries.push((path, entry.mode));
        }
        let files = self
            .export_blobs(&repository, &objects, entries, objects_dir)
            .await?;
        Ok((discovered_skills, files))
    }

    async fn list_tree_entries(
        &self,
        repository: &Path,
        revision: &str,
        paths: &[String],
    ) -> Result<Vec<RawTreeEntry>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        for chunk in chunk_git_paths(paths) {
            let mut command = self.git(repository);
            command
                .args(["ls-tree", "-r", "-z", revision, "--"])
                .args(chunk);
            let tree = self
                .output_bytes_with_input(command, "git ls-tree", None)
                .await?;
            for raw in tree
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
            {
                let tab = raw
                    .iter()
                    .position(|byte| *byte == b'\t')
                    .context("invalid Git tree entry")?;
                let header = std::str::from_utf8(&raw[..tab]).context("invalid Git tree header")?;
                let path = raw[tab + 1..].to_vec();
                let fields: Vec<_> = header.split_whitespace().collect();
                if fields.len() != 3 {
                    bail!(
                        "invalid Git tree entry `{}`",
                        String::from_utf8_lossy(&path)
                    );
                }
                entries.push(RawTreeEntry {
                    mode: fields[0].to_owned(),
                    object_id: fields[2].to_owned(),
                    path,
                });
            }
        }
        Ok(entries)
    }

    async fn export_blobs(
        &self,
        repository: &Path,
        objects: &str,
        entries: Vec<(String, String)>,
        objects_dir: &Path,
    ) -> Result<Vec<TrackedFile>> {
        let mut command = self.git(repository);
        command.args(["cat-file", "--batch"]);
        let (_permit, mut child) = self.start(command, "git cat-file", true).await?;
        let mut stdin = child.stdin.take().context("missing Git stdin")?;
        let stdout = child.stdout.take().context("missing Git stdout")?;
        let write = async move { stdin.write_all(objects.as_bytes()).await };
        let read = async move {
            let mut reader = BufReader::new(stdout);
            let mut files = Vec::new();
            let mut buffer = vec![0; 64 * 1024];
            for (index, (path, mode)) in entries.into_iter().enumerate() {
                let mut header = String::new();
                reader.read_line(&mut header).await?;
                let fields: Vec<_> = header.split_whitespace().collect();
                if fields.len() != 3 || fields[1] != "blob" {
                    bail!("invalid Git blob header `{}`", header.trim());
                }
                let mut remaining: u64 = fields[2].parse()?;
                let temporary = objects_dir.join(format!(".blob-{index}"));
                let mut file = tokio::fs::File::create_new(&temporary).await?;
                let mut hasher = Sha256::new();
                while remaining > 0 {
                    let length = remaining.min(buffer.len() as u64) as usize;
                    let count = reader.read(&mut buffer[..length]).await?;
                    if count == 0 {
                        bail!("truncated Git blob for `{path}`");
                    }
                    file.write_all(&buffer[..count]).await?;
                    hasher.update(&buffer[..count]);
                    remaining -= count as u64;
                }
                if reader.read_u8().await? != b'\n' {
                    bail!("invalid Git blob separator");
                }
                file.flush().await?;
                drop(file);
                let sha256 = hex::encode(hasher.finalize());
                let destination = objects_dir.join(&sha256);
                if tokio::fs::try_exists(&destination).await? {
                    tokio::fs::remove_file(&temporary).await?;
                } else {
                    tokio::fs::rename(&temporary, destination).await?;
                }
                files.push(TrackedFile { path, mode, sha256 });
            }
            Ok::<_, anyhow::Error>(files)
        };
        let (written, files, output) = tokio::join!(write, read, child.wait_with_output());
        let output = output.context("failed to wait for git cat-file")?;
        checked_output(output, "git cat-file")?;
        written.context("failed to write Git object requests")?;
        files
    }

    async fn matching_local_repository(
        &self,
        name: &str,
        source: &SourceConfig,
    ) -> Result<Option<PathBuf>> {
        let repository = self.data_dir.join("repos").join(name);
        if !tokio::fs::try_exists(repository.join(".git")).await? {
            return Ok(None);
        }
        let remote = remote_url(source)?;
        let Ok(origin) = self
            .git_output(
                &repository,
                ["config", "--get", "--default", "", "remote.origin.url"],
            )
            .await
        else {
            return Ok(None);
        };
        let local_path = remote
            .split_once(':')
            .is_none_or(|(prefix, _)| prefix.contains('/'))
            || matches!(
                Path::new(&remote).components().next(),
                Some(Component::Prefix(_))
            );
        // Git records relative local clone origins against the working directory.
        let matches = if local_path {
            Path::new(&origin).is_absolute()
                && canonicalize_with_missing(Path::new(&origin)).await?
                    == canonicalize_with_missing(&std::env::current_dir()?.join(&remote)).await?
        } else {
            origin == remote
        };
        Ok(matches.then_some(repository))
    }

    async fn ensure_repository(
        &self,
        name: &str,
        source: &SourceConfig,
        revision: Option<&str>,
        rebuild: bool,
    ) -> Result<(PathBuf, bool)> {
        if !rebuild && let Some(repository) = self.matching_local_repository(name, source).await? {
            return Ok((repository, false));
        }
        let repository = self.data_dir.join("repos").join(name);
        let remote = remote_url(source)?;
        if tokio::fs::try_exists(&repository).await? {
            let quarantine = self.staging_path(&format!("repo-{name}"));
            tokio::fs::rename(&repository, &quarantine)
                .await
                .with_context(|| format!("failed to quarantine `{}`", repository.display()))?;
        }

        let mut clone_options = vec!["--filter=blob:none", "--no-checkout", "--depth=1"];
        let revision_option = revision.map(|revision| format!("--revision={revision}"));
        clone_options.extend(revision_option.as_deref());

        let mut command = Command::new("git");
        command
            .arg("clone")
            .args(&clone_options)
            .arg(remote)
            .arg(simplified(&repository));
        self.run(command, "git clone").await?;
        Ok((repository, true))
    }

    async fn fetch(&self, repository: &Path, reference: Option<&str>) -> Result<()> {
        // Revision clones have no fetch refspec. Always request the intended ref,
        // including remote HEAD when the source does not configure one.
        let reference = reference.unwrap_or("HEAD");
        let mut command = self.git(repository);
        command.args(["fetch", "--depth=1", "origin", reference]);
        self.run(command, "git fetch")
            .await
            .with_context(|| format!("failed to fetch {reference}"))
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
        // A path-limited checkout from a blobless partial clone fetches missing blobs
        // one at a time. Fetch them together first to avoid one connection per file.
        self.prefetch_missing_objects(repository, revision, selected_paths)
            .await?;

        for chunk in chunk_git_paths(selected_paths) {
            let mut command = self.git(repository);
            command
                .arg(format!("--work-tree={}", simplified(&staging).display()))
                .args(["checkout", "--force", revision, "--"])
                .args(chunk);
            if let Err(error) = self.run(command, "git checkout").await {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                return Err(error);
            }
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
        let entries = self
            .list_tree_entries(repository, revision, selected_paths)
            .await?;
        for entry in &entries {
            if entry.mode == "160000" {
                bail!("selected skill tree contains an unsupported Git submodule");
            }
        }
        let regular_files: BTreeSet<&[u8]> = entries
            .iter()
            .filter(|entry| matches!(entry.mode.as_str(), "100644" | "100755"))
            .map(|entry| entry.path.as_slice())
            .collect();
        for path in required_paths {
            let normalized = Path::new(path)
                .components()
                .filter_map(|component| match component {
                    Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("/");
            let manifest = if normalized.is_empty() {
                "SKILL.md".to_owned()
            } else {
                format!("{normalized}/SKILL.md")
            };
            if !regular_files.contains(manifest.as_bytes()) {
                bail!("`{path}` does not contain a regular tracked SKILL.md");
            }
        }
        Ok(())
    }

    async fn prefetch_missing_objects(
        &self,
        repository: &Path,
        revision: &str,
        selected_paths: &[String],
    ) -> Result<()> {
        let mut missing = BTreeSet::new();
        for chunk in chunk_git_paths(selected_paths) {
            let mut arguments = vec!["rev-list", "--objects", "--missing=print", revision, "--"];
            arguments.extend(chunk.iter().map(String::as_str));
            let objects = self.git_output(repository, arguments).await?;
            for id in missing_object_ids(&objects) {
                missing.insert(id.to_owned());
            }
        }
        if missing.is_empty() {
            return Ok(());
        }

        let mut input = missing.into_iter().collect::<Vec<_>>().join("\n");
        input.push('\n');
        let mut command = self.git(repository);
        command
            .args(["-c", "fetch.negotiationAlgorithm=noop", "fetch", "origin"])
            .args([
                "--no-tags",
                "--no-write-fetch-head",
                "--recurse-submodules=no",
                "--filter=blob:none",
                "--stdin",
            ]);
        self.output_with_input(command, "git fetch", Some(input.as_bytes()))
            .await
            .map(|_| ())
            .context("failed to prefetch selected files")
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
        command.arg("-C").arg(simplified(repository));
        command
    }

    async fn run(&self, command: Command, label: &str) -> Result<()> {
        self.output(command, label).await.map(|_| ())
    }

    async fn output(&self, command: Command, label: &str) -> Result<String> {
        self.output_with_input(command, label, None).await
    }

    async fn output_with_input(
        &self,
        command: Command,
        label: &str,
        input: Option<&[u8]>,
    ) -> Result<String> {
        let bytes = self.output_bytes_with_input(command, label, input).await?;
        String::from_utf8(bytes).with_context(|| format!("{label} returned non-UTF-8 output"))
    }

    async fn output_bytes_with_input(
        &self,
        command: Command,
        label: &str,
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let (_permit, mut child) = self.start(command, label, input.is_some()).await?;
        let stdin = child.stdin.take();
        let write = async {
            if let Some(input) = input {
                stdin
                    .context("failed to open child stdin")?
                    .write_all(input)
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let (written, output) = tokio::join!(write, child.wait_with_output());
        let output = output.with_context(|| format!("failed to wait for {label}"))?;
        let bytes = checked_output(output, label)?;
        written.with_context(|| format!("failed to write to {label}"))?;
        Ok(bytes)
    }

    async fn start(
        &self,
        mut command: Command,
        label: &str,
        has_input: bool,
    ) -> Result<(tokio::sync::SemaphorePermit<'_>, tokio::process::Child)> {
        let permit = self
            .process_limit
            .acquire()
            .await
            .context("process limit closed")?;
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .stdin(if has_input {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(ssh_command) = &self.ssh_command {
            command.env("GIT_SSH_COMMAND", ssh_command);
        }
        let child = command
            .spawn()
            .with_context(|| format!("failed to start {label}"))?;
        Ok((permit, child))
    }

    fn staging_path(&self, prefix: &str) -> PathBuf {
        let id = STAGING_ID.fetch_add(1, Ordering::Relaxed);
        self.data_dir
            .join(".staging")
            .join(format!("{prefix}-{}-{id}", std::process::id()))
    }
}

fn remote_url(source: &SourceConfig) -> Result<String> {
    let (kind, configured) = source.kind_and_repository()?;
    Ok(match kind {
        SourceKind::Git => configured.to_owned(),
        SourceKind::Github => format!("https://github.com/{configured}.git"),
    })
}

fn checked_output(output: std::process::Output, label: &str) -> Result<Vec<u8>> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("{label} failed ({})", output.status);
        }
        bail!("{label} failed ({}): {stderr}", output.status);
    }
    Ok(output.stdout)
}

#[cfg(unix)]
fn multiplexed_ssh_command() -> Option<String> {
    if std::env::var_os("GIT_SSH_COMMAND").is_some() {
        return None;
    }
    // Keep this path short: OpenSSH expands %C and adds a temporary suffix while
    // creating the socket, but Unix socket paths are limited to roughly 100 bytes.
    let control_path = std::env::temp_dir().join(format!("yaasm-ssh-{}-%C", std::process::id()));
    let control_path = control_path.to_str()?;
    Some(format!(
        "ssh -o ControlMaster=auto -o ControlPersist=30s -o ControlPath={}",
        shell_quote(control_path)
    ))
}

#[cfg(not(unix))]
fn multiplexed_ssh_command() -> Option<String> {
    None
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn missing_object_ids(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix('?'))
        .collect()
}

/// Split Git pathspec arguments into batches well under OS command-line limits
/// (including Windows's 32 KiB `CreateProcess` limit).
fn chunk_git_paths(paths: &[String]) -> Vec<&[String]> {
    const MAX_BATCH_BYTES: usize = 16 * 1024;
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes: usize = 0;
    for (index, path) in paths.iter().enumerate() {
        let cost = path.len().saturating_add(1);
        if index > start && bytes.saturating_add(cost) > MAX_BATCH_BYTES {
            chunks.push(&paths[start..index]);
            start = index;
            bytes = 0;
        }
        bytes = bytes.saturating_add(cost);
    }
    if start < paths.len() {
        chunks.push(&paths[start..]);
    }
    chunks
}

pub fn selected_paths(paths: &[String]) -> Vec<String> {
    if paths
        .iter()
        .any(|path| path.is_empty() || Path::new(path) == Path::new("."))
    {
        return vec![".".into()];
    }
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

        for root in [".", ""] {
            let mut paths = paths.clone();
            paths.push(root.into());
            assert_eq!(selected_paths(&paths), ["."]);
        }
    }

    #[test]
    fn snapshot_identity_includes_paths() {
        assert_ne!(
            snapshot_hash("f", "r", &["a".into()]),
            snapshot_hash("f", "r", &["b".into()])
        );
    }

    #[test]
    fn extracts_only_missing_object_ids() {
        let output = "012345 path/to/file\n?abcdef\n?789012\n";
        assert_eq!(missing_object_ids(output), ["abcdef", "789012"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_process_reports_status_with_or_without_stderr() {
        let temp = tempfile::tempdir().unwrap();
        let materializer = SourceMaterializer::new(temp.path().to_owned(), 1);
        for (script, expected) in [
            ("exit 23", "test command failed (exit status: 23)"),
            (
                "printf 'problem\\n' >&2; exit 23",
                "test command failed (exit status: 23): problem",
            ),
        ] {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            let error = materializer
                .output(command, "test command")
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn quotes_ssh_control_paths_for_the_shell() {
        assert_eq!(shell_quote("/tmp/a'b"), "'/tmp/a'\\''b'");
    }

    #[test]
    fn chunk_git_paths_bounds_batch_size() {
        let paths: Vec<String> = (0..500)
            .map(|i| format!("skills/{:0>60}-{i}", "a"))
            .collect();
        let chunks = chunk_git_paths(&paths);
        assert!(chunks.len() > 1);
        let flattened: Vec<String> = chunks.iter().flat_map(|c| c.iter().cloned()).collect();
        assert_eq!(flattened, paths);
        for chunk in chunks {
            assert!(!chunk.is_empty());
            let total: usize = chunk.iter().map(|p| p.len() + 1).sum();
            assert!(total <= 16 * 1024);
        }
    }
}
