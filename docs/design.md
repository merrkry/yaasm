# YAASM design

This document specifies YAASM v1. YAASM is a Rust CLI that deploys a user's declared agent skills into one or more agent-specific directories.

## Scope

YAASM supports Linux, macOS, and Windows. It accepts Git, GitHub, and live local sources. Every successfully resolved network source has a lock entry. YAASM generates the lockfile when it is absent; a configuration with no network sources produces an empty lockfile.

YAASM does not support Git submodules, Git LFS downloads, multiple named profiles, or automatic cleanup of targets removed from configuration.

## Command line

The v1 commands are:

```console
yaasm deploy
yaasm deploy --locked
yaasm deploy --bundle ./remote-bundle
yaasm bundle --output ./remote-bundle
yaasm update
yaasm update foo bar
```

`deploy` fills missing or stale lock entries, then reconciles data entries and targets. `deploy --locked` forbids lock changes and may use the network. `deploy --bundle DIR` deploys from a [remote bundle](#remote-bundles). `--bundle` and `--locked` are mutually exclusive.

`update` resolves every declared network source and commits the lockfile without deploying. Source arguments restrict the forced updates to those names. Local sources cannot be update arguments. The command validates selected network skills before committing a revision, without resolving local paths or targets or cleaning deployed content. Run `deploy` to apply the updated versions.

`bundle --output DIR` always requires a matching lockfile, including entries for unused sources. It never updates that lockfile. It may fetch locked Git contents. The output must be absent and its parent directory must exist. The output cannot overlap the configuration or data directory. A failed build leaves the output unpublished. The caller owns the output and must serialize writes to it.

The global options are:

```console
--config-dir <DIR>
--data-dir <DIR>
--jobs <N>
```

`--config-dir` and `--data-dir` must appear together or not at all. Relative option values use the current working directory. `..` components are invalid. Clap checks the paired options and rejects `--jobs 0`.

The default child-process limit is four.

## Directories

Without overrides, YAASM uses the same XDG-style layout on every platform, including Windows:

```text
config dir = $XDG_CONFIG_HOME/yaasm
             or $HOME/.config/yaasm

data dir   = $XDG_DATA_HOME/yaasm
             or $HOME/.local/share/yaasm
```

The selected directories have this layout:

```text
config-dir/
  config.toml
  lock.toml
  .process.lock

data-dir/
  root.toml
  .process.lock
  repos/<source>/
  snapshots/<snapshot-hash>/
  skills/<skill>
  .staging/<unique-id>/
```

`root.toml` contains the data layout version. YAASM initializes a missing or empty data directory. It refuses to take ownership of a non-empty directory without a supported marker. A marker from a newer format also causes a global error.

The configuration and data directories must be different and cannot contain each other. YAASM uses the system's default permissions and ACL inheritance for its workspace and links. Bundle objects and manifests have mode 0644 on Unix, with directories at 0755. Imported files restore Git executable bits with mode 0755 or 0644. Existing published content is immutable.

Different config/data pairs are independent. A link into another profile's data directory is not managed by the current invocation and causes a normal collision if it uses a desired name.

## Configuration

`config.toml` has a required format version. All collections may be empty.

```toml
version = 1
targets = [
  ".agents/skills",
  "${YAASM_CONFIG_DIR}/codex-skills",
]

[sources.foo]
github = "owner/repo"
ref = "main"

[sources.other]
git = "ssh://git@example.com/owner/repo.git"
ref = "feature/foo"

[skills.bar1]
source = "foo"
path = "skills/bar"

[skills.bar2]
source = "local"
path = "${YAASM_CONFIG_DIR}/skills/bar2"
```

Each source has exactly one of `github` or `git`. `github` uses `owner/repo` syntax. `ref` is optional and defaults to the remote HEAD. YAASM passes ref resolution to Git or the remote instead of defining its own branch and tag precedence.

`local` is a reserved source name and cannot appear under `[sources]`. A local skill uses the same `source` and `path` fields as a network skill.

The configuration key under `[skills]` is the skill name. It is also the entry name under `data-dir/skills` and every target. Source and skill names must match `[A-Za-z0-9][A-Za-z0-9._-]*`. Names are unique under ASCII case folding on all platforms.

Unknown versions, unknown fields, duplicate names, and case-folded name collisions are global configuration errors.

### Path rules

Target paths may be absolute or relative to the user's home directory. YAASM creates missing target directories recursively.

Network skill paths are literal repository-relative paths. They cannot be absolute, contain `..`, or escape the repository.
Both `path = "."` and `path = ""` select the repository root.

Target and local skill paths use `shellexpand::env_with_context`, which supports `$VAR`, `${VAR}`, and `${VAR:-default}`. YAASM provides `YAASM_CONFIG_DIR`; all other names come from the process environment. Tilde expansion is disabled. Expanded relative targets remain relative to the user's home directory. An expanded local skill path must be absolute.

Every selected skill directory must contain a regular `SKILL.md` file.

YAASM compares target roots, local skill roots, and the internal `repos`, `snapshots`, `skills`, and `.staging` roots as one path set. No two entries may be equal or contain each other. Existing paths are canonicalized before comparison. Missing paths resolve their nearest existing ancestor before appending the missing components. Offline deployment forbids any overlap between the bundle root and the data directory, local skill roots, or targets. This also forbids two local skill names from referring to the same directory.

## Lockfile

`lock.toml` is generated. YAASM sorts source names before serialization.

```toml
version = 1

[sources.foo]
fingerprint = "sha256:<digest>"
revision = "<git-object-id>"
```

Every declared network source must be resolved and locked, including an unused source. A failed first resolution may leave that source absent from the lockfile, makes the command fail, and is retried on the next run. An empty source set produces a lockfile containing `version = 1`.

The fingerprint identifies the source request. YAASM hashes a stable length-prefixed encoding of:

```text
fingerprint schema version
source kind
repository
ref, including an absent marker
```

The source name is not part of the fingerprint. An empty explicit ref is invalid. `revision` is the commit object ID returned by Git; YAASM does not assume a specific Git object format.

The fingerprint and revision have separate jobs. The fingerprint detects a changed source declaration. The revision identifies the locked Git commit.

YAASM serializes the complete result after the source-resolution barrier and commits it with `atomic-write-file`. A failed commit leaves the old lockfile intact. Published but unreferenced snapshots remain inert and are cleaned on a later run.

`deploy --locked` requires a matching entry for every declared network source and rejects extra entries for deleted sources. It does not write the lockfile. It may fetch a locked revision to repair a missing local cache or snapshot, so it is not an offline mode.

## Network sources

Online source preparation requires external Git with `git clone --revision` support. GitHub sources use Git over HTTPS. The clone requests a configured or locked revision directly; an existing cache uses `git fetch`.

Child processes are non-interactive. YAASM sets `GIT_TERMINAL_PROMPT=0`, while preserving configured credential helpers, SSH agents, and token environment variables. On Unix, it also gives OpenSSH a per-process control socket under the temporary directory so sibling Git commands can reuse connections. An existing `GIT_SSH_COMMAND` takes precedence. YAASM captures and labels child stderr instead of allowing concurrent commands to write directly to the terminal.

Repository caches use shallow, blobless partial clones. Fetches request only the needed ref at depth one. Snapshot materialization fetches the blobs needed by the configured paths. Git handles servers that do not support partial clone.

YAASM treats source content as trusted. It preserves symlinks without checking their destinations. It protects its own extraction destination against path traversal but does not audit what an agent may reach after deployment.

The selected tree contains only Git-tracked content. It excludes `.git`, preserves executable bits and symlinks, and does not preserve source timestamps. Git LFS pointers remain pointer files. A Git submodule anywhere inside a selected skill tree makes that source revision undeployable.

### Repository cache recovery

The repository cache is disposable:

- A missing cache is cloned.
- A path that is not a valid Git repository is moved into staging and rebuilt.
- A cache whose origin differs from the configured repository is moved into staging and rebuilt.
- An object-corruption error fails the source update. YAASM does not reinterpret network, authentication, or ref failures as cache corruption.

## Snapshots

A snapshot is one partial repository tree shared by every selected skill from a source. It retains repository-relative paths, but each data entry points directly to its configured path. Agents therefore see the selected directory as the skill root.

YAASM derives the snapshot directory name at runtime:

```text
snapshot-hash = SHA-256(
  source fingerprint,
  locked revision,
  normalized selected network paths
)
```

The selected path list is sorted and deduplicated. A child path covered by an already selected parent path does not add another input. The snapshot hash is not stored in the lockfile.

Each snapshot is prepared and validated under `.staging`, then renamed into `snapshots/<snapshot-hash>`. YAASM never mutates a published snapshot. An unchanged input reuses the existing directory.

An unused network source has a repository cache and lock entry but no snapshot.

## Local sources

Local sources are live and do not appear in the lockfile. Deployment creates this chain:

```text
target/<skill> -> data-dir/skills/<skill> -> local directory
```

Changes in the local directory are visible immediately. If the directory disappears or loses its regular `SKILL.md`, deployment reports a skill failure and preserves the previous links.

On Windows, a junction to an unsupported location such as a network share fails. YAASM reports the platform error and does not copy the source or fall back to another link type.

## Source resolution

Explicit updates and automatic lock reconciliation use the same source task.

`deploy` resolves entries that are missing or whose fingerprint differs from the current source declaration. `update` forces resolution of every declared network source or the names passed on the command line.

Each source task:

1. Fetches and resolves the requested ref.
2. Builds its source-level partial snapshot in staging when configured skills reference it.
3. Checks every referenced skill path, regular `SKILL.md`, and unsupported submodule.
4. Publishes the snapshot under its derived hash.
5. Returns a proposed fingerprint and revision.

A revision is deployable only if all skills that reference the source pass these checks. A failed source keeps its old lock entry. If that entry's fingerprint still matches the current declaration, deployment may continue with the old revision. A mismatched old entry cannot supply the changed source, so its downstream skills keep their previous links and report failures.

Sources are siblings. A failure in one source does not roll back another successful source. After every source task finishes, YAASM sorts the final entries and writes the lock once.

`deploy` proceeds with deployment even after partial source failures. `update` ends after source preparation and the lock commit. A source update is complete when the lock commit succeeds; later deployment failures do not roll it back.

## Remote bundles

A bundle contains selected network skills and their locked identities. Local skills and targets are excluded. Construction does not expand deployment environment variables, inspect local skill directories, or touch targets. The same remote configuration and lockfile produce the same bundle file bytes and Unix modes across build locations.

The directory format is versioned independently of the configuration and lockfile:

```text
remote-bundle/
  bundle.toml
  objects/<sha256>
```

`bundle.toml` contains these sorted maps:

- `sources.<name>.locked`: the source fingerprint and locked revision, including unused sources.
- `sources.<name>.files.<repository-relative-path>`: `kind` (`file`, `executable`, or `symlink`) and the object's lowercase hexadecimal `sha256` digest.
- `skills.<name>`: the network source name and normalized repository-relative path. The empty path selects the repository root.

All objects are regular files. File objects contain raw Git blob bytes; symlink objects contain the original link target bytes. Git checkout filters, line-ending conversion, and the build machine's symlink support do not affect the format. Repository URLs are represented by fingerprints rather than copied into the manifest. Bundle metadata contains no absolute build paths, timestamps, or deployment links.

Selected trees must have regular tracked `SKILL.md` files and no Git submodules. Bundle paths use UTF-8 and forward slashes. YAASM rejects parent components, Windows reserved names and filename characters, trailing dots or spaces, ASCII case conflicts, and files used as parent directories. Invalid paths fail the build rather than being renamed. The destination filesystem can impose further filename and path-length restrictions; restoration errors fail the source. Git LFS pointers remain pointer files.

### Offline import and lifetime

`deploy --bundle DIR` deploys strictly offline without invoking child processes or changing the lockfile. It reads the deployment machine's current configuration and lockfile. The source names, fingerprints, revisions, and network skill names and paths must match the manifest exactly. Local skills and targets may differ between machines or invocations. Unsupported format versions and identity mismatches stop deployment globally before changing skill links. Missing or stale lock entries also fail; YAASM never resolves them from a remote.

For each source, YAASM validates its file map, reads every referenced object, and verifies its SHA-256 digest. This check runs even when a previous import exists. A missing object, invalid path, corrupt object, or unsupported symlink fails that source without network recovery. Other sources and live local skills can still deploy. Failed skills retain their previous links, and the existing target cleanup barriers and data cleanup ordering apply.

YAASM imports remote contents into immutable snapshots in its data directory. It treats the bundle as a read-only input. After successful deployment, the caller may move or delete the bundle without breaking deployed skills. Every later `deploy --bundle` invocation still requires its bundle input. The caller owns bundle storage, transport, and removal. Forced process termination during construction can leave a `.yaasm-bundle-*` directory beside the requested output. The caller may remove this unpublished content.

The manifest is trusted metadata, not a signature or proof of origin. Object hashes detect damaged payloads but do not authenticate their origin. Callers must leave published data snapshots unchanged.

### Symlinks and Windows

Unix imports preserve upstream symlink target bytes verbatim and restore executable bits. Relative links retain their repository layout. Absolute links and links outside selected trees can depend on files outside the bundle; YAASM does not rewrite or dereference them. Their destinations must exist on the deployment machine if the skill needs them.

Windows import rejects sources containing upstream symlinks. Regular skill contents import without symlink privileges. Git executable metadata has no Unix mode equivalent on Windows.

### Read-only inputs

Configuration and bundle inputs may be read-only, including paths in a Nix store. The data directory must be writable. Callers must keep inputs stable as specified under [Concurrency and interruption](#concurrency-and-interruption). YAASM has no Nix runtime dependency. The [sandbox build instructions](../README.md#use-immutable-build-inputs) use separate input and workspace directories.

## Deployment

Deployment first establishes each stable data entry:

```text
network: data-dir/skills/<skill> -> snapshots/<snapshot-hash>/<path>
local:   data-dir/skills/<skill> -> expanded local path
```

It then establishes each placement:

```text
target/<skill> -> data-dir/skills/<skill>
```

YAASM uses absolute link targets. Unix uses directory symlinks. Windows uses junctions.

Windows replacement moves the previous junction aside and restores it if publication fails. If restoration also fails, the error reports the preserved junction's path.

An entry in a configured target is managed when it is a link whose immediate target lies inside the current data directory. YAASM scans only direct target children and never traverses deployed skills. A desired name occupied by a regular file, directory, or link outside the current data directory causes a placement failure and is never overwritten.

Failures follow the dependency chain `source -> skill -> placement`. Siblings remain independent. A successful skill may deploy to one target even when its placement in another target fails.

An existing skill keeps its last working data and target links when a new configuration cannot deploy it. A new skill that has never deployed gets no link.

### Cleanup

Each target has a cleanup barrier. YAASM removes stale managed links from a target only if every configured placement for that target succeeded in the current run. This includes broken and oddly named links into the current data directory.

YAASM cleans stale data entries, snapshots, and repository caches only after every current target completes its placement and cleanup step. Until then it retains old content. `.staging` is different: YAASM owns it and may clean abandoned work while holding the data directory lock.

Targets removed from configuration are not scanned or remembered. The user must clean their old YAASM links manually.

Empty collections behave as follows:

- No targets means no placement or target cleanup.
- No skills with configured targets removes every managed link from those targets after the empty placement set succeeds.
- No sources removes old source lock entries and eligible source data.
- A completely empty configuration succeeds and cleans eligible YAASM-owned data, but not removed targets.

## Preflight and failure isolation

YAASM completes structural preflight before starting network or link work. TOML parse errors, unsupported versions, unknown fields, duplicate or case-conflicting names, path-set overlap, lock parse errors, and unsupported data markers stop the command globally.

Errors that belong to a source, skill, or placement stay on that branch. Examples include network failure, an unknown source referenced by one skill, a missing skill path, a missing `SKILL.md`, and a collision in one target.

The command prints progress as tasks finish and sorts its final summary by name. Any failure produces a nonzero result even when sibling work succeeded.

Exit codes are:

```text
0    all requested work succeeded
1    configuration, I/O, network, collision, or partial failure
2    command-line usage error
130  interrupted by Ctrl-C
```

## Concurrency and interruption

YAASM uses a Tokio multi-thread runtime. Independent futures run as tasks. A semaphore limits active Git child processes to `--jobs`, which defaults to four. Snapshot creation is one directory-level task, not one task per file.

Placement work may run concurrently. Directory scans and cleanup run after their target barriers. Lock sorting, serialization, and writing run after the source barrier.

All commands take a non-blocking exclusive OS lock on the data directory. `deploy`, including `--locked`, and `update` also lock the configuration directory. `bundle` and `deploy --bundle` read configuration and lockfile without locking or modifying their directory, so callers must keep inputs stable during these commands. Contention causes the command to fail before source or link work. The files named `.process.lock` persist after their locks are released; their presence alone does not indicate a running process.

On Ctrl-C, the CLI exits with 130 and releases its file locks. Termination of child processes and their descendants is not guaranteed. The next invocation removes abandoned data staging contents.

## Tests

CI runs formatting, Clippy, and tests on Linux, macOS, and Windows. Integration tests use temporary directories and local Git repositories without public network access or a GitHub account. Offline tests remove Git from PATH or replace Git and gh with failing sentinels.
