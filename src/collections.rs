use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf, Prefix};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};

use crate::config::{CollectionConfig, is_valid_name, reject_parent_components};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRule {
    pub negated: bool,
    pub pattern: String,
}

#[derive(Debug, Clone)]
enum Segment {
    DoubleStar,
    Glob(GlobMatcher),
}

impl Segment {
    fn matches(&self, component: &str) -> bool {
        match self {
            Self::DoubleStar => true,
            Self::Glob(matcher) => matcher.is_match(component),
        }
    }
}

/// Segment-wise glob matcher built strictly from `globset` per path component.
/// Each non-`**` segment only ever matches a single filename component, so
/// character classes like `[!x]` or `[/]` can never match `/`, and remote/local
/// matching share identical semantics.
#[derive(Debug, Clone)]
pub struct PathGlob {
    segments: Vec<Segment>,
}

impl PathGlob {
    pub fn compile(normalized_slash_path: &str) -> Result<Self> {
        let mut segments = Vec::new();
        for raw in normalized_slash_path.split('/') {
            if raw == "**" {
                if !matches!(segments.last(), Some(Segment::DoubleStar)) {
                    segments.push(Segment::DoubleStar);
                }
            } else {
                let glob = GlobBuilder::new(raw)
                    .literal_separator(true)
                    .backslash_escape(false)
                    .build()
                    .with_context(|| {
                        format!(
                            "invalid glob segment `{raw}` in `{normalized_slash_path}` (use separate entries in `paths` instead of brace alternations containing `/`)"
                        )
                    })?;
                segments.push(Segment::Glob(glob.compile_matcher()));
            }
        }
        Ok(Self { segments })
    }

    #[cfg(test)]
    pub fn is_match_str(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        self.is_match_segments(&parts)
    }

    pub fn is_match_segments(&self, path_segments: &[&str]) -> bool {
        // Dynamic programming over (pattern segment index, path segment index).
        let n = self.segments.len();
        let m = path_segments.len();
        let mut dp = vec![false; m + 1];
        dp[0] = true;
        for segment in &self.segments {
            let mut next = vec![false; m + 1];
            match segment {
                Segment::DoubleStar => {
                    let mut seen = false;
                    for j in 0..=m {
                        if dp[j] {
                            seen = true;
                        }
                        next[j] = seen;
                    }
                }
                Segment::Glob(matcher) => {
                    for j in 0..m {
                        if dp[j] && matcher.is_match(path_segments[j]) {
                            next[j + 1] = true;
                        }
                    }
                }
            }
            dp = next;
        }
        let _ = n;
        dp[m]
    }
}

#[derive(Debug, Clone)]
pub struct LocalRule {
    pub negated: bool,
    pub matcher: PathGlob,
    pub scan: Option<LocalPattern>,
}

#[derive(Debug, Clone)]
pub struct LocalPattern {
    pub base_dir: PathBuf,
    pub relative_glob: Option<PathGlob>,
}

#[derive(Debug, Clone)]
pub struct CandidateDir {
    pub path: String,
    pub valid_utf8: bool,
}

impl CandidateDir {
    pub fn utf8(path: String) -> Self {
        Self {
            path,
            valid_utf8: true,
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(path) => Self {
                path: path.to_owned(),
                valid_utf8: true,
            },
            Err(_) => Self {
                path: String::from_utf8_lossy(bytes).into_owned(),
                valid_utf8: false,
            },
        }
    }
}

pub fn normalize_remote_rule(raw: &str) -> Result<RemoteRule> {
    let (negated, pattern) = match raw.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let normalized = normalize_remote_pattern(pattern)?;
    Ok(RemoteRule {
        negated,
        pattern: normalized,
    })
}

fn normalize_remote_pattern(pattern: &str) -> Result<String> {
    if pattern.is_empty() {
        bail!("collection path pattern cannot be empty");
    }
    if pattern.contains('\\') {
        bail!("remote collection pattern `{pattern}` must use `/` separators");
    }
    if pattern.starts_with('/') {
        bail!("remote collection pattern `{pattern}` must be relative");
    }
    if pattern.split('/').any(|part| part == "..") {
        bail!(
            "invalid remote collection pattern `{pattern}`: `..` path components are not allowed"
        );
    }

    let normalized = pattern
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        bail!(
            "remote collection pattern `{pattern}` cannot select the repository root; declare a skill entry instead"
        );
    }
    PathGlob::compile(&normalized).with_context(|| format!("invalid glob pattern `{pattern}`"))?;
    Ok(normalized)
}

