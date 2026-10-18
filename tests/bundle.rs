use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    repository: PathBuf,
    config: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        let config = temp.path().join("config");
        let data = temp.path().join("data");
        fs::create_dir_all(repository.join("skills/demo")).unwrap();
        git(&repository, &["init", "-b", "main"]);
        git(&repository, &["config", "core.autocrlf", "false"]);
        fs::write(repository.join("skills/demo/SKILL.md"), "remote\n").unwrap();
        fs::write(
            repository.join("skills/demo/binary"),
            [0, 255, 128, 10, 13].repeat(32 * 1024),
        )
        .unwrap();
        fs::write(
            repository.join("skills/demo/run"),
            "#!/bin/sh\necho hello\n",
        )
        .unwrap();
        fs::write(repository.join("unselected"), "excluded").unwrap();
        git(&repository, &["add", "."]);
        git(
            &repository,
            &["update-index", "--chmod=+x", "skills/demo/run"],
        );
        commit(&repository);
        let fixture = Self {
            temp,
            repository,
            config,
            data,
        };
        fixture.configure(
            "targets = [\"${YAASM_TEST_MISSING}\"]\n",
            "[skills.live]\nsource = \"local\"\npath = \"${YAASM_TEST_MISSING}/absent\"\n",
        );
        success(fixture.command(&["update"]).output().unwrap());
        fixture
    }

    fn configure(&self, targets: &str, locals: &str) {
        fs::create_dir_all(&self.config).unwrap();
        fs::write(self.config.join("config.toml"), format!(
            "version = 1\n{targets}\n[sources.origin]\ngit = {}\nref = \"main\"\n[skills.demo]\nsource = \"origin\"\npath = \"skills/demo\"\n{locals}",
            toml::Value::String(self.repository.to_string_lossy().into_owned()),
        )).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        command(&self.config, &self.data, args)
    }

    fn build(&self, name: &str) -> PathBuf {
        let bundle = self.temp.path().join(name);
        success(
            self.command(&["bundle", "--output"])
                .arg(&bundle)
                .output()
                .unwrap(),
        );
        bundle
    }

    fn target(&self) -> PathBuf {
        let target = self.temp.path().join("target");
        self.configure(
            &format!(
                "targets = [{}]\n",
                toml::Value::String(target.to_string_lossy().into_owned())
            ),
            "",
        );
        target
    }

    fn offline(&self, data: &Path, bundle: &Path) -> Command {
        let bin = self.temp.path().join("empty-bin");
        fs::create_dir_all(&bin).unwrap();
        #[cfg(unix)]
        for program in ["git", "gh"] {
            use std::os::unix::fs::PermissionsExt;
            let path = bin.join(program);
            fs::write(
                &path,
                "#!/bin/sh\nprintf attempted >> \"$YAASM_PROCESS_LOG\"\nexit 99\n",
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut command = command(&self.config, data, &["deploy", "--bundle"]);
        command
            .arg(bundle)
            .env("PATH", &bin)
            .env("YAASM_PROCESS_LOG", self.temp.path().join("process.log"));
        command
    }
}

fn command(config: &Path, data: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_yaasm"));
    command
        .arg("--config-dir")
        .arg(config)
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .env_remove("YAASM_TEST_MISSING")
        .env("HOME", config.parent().unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "");
    command
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn failure(output: Output, message: &str) {
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn commit(repository: &Path) {
    git(
        repository,
        &[
            "-c",
            "user.name=YAASM Test",
            "-c",
            "user.email=yaasm@example.invalid",
            "commit",
            "-m",
            "skills",
        ],
    );
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

fn read_only(root: &Path, enabled: bool) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            read_only(&path, enabled);
        } else {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(if enabled { 0o444 } else { 0o644 });
            }
            #[cfg(windows)]
            permissions.set_readonly(enabled);
            fs::set_permissions(&path, permissions).unwrap();
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            root,
            fs::Permissions::from_mode(if enabled { 0o555 } else { 0o755 }),
        )
        .unwrap();
    }
}

