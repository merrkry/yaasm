use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn yaasm(config: &Path, data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yaasm"))
        .env("HOME", config.parent().unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "")
        .arg("--config-dir")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output, expected: &str) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "stderr: {stderr}");
}

fn git(repository: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "")
        .output()
        .unwrap();
    assert_success(&output);
}

fn init_repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-b", "main"]);
    git(path, &["config", "core.autocrlf", "false"]);
}

fn commit(repository: &Path, message: &str) {
    git(repository, &["add", "-A"]);
    git(
        repository,
        &[
            "-c",
            "user.name=YAASM Test",
            "-c",
            "user.email=yaasm@example.invalid",
            "commit",
            "-m",
            message,
        ],
    );
}

fn write_skill(root: &Path, relative: &str, body: &str) {
    let dir = root.join(relative);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), body).unwrap();
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[test]
fn remote_collection_discovers_reports_update_diffs_and_cleans_removed_skills() {
    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let target = temp.path().join("target");
    init_repo(&repo);

    write_skill(&repo, "skills/alpha", "alpha v1\n");
    write_skill(&repo, "skills/beta", "beta v1\n");
    write_skill(&repo, "skills/draft-gamma", "draft\n");
    write_skill(&repo, "skills/draft-keep", "re-included\n");
    write_skill(&repo, "skills/alpha/examples/nested", "nested fixture\n");
    #[cfg(unix)]
    write_skill(&repo, ":colon/literal-colon", "colon\n");
    fs::create_dir_all(repo.join("skills/notes")).unwrap();
    fs::write(repo.join("skills/notes/README.md"), "not a skill\n").unwrap();
    commit(&repo, "initial");

    let remote_paths = if cfg!(unix) {
        r#"["skills/*", "extra/*", ":colon/*", "!skills/draft-*", "skills/draft-keep"]"#
    } else {
        r#"["skills/*", "extra/*", "!skills/draft-*", "skills/draft-keep"]"#
    };

    fs::create_dir_all(&config).unwrap();
    let write_cfg = |source_repo: &Path, reference: &str| {
        fs::write(
            config.join("config.toml"),
            format!(
                r#"version = 1
targets = [{target}]

[sources.origin]
git = {repo}
ref = {reference}

[[collections]]
source = "origin"
paths = {remote_paths}
"#,
                target = toml::Value::String(target.to_string_lossy().into_owned()),
                repo = toml::Value::String(source_repo.to_string_lossy().into_owned()),
                reference = toml::Value::String(reference.to_owned()),
            ),
        )
        .unwrap();
    };
    write_cfg(&repo, "main");

    let initial_update = yaasm(&config, &data, &["update"]);
    assert_success(&initial_update);
    let stdout = String::from_utf8_lossy(&initial_update.stdout);
    assert!(stdout.contains("Locked origin:"), "{stdout}");
    assert!(stdout.contains("Added skills:"), "{stdout}");
    assert!(stdout.contains("alpha"), "{stdout}");
    assert!(stdout.contains("beta"), "{stdout}");
    assert!(stdout.contains("draft-keep"), "{stdout}");

    let noop_update = yaasm(&config, &data, &["update"]);
    assert_success(&noop_update);
    assert_eq!(
        String::from_utf8_lossy(&noop_update.stdout).trim(),
        "Already up to date"
    );

    assert_success(&yaasm(&config, &data, &["deploy"]));
    assert_eq!(
        fs::read_to_string(target.join("alpha/SKILL.md")).unwrap(),
        "alpha v1\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("beta/SKILL.md")).unwrap(),
        "beta v1\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("draft-keep/SKILL.md")).unwrap(),
        "re-included\n"
    );
    #[cfg(unix)]
    assert_eq!(
        fs::read_to_string(target.join("literal-colon/SKILL.md")).unwrap(),
        "colon\n"
    );
    assert!(fs::symlink_metadata(target.join("draft-gamma")).is_err());
    assert!(fs::symlink_metadata(target.join("notes")).is_err());
    assert!(fs::symlink_metadata(target.join("nested")).is_err());

    // Upstream removes beta, adds delta, and moves alpha to extra/alpha.
    fs::remove_dir_all(repo.join("skills/beta")).unwrap();
    fs::remove_dir_all(repo.join("skills/alpha")).unwrap();
    write_skill(&repo, "extra/alpha", "alpha moved\n");
    write_skill(&repo, "skills/delta", "delta v1\n");
    commit(&repo, "reorganize skills");

    // Run update in a fresh data directory (like CI) so previous revision must be fetched.
    let fresh_data = temp.path().join("fresh-data");
    let update = yaasm(&config, &fresh_data, &["update", "origin"]);
    assert_success(&update);
    let stdout = String::from_utf8_lossy(&update.stdout);
    assert!(stdout.contains("Updated origin:"), "{stdout}");
    assert!(stdout.contains("Added skills: delta"), "{stdout}");
    assert!(stdout.contains("Moved skills: alpha"), "{stdout}");
    assert!(stdout.contains("Removed skills: beta"), "{stdout}");

    assert_success(&yaasm(&config, &data, &["deploy"]));
    assert_eq!(
        fs::read_to_string(target.join("alpha/SKILL.md")).unwrap(),
        "alpha moved\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("delta/SKILL.md")).unwrap(),
        "delta v1\n"
    );
    assert!(fs::symlink_metadata(target.join("beta")).is_err());
    assert!(fs::symlink_metadata(data.join("skills/beta")).is_err());

    // Changing source ref preserves previous locked revision as diff baseline.
    git(&repo, &["checkout", "-b", "next"]);
    write_skill(&repo, "skills/epsilon", "epsilon\n");
    commit(&repo, "next branch");
    write_cfg(&repo, "next");
    let ref_update = yaasm(&config, &data, &["update"]);
    assert_success(&ref_update);
    let ref_stdout = String::from_utf8_lossy(&ref_update.stdout);
    assert!(ref_stdout.contains("Updated origin:"), "{ref_stdout}");
    assert!(ref_stdout.contains("Added skills: epsilon"), "{ref_stdout}");

    // Changing source URL preserves diff baseline from the cached local repo before rebuild.
    let repo_alt = temp.path().join("repo-alt");
    init_repo(&repo_alt);
    write_skill(&repo_alt, "skills/omega", "omega\n");
    commit(&repo_alt, "alt repo");
    write_cfg(&repo_alt, "main");
    let url_update = yaasm(&config, &data, &["update"]);
    assert_success(&url_update);
    let url_stdout = String::from_utf8_lossy(&url_update.stdout);
    assert!(url_stdout.contains("Added skills: omega"), "{url_stdout}");
    assert!(url_stdout.contains("Removed skills:"), "{url_stdout}");
    assert!(
        !url_stdout.contains("Skill diff unavailable"),
        "{url_stdout}"
    );
}