pub fn static_prefix(normalized_pattern: &str) -> String {
    let mut prefix = Vec::new();
    for segment in normalized_pattern.split('/') {
        if has_glob_meta(segment) {
            break;
        }
        prefix.push(segment);
    }
    if prefix.is_empty() {
        ".".into()
    } else {
        prefix.join("/")
    }
}

pub fn remote_collection_prefixes(collections: &[&CollectionConfig]) -> Result<Vec<String>> {
    let mut prefixes = Vec::new();
    for collection in collections {
        for raw in &collection.paths {
            let rule = normalize_remote_rule(raw)?;
            if !rule.negated {
                prefixes.push(static_prefix(&rule.pattern));
            }
        }
    }
    Ok(prefixes)
}

pub fn discover_remote_skills(
    source_name: &str,
    collections: &[&CollectionConfig],
    candidate_dirs: &[CandidateDir],
    explicit_folded: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    discover_remote_skills_inner(
        source_name,
        collections,
        candidate_dirs,
        explicit_folded,
        true,
        true,
    )
}

/// Validates discovered skills from an exported bundle's file map.
/// Excluded skills are intentionally omitted from bundle payloads, so empty
/// matches after export are allowed for collections with `!` rules while
/// naming and conflict checks remain strict.
pub fn discover_remote_skills_from_bundle(
    source_name: &str,
    collections: &[&CollectionConfig],
    candidate_dirs: &[CandidateDir],
    explicit_folded: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    discover_remote_skills_inner(
        source_name,
        collections,
        candidate_dirs,
        explicit_folded,
        true,
        false,
    )
}

pub fn discover_remote_skills_lenient(
    source_name: &str,
    collections: &[&CollectionConfig],
    candidate_dirs: &[CandidateDir],
) -> Result<BTreeMap<String, String>> {
    let empty = BTreeMap::new();
    discover_remote_skills_inner(
        source_name,
        collections,
        candidate_dirs,
        &empty,
        false,
        false,
    )
}

fn discover_remote_skills_inner(
    source_name: &str,
    collections: &[&CollectionConfig],
    candidate_dirs: &[CandidateDir],
    explicit_folded: &BTreeMap<String, String>,
    strict: bool,
    require_non_empty: bool,
) -> Result<BTreeMap<String, String>> {
    let mut discovered = BTreeMap::<String, String>::new();
    let mut folded = BTreeMap::<String, String>::new();

    for collection in collections {
        let mut compiled_rules = Vec::with_capacity(collection.paths.len());
        for raw in &collection.paths {
            let rule = normalize_remote_rule(raw)?;
            let matcher = PathGlob::compile(&rule.pattern)?;
            compiled_rules.push((rule.negated, matcher));
        }
        let mut matched_any_positive = false;

        for candidate in candidate_dirs {
            let dir = &candidate.path;
            let (matched_positive, selected) = evaluate_rules(&compiled_rules, dir);
            if matched_positive {
                matched_any_positive = true;
            }
            if !selected {
                continue;
            }
            let Some(name) = dir.rsplit('/').next() else {
                continue;
            };
            if !candidate.valid_utf8 || !is_valid_name(name) {
                if strict {
                    bail!(
                        "collection for source `{source_name}` discovered invalid skill name `{name}` at `{dir}`"
                    );
                }
                continue;
            }
            let lower = name.to_ascii_lowercase();
            if let Some(existing_explicit) = explicit_folded.get(&lower) {
                bail!(
                    "collection for source `{source_name}` discovered skill `{name}` which conflicts with configured skill `{existing_explicit}`"
                );
            }
            if let Some(existing) = folded.get(&lower) {
                bail!(
                    "collection for source `{source_name}` discovered conflicting skills `{existing}` (`{}`) and `{name}` (`{dir}`)",
                    discovered[existing]
                );
            }
            folded.insert(lower, name.to_owned());
            discovered.insert(name.to_owned(), dir.clone());
        }

        let has_negation = compiled_rules.iter().any(|(negated, _)| *negated);
        if (require_non_empty || (strict && !has_negation)) && !matched_any_positive {
            bail!("collection for source `{source_name}` matched no skills");
        }
    }

    Ok(discovered)
}

fn evaluate_rules(rules: &[(bool, PathGlob)], path: &str) -> (bool, bool) {
    let parts: Vec<&str> = path.split('/').collect();
    let mut matched_positive = false;
    let mut selected = false;
    for (negated, matcher) in rules {
        if matcher.is_match_segments(&parts) {
            if *negated {
                selected = false;
            } else {
                matched_positive = true;
                selected = true;
            }
        }
    }
    (matched_positive, selected)
}

