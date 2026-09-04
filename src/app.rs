use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::cli::{Cli, Command};
use crate::config::{Config, LOCAL_SOURCE};
use crate::links::{clean_target, establish_owned_link, establish_placement, link_destination};
use crate::lockfile::{LockedSource, Lockfile, fingerprint};
use crate::output::{Output, SkillChange};
use crate::paths::{AppPaths, absolute_without_parent, home_dir, reject_overlap};
use crate::process_lock::ProcessLocks;
use crate::source::{MaterializedSource, SourceMaterializer};

const DATA_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DataMarker {
    version: u32,
}

#[derive(Debug)]
struct RuntimePaths {
    targets: Vec<PathBuf>,
    locals: BTreeMap<String, PathBuf>,
}

#[derive(Debug)]
struct SourceOutcome {
    name: String,
    fingerprint: String,
    materialized: Result<MaterializedSource>,
    updates_lock: bool,
}

pub async fn run(cli: Cli) -> Result<()> {
    let output = Output::new();
    let result = run_inner(cli, &output).await;
    if result.is_err() {
        output.finish();
    }
    result
}

async fn run_inner(cli: Cli, output: &Output) -> Result<()> {
    output.phase("Preparing workspace");
    let mut paths = AppPaths::resolve(cli.config_dir, cli.data_dir)?;
    tokio::fs::create_dir_all(&paths.config_dir).await?;
    tokio::fs::create_dir_all(&paths.data_dir).await?;
    paths.config_dir = tokio::fs::canonicalize(&paths.config_dir).await?;
    paths.data_dir = tokio::fs::canonicalize(&paths.data_dir).await?;
    reject_overlap(&paths.config_dir, &paths.data_dir)?;
    let _locks = ProcessLocks::acquire(&paths.config_dir, &paths.data_dir)?;
    initialize_data_dir(&paths.data_dir).await?;
    clean_staging(&paths.data_dir).await?;

    output.phase("Reading configuration");
    let config = Config::load(&paths.config_dir.join("config.toml")).await?;
    let runtime = resolve_runtime_paths(&config, &paths).await?;
    let mut lock = Lockfile::load_if_exists(&paths.config_dir.join("lock.toml")).await?;

    let (locked, forced) = command_policy(&cli.command, &config)?;
    let materializer = SourceMaterializer::new(paths.data_dir.clone(), cli.jobs);
    let paths_by_source = network_paths(&config);

    if locked {
        validate_locked(&config, &lock)?;
    }

    output.phase("Resolving sources");
    let mut ready = BTreeMap::new();
    let mut tasks = JoinSet::new();
    for (name, source) in &config.sources {
        let source_fingerprint = fingerprint(source)?;
        let selected = paths_by_source.get(name).cloned().unwrap_or_default();
        let old = lock.sources.get(name);
        let must_update = forced.contains(name)
            || old.is_none_or(|entry| entry.fingerprint != source_fingerprint);

        if !must_update
            && let Some(entry) = old
            && let Some(snapshot) =
                materializer.snapshot_path(&source_fingerprint, &entry.revision, &selected)
            && tokio::fs::try_exists(&snapshot).await?
        {
            ready.insert(
                name.clone(),
                MaterializedSource {
                    revision: entry.revision.clone(),
                    snapshot: Some(snapshot),
                },
            );
            continue;
        }
        if !must_update
            && selected.is_empty()
            && let Some(entry) = old
        {
            ready.insert(
                name.clone(),
                MaterializedSource {
                    revision: entry.revision.clone(),
                    snapshot: None,
                },
            );
            continue;
        }

        let worker = materializer.clone();
        let progress = output.source(name);
        let name = name.clone();
        let source = source.clone();
        let requested_revision = (!must_update)
            .then(|| old.map(|entry| entry.revision.clone()))
            .flatten();
        let rebuild_repository =
            must_update && old.is_none_or(|entry| entry.fingerprint != source_fingerprint);
        tasks.spawn(async move {
            let materialized = worker
                .resolve(
                    &name,
                    &source,
                    &source_fingerprint,
                    requested_revision.as_deref(),
                    &selected,
                    rebuild_repository,
                )
                .await;
            progress.finish_and_clear();
            SourceOutcome {
                name,
                fingerprint: source_fingerprint,
                materialized,
                updates_lock: must_update,
            }
        });
    }

    let mut errors = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let outcome = joined.context("source task panicked")?;
        match outcome.materialized {
            Ok(materialized) => {
                if outcome.updates_lock {
                    lock.sources.insert(
                        outcome.name.clone(),
                        LockedSource {
                            fingerprint: outcome.fingerprint,
                            revision: materialized.revision.clone(),
                        },
                    );
                }
                ready.insert(outcome.name, materialized);
            }
            Err(error) => {
                errors.push(format!("source `{}`: {error:#}", outcome.name));
                if let Some(old) = lock.sources.get(&outcome.name)
                    && old.fingerprint == outcome.fingerprint
                    && let Some(snapshot) = materializer.snapshot_path(
                        &old.fingerprint,
                        &old.revision,
                        paths_by_source
                            .get(&outcome.name)
                            .map_or(&[], Vec::as_slice),
                    )
                    && tokio::fs::try_exists(&snapshot).await?
                {
                    ready.insert(
                        outcome.name,
                        MaterializedSource {
                            revision: old.revision.clone(),
                            snapshot: Some(snapshot),
                        },
                    );
                }
            }
        }
    }

    if !locked {
        lock.sources
            .retain(|name, _| config.sources.contains_key(name));
        write_lock(&paths.config_dir.join("lock.toml"), lock.to_toml()?).await?;
    }

    output.phase(format!("Deploying {} skills", config.skills.len()));
    let skill_results = establish_skill_entries(&config, &runtime, &paths, &ready).await;
    for (name, result) in &skill_results {
        if let Err(error) = result {
            errors.push(format!("skill `{name}`: {error:#}"));
        }
    }

    let targets_clean =
        deploy_targets(&config, &runtime, &paths, &skill_results, &mut errors).await;
    let mut removed = BTreeSet::new();
    if targets_clean && errors.is_empty() {
        output.phase("Cleaning up");
        removed = clean_data(&config, &paths, &ready).await?;
    }

    if errors.is_empty() {
        let mut added = BTreeSet::new();
        let mut updated = BTreeSet::new();
        for (name, result) in skill_results {
            match result? {
                SkillChange::Added => {
                    added.insert(name);
                }
                SkillChange::Updated => {
                    updated.insert(name);
                }
                SkillChange::Unchanged => {}
            }
        }
        output.summary(&added, &updated, &removed);
        Ok(())
    } else {
        errors.sort();
        bail!("{}", errors.join("\n"));
    }
}

