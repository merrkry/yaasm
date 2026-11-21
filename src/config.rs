use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::collections::normalize_remote_rule;

pub const CONFIG_VERSION: u32 = 1;
pub const LOCAL_SOURCE: &str = "local";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub targets: Vec<PathBuf>,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceConfig>,
    #[serde(default)]
    pub skills: BTreeMap<String, SkillConfig>,
    #[serde(default)]
    pub collections: Vec<CollectionConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub github: Option<String>,
    pub git: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillConfig {
    pub source: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionConfig {
    pub source: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Git,
    Github,
}

impl SourceConfig {
    pub fn kind_and_repository(&self) -> Result<(SourceKind, &str)> {
        match (&self.git, &self.github) {
            (Some(repository), None) => Ok((SourceKind::Git, repository)),
            (None, Some(repository)) => Ok((SourceKind::Github, repository)),
            (Some(_), Some(_)) => bail!("source must set exactly one of `git` and `github`"),
            (None, None) => bail!("source must set one of `git` and `github`"),
        }
    }
}

impl Config {
    pub async fn load(path: &Path) -> Result<Self> {
        let input = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&input)
            .with_context(|| format!("failed to parse configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            bail!(
                "unsupported configuration version {}, expected {}",
                self.version,
                CONFIG_VERSION
            );
        }

        validate_named_collection("source", self.sources.keys())?;
        validate_named_collection("skill", self.skills.keys())?;

        if self
            .sources
            .keys()
            .any(|name| name.eq_ignore_ascii_case(LOCAL_SOURCE))
        {
            bail!("`local` is reserved and cannot be declared as a source");
        }

        for (name, source) in &self.sources {
            let (kind, repository) = source
                .kind_and_repository()
                .with_context(|| format!("invalid source `{name}`"))?;
            if repository.is_empty() {
                bail!("source `{name}` has an empty repository");
            }
            if matches!(kind, SourceKind::Github)
                && (repository.split('/').count() != 2
                    || repository.starts_with('/')
                    || repository.ends_with('/'))
            {
                bail!("GitHub source `{name}` must use `owner/repo` syntax");
            }
            if source.reference.as_deref() == Some("") {
                bail!("source `{name}` has an empty ref");
            }
        }

        for (name, skill) in &self.skills {
            validate_skill_path(name, skill)?;
        }

        for (index, collection) in self.collections.iter().enumerate() {
            validate_collection(index + 1, collection)?;
        }

        for target in &self.targets {
            reject_parent_components(target)
                .with_context(|| format!("invalid target `{}`", target.display()))?;
        }

        Ok(())
    }
}

pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

pub fn validate_named_collection<'a>(
    kind: &str,
    names: impl Iterator<Item = &'a String>,
) -> Result<()> {
    let mut folded = BTreeSet::new();
    for name in names {
        if !is_valid_name(name) {
            bail!("invalid {kind} name `{name}`");
        }
        if !folded.insert(name.to_ascii_lowercase()) {
            bail!("{kind} names conflict under ASCII case folding: `{name}`");
        }
    }
    Ok(())
}

fn validate_collection(index: usize, collection: &CollectionConfig) -> Result<()> {
    if collection.source.is_empty() {
        bail!("collection #{index} has an empty source");
    }
    if collection.paths.is_empty() {
        bail!("collection #{index} has no paths");
    }
    let mut has_positive = false;
    for raw_pattern in &collection.paths {
        let (negated, pattern) = match raw_pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, raw_pattern.as_str()),
        };
        if !negated {
            has_positive = true;
        }
        if collection.source == LOCAL_SOURCE {
            if pattern.is_empty() {
                bail!("local collection #{index} has an empty path pattern");
            }
        } else {
            normalize_remote_rule(raw_pattern)
                .with_context(|| format!("invalid collection #{index}"))?;
        }
    }
    if !has_positive {
        bail!("collection #{index} must include at least one positive path pattern");
    }
    Ok(())
}

fn validate_skill_path(name: &str, skill: &SkillConfig) -> Result<()> {
    let path = Path::new(&skill.path);
    if skill.source == LOCAL_SOURCE {
        if skill.path.is_empty() {
            bail!("local skill `{name}` has an empty path");
        }
    } else {
        if path.has_root()
            || path
                .components()
                .any(|component| matches!(component, Component::Prefix(_)))
        {
            bail!("network skill `{name}` path must be relative");
        }
        reject_parent_components(path)
            .with_context(|| format!("invalid path for network skill `{name}`"))?;
    }
    Ok(())
}

pub fn reject_parent_components(path: &Path) -> Result<()> {
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        bail!("`..` path components are not allowed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Result<Config> {
        let config: Config = toml::from_str(input)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn parses_empty_config() {
        let config = parse("version = 1\n").unwrap();
        assert!(config.sources.is_empty());
        assert!(config.skills.is_empty());
        assert!(config.targets.is_empty());
    }

    #[test]
    fn rejects_case_conflicts() {
        let error = parse(
            r#"
version = 1
[skills.Foo]
source = "local"
path = "/tmp/foo"
[skills.foo]
source = "local"
path = "/tmp/bar"
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("case folding"));
    }

    #[test]
    fn rejects_parent_network_path() {
        let error = parse(
            r#"
version = 1
[sources.foo]
git = "https://example.com/repo.git"
[skills.bar]
source = "foo"
path = "../bar"
"#,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("`..`"));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_rooted_and_drive_relative_network_paths() {
        for path in ["/skills/demo", "C:skills/demo"] {
            let input =
                format!("version = 1\n[skills.demo]\nsource = \"remote\"\npath = {path:?}\n");
            assert!(
                parse(&input)
                    .unwrap_err()
                    .to_string()
                    .contains("must be relative")
            );
        }
    }
}
