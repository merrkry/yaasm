use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::bundle::{Bundle, RemoteSkill};
use crate::cli::{Cli, Command};
use crate::collections::{LocalRule, discover_local_collection, parse_local_rule};
use crate::config::{CollectionConfig, Config, LOCAL_SOURCE};
use crate::links::{
    clean_target, establish_owned_link, establish_placement, link_destination, remove_link,
};
use crate::lockfile::{LockedSource, Lockfile, fingerprint};
use crate::output::{Output, SkillChange, SourceUpdateReport};
use crate::paths::{
    AppPaths, absolute_without_parent, canonicalize_with_missing, home_dir, reject_overlap,
    simplified,
};
use crate::process_lock::ProcessLocks;
use crate::source::{MaterializedSource, SourceMaterializer};

const DATA_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DataMarker {
    version: u32,
}

#[derive(Debug)]
struct LocalCollectionRuntime {
    index: usize,
    rules: Vec<LocalRule>,
}

#[derive(Debug)]
struct RuntimePaths {
    targets: Vec<PathBuf>,
    locals: BTreeMap<String, PathBuf>,
    local_collections: Vec<LocalCollectionRuntime>,
    reserved_canonical: Vec<PathBuf>,
}

#[derive(Debug)]
struct SourceOutcome {
    name: String,
    fingerprint: String,
    previous_revision: Option<String>,
    materialized: Result<MaterializedSource>,
    updates_lock: bool,
}

#[derive(Debug, Clone)]
enum SkillOrigin {
    Local(PathBuf),
    Remote { source: String, path: String },
}

#[derive(Debug)]
struct PreparedSources {
    ready: BTreeMap<String, MaterializedSource>,
    errors: Vec<String>,
    reports: Vec<SourceUpdateReport>,
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
    let read_only_config = matches!(
        cli.command,
        Command::Bundle { .. }
            | Command::Deploy {
                bundle: Some(_),
                ..
            }
    );
    if !read_only_config {
        tokio::fs::create_dir_all(&paths.config_dir).await?;
    }
    paths.config_dir = tokio::fs::canonicalize(&paths.config_dir).await?;
    paths.data_dir = canonicalize_with_missing(&paths.data_dir).await?;
    reject_overlap(&paths.config_dir, &paths.data_dir)?;
    let bundle_root = if let Command::Deploy {
        bundle: Some(root), ..
    } = &cli.command
    {
        let root = tokio::fs::canonicalize(root)
            .await
            .context("failed to open bundle directory")?;
        reject_overlap(&root, &paths.data_dir)?;
        Some(root)
    } else {
        None
    };
    tokio::fs::create_dir_all(&paths.data_dir).await?;
    paths.data_dir = tokio::fs::canonicalize(&paths.data_dir).await?;
    let _locks = if read_only_config {
        ProcessLocks::acquire_data(&paths.data_dir)?
    } else {
        ProcessLocks::acquire(&paths.config_dir, &paths.data_dir)?
    };
    initialize_data_dir(&paths.data_dir).await?;
    clean_staging(&paths.data_dir).await?;

    output.phase("Reading configuration");
    let config = Config::load(&paths.config_dir.join("config.toml")).await?;
    let runtime = if matches!(cli.command, Command::Deploy { .. }) {
        Some(resolve_runtime_paths(&config, &paths, bundle_root.as_deref()).await?)
    } else {
        None
    };
    let mut lock = Lockfile::load_if_exists(&paths.config_dir.join("lock.toml")).await?;

    if let Command::Bundle {
        output: destination,
    } = &cli.command
    {
        validate_locked(&config, &lock)?;
        let destination = absolute_without_parent(&env::current_dir()?, destination)?;
        let parent = tokio::fs::canonicalize(
            destination
                .parent()
                .context("bundle output has no parent")?,
        )
        .await?;
        let destination = parent.join(
            destination
                .file_name()
                .context("bundle output has no name")?,
        );
        reject_overlap(&destination, &paths.config_dir)?;
        reject_overlap(&destination, &paths.data_dir)?;
        output.phase("Building remote bundle");
        let materializer = SourceMaterializer::new(paths.data_dir.clone(), cli.jobs);
        Bundle::build(&destination, &config, &lock, &materializer).await?;
        output.finish();
        println!("Bundle written to {}", destination.display());
        return Ok(());
    }