fn command_policy(command: &Command, config: &Config) -> Result<(bool, BTreeSet<String>)> {
    match command {
        Command::Deploy { locked } => Ok((*locked, BTreeSet::new())),
        Command::Update { sources } => {
            let forced = if sources.is_empty() {
                config.sources.keys().cloned().collect()
            } else {
                let requested: BTreeSet<_> = sources.iter().cloned().collect();
                for name in &requested {
                    if name == LOCAL_SOURCE || !config.sources.contains_key(name) {
                        bail!("cannot update unknown or local source `{name}`");
                    }
                }
                requested
            };
            Ok((false, forced))
        }
    }
}

fn validate_locked(config: &Config, lock: &Lockfile) -> Result<()> {
    for (name, source) in &config.sources {
        let entry = lock
            .sources
            .get(name)
            .with_context(|| format!("lockfile has no entry for source `{name}`"))?;
        if entry.fingerprint != fingerprint(source)? {
            bail!("lockfile entry for source `{name}` does not match configuration");
        }
    }
    for name in lock.sources.keys() {
        if !config.sources.contains_key(name) {
            bail!("lockfile contains deleted source `{name}`");
        }
    }
    Ok(())
}

fn network_paths(config: &Config) -> BTreeMap<String, Vec<String>> {
    let mut result: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for skill in config.skills.values() {
        if skill.source != LOCAL_SOURCE {
            result
                .entry(skill.source.clone())
                .or_default()
                .push(skill.path.clone());
        }
    }
    result
}

