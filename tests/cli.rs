use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn yaasm(config: &Path, data: &Path, command: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yaasm"))
        .arg("--config-dir")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .arg(command)
        .output()
        .unwrap()
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_config(config_dir: &Path, input: &str) {
    fs::create_dir_all(config_dir).unwrap();
    fs::write(config_dir.join("config.toml"), input).unwrap();
}

#[cfg(unix)]
#[test]
fn deploys_local_skill_and_cleans_stale_link() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let target = temp.path().join("target");
    let local = temp.path().join("local-skill");
    fs::create_dir_all(&local).unwrap();
    fs::write(local.join("SKILL.md"), "local").unwrap();
    write_config(
        &config,
        &format!(
            "version = 1\ntargets = [{}]\n[skills.demo]\nsource = \"local\"\npath = {}\n",
            toml::Value::String(target.to_string_lossy().into_owned()),
            toml::Value::String(local.to_string_lossy().into_owned())
        ),
    );

    assert_success(yaasm(&config, &data, "deploy"));
    assert_eq!(
        fs::read_link(target.join("demo")).unwrap(),
        data.join("skills/demo")
    );
    assert_eq!(
        fs::read_to_string(target.join("demo/SKILL.md")).unwrap(),
        "local"
    );
    assert!(config.join("lock.toml").is_file());

    write_config(
        &config,
        &format!(
            "version = 1\ntargets = [{}]\n",
            toml::Value::String(target.to_string_lossy().into_owned())
        ),
    );
    assert_success(yaasm(&config, &data, "deploy"));
    assert!(!target.join("demo").exists());
    assert!(fs::symlink_metadata(target.join("demo")).is_err());
}

#[test]
fn refuses_target_collision() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let target = temp.path().join("target");
    let local = temp.path().join("local-skill");
    fs::create_dir_all(&local).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::write(local.join("SKILL.md"), "local").unwrap();
    fs::write(target.join("demo"), "keep").unwrap();
    write_config(
        &config,
        &format!(
            "version = 1\ntargets = [{}]\n[skills.demo]\nsource = \"local\"\npath = {}\n",
            toml::Value::String(target.to_string_lossy().into_owned()),
            toml::Value::String(local.to_string_lossy().into_owned())
        ),
    );

    let output = yaasm(&config, &data, "deploy");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read_to_string(target.join("demo")).unwrap(), "keep");
}

#[cfg(unix)]
#[test]
fn one_broken_skill_does_not_block_siblings_or_clean_target() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let target = temp.path().join("target");
    let stale = temp.path().join("stale");
    let working = temp.path().join("working");
    let missing = temp.path().join("missing");
    fs::create_dir_all(&stale).unwrap();
    fs::create_dir_all(&working).unwrap();
    fs::write(stale.join("SKILL.md"), "stale").unwrap();
    fs::write(working.join("SKILL.md"), "working").unwrap();
    write_config(
        &config,
        &format!(
            "version = 1\ntargets = [{}]\n[skills.stale]\nsource = \"local\"\npath = {}\n",
            toml::Value::String(target.to_string_lossy().into_owned()),
            toml::Value::String(stale.to_string_lossy().into_owned())
        ),
    );
    assert_success(yaasm(&config, &data, "deploy"));

    write_config(
        &config,
        &format!(
            "version = 1\ntargets = [{}]\n[skills.working]\nsource = \"local\"\npath = {}\n[skills.broken]\nsource = \"local\"\npath = {}\n",
            toml::Value::String(target.to_string_lossy().into_owned()),
            toml::Value::String(working.to_string_lossy().into_owned()),
            toml::Value::String(missing.to_string_lossy().into_owned())
        ),
    );
    let output = yaasm(&config, &data, "deploy");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(target.join("working/SKILL.md")).unwrap(),
        "working"
    );
    assert!(fs::symlink_metadata(target.join("broken")).is_err());
    assert!(fs::symlink_metadata(target.join("stale")).is_ok());
}

#[cfg(unix)]
#[test]
fn unknown_source_fails_only_its_skill() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let target = temp.path().join("target");
    let local = temp.path().join("local");
    fs::create_dir_all(&local).unwrap();
    fs::write(local.join("SKILL.md"), "working").unwrap();
    write_config(
        &config,
        &format!(
            "version = 1\ntargets = [{}]\n[skills.working]\nsource = \"local\"\npath = {}\n[skills.broken]\nsource = \"unknown\"\npath = \"skills/broken\"\n",
            toml::Value::String(target.to_string_lossy().into_owned()),
            toml::Value::String(local.to_string_lossy().into_owned())
        ),
    );
    let output = yaasm(&config, &data, "deploy");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(target.join("working/SKILL.md")).unwrap(),
        "working"
    );
    assert!(fs::symlink_metadata(target.join("broken")).is_err());
}