#[test]
fn bundle_reuses_a_relative_local_repository_cache() {
    let fixture = Fixture::new();
    let config = fs::read_to_string(fixture.config.join("config.toml")).unwrap();
    fs::write(
        fixture.config.join("config.toml"),
        config.replace(
            &toml::Value::String(fixture.repository.to_string_lossy().into_owned()).to_string(),
            "\"../repository\"",
        ),
    )
    .unwrap();
    success(
        fixture
            .command(&["update"])
            .current_dir(&fixture.config)
            .output()
            .unwrap(),
    );
    git(
        &fixture.data.join("repos/origin"),
        &[
            "remote",
            "set-url",
            "origin",
            fixture.repository.to_str().unwrap(),
        ],
    );
    let marker = fixture.data.join("repos/origin/.git/cache-marker");
    fs::write(&marker, "existing cache").unwrap();
    success(
        fixture
            .command(&["bundle", "--output"])
            .arg(fixture.temp.path().join("bundle"))
            .current_dir(&fixture.config)
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_to_string(marker).unwrap(), "existing cache");
}

#[test]
fn bundle_replaces_a_cache_from_another_repository_with_the_same_source_name() {
    let old = Fixture::new();
    let current = Fixture::new();
    fs::write(
        current.repository.join("skills/demo/SKILL.md"),
        "new repository\n",
    )
    .unwrap();
    git(&current.repository, &["add", "."]);
    commit(&current.repository);
    success(current.command(&["update"]).output().unwrap());
    let target = current.target();
    fs::copy(
        current.config.join("config.toml"),
        old.config.join("config.toml"),
    )
    .unwrap();
    fs::copy(
        current.config.join("lock.toml"),
        old.config.join("lock.toml"),
    )
    .unwrap();
    let bundle = old.build("bundle");
    success(old.offline(&old.data, &bundle).output().unwrap());
    assert_eq!(
        fs::read_to_string(target.join("demo/SKILL.md")).unwrap(),
        "new repository\n"
    );
}