    let (ready, bundle_skills, mut errors, update_reports) = if let Some(root) = &bundle_root {
        validate_locked(&config, &lock)?;
        let bundle = Bundle::load(root, &config, &lock).await?;
        let mut ready = BTreeMap::new();
        let mut errors = Vec::new();
        output.phase("Importing remote bundle (offline)");
        for name in bundle.source_names() {
            match bundle
                .import_source(root, &paths.data_dir, name, &config)
                .await
            {
                Ok(source) => {
                    ready.insert(name.clone(), source);
                }
                Err(error) => errors.push(format!("source `{name}`: {error:#}")),
            }
        }
        (ready, Some(bundle.skills().clone()), errors, Vec::new())
    } else {
        let prepared =
            prepare_sources(&config, &paths, &mut lock, &cli.command, cli.jobs, output).await?;
        (prepared.ready, None, prepared.errors, prepared.reports)
    };
    if matches!(cli.command, Command::Update { .. }) {
        for (name, skill) in &config.skills {
            if skill.source != LOCAL_SOURCE && !config.sources.contains_key(&skill.source) {
                errors.push(format!("skill `{name}`: unknown source `{}`", skill.source));
            }
        }
        for (index, collection) in config.collections.iter().enumerate() {
            if collection.source != LOCAL_SOURCE && !config.sources.contains_key(&collection.source)
            {
                errors.push(format!(
                    "collection #{}: unknown source `{}`",
                    index + 1,
                    collection.source
                ));
            }
        }
        if !errors.is_empty() {
            if !update_reports.is_empty() {
                output.update_summary(&update_reports);
            } else {
                output.finish();
            }
            errors.sort();
            bail!("{}", errors.join("\n"));
        }
        output.update_summary(&update_reports);
        return Ok(());
    }
    let runtime = runtime.context("deployment paths were not resolved")?;

    let (resolved_skills, collections_complete) = resolve_deploy_skills(
        &config,
        &runtime,
        &ready,
        bundle_skills.as_ref(),
        &mut errors,
    )
    .await;

    output.phase(format!("Deploying {} skills", resolved_skills.len()));
    let skill_results = establish_skill_entries(&resolved_skills, &paths, &ready).await;
    for (name, result) in &skill_results {
        if let Err(error) = result {
            errors.push(format!("skill `{name}`: {error:#}"));
        }
    }