async fn initialize_data_dir(data_dir: &Path) -> Result<()> {
    let existed = tokio::fs::try_exists(data_dir).await?;
    if !existed {
        tokio::fs::create_dir_all(data_dir).await?;
    }
    let marker_path = data_dir.join("root.toml");
    if tokio::fs::try_exists(&marker_path).await? {
        let input = tokio::fs::read_to_string(&marker_path).await?;
        let marker: DataMarker = toml::from_str(&input).context("invalid data root marker")?;
        if marker.version != DATA_VERSION {
            bail!("unsupported data layout version {}", marker.version);
        }
    } else {
        let mut entries = tokio::fs::read_dir(data_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_name() != ".process.lock" {
                bail!(
                    "refusing non-empty unmarked data directory `{}`",
                    data_dir.display()
                );
            }
        }
        tokio::fs::write(
            &marker_path,
            toml::to_string(&DataMarker {
                version: DATA_VERSION,
            })?,
        )
        .await?;
    }
    for child in ["repos", "snapshots", "skills", ".staging"] {
        tokio::fs::create_dir_all(data_dir.join(child)).await?;
    }
    Ok(())
}

async fn clean_staging(data_dir: &Path) -> Result<()> {
    let staging = data_dir.join(".staging");
    if tokio::fs::try_exists(&staging).await? {
        tokio::fs::remove_dir_all(&staging).await?;
    }
    tokio::fs::create_dir_all(staging).await?;
    Ok(())
}

async fn resolve_runtime_paths(config: &Config, paths: &AppPaths) -> Result<RuntimePaths> {
    let home = home_dir()?;
    let config_value = paths.config_dir.to_string_lossy();
    let mut targets = Vec::new();
    for target in &config.targets {
        let expanded = expand_config_path(&target.to_string_lossy(), &config_value)
            .with_context(|| format!("failed to expand target `{}`", target.display()))?;
        targets.push(
            absolute_without_parent(&home, &expanded)
                .with_context(|| format!("invalid expanded target `{}`", target.display()))?,
        );
    }

    let mut locals = BTreeMap::new();
    for (name, skill) in &config.skills {
        if skill.source != LOCAL_SOURCE {
            continue;
        }
        let path = expand_config_path(&skill.path, &config_value)
            .with_context(|| format!("failed to expand local skill `{name}`"))?;
        if !path.is_absolute() {
            bail!("expanded path for local skill `{name}` must be absolute");
        }
        crate::config::reject_parent_components(&path)
            .with_context(|| format!("invalid expanded path for local skill `{name}`"))?;
        locals.insert(name.clone(), path);
    }

    let mut pool = Vec::new();
    pool.extend(targets.iter().cloned());
    pool.extend(locals.values().cloned());
    for internal in ["repos", "snapshots", "skills", ".staging"] {
        pool.push(paths.data_dir.join(internal));
    }
    for path in &mut pool {
        if tokio::fs::try_exists(&path).await? {
            *path = tokio::fs::canonicalize(&path).await?;
        }
    }
    for left in 0..pool.len() {
        for right in (left + 1)..pool.len() {
            reject_overlap(&pool[left], &pool[right])?;
        }
    }

    Ok(RuntimePaths { targets, locals })
}

fn expand_config_path(path: &str, config_dir: &str) -> Result<PathBuf> {
    let expanded = shellexpand::env_with_context(path, |variable| {
        if variable == "YAASM_CONFIG_DIR" {
            return Ok::<_, env::VarError>(Some(config_dir.to_owned()));
        }
        match env::var(variable) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(error) => Err(error),
        }
    })?;
    Ok(PathBuf::from(expanded.as_ref()))
}