#[test]
fn remote_bundle_is_deterministic_and_deploys_live_locals_offline_after_relocation() {
    let fixture = Fixture::new();
    git(
        &fixture.data.join("repos/origin"),
        &["config", "core.autocrlf", "true"],
    );
    assert!(!fixture.data.join("skills/demo").exists());
    let lock = fs::read(fixture.config.join("lock.toml")).unwrap();
    let first = fixture.build("bundle-one");
    let expected = tree(&first);
    assert!(
        !fs::read_to_string(first.join("bundle.toml"))
            .unwrap()
            .contains(fixture.temp.path().to_str().unwrap())
    );
    let local = fixture.config.join("live");
    fs::create_dir(&local).unwrap();
    fs::write(local.join("SKILL.md"), "editable").unwrap();
    fixture.configure("targets = [\"${YAASM_TEST_MISSING}/different-target\"]\n", "[skills.renamed-local]\nsource = \"local\"\npath = \"${YAASM_TEST_MISSING}/different-local\"\n");
    let second = fixture.temp.path().join("bundle-two");
    fs::remove_file(fixture.config.join(".process.lock")).unwrap();
    read_only(&fixture.config, true);
    success(
        command(
            &fixture.config,
            &fixture.temp.path().join("cold-builder"),
            &["bundle", "--output"],
        )
        .arg(&second)
        .env_remove("HOME")
        .output()
        .unwrap(),
    );
    assert!(!fixture.config.join(".process.lock").exists());
    read_only(&fixture.config, false);
    assert_eq!(expected, tree(&second));
    assert_eq!(lock, fs::read(fixture.config.join("lock.toml")).unwrap());
    let moved = fixture.temp.path().join("relocated");
    fs::rename(&second, &moved).unwrap();
    let target = fixture.temp.path().join("target");
    fixture.configure(
        &format!(
            "targets = [{}]\n",
            toml::Value::String(target.to_string_lossy().into_owned())
        ),
        "[skills.live]\nsource = \"local\"\npath = \"${YAASM_CONFIG_DIR}/live\"\n",
    );
    let cold = fixture.temp.path().join("cold-deployment");
    fs::remove_dir_all(&fixture.repository).unwrap();
    read_only(&fixture.config, true);
    read_only(&moved, true);
    success(fixture.offline(&cold, &moved).output().unwrap());
    assert!(!fixture.config.join(".process.lock").exists());
    assert!(!fixture.temp.path().join("process.log").exists());
    assert_eq!(
        fs::read(target.join("demo/binary")).unwrap(),
        [0, 255, 128, 10, 13].repeat(32 * 1024)
    );
    assert_eq!(
        fs::read_to_string(target.join("live/SKILL.md")).unwrap(),
        "editable"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(target.join("demo/run"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
        assert_eq!(
            fs::read_link(target.join("demo")).unwrap(),
            fs::canonicalize(&cold).unwrap().join("skills/demo")
        );
    }
    assert_eq!(tree(&moved), expected);
    read_only(&fixture.config, false);
    fs::write(local.join("SKILL.md"), "changed live").unwrap();
    assert_eq!(
        fs::read_to_string(target.join("live/SKILL.md")).unwrap(),
        "changed live"
    );
    success(fixture.offline(&cold, &moved).output().unwrap());
    read_only(&moved, false);
    fs::remove_dir_all(moved).unwrap();
    assert_eq!(
        fs::read_to_string(target.join("demo/SKILL.md")).unwrap(),
        "remote\n"
    );
    assert_eq!(lock, fs::read(fixture.config.join("lock.toml")).unwrap());
}

#[test]
fn offline_bundle_rejects_mismatched_inputs_and_missing_or_corrupt_objects() {
    let fixture = Fixture::new();
    let bundle = fixture.build("bundle");
    let target = fixture.target();
    let cold = fixture.temp.path().join("cold");
    let config = fs::read_to_string(fixture.config.join("config.toml")).unwrap();
    let lock = fs::read_to_string(fixture.config.join("lock.toml")).unwrap();
    fs::write(
        fixture.config.join("config.toml"),
        config.replace("path = \"skills/demo\"", "path = \"other\""),
    )
    .unwrap();
    failure(
        fixture.offline(&cold, &bundle).output().unwrap(),
        "bundle does not match",
    );
    assert!(!target.exists());
    fs::write(fixture.config.join("config.toml"), &config).unwrap();
    let parsed: toml::Value = toml::from_str(&lock).unwrap();
    let revision = parsed["sources"]["origin"]["revision"].as_str().unwrap();
    fs::write(
        fixture.config.join("lock.toml"),
        lock.replace(revision, &"0".repeat(revision.len())),
    )
    .unwrap();
    failure(
        fixture.offline(&cold, &bundle).output().unwrap(),
        "bundle does not match",
    );
    fs::write(fixture.config.join("lock.toml"), &lock).unwrap();
    fs::write(
        fixture.config.join("config.toml"),
        config.replace("ref = \"main\"", "ref = \"different\""),
    )
    .unwrap();
    failure(
        fixture.offline(&cold, &bundle).output().unwrap(),
        "does not match configuration",
    );
    fs::write(fixture.config.join("config.toml"), &config).unwrap();
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(bundle.join("bundle.toml")).unwrap()).unwrap();
    let hash = manifest["sources"]["origin"]["files"]["skills/demo/binary"]["sha256"]
        .as_str()
        .unwrap();
    let object = bundle.join("objects").join(hash);
    let contents = fs::read(&object).unwrap();
    fs::remove_file(&object).unwrap();
    failure(
        fixture.offline(&cold, &bundle).output().unwrap(),
        "missing bundle object",
    );
    fs::write(&object, &contents).unwrap();
    success(fixture.offline(&cold, &bundle).output().unwrap());
    let mut corrupt = contents.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&object, corrupt).unwrap();
    failure(
        fixture.offline(&cold, &bundle).output().unwrap(),
        "integrity check failed",
    );
    assert_eq!(
        fs::read_to_string(target.join("demo/SKILL.md")).unwrap(),
        "remote\n"
    );
    assert!(!fixture.temp.path().join("process.log").exists());
}