pub fn parse_local_rule(expanded: &Path, negated: bool) -> Result<LocalRule> {
    if !expanded.is_absolute() {
        bail!(
            "expanded local collection path `{}` must be absolute",
            expanded.display()
        );
    }
    reject_parent_components(expanded).with_context(|| {
        format!(
            "invalid expanded local collection path `{}`",
            expanded.display()
        )
    })?;

    let match_pattern = path_to_match_string(expanded)?;
    let matcher = PathGlob::compile(&match_pattern)
        .with_context(|| format!("invalid local collection glob `{}`", expanded.display()))?;
    let scan = if negated {
        None
    } else {
        Some(parse_local_scan_pattern(expanded)?)
    };
    Ok(LocalRule {
        negated,
        matcher,
        scan,
    })
}

fn parse_local_scan_pattern(expanded: &Path) -> Result<LocalPattern> {
    let mut base_dir = PathBuf::new();
    let mut glob_segments = Vec::new();
    let mut in_glob = false;

    for component in expanded.components() {
        match component {
            Component::Prefix(prefix) => {
                if in_glob {
                    bail!("invalid local collection path `{}`", expanded.display());
                }
                base_dir.push(prefix.as_os_str());
            }
            Component::RootDir => {
                if in_glob {
                    bail!("invalid local collection path `{}`", expanded.display());
                }
                base_dir.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("`..` path components are not allowed"),
            Component::Normal(part) => {
                let text = part.to_str().with_context(|| {
                    format!(
                        "local collection path `{}` must be valid UTF-8",
                        expanded.display()
                    )
                })?;
                if !in_glob && !has_glob_meta(text) {
                    base_dir.push(part);
                } else {
                    in_glob = true;
                    glob_segments.push(text.to_owned());
                }
            }
        }
    }

    if glob_segments.is_empty() {
        if base_dir.file_name().is_none() {
            bail!(
                "local collection path `{}` cannot select a filesystem root",
                expanded.display()
            );
        }
        return Ok(LocalPattern {
            base_dir,
            relative_glob: None,
        });
    }

    let relative_glob = PathGlob::compile(&glob_segments.join("/"))
        .with_context(|| format!("invalid local collection glob `{}`", expanded.display()))?;
    Ok(LocalPattern {
        base_dir,
        relative_glob: Some(relative_glob),
    })
}

fn path_to_match_string(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    let mut has_root = false;
    let mut prefix_str = None;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                let normalized = match prefix.kind() {
                    Prefix::VerbatimDisk(drive) | Prefix::Disk(drive) => {
                        format!("{}:", (drive as char).to_ascii_uppercase())
                    }
                    Prefix::VerbatimUNC(server, share) | Prefix::UNC(server, share) => {
                        format!("//{}/{}", server.to_string_lossy(), share.to_string_lossy())
                    }
                    _ => prefix.as_os_str().to_string_lossy().replace('\\', "/"),
                };
                prefix_str = Some(normalized);
            }
            Component::RootDir => {
                has_root = true;
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("`..` path components are not allowed"),
            Component::Normal(part) => {
                parts.push(part.to_string_lossy().into_owned());
            }
        }
    }
    let body = parts.join("/");
    let result = match (prefix_str, has_root) {
        (Some(prefix), true) => format!("{prefix}/{body}"),
        (Some(prefix), false) => format!("{prefix}{body}"),
        (None, true) => format!("/{body}"),
        (None, false) => body,
    };
    Ok(result)
}

pub async fn discover_local_collection(rules: &[LocalRule]) -> Result<BTreeMap<String, PathBuf>> {
    let mut candidates = Vec::<PathBuf>::new();
    let mut seen_paths = BTreeSet::<PathBuf>::new();

    for rule in rules {
        let Some(scan) = &rule.scan else {
            continue;
        };
        let mut pattern_candidates = Vec::new();
        collect_local_candidates(scan, &mut pattern_candidates).await?;
        for path in pattern_candidates {
            if seen_paths.insert(path.clone()) {
                candidates.push(path);
            }
        }
    }

    let compiled: Vec<(bool, PathGlob)> = rules
        .iter()
        .map(|rule| (rule.negated, rule.matcher.clone()))
        .collect();

    let mut matched_any_positive = false;
    let mut discovered = BTreeMap::<String, PathBuf>::new();
    let mut folded = BTreeMap::<String, String>::new();

    for path in candidates {
        let match_path = path_to_match_string(&path)?;
        let (matched_positive, selected) = evaluate_rules(&compiled, &match_path);
        if matched_positive {
            matched_any_positive = true;
        }
        if !selected {
            continue;
        }
        let raw_name = path
            .file_name()
            .with_context(|| format!("local skill directory `{}` has no name", path.display()))?;
        let name_lossy = raw_name.to_string_lossy();
        let Some(name) = raw_name.to_str() else {
            bail!(
                "local collection discovered invalid skill name `{name_lossy}` at `{}`",
                path.display()
            );
        };
        if !is_valid_name(name) {
            bail!(
                "local collection discovered invalid skill name `{name}` at `{}`",
                path.display()
            );
        }
        let lower = name.to_ascii_lowercase();
        if let Some(existing) = folded.get(&lower) {
            bail!(
                "local collection discovered conflicting skills `{existing}` (`{}`) and `{name}` (`{}`)",
                discovered[existing].display(),
                path.display()
            );
        }
        folded.insert(lower, name.to_owned());
        discovered.insert(name.to_owned(), path);
    }

    if !matched_any_positive {
        bail!("local collection matched no skills");
    }

    Ok(discovered)
}

