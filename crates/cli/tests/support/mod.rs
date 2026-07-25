#![allow(dead_code)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use devspace_machine::MachineGitRepository as MachineRepository;
use devspace_machine::{
    CatalogEntry, MACHINE_STORE_OVERRIDE, MachineConfig, MachineId, MachineStore, RepositoryId,
    RepositoryIdentity, RepositoryIncarnation, RepositoryName, SharedSecret,
};
use jj_lib::object_id::ObjectId as _;
use jj_lib::settings::UserSettings;

pub mod worker;

pub const TEST_MACHINE_ID: &str = "12121212121212121212121212121212";
pub const TEST_SHARED_SECRET: &str = "cli-development-secret";
const TEST_REPOSITORY_ID: &str = "abababababababababababababababababababababababababababababababab";
const TEST_INCARNATION: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

pub fn settings() -> UserSettings {
    devspace_testutils::settings("Devspace Test", "devspace@example.invalid", false)
}

pub fn write_cli_config(root: &Path) -> PathBuf {
    let path = root.join("jj-config.toml");
    fs::write(
        &path,
        r#"
            [user]
            name = "Devspace Test"
            email = "devspace@example.invalid"

            [ui]
            color = "never"

            [snapshot]
            auto-update-stale = true
        "#,
    )
    .unwrap();
    path
}

/// Append more TOML to a config already written by [`write_cli_config`].
pub fn append_cli_config(path: &Path, extra: &str) {
    let mut contents = fs::read_to_string(path).unwrap();
    contents.push_str(extra);
    fs::write(path, contents).unwrap();
}

pub fn ds(cwd: &Path, config: &Path, args: &[&str]) -> Output {
    ds_command(cwd, config).args(args).output().unwrap()
}

pub fn ds_with_home(cwd: &Path, home: &Path, config: &Path, args: &[&str]) -> Output {
    ds_command_with_home(cwd, home, config)
        .args(args)
        .output()
        .unwrap()
}

pub fn ds_with_env(
    cwd: &Path,
    home: &Path,
    config: &Path,
    args: &[&str],
    environment: &[(&str, &str)],
) -> Output {
    let mut command = ds_command_with_home(cwd, home, config);
    command.args(args).envs(environment.iter().copied());
    command.output().unwrap()
}

pub fn ds_command(cwd: &Path, config: &Path) -> Command {
    ds_command_with_home(cwd, config.parent().unwrap(), config)
}

