use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::config::{SourceConfig, SourceKind};
use crate::paths::simplified;

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
}

pub struct TrackedFile {
    pub path: String,
    pub mode: String,
    pub sha256: String,
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

    pub async fn resolve(
        &self,
        name: &str,
        source: &SourceConfig,
        fingerprint: &str,
        requested_revision: Option<&str>,
        paths: &[String],
        rebuild_repository: bool,
    ) -> Result<MaterializedSource> {
        let (repository, revision) = self
            .resolve_repository(name, source, requested_revision, rebuild_repository)
            .await?;

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

    /// Export locked blobs as SHA-256 objects in an existing writable directory.
    /// Contents and recorded modes are independent of Git checkout settings.
    /// This operation may access the network.
    pub async fn export_locked_tree(
        &self,
        name: &str,
        source: &SourceConfig,
        revision: &str,
        paths: &[String],
        objects_dir: &Path,
    ) -> Result<Vec<TrackedFile>> {
        let (repository, resolved) = self
            .resolve_repository(name, source, Some(revision), false)
            .await?;
        if resolved != revision {
            bail!("resolved revision does not match lockfile");
        }
        let selected = selected_paths(paths);
        self.prefetch_missing_objects(&repository, revision, &selected)
            .await?;
        let mut command = self.git(&repository);
        command
            .args(["ls-tree", "-r", "-z", revision, "--"])
            .args(&selected);
        let tree = self
            .output_bytes_with_input(command, "git ls-tree", None)
            .await?;
        let mut entries = Vec::new();
        let mut objects = String::new();
        for entry in tree
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let entry = std::str::from_utf8(entry).context("bundle paths must be UTF-8")?;
            let (header, path) = entry.split_once('\t').context("invalid Git tree entry")?;
            let fields: Vec<_> = header.split_whitespace().collect();
            if fields.len() != 3 || !matches!(fields[0], "100644" | "100755" | "120000") {
                bail!("unsupported Git tree entry `{path}`");
            }
            objects.push_str(fields[2]);
            objects.push('\n');
            entries.push((path.to_owned(), fields[0].to_owned()));
        }
        self.export_blobs(&repository, &objects, entries, objects_dir)
            .await
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

    async fn ensure_repository(
        &self,
        name: &str,
        source: &SourceConfig,
        revision: Option<&str>,
        rebuild: bool,
    ) -> Result<(PathBuf, bool)> {
        let repository = self.data_dir.join("repos").join(name);
        let (kind, configured) = source.kind_and_repository()?;
        let remote = match kind {
            SourceKind::Git => configured.to_owned(),
            SourceKind::Github => format!("https://github.com/{configured}.git"),
        };
        if !rebuild && tokio::fs::try_exists(repository.join(".git")).await? {
            let origin = self
                .git_output(
                    &repository,
                    ["config", "--get", "--default", "", "remote.origin.url"],
                )
                .await?;
            let local_path = remote
                .split_once(':')
                .is_none_or(|(prefix, _)| prefix.contains('/'))
                || matches!(
                    Path::new(&remote).components().next(),
                    Some(Component::Prefix(_))
                );
            // Git records relative local clone origins against the working directory.
            let matches = if local_path {
                Path::new(&origin) == std::env::current_dir()?.join(&remote)
            } else {
                origin == remote
            };
            if matches {
                return Ok((repository, false));
            }
        }
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

        let mut command = self.git(repository);
        command
            .arg(format!("--work-tree={}", simplified(&staging).display()))
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
            let manifest = Path::new(path)
                .components()
                .filter(|component| *component != Component::CurDir)
                .collect::<PathBuf>()
                .join("SKILL.md");
            let found = tree.lines().any(|line| {
                line.starts_with("100")
                    && line
                        .split_once('\t')
                        .is_some_and(|(_, item)| Path::new(item) == manifest)
            });
            if !found {
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
        let mut arguments = vec!["rev-list", "--objects", "--missing=print", revision, "--"];
        arguments.extend(selected_paths.iter().map(String::as_str));
        let objects = self.git_output(repository, arguments).await?;
        let missing = missing_object_ids(&objects);
        if missing.is_empty() {
            return Ok(());
        }

        let mut input = missing.join("\n");
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
}