#[test]
fn offline_deployment_preserves_collision_and_cleanup_barriers() {
    let fixture = Fixture::new();
    let bundle = fixture.build("bundle");
    let target = fixture.target();
    fs::create_dir(&target).unwrap();
    fs::write(target.join("foreign-keep"), "unmanaged").unwrap();
    success(fixture.offline(&fixture.data, &bundle).output().unwrap());
    fixture.configure(
        &format!(
            "targets = [{}]\n",
            toml::Value::String(target.to_string_lossy().into_owned())
        ),
        "[skills.stale]\nsource = \"local\"\npath = \"${YAASM_CONFIG_DIR}/stale\"\n",
    );
    fs::create_dir(fixture.config.join("stale")).unwrap();
    fs::write(fixture.config.join("stale/SKILL.md"), "stale").unwrap();
    success(fixture.offline(&fixture.data, &bundle).output().unwrap());
    fixture.target();
    fs::rename(target.join("demo"), target.join("old-demo")).unwrap();
    fs::write(target.join("demo"), "foreign").unwrap();
    failure(
        fixture.offline(&fixture.data, &bundle).output().unwrap(),
        "not managed by YAASM",
    );
    assert_eq!(fs::read_to_string(target.join("demo")).unwrap(), "foreign");
    assert!(target.join("stale/SKILL.md").exists());
    assert!(fixture.data.join("skills/stale").exists());
    fs::remove_file(target.join("demo")).unwrap();
    success(fixture.offline(&fixture.data, &bundle).output().unwrap());
    assert!(fs::symlink_metadata(target.join("stale")).is_err());
    assert!(fs::symlink_metadata(target.join("old-demo")).is_err());
    assert!(fs::symlink_metadata(fixture.data.join("skills/stale")).is_err());
    assert_eq!(
        fs::read_to_string(target.join("demo/SKILL.md")).unwrap(),
        "remote\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("foreign-keep")).unwrap(),
        "unmanaged"
    );
}

#[test]
fn bundle_preserves_git_symlink_metadata_and_reports_platform_limits() {
    let fixture = Fixture::new();
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&fixture.repository)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"SKILL.md").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let object = String::from_utf8(output.stdout).unwrap();
    git(
        &fixture.repository,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{},skills/demo/link", object.trim()),
        ],
    );
    commit(&fixture.repository);
    git(
        &fixture.data.join("repos/origin"),
        &["config", "core.symlinks", "false"],
    );
    success(fixture.command(&["update"]).output().unwrap());
    let bundle = fixture.build("bundle");
    let target = fixture.target();
    let output = fixture.offline(&fixture.data, &bundle).output().unwrap();
    #[cfg(unix)]
    {
        success(output);
        assert_eq!(
            fs::read_link(target.join("demo/link")).unwrap(),
            Path::new("SKILL.md")
        );
        assert_eq!(
            fs::read_to_string(target.join("demo/link")).unwrap(),
            "remote\n"
        );
    }
    #[cfg(windows)]
    {
        failure(output, "cannot be restored on Windows");
        assert!(fs::symlink_metadata(target.join("demo")).is_err());
    }
}