pub fn ds_command_with_home(cwd: &Path, home: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ds"));
    command
        .current_dir(cwd)
        .env(MACHINE_STORE_OVERRIDE, home.join("machine-store"))
        .env("JJ_CONFIG", config)
        .env("DEVSPACE_BOUNDARY_SYNC", "0")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat");
    command
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn machine_store(root: &Path) -> MachineStore {
    MachineStore::new(root.join("machine-store"))
}

pub fn configure_machine(root: &Path, base_url: &str) {
    configure_machine_as(root, base_url, TEST_MACHINE_ID, TEST_SHARED_SECRET);
}

pub fn configure_machine_as(root: &Path, base_url: &str, machine_id: &str, secret: &str) {
    write_machine_config(root, base_url, machine_id, secret, None);
}

pub fn configure_machine_from_env(root: &Path, machine_id: &str) {
    let base_url = env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret = env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    configure_machine_as(root, &base_url, machine_id, &shared_secret);
}

pub fn configure_machine_with_name(root: &Path, base_url: &str, machine_name: Option<&str>) {
    write_machine_config(
        root,
        base_url,
        TEST_MACHINE_ID,
        TEST_SHARED_SECRET,
        machine_name,
    );
}

pub fn set_machine_git_shim(root: &Path, enabled: bool) {
    let store = machine_store(root);
    let config = store.load_config().unwrap().with_git_shim(enabled);
    store.write_config(&config).unwrap();
}

fn write_machine_config(
    root: &Path,
    base_url: &str,
    machine_id: &str,
    secret: &str,
    machine_name: Option<&str>,
) {
    let config = MachineConfig::new(
        base_url,
        MachineId::parse(machine_id).unwrap(),
        SharedSecret::new(secret).unwrap(),
    )
    .unwrap();
    let config = match machine_name {
        Some(name) => config.with_machine_name(name).unwrap(),
        None => config,
    };
    machine_store(root).write_config(&config).unwrap();
}

pub fn seal_commit(cwd: &Path, home: &Path, config: &Path, description: &str) {
    let described = ds_with_home(cwd, home, config, &["describe", "-m", description]);
    assert!(described.status.success(), "{}", stderr(&described));
    let sealed = ds_with_home(cwd, home, config, &["new"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
}

pub fn commit_id(cwd: &Path, config: &Path, revision: &str) -> String {
    commit_id_with_home(cwd, config.parent().unwrap(), config, revision)
}

pub fn commit_id_with_home(cwd: &Path, home: &Path, config: &Path, revision: &str) -> String {
    let output = ds_with_home(
        cwd,
        home,
        config,
        &[
            "log",
            "-r",
            revision,
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).trim().to_owned()
}

pub fn repository_commit_ids(home: &Path, config: &Path, name: &str) -> Vec<String> {
    let output = ds_with_home(
        home,
        home,
        config,
        &[
            "-R",
            name,
            "log",
            "-r",
            "all()",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).lines().map(str::to_owned).collect()
}

pub async fn operation_heads(repository_path: &Path) -> Vec<String> {
    let repository = MachineRepository::open(repository_path, &settings())
        .await
        .unwrap();
    let mut heads = repository
        .repo()
        .op_heads_store()
        .get_op_heads()
        .await
        .unwrap()
        .into_iter()
        .map(|head| head.hex())
        .collect::<Vec<_>>();
    heads.sort();
    heads
}

pub fn poll_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    condition()
}

pub fn daemon_socket_path(store_root: &Path) -> PathBuf {
    devspace_cli::daemon_socket_path(store_root).expect("a supported daemon socket path")
}

/// The identity every local-only fixture registers its repository under.
pub fn test_identity() -> RepositoryIdentity {
    RepositoryIdentity::new(
        RepositoryId::parse(TEST_REPOSITORY_ID).unwrap(),
        RepositoryIncarnation::parse(TEST_INCARNATION).unwrap(),
    )
}

/// A distinct identity per `byte`, for suites that register several repositories.
pub fn identity(byte: u8) -> RepositoryIdentity {
    RepositoryIdentity::new(
        RepositoryId::parse(format!("{byte:02x}").repeat(32)).unwrap(),
        RepositoryIncarnation::parse(format!("{:02x}", byte + 1).repeat(16)).unwrap(),
    )
}

/// Register `name` under the test identity and initialize its native repository.
pub async fn registered_repository(root: &Path, name: &str) -> CatalogEntry {
    registered_repository_with_identity(root, name, test_identity()).await
}

pub async fn registered_repository_with_identity(
    root: &Path,
    name: &str,
    identity: RepositoryIdentity,
) -> CatalogEntry {
    let entry = machine_store(root)
        .register_repository(RepositoryName::parse(name).unwrap(), identity)
        .unwrap();
    MachineRepository::init(&entry.native_repository_path, &settings())
        .await
        .unwrap();
    entry
}

pub fn request_body(request: &str) -> &str {
    request.split_once("\r\n\r\n").unwrap().1
}

pub fn request_json(request: &str) -> serde_json::Value {
    serde_json::from_str(request_body(request)).unwrap()
}

pub fn set_bookmark(cwd: &Path, home: &Path, config: &Path, name: &str, revision: &str) {
    let output = ds_with_home(
        cwd,
        home,
        config,
        &["bookmark", "set", name, "-r", revision],
    );
    assert!(output.status.success(), "{}", stderr(&output));
}

/// The Git object a bare remote's `refs/heads/{bookmark}` points at.
pub fn remote_ref(remote: &Path, bookmark: &str) -> Option<[u8; 20]> {
    let output = git_command(
        &[
            "show-ref",
            "--hash",
            "--verify",
            &format!("refs/heads/{bookmark}"),
        ],
        Some(remote),
    )
    .output()
    .unwrap();
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).unwrap();
    Some(parse_git_oid(value.trim()))
}

pub fn parse_git_oid(value: &str) -> [u8; 20] {
    std::array::from_fn(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).unwrap())
}

pub fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

pub fn git(args: &[&str], git_dir: Option<&Path>) {
    let output = git_command(args, git_dir).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        stderr(&output)
    );
}

pub fn git_output(args: &[&str], git_dir: Option<&Path>) -> String {
    let output = git_command(args, git_dir).output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    String::from_utf8(output.stdout).unwrap()
}

pub fn git_command(args: &[&str], git_dir: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(git_dir) = git_dir {
        command.arg("--git-dir").arg(git_dir);
    }
    command.args(args);
    command
}

pub fn assert_no_private_objects(remote: &Path, sentinel: &[u8]) {
    let objects = git_output(
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ],
        Some(remote),
    );
    for line in objects.lines() {
        let (id, _) = line.split_once(' ').unwrap();
        let object = git_output(&["cat-file", "-p", id], Some(remote));
        assert!(!contains_bytes(object.as_bytes(), sentinel));
    }
}

/// A repository name unique to this process and temporary directory, so the live
/// suites never collide on a shared Worker.
pub fn unique_repository_name(temp: &Path, prefix: &str) -> String {
    let suffix = temp
        .file_name()
        .unwrap()
        .to_string_lossy()
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_lowercase() as char)
        .collect::<String>();
    format!("{prefix}-{}-{suffix}", std::process::id())
}

pub mod fs_util {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    pub fn remove_dir_all(path: &Path) {
        make_directories_writable(path);
        fs::remove_dir_all(path).unwrap();
    }

    fn make_directories_writable(path: &Path) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                make_directories_writable(&entry.path());
            }
        }
        let mut permissions = fs::symlink_metadata(path).unwrap().permissions();
        permissions.set_mode(permissions.mode() | 0o700);
        fs::set_permissions(path, permissions).unwrap();
    }
}