async fn collect_local_candidates(pattern: &LocalPattern, out: &mut Vec<PathBuf>) -> Result<()> {
    let base_meta = match tokio::fs::metadata(&pattern.base_dir).await {
        Ok(meta) => meta,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect `{}`", pattern.base_dir.display()));
        }
    };
    if !base_meta.is_dir() {
        return Ok(());
    }

    let Some(matcher) = &pattern.relative_glob else {
        if has_regular_skill_manifest(&pattern.base_dir).await? {
            out.push(pattern.base_dir.clone());
        }
        return Ok(());
    };
    if pattern.base_dir.file_name().is_some()
        && matcher.is_match_segments(&[])
        && has_regular_skill_manifest(&pattern.base_dir).await?
    {
        out.push(pattern.base_dir.clone());
    }

    let has_double_star = matcher
        .segments
        .iter()
        .any(|segment| matches!(segment, Segment::DoubleStar));
    let max_depth = (!has_double_star).then_some(matcher.segments.len());

    let base_canonical = tokio::fs::canonicalize(&pattern.base_dir)
        .await
        .with_context(|| format!("failed to resolve `{}`", pattern.base_dir.display()))?;
    let mut stack = vec![(
        pattern.base_dir.clone(),
        Vec::<String>::new(),
        vec![base_canonical],
    )];
    while let Some((directory, relative_parts, ancestors)) = stack.pop() {
        let depth = relative_parts.len();
        if let Some(limit) = max_depth
            && depth >= limit
        {
            continue;
        }
        let mut entries = tokio::fs::read_dir(&directory)
            .await
            .with_context(|| format!("failed to read `{}`", directory.display()))?;
        let mut children = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            children.push(entry);
        }
        children.sort_by_key(|entry| entry.file_name());

        // Before reaching any `**` segment, prune non-matching children directly.
        let prune_segment = matcher
            .segments
            .get(depth)
            .filter(|_| {
                !matcher.segments[..depth]
                    .iter()
                    .any(|s| matches!(s, Segment::DoubleStar))
            })
            .filter(|segment| matches!(segment, Segment::Glob(_)));

        for entry in children {
            let file_name = entry.file_name();
            let name_lossy = file_name.to_string_lossy();
            let name_str = name_lossy.as_ref();
            if let Some(segment) = prune_segment
                && !segment.matches(name_str)
            {
                continue;
            }
            let child_path = entry.path();
            let link_meta = tokio::fs::symlink_metadata(&child_path)
                .await
                .with_context(|| format!("failed to inspect `{}`", child_path.display()))?;
            let is_real_dir = link_meta.file_type().is_dir();
            let is_dir_symlink = if link_meta.file_type().is_symlink() {
                match tokio::fs::metadata(&child_path).await {
                    Ok(meta) => meta.is_dir(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to follow symlink `{}`", child_path.display())
                        });
                    }
                }
            } else {
                false
            };
            if !is_real_dir && !is_dir_symlink {
                continue;
            }

            let mut child_parts = relative_parts.clone();
            child_parts.push(name_str.to_owned());
            let refs: Vec<&str> = child_parts.iter().map(String::as_str).collect();
            if matcher.is_match_segments(&refs) && has_regular_skill_manifest(&child_path).await? {
                out.push(child_path.clone());
            }

            let next_depth = child_parts.len();
            let can_descend = max_depth.is_none_or(|limit| next_depth < limit);
            if can_descend && (max_depth.is_some() || name_str != ".git") {
                let mut next_ancestors = ancestors.clone();
                if max_depth.is_none() {
                    let canonical = tokio::fs::canonicalize(&child_path)
                        .await
                        .with_context(|| format!("failed to resolve `{}`", child_path.display()))?;
                    if next_ancestors.contains(&canonical) {
                        continue;
                    }
                    next_ancestors.push(canonical);
                }
                stack.push((child_path, child_parts, next_ancestors));
            }
        }
    }

    Ok(())
}