#[test]
fn locked_deploy_requires_complete_matching_lock() {
    let temp = TempDir::new().unwrap();
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    write_config(
        &config,
        "version = 1\n[sources.origin]\ngit = \"https://example.invalid/repo.git\"\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_yaasm"))
        .arg("--config-dir")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .args(["deploy", "--locked"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("lockfile has no entry"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!config.join("lock.toml").exists());
}

#[cfg(unix)]
#[test]
fn deploys_and_updates_git_skill() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repository");
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let target = temp.path().join("target");
    fs::create_dir_all(repository.join("skills/demo")).unwrap();
    git(
        temp.path(),
        ["init", "-b", "main", repository.to_str().unwrap()],
    );
    fs::write(repository.join("skills/demo/SKILL.md"), "one").unwrap();
    git(&repository, ["add", "."]);
    git(
        &repository,
        [
            "-c",
            "user.name=YAASM Test",
            "-c",
            "user.email=yaasm@example.invalid",
            "commit",
            "-m",
            "one",
        ],
    );
    write_config(
        &config,
        &format!(
            "version = 1\ntargets = [{}]\n[sources.origin]\ngit = {}\nref = \"main\"\n[skills.demo]\nsource = \"origin\"\npath = \"skills/demo\"\n",
            toml::Value::String(target.to_string_lossy().into_owned()),
            toml::Value::String(repository.to_string_lossy().into_owned())
        ),
    );

    assert_success(yaasm(&config, &data, "deploy"));
    assert_eq!(
        fs::read_to_string(target.join("demo/SKILL.md")).unwrap(),
        "one"
    );
    let first_lock = fs::read_to_string(config.join("lock.toml")).unwrap();
    assert!(first_lock.contains("fingerprint = \"sha256:"));
    assert!(first_lock.contains("revision = "));
    assert!(!data.join("repos/origin/.git/FETCH_HEAD").exists());

    fs::write(repository.join("skills/demo/SKILL.md"), "two").unwrap();
    git(&repository, ["add", "."]);
    git(
        &repository,
        [
            "-c",
            "user.name=YAASM Test",
            "-c",
            "user.email=yaasm@example.invalid",
            "commit",
            "-m",
            "two",
        ],
    );
    assert_success(yaasm(&config, &data, "update"));
    assert_eq!(
        fs::read_to_string(target.join("demo/SKILL.md")).unwrap(),
        "two"
    );
    assert_ne!(
        first_lock,
        fs::read_to_string(config.join("lock.toml")).unwrap()
    );

    let replacement = temp.path().join("replacement");
    fs::create_dir_all(replacement.join("skills/demo")).unwrap();
    git(
        temp.path(),
        ["init", "-b", "main", replacement.to_str().unwrap()],
    );
    fs::write(replacement.join("skills/demo/SKILL.md"), "replacement").unwrap();
    git(&replacement, ["add", "."]);
    git(
        &replacement,
        [
            "-c",
            "user.name=YAASM Test",
            "-c",
            "user.email=yaasm@example.invalid",
            "commit",
            "-m",
            "replacement",
        ],
    );
    write_config(
        &config,
        &format!(
            "version = 1\ntargets = [{}]\n[sources.origin]\ngit = {}\nref = \"main\"\n[skills.demo]\nsource = \"origin\"\npath = \"skills/demo\"\n",
            toml::Value::String(target.to_string_lossy().into_owned()),
            toml::Value::String(replacement.to_string_lossy().into_owned())
        ),
    );
    assert_success(yaasm(&config, &data, "deploy"));
    assert_eq!(
        fs::read_to_string(target.join("demo/SKILL.md")).unwrap(),
        "replacement"
    );
}

#[cfg(unix)]
#[test]
fn checks_gh_auth_once_for_concurrent_clones() {
    let temp = TempDir::new().unwrap();
    let repository = temp.path().join("repository");
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let bin = temp.path().join("bin");
    let log = temp.path().join("gh.log");

    fs::create_dir_all(repository.join("skills/demo")).unwrap();
    git(
        temp.path(),
        ["init", "-b", "main", repository.to_str().unwrap()],
    );
    fs::write(repository.join("skills/demo/SKILL.md"), "demo").unwrap();
    git(&repository, ["add", "."]);
    git(
        &repository,
        [
            "-c",
            "user.name=YAASM Test",
            "-c",
            "user.email=yaasm@example.invalid",
            "commit",
            "-m",
            "demo",
        ],
    );

    fs::create_dir_all(&bin).unwrap();
    let fake_gh = bin.join("gh");
    fs::write(
        &fake_gh,
        r#"#!/bin/sh
if [ "$1" = auth ] && [ "$2" = status ]; then
    printf 'auth\n' >> "$FAKE_GH_LOG"
    exit 0
fi
if [ "$1" = repo ] && [ "$2" = clone ]; then
    destination=$4
    shift 5
    exec git clone "$@" "$FAKE_GH_REPOSITORY" "$destination"
fi
exit 2
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_gh).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_gh, permissions).unwrap();

    write_config(
        &config,
        "version = 1\n[sources.one]\ngithub = \"example/one\"\nref = \"main\"\n[sources.two]\ngithub = \"example/two\"\nref = \"main\"\n[skills.one]\nsource = \"one\"\npath = \"skills/demo\"\n[skills.two]\nsource = \"two\"\npath = \"skills/demo\"\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_yaasm"))
        .arg("--config-dir")
        .arg(&config)
        .arg("--data-dir")
        .arg(&data)
        .arg("deploy")
        .env("PATH", path)
        .env("FAKE_GH_LOG", &log)
        .env("FAKE_GH_REPOSITORY", &repository)
        .output()
        .unwrap();
    assert_success(output);
    assert_eq!(fs::read_to_string(log).unwrap(), "auth\n");
}

fn git<const N: usize>(directory: &Path, arguments: [&str; N]) {
    let output = Command::new("git")
        .current_dir(directory)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert_success(output);
}