#[test]
fn bundle_build_requires_matching_lock_and_complete_skill_content() {
    let fixture = Fixture::new();
    let output = fixture.temp.path().join("output");
    let config = fs::read_to_string(fixture.config.join("config.toml")).unwrap();
    fs::write(
        fixture.config.join("config.toml"),
        format!("{config}\n[skills.broken]\nsource = \"unknown\"\npath = \"skills/broken\"\n"),
    )
    .unwrap();
    failure(
        fixture.command(&["update"]).output().unwrap(),
        "unknown source `unknown`",
    );
    fs::write(fixture.config.join("config.toml"), &config).unwrap();
    let lock = fs::read(fixture.config.join("lock.toml")).unwrap();
    fs::remove_file(fixture.config.join("lock.toml")).unwrap();
    failure(
        fixture
            .command(&["bundle", "--output"])
            .arg(&output)
            .output()
            .unwrap(),
        "lockfile has no entry",
    );
    fs::write(fixture.config.join("lock.toml"), lock).unwrap();
    let config = fs::read_to_string(fixture.config.join("config.toml")).unwrap();
    fs::write(
        fixture.config.join("config.toml"),
        config.replace("path = \"skills/demo\"", "path = \"unselected\""),
    )
    .unwrap();
    failure(
        fixture
            .command(&["bundle", "--output"])
            .arg(&output)
            .output()
            .unwrap(),
        "no regular SKILL.md",
    );
    assert!(!output.exists());
    assert!(!fs::read_dir(fixture.temp.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".yaasm-bundle-")
    }));
    fs::write(fixture.config.join("config.toml"), config).unwrap();
    let bundle = fixture.build("output");
    let before = tree(&bundle);
    failure(
        fixture
            .command(&["bundle", "--output"])
            .arg(&bundle)
            .output()
            .unwrap(),
        "already exists",
    );
    assert_eq!(tree(&bundle), before);
}

#[test]
fn bundle_manifest_errors_and_input_overlap_never_write_to_the_bundle() {
    let fixture = Fixture::new();
    let bundle = fixture.build("bundle");
    fixture.target();
    let original = fs::read_to_string(bundle.join("bundle.toml")).unwrap();
    fs::write(
        bundle.join("bundle.toml"),
        original.replacen("version = 1", "version = 999", 1),
    )
    .unwrap();
    failure(
        fixture.offline(&fixture.data, &bundle).output().unwrap(),
        "unsupported bundle version",
    );
    let mut manifest: toml::Value = toml::from_str(&original).unwrap();
    let files = manifest["sources"]["origin"]["files"]
        .as_table_mut()
        .unwrap();
    let file = files.values().next().unwrap().clone();
    files.insert("../escaped".into(), file);
    fs::write(
        bundle.join("bundle.toml"),
        toml::to_string(&manifest).unwrap(),
    )
    .unwrap();
    failure(
        fixture.offline(&fixture.data, &bundle).output().unwrap(),
        "not portable",
    );
    assert!(!fixture.data.join("escaped").exists());
    fs::write(bundle.join("bundle.toml"), original).unwrap();
    let before = tree(&bundle);
    failure(
        fixture
            .offline(&bundle.join("new-data"), &bundle)
            .output()
            .unwrap(),
        "paths must not be equal or nested",
    );
    assert!(!bundle.join("new-data").exists());
    #[cfg(unix)]
    {
        let alias = fixture.temp.path().join("bundle-alias");
        std::os::unix::fs::symlink(&bundle, &alias).unwrap();
        fixture.configure(
            &format!(
                "targets = [{}]\n",
                toml::Value::String(alias.join("new-target").to_string_lossy().into_owned())
            ),
            "",
        );
        failure(
            fixture.offline(&fixture.data, &bundle).output().unwrap(),
            "paths must not be equal or nested",
        );
        assert!(!bundle.join("new-target").exists());
    }
    assert_eq!(tree(&bundle), before);
}