async fn establish_skill_entries(
    config: &Config,
    runtime: &RuntimePaths,
    paths: &AppPaths,
    ready: &BTreeMap<String, MaterializedSource>,
) -> BTreeMap<String, Result<SkillChange>> {
    let mut tasks = JoinSet::new();
    for (name, skill) in &config.skills {
        let name = name.clone();
        let skill = skill.clone();
        let link = paths.data_dir.join("skills").join(&name);
        let destination = if skill.source == LOCAL_SOURCE {
            runtime.locals.get(&name).cloned()
        } else {
            ready
                .get(&skill.source)
                .and_then(|source| source.snapshot.as_ref())
                .map(|snapshot| snapshot.join(&skill.path))
        };
        tasks.spawn(async move {
            let result = match destination {
                Some(destination) => establish_skill_entry(&link, &destination).await,
                None => Err(anyhow::anyhow!(
                    "source `{}` is not deployable",
                    skill.source
                )),
            };
            (name, result)
        });
    }
    let mut results = BTreeMap::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((name, result)) => {
                results.insert(name, result);
            }
            Err(error) => {
                results.insert("<task>".into(), Err(error.into()));
            }
        }
    }
    results
}

async fn establish_skill_entry(link: &Path, destination: &Path) -> Result<SkillChange> {
    validate_local_skill(destination).await?;
    let previous = link_destination(link).await?;
    establish_owned_link(link, destination).await?;
    Ok(match previous {
        None => SkillChange::Added,
        Some(previous) if previous == destination => SkillChange::Unchanged,
        Some(_) => SkillChange::Updated,
    })
}

async fn validate_local_skill(path: &Path) -> Result<()> {
    let metadata = tokio::fs::symlink_metadata(path.join("SKILL.md"))
        .await
        .with_context(|| format!("`{}` has no SKILL.md", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("`{}` has no regular SKILL.md", path.display());
    }
    Ok(())
}

async fn deploy_targets(
    config: &Config,
    runtime: &RuntimePaths,
    paths: &AppPaths,
    skill_results: &BTreeMap<String, Result<SkillChange>>,
    errors: &mut Vec<String>,
) -> bool {
    let desired: BTreeSet<_> = config.skills.keys().cloned().collect();
    let mut all_clean = true;
    for target in &runtime.targets {
        if let Err(error) = tokio::fs::create_dir_all(target).await {
            errors.push(format!("target `{}`: {error}", target.display()));
            all_clean = false;
            continue;
        }
        let mut complete = true;
        for name in config.skills.keys() {
            if skill_results.get(name).is_none_or(Result::is_err) {
                complete = false;
                continue;
            }
            let destination = paths.data_dir.join("skills").join(name);
            if let Err(error) =
                establish_placement(&target.join(name), &destination, &paths.data_dir).await
            {
                errors.push(format!(
                    "placement `{}` in `{}`: {error:#}",
                    name,
                    target.display()
                ));
                complete = false;
            }
        }
        if complete {
            if let Err(error) = clean_target(target, &desired, &paths.data_dir).await {
                errors.push(format!("target cleanup `{}`: {error:#}", target.display()));
                all_clean = false;
            }
        } else {
            all_clean = false;
        }
    }
    all_clean
}

async fn clean_data(
    config: &Config,
    paths: &AppPaths,
    ready: &BTreeMap<String, MaterializedSource>,
) -> Result<BTreeSet<String>> {
    let removed = clean_named_children(
        &paths.data_dir.join("skills"),
        &config.skills.keys().cloned().collect(),
    )
    .await?;
    clean_named_children(
        &paths.data_dir.join("repos"),
        &config.sources.keys().cloned().collect(),
    )
    .await?;
    let snapshots = ready
        .values()
        .filter_map(|source| {
            source
                .snapshot
                .as_ref()?
                .file_name()?
                .to_str()
                .map(str::to_owned)
        })
        .collect();
    clean_named_children(&paths.data_dir.join("snapshots"), &snapshots).await?;
    Ok(removed)
}

async fn clean_named_children(
    directory: &Path,
    keep: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut removed = BTreeSet::new();
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        if keep.contains(entry.file_name().to_string_lossy().as_ref()) {
            continue;
        }
        let file_type = entry.file_type().await?;
        if file_type.is_dir() && !file_type.is_symlink() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
        }
        removed.insert(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(removed)
}

async fn write_lock(path: &Path, contents: String) -> Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut file = AtomicWriteFile::options()
            .open(&path)
            .with_context(|| format!("failed to open lockfile `{}`", path.display()))?;
        file.write_all(contents.as_bytes())?;
        file.commit().context("failed to commit lockfile")?;
        Ok(())
    })
    .await
    .context("lockfile writer task failed")??;
    Ok(())
}
