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
yaasm update
yaasm update foo bar
```

`deploy` fills missing or stale lock entries, then reconciles data entries and targets. `deploy --locked` forbids lock changes.

`update` resolves every declared network source. Source arguments restrict the forced updates to those names. After source resolution, it runs the same deployment phase as `deploy`. Local sources cannot be update arguments.

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

The configuration and data directories must be different and cannot contain each other. YAASM uses the system's default permissions and ACL inheritance. It does not rewrite permissions on existing files or directories.

Different config/data pairs are independent. A link into another profile's data directory is not managed by the current invocation and causes a normal collision if it uses a desired name.

## Configuration

`config.toml` has a required format version. All collections may be empty.

```toml
version = 1
targets = [
  ".agents/skills",
  ".codex/skills",
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

Target paths may be absolute or relative to the user's home directory. YAASM creates missing target directories recursively. Targets do not support variable expansion.

Network skill paths are literal repository-relative paths. They cannot be absolute, contain `..`, or escape the repository.

Only local skill paths use variable expansion. YAASM uses `shellexpand::env_with_context`, which supports `$VAR`, `${VAR}`, and `${VAR:-default}`. YAASM provides `YAASM_CONFIG_DIR`; all other names come from the process environment. Tilde expansion is disabled. The expanded local path must be absolute.

Every selected skill directory must contain a regular `SKILL.md` file.

YAASM compares target roots, local skill roots, and the internal `repos`, `snapshots`, `skills`, and `.staging` roots as one path set. No two entries may be equal or contain each other. Existing paths are canonicalized before comparison. Missing targets use normalized absolute paths. This also forbids two local skill names from referring to the same directory.

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

The external `git` command is required. `gh` is optional.

For a GitHub source, YAASM uses `gh repo clone` for the initial clone when `gh` exists and is authenticated. It checks authentication lazily and at most once per run, so existing caches do not invoke `gh`. Otherwise it falls back to Git. The clone requests a configured or locked revision directly; an existing cache uses `git fetch` because GitHub CLI has no general fetch operation for this workflow.

Child processes are non-interactive. YAASM sets `GIT_TERMINAL_PROMPT=0` and `GH_PROMPT_DISABLED=1`, while preserving configured credential helpers, SSH agents, and token environment variables. On Unix, it also gives OpenSSH a per-process control socket under the temporary directory so sibling Git commands can reuse connections. An existing `GIT_SSH_COMMAND` takes precedence. YAASM captures and labels child stderr instead of allowing concurrent commands to write directly to the terminal.

Repository caches use shallow, blobless partial clones. Fetches request only the needed ref at depth one. Snapshot materialization fetches the blobs needed by the configured paths. Git handles servers that do not support partial clone.

YAASM treats source content as trusted. It preserves symlinks without checking their destinations. It protects its own extraction destination against path traversal but does not audit what an agent may reach after deployment.

The selected tree contains only Git-tracked content. It excludes `.git`, preserves executable bits and symlinks, and does not preserve source timestamps. Git LFS pointers remain pointer files. A Git submodule anywhere inside a selected skill tree makes that source revision undeployable.

### Repository cache recovery

The repository cache is disposable:

- A missing cache is cloned.
- A path that is not a valid Git repository is moved into staging and rebuilt.
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

Regardless of partial source failures, YAASM then runs deployment. A source update is complete when the lock commit succeeds; later deployment failures do not roll it back.

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

An entry in a configured target is managed when it is a link whose immediate target lies inside the current data directory. YAASM scans only direct target children and never traverses deployed skills. A desired name occupied by a regular file, directory, or link outside the current data directory causes a placement failure and is never overwritten.

Failures follow the dependency chain `source -> skill -> placement`. Siblings remain independent. A successful skill may deploy to one target even when its placement in another target fails.

An existing skill keeps its last working data and target links when a new configuration cannot deploy it. A new skill that has never deployed gets no link.

### Cleanup

Each target has a cleanup barrier. YAASM removes stale managed links from a target only if every configured placement for that target succeeded in the current run. This includes broken and oddly named links into the current data directory.

YAASM cleans stale data entries, snapshots, and repository caches only after every current target completes its placement and cleanup step. Until then it retains old content. `.staging` is different: YAASM owns it and may clean abandoned work while holding both process locks.

Targets removed from configuration are not scanned or remembered. The user must clean their old YAASM links manually.

Empty collections behave as follows:

- No targets means no placement or target cleanup.
- No skills with configured targets removes every managed link from those targets after the empty placement set succeeds.
- No sources removes old source lock entries and eligible source data.
- A completely empty configuration succeeds and cleans eligible YAASM-owned data, but not removed targets.

## Preflight and failure isolation

YAASM completes structural preflight before starting network or link work. TOML parse errors, unsupported versions, unknown fields, duplicate or case-conflicting names, path-set overlap, lock parse errors, and unsupported data markers stop the command globally.

Errors that belong to a source, skill, or placement stay on that branch. Examples include network failure, an unknown source referenced by one skill, a missing skill path, a missing `SKILL.md`, and a collision in one target.

The command prints progress as tasks finish and sorts its final summary by name. Child-process output has a size limit. Any failure produces a nonzero result even when sibling work succeeded.

Exit codes are:

```text
0    all requested work succeeded
1    configuration, I/O, network, collision, or partial failure
2    command-line usage error
130  interrupted by Ctrl-C
```

## Concurrency and interruption

YAASM uses a Tokio multi-thread runtime. Independent futures run as tasks. A semaphore limits active Git and gh process trees to `--jobs`, which defaults to four. Snapshot creation is one directory-level task, not one task per file.

Placement work may run concurrently. Directory scans and cleanup run after their target barriers. Lock sorting, serialization, and writing run after the source barrier.

YAASM takes non-blocking exclusive OS file locks on `.process.lock` in the configuration and data directories. It orders the normalized paths before acquiring them. If either `std::fs::File::try_lock` call reports contention, YAASM drops any acquired lock and exits. Lock files persist, but their presence does not mean a process owns the lock. Closing the handles releases the kernel locks. This requires Rust 1.89 or newer.

Git and gh run in Unix process groups or Windows Job Objects through `process-wrap`. On the first Ctrl-C, YAASM stops new work, cancels tasks waiting for permits, terminates active process trees, skips later commit and cleanup barriers, drops both file locks, and exits with 130. The next invocation removes abandoned staging data. A second Ctrl-C exits immediately.

## Tests

CI runs on Linux, macOS, and Windows. Integration tests use temporary config, data, and target directories plus local bare Git remotes. They do not depend on public network access or a real GitHub account. A fake `gh` executable verifies arguments, fallback, and non-interactive environment variables.

The required tests cover:

- Stable config parsing, fingerprinting, and sorted lock output
- Empty configurations and `--locked`
- Source process limits and future concurrency
- Source, skill, and placement failure isolation
- Snapshot identity and reuse after path-set changes
- Ctrl-C process-tree termination and lock integrity
- Unix symlinks and Windows junctions
- Target cleanup barriers and data cleanup ordering
- Paired process locks
- Local expansion and path overlap checks
- Repeated deployment without changes