#[test]
fn damaged_bundle_source_preserves_previous_links_and_deploys_healthy_siblings() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.repository.join("skills/other")).unwrap();
    fs::write(
        fixture.repository.join("skills/other/SKILL.md"),
        "other source",
    )
    .unwrap();
    git(&fixture.repository, &["add", "."]);
    commit(&fixture.repository);
    let target = fixture.temp.path().join("target");
    let targets = format!(
        "targets = [{}]\n",
        toml::Value::String(target.to_string_lossy().into_owned())
    );
    let siblings = format!(
        "[sources.second]\ngit = {}\nref = \"main\"\n[skills.other]\nsource = \"second\"\npath = \"skills/other\"\n[skills.live]\nsource = \"local\"\npath = \"${{YAASM_CONFIG_DIR}}/live\"\n",
        toml::Value::String(fixture.repository.to_string_lossy().into_owned())
    );
    fs::create_dir_all(fixture.config.join("live")).unwrap();
    fs::write(fixture.config.join("live/SKILL.md"), "live source").unwrap();
    fixture.configure(&targets, &siblings);
    success(fixture.command(&["update"]).output().unwrap());
    let bundle = fixture.build("bundle");
    success(fixture.offline(&fixture.data, &bundle).output().unwrap());
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(bundle.join("bundle.toml")).unwrap()).unwrap();
    let hash = manifest["sources"]["origin"]["files"]["skills/demo/SKILL.md"]["sha256"]
        .as_str()
        .unwrap();
    fs::remove_file(bundle.join("objects").join(hash)).unwrap();
    let cold = fixture.temp.path().join("cold-sibling-test");
    failure(
        fixture.offline(&fixture.data, &bundle).output().unwrap(),
        "missing bundle object",
    );
    assert_eq!(
        fs::read_to_string(target.join("demo/SKILL.md")).unwrap(),
        "remote\n"
    );
    let cold_target = fixture.temp.path().join("cold-target");
    fixture.configure(
        &format!(
            "targets = [{}]\n",
            toml::Value::String(cold_target.to_string_lossy().into_owned())
        ),
        &siblings,
    );
    failure(
        fixture.offline(&cold, &bundle).output().unwrap(),
        "missing bundle object",
    );
    assert_eq!(
        fs::read_to_string(cold_target.join("other/SKILL.md")).unwrap(),
        "other source"
    );
    assert_eq!(
        fs::read_to_string(cold_target.join("live/SKILL.md")).unwrap(),
        "live source"
    );
    assert!(fs::symlink_metadata(cold_target.join("demo")).is_err());
    assert!(!fixture.temp.path().join("process.log").exists());
}

#[cfg(unix)]
#[test]
fn filesystem_aliases_cannot_follow_a_restored_symlink_out_of_staging() {
    use sha2::{Digest, Sha256};

    let fixture = Fixture::new();
    let bundle = fixture.build("bundle");
    fixture.target();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let target = outside.to_str().unwrap().as_bytes();
    let hash = hex::encode(Sha256::digest(target));
    fs::write(bundle.join("objects").join(&hash), target).unwrap();
    let mut manifest: toml::Value =
        toml::from_str(&fs::read_to_string(bundle.join("bundle.toml")).unwrap()).unwrap();
    let files = manifest["sources"]["origin"]["files"]
        .as_table_mut()
        .unwrap();
    let regular = files["skills/demo/SKILL.md"].clone();
    let symlink = toml::from_str(&format!("kind = \"symlink\"\nsha256 = \"{hash}\"\n")).unwrap();
    // The decomposed spelling sorts first. On a normalization-insensitive
    // filesystem the later composed parent names the restored symlink.
    files.insert("skills/demo/e\u{301}".into(), symlink);
    files.insert("skills/demo/é/escaped".into(), regular);
    fs::write(
        bundle.join("bundle.toml"),
        toml::to_string(&manifest).unwrap(),
    )
    .unwrap();
    let output = fixture.offline(&fixture.data, &bundle).output().unwrap();
    if !output.status.success() {
        failure(output, "bundle parent is not a directory");
    }
    assert!(!outside.join("escaped").exists());
}
