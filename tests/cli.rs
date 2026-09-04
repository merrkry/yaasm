use std::fs;
use std::path::Path;
use std::process::{Command, Output};

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