async fn has_regular_skill_manifest(directory: &Path) -> Result<bool> {
    match tokio::fs::symlink_metadata(directory.join("SKILL.md")).await {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to inspect `{}`",
                directory.join("SKILL.md").display()
            )
        }),
    }
}

fn has_glob_meta(segment: &str) -> bool {
    segment
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(paths: &[&str]) -> CollectionConfig {
        CollectionConfig {
            source: "origin".into(),
            paths: paths.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn extracts_static_prefix_before_first_glob_segment() {
        assert_eq!(static_prefix("skills/*"), "skills");
        assert_eq!(static_prefix("skills/engineering/*"), "skills/engineering");
        assert_eq!(static_prefix("skills/principle-*"), "skills");
        assert_eq!(static_prefix("*"), ".");
        assert_eq!(static_prefix("skills/**"), "skills");
    }

    #[test]
    fn literal_separator_and_ordered_negation_rules() {
        let c = collection(&["skills/*", "!skills/draft-*", "skills/draft-ready"]);
        let candidates = vec![
            CandidateDir::utf8("skills/alpha".into()),
            CandidateDir::utf8("skills/alpha/examples/nested".into()),
            CandidateDir::utf8("skills/draft-beta".into()),
            CandidateDir::utf8("skills/draft-ready".into()),
        ];
        let discovered =
            discover_remote_skills("origin", &[&c], &candidates, &BTreeMap::new()).unwrap();
        assert_eq!(
            discovered,
            BTreeMap::from([
                ("alpha".into(), "skills/alpha".into()),
                ("draft-ready".into(), "skills/draft-ready".into()),
            ])
        );
    }

    #[test]
    fn rejects_invalid_and_conflicting_discovered_names() {
        let c = collection(&["skills/*/*"]);
        let candidates = vec![
            CandidateDir::utf8("skills/a/demo".into()),
            CandidateDir::utf8("skills/b/Demo".into()),
        ];
        let error =
            discover_remote_skills("origin", &[&c], &candidates, &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("conflicting skills"));

        // Two separate collections claiming the exact same skill must conflict.
        let c1 = collection(&["skills/*"]);
        let c2 = collection(&["skills/demo"]);
        let error = discover_remote_skills(
            "origin",
            &[&c1, &c2],
            &[CandidateDir::utf8("skills/demo".into())],
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("conflicting skills"));

        let c_invalid = collection(&["skills/*"]);
        let error = discover_remote_skills(
            "origin",
            &[&c_invalid],
            &[CandidateDir::utf8("skills/_draft".into())],
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid skill name"));

        // Excluding an invalidly named directory via `!` skips it cleanly.
        let c_excluded = collection(&["skills/*", "!skills/_*"]);
        let discovered = discover_remote_skills(
            "origin",
            &[&c_excluded],
            &[CandidateDir::utf8("skills/_draft".into())],
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(discovered.is_empty());
    }

    #[test]
    fn segment_matching_handles_character_classes_and_rejects_cross_separator_braces() {
        let negated = PathGlob::compile("skills/a[!x]b").unwrap();
        assert!(negated.is_match_str("skills/ayb"));
        assert!(!negated.is_match_str("skills/a/b"));

        let range = PathGlob::compile("skills/a[.-0]b").unwrap();
        assert!(range.is_match_str("skills/a.b"));
        assert!(range.is_match_str("skills/a0b"));
        assert!(!range.is_match_str("skills/a/b"));

        assert!(PathGlob::compile("skills/a[/]b").is_err());

        let literal_rbracket = PathGlob::compile("skills/[!]]*").unwrap();
        assert!(literal_rbracket.is_match_str("skills/alpha"));

        let single_seg_brace = PathGlob::compile("skills/{alpha,beta}").unwrap();
        assert!(single_seg_brace.is_match_str("skills/alpha"));
        assert!(single_seg_brace.is_match_str("skills/beta"));

        assert!(PathGlob::compile("skills/{group/one,group/two}/*").is_err());
    }

    #[test]
    fn rejects_empty_matches_before_negation() {
        let c = collection(&["missing/*"]);
        let error = discover_remote_skills(
            "origin",
            &[&c],
            &[CandidateDir::utf8("skills/demo".into())],
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("matched no skills"));
    }
}