#[test]
fn collection_bundles_deterministically_and_deploys_offline_with_local_collections() {
    let temp = TempDir::new().unwrap();
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let target = temp.path().join("target");
    init_repo(&repo_a);
    init_repo(&repo_b);
    write_skill(&repo_a, "skills/engineering/arch", "arch\n");
    write_skill(&repo_a, "skills/productivity/focus", "focus\n");
    write_skill(&repo_a, "skills/productivity/skip-me", "skip\n");
    write_skill(&repo_a, "!group/bang-skill", "bang\n");
    write_skill(&repo_b, "skills/bravo", "bravo\n");
    write_skill(&repo_b, "excluded/only-draft", "draft\n");
    commit(&repo_a, "initial a");
    commit(&repo_b, "initial b");

    let write_cfg = |local_root: &str, extra_remote: &str| {
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.toml"),
            format!(
                r#"version = 1
targets = [{target}]

[sources.a]
git = {a}
ref = "main"

[sources.b]
git = {b}
ref = "main"

[[collections]]
source = "a"
paths = ["skills/engineering/*", "skills/productivity/*", "!skills/productivity/skip-*", {extra_remote}]

[[collections]]
source = "b"
paths = ["skills/*"]

[[collections]]
source = "b"
paths = ["excluded/*", "!excluded/*"]

[[collections]]
source = "local"
paths = [{local_pattern}, {local_negated}]
"#,
                target = toml::Value::String(target.to_string_lossy().into_owned()),
                a = toml::Value::String(repo_a.to_string_lossy().into_owned()),
                b = toml::Value::String(repo_b.to_string_lossy().into_owned()),
                local_pattern = toml::Value::String(format!("{local_root}/*")),
                local_negated = toml::Value::String(format!("!{local_root}/wip-*")),
            ),
        )
        .unwrap();
    };

    write_cfg("${YAASM_TEST_MISSING}/nonexistent", "\"!group/*\"");
    assert_success(&yaasm(&config, &data, &["update"]));

    let bundle_one = temp.path().join("bundle-one");
    assert_success(&yaasm(
        &config,
        &data,
        &["bundle", "--output", bundle_one.to_str().unwrap()],
    ));

    // Changing local collection configuration must not change the remote bundle.
    write_cfg("${YAASM_CONFIG_DIR}/local-skills", "\"!group/*\"");
    let bundle_two = temp.path().join("bundle-two");
    assert_success(&yaasm(
        &config,
        &temp.path().join("builder-two"),
        &["bundle", "--output", bundle_two.to_str().unwrap()],
    ));
    assert_eq!(tree(&bundle_one), tree(&bundle_two));

    write_skill(&config, "local-skills/local-one", "local v1\n");
    write_skill(&config, "local-skills/wip-two", "ignored\n");
    fs::create_dir_all(config.join("local-skills/not-a-skill")).unwrap();

    let bin = temp.path().join("empty-bin");
    fs::create_dir_all(&bin).unwrap();
    #[cfg(unix)]
    for program in ["git", "gh"] {
        use std::os::unix::fs::PermissionsExt;
        let path = bin.join(program);
        fs::write(&path, "#!/bin/sh\nexit 99\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let cold_data = temp.path().join("cold-data");
    let deploy_offline = |data_dir: &Path| {
        Command::new(env!("CARGO_BIN_EXE_yaasm"))
            .arg("--config-dir")
            .arg(&config)
            .arg("--data-dir")
            .arg(data_dir)
            .args(["deploy", "--bundle"])
            .arg(&bundle_one)
            .env("HOME", temp.path())
            .env("PATH", &bin)
            .output()
            .unwrap()
    };

    // Offline deploy succeeds including excluded-only collection round-trip.
    assert_success(&deploy_offline(&cold_data));
    assert_eq!(
        fs::read_to_string(target.join("arch/SKILL.md")).unwrap(),
        "arch\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("focus/SKILL.md")).unwrap(),
        "focus\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("bravo/SKILL.md")).unwrap(),
        "bravo\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("local-one/SKILL.md")).unwrap(),
        "local v1\n"
    );
    assert!(fs::symlink_metadata(target.join("skip-me")).is_err());
    assert!(fs::symlink_metadata(target.join("wip-two")).is_err());
    assert!(fs::symlink_metadata(target.join("only-draft")).is_err());

    // Selection fingerprint distinguishes negated `!group/*` from literal `./!group/*`.
    write_cfg("${YAASM_CONFIG_DIR}/local-skills", "\"./!group/*\"");
    assert_failure(&deploy_offline(&cold_data), "bundle does not match");

    // Restoring config and tampering with source `a`'s discovered skills isolates only `a`;
    // healthy sibling source `b` still deploys into a cold target.
    write_cfg("${YAASM_CONFIG_DIR}/local-skills", "\"!group/*\"");
    let original_manifest = fs::read_to_string(bundle_one.join("bundle.toml")).unwrap();
    fs::write(
        bundle_one.join("bundle.toml"),
        original_manifest.replace("[skills.arch]", "[skills.\"bad/name\"]"),
    )
    .unwrap();
    let isolated_data = temp.path().join("isolated-data");
    let target_isolated = temp.path().join("target-isolated");
    let original_cfg = fs::read_to_string(config.join("config.toml")).unwrap();
    fs::write(
        config.join("config.toml"),
        original_cfg.replace(
            &toml::Value::String(target.to_string_lossy().into_owned()).to_string(),
            &toml::Value::String(target_isolated.to_string_lossy().into_owned()).to_string(),
        ),
    )
    .unwrap();
    assert_failure(&deploy_offline(&isolated_data), "source `a`");
    assert_eq!(
        fs::read_to_string(target_isolated.join("bravo/SKILL.md")).unwrap(),
        "bravo\n"
    );
    assert!(fs::symlink_metadata(target_isolated.join("arch")).is_err());
    fs::write(bundle_one.join("bundle.toml"), original_manifest).unwrap();
}

#[test]
fn local_collection_discovers_symlinks_root_glob_prunes_and_rejects_overlap() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let target = temp.path().join("target");
    let keep = temp.path().join("keep");
    let root = temp.path().join("workspace");

    write_skill(temp.path(), "keep", "keep\n");
    write_skill(&root, "skills/group/one/s1", "s1\n");
    write_skill(&root, "skills/group/two/s2", "s2\n");
    write_skill(&root, "literal-skill", "lit\n");
    write_skill(&root, "root-glob", "root glob\n");
    fs::write(root.join("notes"), "regular file\n").unwrap();

    // Unrelated unreadable sibling outside the glob must be pruned cleanly.
    let unreadable = root.join("skills/unrelated-locked");
    fs::create_dir_all(&unreadable).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    }

    let linked_target = temp.path().join("linked-target");
    write_skill(&linked_target, "skills/sym-skill", "sym\n");
    let roots_dir = temp.path().join("roots");
    fs::create_dir_all(&roots_dir).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&linked_target, roots_dir.join("link")).unwrap();
        // Cycle inside linked target must not cause infinite traversal under `**`.
        std::os::unix::fs::symlink(&roots_dir, linked_target.join("cycle")).unwrap();
    }

    fs::create_dir_all(&config).unwrap();
    #[cfg(unix)]
    let extra_collection = format!(
        r#"
[[collections]]
source = "local"
paths = [{sym}]
"#,
        sym = toml::Value::String(roots_dir.join("*/skills/*").to_string_lossy().into_owned()),
    );
    #[cfg(not(unix))]
    let extra_collection = "";
    fs::write(
        config.join("config.toml"),
        format!(
            r#"version = 1
targets = [{target}]

[skills.keep]
source = "local"
path = {keep}

[[collections]]
source = "local"
paths = [{missing}, {braces}, {notes}, {lit}, {root_glob}]
{extra_collection}"#,
            target = toml::Value::String(target.to_string_lossy().into_owned()),
            keep = toml::Value::String(keep.to_string_lossy().into_owned()),
            missing = toml::Value::String(
                temp.path()
                    .join("missing-branch/*")
                    .to_string_lossy()
                    .into_owned()
            ),
            braces = toml::Value::String(
                root.join("skills/group/{one,two}/*")
                    .to_string_lossy()
                    .into_owned()
            ),
            notes = toml::Value::String(root.join("notes").to_string_lossy().into_owned()),
            lit = toml::Value::String(root.join("literal-skill").to_string_lossy().into_owned()),
            root_glob =
                toml::Value::String(root.join("root-glob/**").to_string_lossy().into_owned()),
        ),
    )
    .unwrap();

    let result = yaasm(&config, &data, &["deploy"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_success(&result);
    assert_eq!(
        fs::read_to_string(target.join("s1/SKILL.md")).unwrap(),
        "s1\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("s2/SKILL.md")).unwrap(),
        "s2\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("literal-skill/SKILL.md")).unwrap(),
        "lit\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("root-glob/SKILL.md")).unwrap(),
        "root glob\n"
    );
    #[cfg(unix)]
    assert_eq!(
        fs::read_to_string(target.join("sym-skill/SKILL.md")).unwrap(),
        "sym\n"
    );

    // Missing local collection fails without deleting existing managed links.
    fs::write(
        config.join("config.toml"),
        format!(
            r#"version = 1
targets = [{target}]

[skills.keep]
source = "local"
path = {keep}

[[collections]]
source = "local"
paths = [{missing}]
"#,
            target = toml::Value::String(target.to_string_lossy().into_owned()),
            keep = toml::Value::String(keep.to_string_lossy().into_owned()),
            missing = toml::Value::String(
                temp.path()
                    .join("absent-dir/*")
                    .to_string_lossy()
                    .into_owned()
            ),
        ),
    )
    .unwrap();
    assert_failure(&yaasm(&config, &data, &["deploy"]), "local collection #1");
    assert_eq!(
        fs::read_to_string(target.join("s1/SKILL.md")).unwrap(),
        "s1\n"
    );

    // Nested overlap across local collections and explicit local skills is rejected symmetrically.
    write_skill(&keep, "nested", "nested\n");
    let overlap_root = temp.path().join("overlap-root");
    write_skill(&overlap_root, "outer", "outer\n");
    write_skill(&overlap_root, "outer/inner", "inner\n");
    fs::write(
        config.join("config.toml"),
        format!(
            r#"version = 1
targets = [{target}]

[skills.keep]
source = "local"
path = {keep}

[[collections]]
source = "local"
paths = [{keep_glob}]

[[collections]]
source = "local"
paths = [{outer}]

[[collections]]
source = "local"
paths = [{inner}]
"#,
            target = toml::Value::String(target.to_string_lossy().into_owned()),
            keep = toml::Value::String(keep.to_string_lossy().into_owned()),
            keep_glob = toml::Value::String(keep.join("*").to_string_lossy().into_owned()),
            outer = toml::Value::String(overlap_root.join("*").to_string_lossy().into_owned()),
            inner =
                toml::Value::String(overlap_root.join("outer/*").to_string_lossy().into_owned()),
        ),
    )
    .unwrap();
    assert_failure(&yaasm(&config, &data, &["deploy"]), "overlaps");
    assert!(fs::symlink_metadata(target.join("nested")).is_err());
    assert!(fs::symlink_metadata(target.join("outer")).is_err());
    assert!(fs::symlink_metadata(target.join("inner")).is_err());
}

#[test]
fn collection_conflicts_and_empty_matches_do_not_advance_lockfile() {
    let temp = TempDir::new().unwrap();
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    let repo_c = temp.path().join("repo-c");
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    init_repo(&repo_a);
    init_repo(&repo_b);
    init_repo(&repo_c);

    write_skill(&repo_a, "skills/alpha", "a\n");
    write_skill(&repo_b, "skills/beta", "b\n");
    write_skill(&repo_c, "skills/gamma", "c\n");
    commit(&repo_a, "initial a");
    commit(&repo_b, "initial b");
    commit(&repo_c, "initial c");

    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        format!(
            r#"version = 1

[sources.a]
git = {a}
ref = "main"

[sources.b]
git = {b}
ref = "main"

[sources.c]
git = {c}
ref = "main"

[skills.explicit-local]
source = "local"
path = {local}

[[collections]]
source = "a"
paths = ["skills/*"]

[[collections]]
source = "b"
paths = ["skills/*"]

[[collections]]
source = "c"
paths = ["skills/*"]
"#,
            a = toml::Value::String(repo_a.to_string_lossy().into_owned()),
            b = toml::Value::String(repo_b.to_string_lossy().into_owned()),
            c = toml::Value::String(repo_c.to_string_lossy().into_owned()),
            local = toml::Value::String(
                temp.path()
                    .join("local-explicit")
                    .to_string_lossy()
                    .into_owned()
            ),
        ),
    )
    .unwrap();

    assert_success(&yaasm(&config, &data, &["update"]));
    let initial_lock: toml::Value =
        toml::from_str(&fs::read_to_string(config.join("lock.toml")).unwrap()).unwrap();
    let rev_a = initial_lock["sources"]["a"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let rev_b = initial_lock["sources"]["b"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();
    let rev_c = initial_lock["sources"]["c"]["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    // Cross-source conflict blocks `b` while unrelated healthy source `c` still updates and reports.
    write_skill(&repo_b, "skills/Alpha", "conflicts with a\n");
    write_skill(&repo_c, "skills/delta", "new healthy skill\n");
    commit(&repo_b, "add conflicting Alpha");
    commit(&repo_c, "add delta");
    let conflict_out = yaasm(&config, &data, &["update"]);
    assert_failure(&conflict_out, "discovered skill conflict");
    let stdout = String::from_utf8_lossy(&conflict_out.stdout);
    assert!(stdout.contains("Updated c:"), "{stdout}");
    assert!(stdout.contains("Added skills: delta"), "{stdout}");
    let after_conflict: toml::Value =
        toml::from_str(&fs::read_to_string(config.join("lock.toml")).unwrap()).unwrap();
    assert_eq!(
        after_conflict["sources"]["a"]["revision"].as_str(),
        Some(rev_a.as_str())
    );
    assert_eq!(
        after_conflict["sources"]["b"]["revision"].as_str(),
        Some(rev_b.as_str())
    );
    assert_ne!(
        after_conflict["sources"]["c"]["revision"].as_str(),
        Some(rev_c.as_str())
    );

    // Colliding with an explicit local skill fails `b` without advancing `b`'s lock.
    fs::remove_dir_all(repo_b.join("skills/Alpha")).unwrap();
    write_skill(&repo_b, "skills/Explicit-Local", "collides with local\n");
    commit(&repo_b, "collide with explicit local");
    assert_failure(
        &yaasm(&config, &data, &["update", "b"]),
        "conflicts with configured skill",
    );
    let after_explicit: toml::Value =
        toml::from_str(&fs::read_to_string(config.join("lock.toml")).unwrap()).unwrap();
    assert_eq!(
        after_explicit["sources"]["b"]["revision"].as_str(),
        Some(rev_b.as_str())
    );

    // Empty match fails `b` without advancing `b`'s lock.
    fs::remove_dir_all(repo_b.join("skills")).unwrap();
    fs::write(repo_b.join("README.md"), "empty\n").unwrap();
    commit(&repo_b, "remove all skills");
    assert_failure(
        &yaasm(&config, &data, &["update", "b"]),
        "matched no skills",
    );
    let after_empty: toml::Value =
        toml::from_str(&fs::read_to_string(config.join("lock.toml")).unwrap()).unwrap();
    assert_eq!(
        after_empty["sources"]["b"]["revision"].as_str(),
        Some(rev_b.as_str())
    );
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_paths_are_ignored_when_unselected_and_rejected_when_matched() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let temp = TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let target = temp.path().join("target");
    init_repo(&repo);
    write_skill(&repo, "skills/alpha", "alpha\n");
    // Unselected non-UTF-8 file outside collection must not break discovery.
    let bad_file = repo.join(OsStr::from_bytes(b"docs/bad-\xff.txt"));
    fs::create_dir_all(bad_file.parent().unwrap()).unwrap();
    fs::write(&bad_file, "bytes\n").unwrap();
    commit(&repo, "with unselected non-utf8");

    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        format!(
            r#"version = 1
targets = [{target}]

[sources.origin]
git = {repo}
ref = "main"

[[collections]]
source = "origin"
paths = ["skills/*"]
"#,
            target = toml::Value::String(target.to_string_lossy().into_owned()),
            repo = toml::Value::String(repo.to_string_lossy().into_owned()),
        ),
    )
    .unwrap();
    assert_success(&yaasm(&config, &data, &["deploy"]));
    assert_eq!(
        fs::read_to_string(target.join("alpha/SKILL.md")).unwrap(),
        "alpha\n"
    );

    // Matched non-UTF-8 skill directory must be rejected rather than silently skipped.
    let bad_skill_dir = repo
        .join("skills")
        .join(OsStr::from_bytes(b"bad-\xff-skill"));
    fs::create_dir_all(&bad_skill_dir).unwrap();
    fs::write(bad_skill_dir.join("SKILL.md"), "bad\n").unwrap();
    commit(&repo, "add matched non-utf8 skill");
    assert_failure(&yaasm(&config, &data, &["update"]), "invalid skill name");
}

#[cfg(unix)]
#[test]
fn local_symlink_permission_error_preserves_deployed_links() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    // Local symlink permission error propagates as collection failure and preserves links.
    let cfg_local = temp.path().join("cfg-local");
    let data_local = temp.path().join("data-local");
    let target_local = temp.path().join("target-local");
    let parent = temp.path().join("secret-parent");
    let skills_root = temp.path().join("local-skills");
    write_skill(&parent, "real-alpha", "alpha\n");
    write_skill(&skills_root, "bravo", "bravo\n");
    std::os::unix::fs::symlink(parent.join("real-alpha"), skills_root.join("alpha")).unwrap();

    fs::create_dir_all(&cfg_local).unwrap();
    fs::write(
        cfg_local.join("config.toml"),
        format!(
            r#"version = 1
targets = [{target}]

[[collections]]
source = "local"
paths = [{pattern}]
"#,
            target = toml::Value::String(target_local.to_string_lossy().into_owned()),
            pattern = toml::Value::String(skills_root.join("*").to_string_lossy().into_owned()),
        ),
    )
    .unwrap();
    assert_success(&yaasm(&cfg_local, &data_local, &["deploy"]));
    assert!(fs::symlink_metadata(target_local.join("alpha")).is_ok());

    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
    let denied = yaasm(&cfg_local, &data_local, &["deploy"]);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    assert_failure(&denied, "failed to follow symlink");
    assert!(fs::symlink_metadata(target_local.join("alpha")).is_ok());
    assert!(fs::symlink_metadata(data_local.join("skills/alpha")).is_ok());
}