    let targets_clean = deploy_targets(
        &resolved_skills,
        &runtime,
        &paths,
        &skill_results,
        collections_complete,
        &mut errors,
    )
    .await;
    let mut removed = BTreeSet::new();
    if targets_clean && collections_complete && errors.is_empty() {
        output.phase("Cleaning up");
        removed = clean_data(&config, &resolved_skills, &paths, &ready).await?;
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

async fn resolve_deploy_skills(
    config: &Config,
    runtime: &RuntimePaths,
    ready: &BTreeMap<String, MaterializedSource>,
    bundle_skills: Option<&BTreeMap<String, RemoteSkill>>,
    errors: &mut Vec<String>,
) -> (BTreeMap<String, SkillOrigin>, bool) {
    let mut buckets: BTreeMap<String, Vec<(String, SkillOrigin, String)>> = BTreeMap::new();
    let mut collections_complete = true;

    for (name, skill) in &config.skills {
        if skill.source == LOCAL_SOURCE {
            if let Some(local_path) = runtime.locals.get(name) {
                buckets.entry(name.to_ascii_lowercase()).or_default().push((
                    name.clone(),
                    SkillOrigin::Local(local_path.clone()),
                    format!("local skill `{name}`"),
                ));
            }
        } else if bundle_skills.is_none() {
            buckets.entry(name.to_ascii_lowercase()).or_default().push((
                name.clone(),
                SkillOrigin::Remote {
                    source: skill.source.clone(),
                    path: skill.path.clone(),
                },
                format!("skill `{name}` from `{}`", skill.source),
            ));
        }
    }

    for (index, collection) in config.collections.iter().enumerate() {
        if collection.source == LOCAL_SOURCE {
            continue;
        }
        if !config.sources.contains_key(&collection.source) {
            if bundle_skills.is_none() {
                errors.push(format!(
                    "collection #{}: unknown source `{}`",
                    index + 1,
                    collection.source
                ));
            }
            collections_complete = false;
        } else if !ready.contains_key(&collection.source) {
            collections_complete = false;
        }
    }

    if let Some(bundle_skills) = bundle_skills {
        for (name, skill) in bundle_skills {
            let is_explicit = config
                .skills
                .get(name)
                .is_some_and(|configured| configured.source != LOCAL_SOURCE);
            if !is_explicit && !ready.contains_key(&skill.source) {
                continue;
            }
            buckets.entry(name.to_ascii_lowercase()).or_default().push((
                name.clone(),
                SkillOrigin::Remote {
                    source: skill.source.clone(),
                    path: skill.path.clone(),
                },
                format!("bundle skill `{name}` from `{}`", skill.source),
            ));
        }
    } else {
        for (source_name, materialized) in ready {
            for (skill_name, skill_path) in &materialized.discovered_skills {
                buckets
                    .entry(skill_name.to_ascii_lowercase())
                    .or_default()
                    .push((
                        skill_name.clone(),
                        SkillOrigin::Remote {
                            source: source_name.clone(),
                            path: skill_path.clone(),
                        },
                        format!("collection skill `{skill_name}` from `{source_name}`"),
                    ));
            }
        }
    }

    // Collect every local collection candidate first so overlap checks are
    // symmetric across collection boundaries and order-independent.
    struct LocalCandidate {
        collection_index: usize,
        name: String,
        path: PathBuf,
        canonical: PathBuf,
    }
    let mut local_candidates = Vec::<LocalCandidate>::new();
    for local_collection in &runtime.local_collections {
        match discover_local_collection(&local_collection.rules).await {
            Ok(discovered) => {
                for (skill_name, skill_path) in discovered {
                    match canonicalize_with_missing(&skill_path).await {
                        Ok(canonical) => {
                            local_candidates.push(LocalCandidate {
                                collection_index: local_collection.index,
                                name: skill_name,
                                path: skill_path,
                                canonical,
                            });
                        }
                        Err(error) => {
                            errors.push(format!(
                                "local collection #{}: skill `{skill_name}`: {error:#}",
                                local_collection.index
                            ));
                            collections_complete = false;
                        }
                    }
                }
            }
            Err(error) => {
                errors.push(format!(
                    "local collection #{}: {error:#}",
                    local_collection.index
                ));
                collections_complete = false;
            }
        }
    }

    let mut reserved_pool: Vec<(PathBuf, String)> = runtime
        .reserved_canonical
        .iter()
        .cloned()
        .map(|path| (path, "configured path".to_owned()))
        .collect();
    for (name, path) in &runtime.locals {
        if let Ok(canonical) = canonicalize_with_missing(path).await {
            reserved_pool.push((canonical, format!("local skill `{name}`")));
        }
    }

    let mut rejected_local = vec![false; local_candidates.len()];
    for (index, candidate) in local_candidates.iter().enumerate() {
        if let Some((_, existing_label)) = reserved_pool
            .iter()
            .find(|(existing, _)| reject_overlap(existing, &candidate.canonical).is_err())
        {
            errors.push(format!(
                "local collection #{}: skill `{}` (`{}`) overlaps with {existing_label}",
                candidate.collection_index,
                candidate.name,
                candidate.path.display()
            ));
            rejected_local[index] = true;
            collections_complete = false;
        }
    }
    for left in 0..local_candidates.len() {
        for right in (left + 1)..local_candidates.len() {
            if reject_overlap(
                &local_candidates[left].canonical,
                &local_candidates[right].canonical,
            )
            .is_err()
            {
                errors.push(format!(
                    "local collection #{} skill `{}` (`{}`) overlaps with local collection #{} skill `{}` (`{}`)",
                    local_candidates[left].collection_index,
                    local_candidates[left].name,
                    local_candidates[left].path.display(),
                    local_candidates[right].collection_index,
                    local_candidates[right].name,
                    local_candidates[right].path.display(),
                ));
                rejected_local[left] = true;
                rejected_local[right] = true;
                collections_complete = false;
            }
        }
    }

    for (index, candidate) in local_candidates.into_iter().enumerate() {
        if rejected_local[index] {
            continue;
        }
        let label = format!(
            "local collection #{} skill `{}`",
            candidate.collection_index, candidate.name
        );
        buckets
            .entry(candidate.name.to_ascii_lowercase())
            .or_default()
            .push((candidate.name, SkillOrigin::Local(candidate.path), label));
    }

    let mut resolved = BTreeMap::new();
    for entries in buckets.into_values() {
        if entries.len() > 1 {
            let descriptions: Vec<_> = entries.iter().map(|(_, _, label)| label.as_str()).collect();
            errors.push(format!(
                "skill name conflict between {}",
                descriptions.join(" and ")
            ));
            collections_complete = false;
            continue;
        }
        let (name, origin, _) = entries.into_iter().next().unwrap();
        resolved.insert(name, origin);
    }

    (resolved, collections_complete)
}

async fn prepare_sources(
    config: &Config,
    paths: &AppPaths,
    lock: &mut Lockfile,
    command: &Command,
    jobs: u16,
    output: &Output,
) -> Result<PreparedSources> {
    let (locked, forced) = command_policy(command, config)?;
    let is_update = matches!(command, Command::Update { .. });
    let materializer = SourceMaterializer::new(paths.data_dir.clone(), jobs);
    let paths_by_source = network_paths(config);
    let collections_by_source = network_collections(config);
    let explicit_all_folded: BTreeMap<String, String> = config
        .skills
        .keys()
        .map(|name| (name.to_ascii_lowercase(), name.clone()))
        .collect();

    if locked {
        validate_locked(config, lock)?;
    }

    output.phase("Resolving sources");
    let mut ready = BTreeMap::new();
    let mut pending_locks: BTreeMap<String, (LockedSource, Option<String>)> = BTreeMap::new();
    let mut precomputed_old_discovered: BTreeMap<String, BTreeMap<String, String>> =
        BTreeMap::new();
    let mut tasks = JoinSet::new();
    for (name, source) in &config.sources {
        let source_fingerprint = fingerprint(source)?;
        let selected = paths_by_source.get(name).cloned().unwrap_or_default();
        let source_collections = collections_by_source.get(name).cloned().unwrap_or_default();
        let old = lock.sources.get(name);
        let previous_locked_revision = old.map(|entry| entry.revision.clone());
        let matching_old_revision = old
            .filter(|entry| entry.fingerprint == source_fingerprint)
            .map(|entry| entry.revision.clone());
        let must_update = forced.contains(name) || matching_old_revision.is_none();

        if !must_update
            && let Some(revision) = &matching_old_revision
            && let Ok(Some(cached)) = materializer
                .try_cached_source(
                    name,
                    source,
                    &source_fingerprint,
                    revision,
                    &selected,
                    &source_collections,
                    &explicit_all_folded,
                )
                .await
        {
            ready.insert(name.clone(), cached);
            continue;
        }

        let worker = materializer.clone();
        let progress = output.source(name);
        let name = name.clone();
        let source = source.clone();
        let requested_revision = (!must_update)
            .then_some(matching_old_revision.clone())
            .flatten();
        let rebuild_repository = must_update && matching_old_revision.is_none();
        // When the source URL or ref changes, read the old cached inventory
        // before `ensure_repository` quarantines the previous repository cache.
        if is_update
            && rebuild_repository
            && !source_collections.is_empty()
            && let Some(old_rev) = &previous_locked_revision
            && let Ok(Some(old_discovered)) = materializer
                .discover_skills_in_cached_repo(&name, old_rev, &source_collections)
                .await
        {
            precomputed_old_discovered.insert(name.clone(), old_discovered);
        }
        let explicit_folded = explicit_all_folded.clone();
        tasks.spawn(async move {
            let materialized = worker
                .resolve(
                    &name,
                    &source,
                    &source_fingerprint,
                    requested_revision.as_deref(),
                    &selected,
                    &source_collections,
                    &explicit_folded,
                    rebuild_repository,
                )
                .await;
            progress.finish_and_clear();
            SourceOutcome {
                name,
                fingerprint: source_fingerprint,
                previous_revision: previous_locked_revision,
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
                    pending_locks.insert(
                        outcome.name.clone(),
                        (
                            LockedSource {
                                fingerprint: outcome.fingerprint,
                                revision: materialized.revision.clone(),
                            },
                            outcome.previous_revision,
                        ),
                    );
                }
                ready.insert(outcome.name, materialized);
            }
            Err(error) => {
                errors.push(format!("source `{}`: {error:#}", outcome.name));
                if let Some(old) = lock.sources.get(&outcome.name)
                    && old.fingerprint == outcome.fingerprint
                    && let Some(source) = config.sources.get(&outcome.name)
                {
                    let explicit_paths: &[String] = paths_by_source
                        .get(&outcome.name)
                        .map_or(&[], Vec::as_slice);
                    let source_collections: &[CollectionConfig] = collections_by_source
                        .get(&outcome.name)
                        .map_or(&[], Vec::as_slice);
                    if let Ok(Some(cached)) = materializer
                        .try_cached_source(
                            &outcome.name,
                            source,
                            &old.fingerprint,
                            &old.revision,
                            explicit_paths,
                            source_collections,
                            &explicit_all_folded,
                        )
                        .await
                    {
                        ready.insert(outcome.name, cached);
                    }
                }
            }
        }
    }

    // Reject cross-source discovered skill collisions before committing lock updates.
    let mut discovered_buckets: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (source_name, materialized) in &ready {
        for skill_name in materialized.discovered_skills.keys() {
            discovered_buckets
                .entry(skill_name.to_ascii_lowercase())
                .or_default()
                .push((source_name.clone(), skill_name.clone()));
        }
    }
    let mut conflicting_sources = BTreeSet::new();
    for entries in discovered_buckets.values() {
        if entries.len() <= 1 {
            continue;
        }
        let participants: Vec<_> = entries
            .iter()
            .map(|(source, skill)| format!("`{skill}` from `{source}`"))
            .collect();
        errors.push(format!(
            "discovered skill conflict between {}",
            participants.join(" and ")
        ));
        for (source, _) in entries {
            conflicting_sources.insert(source.clone());
        }
    }
    for source in &conflicting_sources {
        pending_locks.remove(source);
        ready.remove(source);
    }

    let mut reports = Vec::new();
    for (name, (new_entry, previous_revision)) in pending_locks {
        if is_update && previous_revision.as_deref() != Some(new_entry.revision.as_str()) {
            let new_discovered = ready
                .get(&name)
                .map(|source| &source.discovered_skills)
                .cloned()
                .unwrap_or_default();
            let source_collections = collections_by_source
                .get(&name)
                .cloned()
                .unwrap_or_default();
            let precomputed_old = precomputed_old_discovered.remove(&name);
            let report = build_update_report(
                &materializer,
                &name,
                previous_revision,
                &new_entry.revision,
                &source_collections,
                &new_discovered,
                precomputed_old,
            )
            .await;
            reports.push(report);
        }
        lock.sources.insert(name, new_entry);
    }
    reports.sort_by(|left, right| left.name.cmp(&right.name));

    if !locked {
        lock.sources
            .retain(|name, _| config.sources.contains_key(name));
        write_atomic(
            &paths.config_dir.join("lock.toml"),
            lock.to_toml()?,
            "lockfile",
        )
        .await?;
    }

    Ok(PreparedSources {
        ready,
        errors,
        reports,
    })
}

async fn build_update_report(
    materializer: &SourceMaterializer,
    name: &str,
    previous_revision: Option<String>,
    new_revision: &str,
    collections: &[CollectionConfig],
    new_discovered: &BTreeMap<String, String>,
    precomputed_old_discovered: Option<BTreeMap<String, String>>,
) -> SourceUpdateReport {
    let mut added_skills = BTreeSet::new();
    let mut removed_skills = BTreeSet::new();
    let mut moved_skills = BTreeSet::new();
    let mut diff_unavailable = None;

    if !collections.is_empty() {
        match &previous_revision {
            None => {
                added_skills.extend(new_discovered.keys().cloned());
            }
            Some(old_revision) => {
                let old_result = match precomputed_old_discovered {
                    Some(old) => Ok(old),
                    None => {
                        materializer
                            .discover_skills_for_diff(name, old_revision, collections)
                            .await
                    }
                };
                match old_result {
                    Ok(old_discovered) => {
                        for (skill, new_path) in new_discovered {
                            match old_discovered.get(skill) {
                                None => {
                                    added_skills.insert(skill.clone());
                                }
                                Some(old_path) if old_path != new_path => {
                                    moved_skills.insert(skill.clone());
                                }
                                Some(_) => {}
                            }
                        }
                        for skill in old_discovered.keys() {
                            if !new_discovered.contains_key(skill) {
                                removed_skills.insert(skill.clone());
                            }
                        }
                    }
                    Err(error) => {
                        diff_unavailable = Some(format!("{error:#}"));
                    }
                }
            }
        }
    }

    SourceUpdateReport {
        name: name.to_owned(),
        previous_revision,
        new_revision: new_revision.to_owned(),
        added_skills,
        removed_skills,
        moved_skills,
        diff_unavailable,
    }
}

fn command_policy(command: &Command, config: &Config) -> Result<(bool, BTreeSet<String>)> {
    match command {
        Command::Deploy { locked, .. } => Ok((*locked, BTreeSet::new())),
        Command::Bundle { .. } => Ok((true, BTreeSet::new())),
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

fn network_collections(config: &Config) -> BTreeMap<String, Vec<CollectionConfig>> {
    let mut result: BTreeMap<String, Vec<CollectionConfig>> = BTreeMap::new();
    for collection in &config.collections {
        if collection.source != LOCAL_SOURCE {
            result
                .entry(collection.source.clone())
                .or_default()
                .push(collection.clone());
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
        let marker = DataMarker {
            version: DATA_VERSION,
        };
        tokio::fs::write(&marker_path, toml::to_string(&marker)?).await?;
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

async fn resolve_runtime_paths(
    config: &Config,
    paths: &AppPaths,
    bundle: Option<&Path>,
) -> Result<RuntimePaths> {
    let home = home_dir()?;
    let config_value = simplified(&paths.config_dir).to_string_lossy();
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

    let mut local_collections = Vec::new();
    for (index, collection) in config.collections.iter().enumerate() {
        if collection.source != LOCAL_SOURCE {
            continue;
        }
        let mut rules = Vec::with_capacity(collection.paths.len());
        for raw_entry in &collection.paths {
            let (negated, raw_pattern) = match raw_entry.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw_entry.as_str()),
            };
            let expanded = expand_config_path(raw_pattern, &config_value).with_context(|| {
                format!(
                    "failed to expand local collection #{} pattern `{raw_entry}`",
                    index + 1
                )
            })?;
            rules.push(
                parse_local_rule(&expanded, negated)
                    .with_context(|| format!("invalid local collection #{} pattern", index + 1))?,
            );
        }
        local_collections.push(LocalCollectionRuntime {
            index: index + 1,
            rules,
        });
    }

    let mut reserved_canonical = Vec::new();
    for path in &targets {
        reserved_canonical.push(canonicalize_with_missing(path).await?);
    }
    for internal in ["repos", "snapshots", "skills", ".staging"] {
        reserved_canonical.push(canonicalize_with_missing(&paths.data_dir.join(internal)).await?);
    }
    if let Some(bundle) = bundle {
        reserved_canonical.push(canonicalize_with_missing(bundle).await?);
    }

    let mut pool = reserved_canonical.clone();
    for path in locals.values() {
        pool.push(canonicalize_with_missing(path).await?);
    }
    for left in 0..pool.len() {
        for right in (left + 1)..pool.len() {
            reject_overlap(&pool[left], &pool[right])?;
        }
    }

    Ok(RuntimePaths {
        targets,
        locals,
        local_collections,
        reserved_canonical,
    })
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
    skills: &BTreeMap<String, SkillOrigin>,
    paths: &AppPaths,
    ready: &BTreeMap<String, MaterializedSource>,
) -> BTreeMap<String, Result<SkillChange>> {
    let mut tasks = JoinSet::new();
    for (name, origin) in skills {
        let name = name.clone();
        let link = paths.data_dir.join("skills").join(&name);
        let destination = match origin {
            SkillOrigin::Local(path) => Ok(path.clone()),
            SkillOrigin::Remote { source, path } => ready
                .get(source)
                .and_then(|materialized| materialized.snapshot.as_ref())
                .map(|snapshot| {
                    if path.is_empty() {
                        snapshot.clone()
                    } else {
                        snapshot.join(path)
                    }
                })
                .ok_or_else(|| anyhow::anyhow!("source `{source}` is not deployable")),
        };
        tasks.spawn(async move {
            let result = match destination {
                Ok(destination) => establish_skill_entry(&link, &destination).await,
                Err(error) => Err(error),
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
        Some(previous) if simplified(&previous) == simplified(destination) => {
            SkillChange::Unchanged
        }
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
    skills: &BTreeMap<String, SkillOrigin>,
    runtime: &RuntimePaths,
    paths: &AppPaths,
    skill_results: &BTreeMap<String, Result<SkillChange>>,
    collections_complete: bool,
    errors: &mut Vec<String>,
) -> bool {
    let desired: BTreeSet<_> = skills.keys().cloned().collect();
    let mut all_clean = collections_complete;
    for target in &runtime.targets {
        if let Err(error) = tokio::fs::create_dir_all(target).await {
            errors.push(format!("target `{}`: {error}", target.display()));
            all_clean = false;
            continue;
        }
        let mut complete = collections_complete;
        for name in skills.keys() {
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
    skills: &BTreeMap<String, SkillOrigin>,
    paths: &AppPaths,
    ready: &BTreeMap<String, MaterializedSource>,
) -> Result<BTreeSet<String>> {
    let removed = clean_named_children(
        &paths.data_dir.join("skills"),
        &skills.keys().cloned().collect(),
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
        if link_destination(&entry.path()).await?.is_some() {
            remove_link(&entry.path()).await?;
        } else if file_type.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
        }
        removed.insert(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(removed)
}

async fn write_atomic(path: &Path, contents: String, label: &'static str) -> Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut file = AtomicWriteFile::options()
            .open(&path)
            .with_context(|| format!("failed to open {label} `{}`", path.display()))?;
        file.write_all(contents.as_bytes())?;
        file.commit()
            .with_context(|| format!("failed to commit {label}"))?;
        Ok(())
    })
    .await
    .with_context(|| format!("{label} writer task failed"))??;
    Ok(())
}
