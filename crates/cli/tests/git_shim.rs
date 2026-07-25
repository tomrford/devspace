#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use devspace_machine::MachineGitRepository as MachineRepository;
use gix::bstr::BStr;
use jj_lib::repo::Repo as _;

mod support;

use support::{
    commit_id, configure_machine, ds, ds_command, machine_store, registered_repository,
    set_machine_git_shim, settings, stderr, stdout, write_cli_config,
};

async fn local_repository(root: &Path, name: &str) -> PathBuf {
    let store = machine_store(root);
    if store.load_config().is_err() {
        configure_machine(root, "http://127.0.0.1:1");
    }
    registered_repository(root, name)
        .await
        .native_repository_path
}

fn set_git_shim(config: &Path, enabled: bool) {
    let root = config.parent().unwrap();
    if machine_store(root).load_config().is_err() {
        configure_machine(root, "http://127.0.0.1:1");
    }
    set_machine_git_shim(root, enabled);
}

fn add_checkout(root: &Path, config: &Path, name: &str, checkout: &Path) -> Output {
    ds_command(root, config)
        .args(["add", name, "-r", "root()"])
        .arg(checkout)
        .output()
        .unwrap()
}

fn git(checkout: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(checkout)
        .args(args)
        .output()
        .unwrap()
}

fn git_ls_files(checkout: &Path) -> Vec<String> {
    let output = git(checkout, &["ls-files"]);
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output).lines().map(str::to_owned).collect()
}

fn index_entry(checkout: &Path, path: &str) -> gix::index::Entry {
    let repo = gix::open(checkout).unwrap();
    let index = repo.open_index().unwrap();
    index
        .entry_by_path(BStr::new(path))
        .unwrap_or_else(|| panic!("missing index entry for {path}"))
        .clone()
}

fn assert_git_directories_read_only(path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert_eq!(
        metadata.permissions().mode() & 0o222,
        0,
        "{} is writable",
        path.display()
    );
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            assert_git_directories_read_only(&entry.path());
        }
    }
}

fn rewrite_git_directory_modes(path: &Path, writable: bool) {
    let metadata = fs::symlink_metadata(path).unwrap();
    if metadata.is_dir() {
        let mut permissions = metadata.permissions();
        let mode = if writable {
            permissions.mode() | 0o700
        } else {
            permissions.mode() & !0o222
        };
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                rewrite_git_directory_modes(&entry.path(), writable);
            }
        }
    }
}

async fn canonical_git_dir(repository_path: &Path) -> PathBuf {
    let repository = MachineRepository::open(repository_path, &settings())
        .await
        .unwrap();
    jj_lib::git::get_git_backend(repository.repo().store())
        .unwrap()
        .git_repo_path()
        .to_owned()
}

fn file_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &entry.path(), files);
            } else {
                files.insert(
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[tokio::test]
async fn git_shim_is_off_by_default() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    local_repository(temp.path(), "shim-default-off").await;
    let checkout = temp.path().join("checkout");

    let added = add_checkout(temp.path(), &config, "shim-default-off", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(!checkout.join(".git").exists());

    fs::write(checkout.join("public.txt"), "visible\n").unwrap();
    let status = ds(&checkout, &config, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(!checkout.join(".git").exists());
}

#[tokio::test]
async fn root_checkout_has_minimal_unborn_git_view() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    let repository_path = local_repository(temp.path(), "shim-root").await;
    let checkout = temp.path().join("checkout");

    let added = add_checkout(temp.path(), &config, "shim-root", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    let git_dir = checkout.join(".git");
    assert_eq!(
        fs::read_to_string(git_dir.join("HEAD")).unwrap(),
        "ref: refs/jj/root\n"
    );
    assert!(!git_dir.join("refs/jj/root").exists());
    assert!(
        !git(&checkout, &["rev-parse", "--verify", "HEAD"])
            .status
            .success()
    );
    assert_eq!(git_ls_files(&checkout), Vec::<String>::new());

    for obsolete in ["description", "hooks", "info"] {
        assert!(!git_dir.join(obsolete).exists());
    }
    let canonical_git = canonical_git_dir(&repository_path).await;
    let alternate = fs::read_to_string(git_dir.join("objects/info/alternates")).unwrap();
    assert_eq!(
        alternate,
        format!("{}\n", canonical_git.join("objects").display())
    );
    assert_ne!(git_dir.join("objects"), canonical_git.join("objects"));
    assert_git_directories_read_only(&git_dir);
}

#[tokio::test]
async fn canonical_hidden_and_ignored_paths_are_visible_as_intent_to_add() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-local-private").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-local-private", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));

    fs::write(checkout.join(".gitignore"), "*.env\n").unwrap();
    fs::write(checkout.join(".dsprivate"), "*.env\n").unwrap();
    fs::write(checkout.join("secret.env"), "private\n").unwrap();
    fs::write(checkout.join("public.txt"), "public\n").unwrap();
    let refreshed = ds(&checkout, &config, &["status"]);
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));

    assert_eq!(
        git_ls_files(&checkout),
        [".dsprivate", ".gitignore", "public.txt", "secret.env"]
    );
    for path in [".dsprivate", ".gitignore", "public.txt", "secret.env"] {
        let entry = index_entry(&checkout, path);
        assert!(
            entry
                .flags
                .contains(gix::index::entry::Flags::INTENT_TO_ADD),
            "{path} was not intent-to-add"
        );
    }
    assert!(!checkout.join(".git/info/exclude").exists());
}

#[tokio::test]
async fn head_and_index_show_parent_to_working_copy_changes() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-parent").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-parent", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));

    fs::write(checkout.join("modified.txt"), "base\n").unwrap();
    fs::write(checkout.join("deleted.txt"), "base\n").unwrap();
    fs::write(checkout.join("unchanged.txt"), "base\n").unwrap();
    let sealed = ds(&checkout, &config, &["new", "-m", "base"]);
    assert!(sealed.status.success(), "{}", stderr(&sealed));
    let parent_id = commit_id(&checkout, &config, "@-");
    rewrite_git_directory_modes(&checkout.join(".git"), true);
    let refreshed_stat = git(&checkout, &["update-index", "--refresh"]);
    assert!(
        refreshed_stat.status.success(),
        "{}",
        stderr(&refreshed_stat)
    );
    let unchanged_stat = index_entry(&checkout, "unchanged.txt").stat;
    assert_ne!(unchanged_stat, gix::index::entry::Stat::default());
    rewrite_git_directory_modes(&checkout.join(".git"), false);

    fs::write(checkout.join("modified.txt"), "working copy\n").unwrap();
    fs::remove_file(checkout.join("deleted.txt")).unwrap();
    fs::write(checkout.join("new.txt"), "new\n").unwrap();
    let refreshed = ds(&checkout, &config, &["status"]);
    assert!(refreshed.status.success(), "{}", stderr(&refreshed));

    assert_eq!(
        fs::read_to_string(checkout.join(".git/HEAD")).unwrap(),
        format!("{parent_id}\n")
    );
    assert_eq!(
        git_ls_files(&checkout),
        ["deleted.txt", "modified.txt", "new.txt", "unchanged.txt"]
    );
    assert!(
        index_entry(&checkout, "new.txt")
            .flags
            .contains(gix::index::entry::Flags::INTENT_TO_ADD)
    );
    for path in ["deleted.txt", "modified.txt", "unchanged.txt"] {
        assert!(
            !index_entry(&checkout, path)
                .flags
                .contains(gix::index::entry::Flags::INTENT_TO_ADD)
        );
    }
    assert_eq!(index_entry(&checkout, "unchanged.txt").stat, unchanged_stat);
    let diff = git(&checkout, &["diff", "--name-status"]);
    assert!(diff.status.success(), "{}", stderr(&diff));
    let diff = stdout(&diff);
    assert!(diff.contains("D\tdeleted.txt"), "{diff}");
    assert!(diff.contains("M\tmodified.txt"), "{diff}");
    assert!(diff.contains("A\tnew.txt"), "{diff}");
}

#[tokio::test]
async fn two_sided_parent_conflict_uses_git_index_stages() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-conflict").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-conflict", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));

    fs::write(checkout.join("conflict.txt"), "base\n").unwrap();
    assert!(
        ds(&checkout, &config, &["new", "-m", "base"])
            .status
            .success()
    );
    let base_id = commit_id(&checkout, &config, "@-");

    fs::write(checkout.join("conflict.txt"), "left\n").unwrap();
    assert!(
        ds(&checkout, &config, &["new", "-m", "left"])
            .status
            .success()
    );
    let left_id = commit_id(&checkout, &config, "@-");

    assert!(ds(&checkout, &config, &["new", &base_id]).status.success());
    fs::write(checkout.join("conflict.txt"), "right\n").unwrap();
    assert!(
        ds(&checkout, &config, &["new", "-m", "right"])
            .status
            .success()
    );
    let right_id = commit_id(&checkout, &config, "@-");

    let merged = ds(
        &checkout,
        &config,
        &["new", &left_id, &right_id, "-m", "merge"],
    );
    assert!(merged.status.success(), "{}", stderr(&merged));
    let unmerged = git(&checkout, &["ls-files", "--unmerged"]);
    assert!(unmerged.status.success(), "{}", stderr(&unmerged));
    let stages = stdout(&unmerged)
        .lines()
        .map(|line| line.split_whitespace().nth(2).unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(stages, ["1", "2", "3"]);
    assert_eq!(
        fs::read_to_string(checkout.join(".git/HEAD")).unwrap(),
        format!("{left_id}\n")
    );
}

#[tokio::test]
async fn many_sided_parent_conflict_adds_dummy_safety_entry() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-many-conflict").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-many-conflict", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));

    fs::write(checkout.join("conflict.txt"), "base\n").unwrap();
    assert!(
        ds(&checkout, &config, &["new", "-m", "base"])
            .status
            .success()
    );
    let base_id = commit_id(&checkout, &config, "@-");
    let mut sides = Vec::new();
    for side in ["left", "middle", "right"] {
        assert!(ds(&checkout, &config, &["new", &base_id]).status.success());
        fs::write(checkout.join("conflict.txt"), format!("{side}\n")).unwrap();
        assert!(
            ds(&checkout, &config, &["new", "-m", side])
                .status
                .success()
        );
        sides.push(commit_id(&checkout, &config, "@-"));
    }

    let merged = ds(
        &checkout,
        &config,
        &[
            "new",
            &sides[0],
            &sides[1],
            &sides[2],
            "-m",
            "many-sided merge",
        ],
    );
    assert!(merged.status.success(), "{}", stderr(&merged));
    let unmerged = git(&checkout, &["ls-files", "--unmerged"]);
    assert!(unmerged.status.success(), "{}", stderr(&unmerged));
    assert!(
        stdout(&unmerged).contains(".jj-do-not-resolve-this-conflict"),
        "{}",
        stdout(&unmerged)
    );
}

#[tokio::test]
async fn unchanged_view_skips_refresh_and_concurrent_refreshes_serialize() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-concurrent").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-concurrent", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    let state_before = fs::read(checkout.join(".jj/devspace-git-shim.state")).unwrap();

    let no_op = ds_command(&checkout, &config)
        .env("DEVSPACE_FAILPOINT", "git_shim_after_head")
        .arg("status")
        .output()
        .unwrap();
    assert!(no_op.status.success(), "{}", stderr(&no_op));
    assert!(!stderr(&no_op).contains("git_shim_after_head"));
    assert_eq!(
        fs::read(checkout.join(".jj/devspace-git-shim.state")).unwrap(),
        state_before
    );

    fs::write(checkout.join("concurrent.txt"), "visible\n").unwrap();
    let children = (0..3)
        .map(|_| {
            let mut command = ds_command(&checkout, &config);
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .arg("status")
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(
            !stderr(&output).contains("index.lock"),
            "{}",
            stderr(&output)
        );
    }
    assert_eq!(git_ls_files(&checkout), ["concurrent.txt"]);
    assert_git_directories_read_only(&checkout.join(".git"));
}

#[tokio::test]
async fn interrupted_refresh_and_stale_index_lock_are_repaired() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-recovery").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-recovery", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    fs::write(checkout.join("recovered.txt"), "visible\n").unwrap();

    let interrupted = ds_command(&checkout, &config)
        .env("DEVSPACE_FAILPOINT", "git_shim_after_head")
        .arg("status")
        .output()
        .unwrap();
    assert!(interrupted.status.success(), "{}", stderr(&interrupted));
    assert!(
        stderr(&interrupted).contains("git_shim_after_head"),
        "{}",
        stderr(&interrupted)
    );
    assert!(!git_ls_files(&checkout).contains(&"recovered.txt".to_owned()));

    rewrite_git_directory_modes(&checkout.join(".git"), true);
    fs::write(checkout.join(".git/index.lock"), "interrupted\n").unwrap();
    let repaired = ds(&checkout, &config, &["status"]);
    assert!(repaired.status.success(), "{}", stderr(&repaired));
    assert!(!checkout.join(".git/index.lock").exists());
    assert_eq!(git_ls_files(&checkout), ["recovered.txt"]);
    assert_git_directories_read_only(&checkout.join(".git"));
}

#[tokio::test]
async fn interrupted_refresh_is_repaired_after_view_returns_to_previous_state() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-reverted-recovery").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-reverted-recovery", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));

    fs::write(checkout.join("base.txt"), "base\n").unwrap();
    let base = ds(&checkout, &config, &["new", "-m", "base"]);
    assert!(base.status.success(), "{}", stderr(&base));
    let base_id = commit_id(&checkout, &config, "@-");
    assert_eq!(
        fs::read_to_string(checkout.join(".git/HEAD")).unwrap(),
        format!("{base_id}\n")
    );

    fs::write(checkout.join("next.txt"), "next\n").unwrap();
    let interrupted = ds_command(&checkout, &config)
        .env("DEVSPACE_FAILPOINT", "git_shim_after_head")
        .args(["new", "-m", "next"])
        .output()
        .unwrap();
    assert!(interrupted.status.success(), "{}", stderr(&interrupted));
    assert!(
        stderr(&interrupted).contains("git_shim_after_head"),
        "{}",
        stderr(&interrupted)
    );
    let next_id = fs::read_to_string(checkout.join(".git/HEAD"))
        .unwrap()
        .trim()
        .to_owned();
    assert_ne!(next_id, base_id);
    assert!(!checkout.join(".jj/devspace-git-shim.state").exists());

    let returned = ds(&checkout, &config, &["new", &base_id]);
    assert!(returned.status.success(), "{}", stderr(&returned));
    assert_eq!(
        fs::read_to_string(checkout.join(".git/HEAD")).unwrap(),
        format!("{base_id}\n")
    );
    assert_eq!(git_ls_files(&checkout), ["base.txt"]);
    assert_git_directories_read_only(&checkout.join(".git"));
}

#[tokio::test]
async fn git_mutation_fails_and_bypassed_writes_stay_in_the_shim() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    let repository_path = local_repository(temp.path(), "shim-isolation").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-isolation", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    fs::write(checkout.join("base.txt"), "base\n").unwrap();
    assert!(
        ds(&checkout, &config, &["new", "-m", "base"])
            .status
            .success()
    );
    let canonical_git = canonical_git_dir(&repository_path).await;
    let canonical_before = file_snapshot(&canonical_git);

    fs::write(checkout.join("blocked.txt"), "blocked\n").unwrap();
    let blocked = git(&checkout, &["add", "blocked.txt"]);
    assert!(
        !blocked.status.success(),
        "Git mutation unexpectedly succeeded"
    );
    assert!(!checkout.join(".git/index.lock").exists());
    assert_eq!(file_snapshot(&canonical_git), canonical_before);

    rewrite_git_directory_modes(&checkout.join(".git"), true);
    let configured = git(&checkout, &["config", "shim.test", "local-only"]);
    assert!(configured.status.success(), "{}", stderr(&configured));
    let updated_ref = git(&checkout, &["update-ref", "refs/heads/shim-test", "HEAD"]);
    assert!(updated_ref.status.success(), "{}", stderr(&updated_ref));
    let mut child = Command::new("git")
        .current_dir(&checkout)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"shim only\n")
        .unwrap();
    let hashed = child.wait_with_output().unwrap();
    assert!(hashed.status.success(), "{}", stderr(&hashed));
    let oid = stdout(&hashed).trim().to_owned();
    let (prefix, suffix) = oid.split_at(2);
    assert!(
        checkout
            .join(".git/objects")
            .join(prefix)
            .join(suffix)
            .is_file()
    );
    assert!(
        !canonical_git
            .join("objects")
            .join(prefix)
            .join(suffix)
            .exists()
    );
    assert_eq!(file_snapshot(&canonical_git), canonical_before);
    assert!(checkout.join(".git/refs/heads/shim-test").is_file());
    assert!(!canonical_git.join("refs/heads/shim-test").exists());
    rewrite_git_directory_modes(&checkout.join(".git"), false);
}

#[tokio::test]
async fn boundary_suppression_and_disabled_setting_skip_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    local_repository(temp.path(), "shim-suppressed").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-suppressed", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(!checkout.join(".git").exists());

    set_git_shim(&config, true);
    let listed = ds(&checkout, &config, &["list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(!checkout.join(".git").exists());
}

#[tokio::test]
async fn checkout_removal_unlocks_existing_shim_after_disabling() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    set_git_shim(&config, true);
    local_repository(temp.path(), "shim-disabled-removal").await;
    let checkout = temp.path().join("checkout");
    let added = add_checkout(temp.path(), &config, "shim-disabled-removal", &checkout);
    assert!(added.status.success(), "{}", stderr(&added));
    assert_git_directories_read_only(&checkout.join(".git"));

    set_git_shim(&config, false);
    fs::write(checkout.join("disabled.txt"), "not refreshed\n").unwrap();
    let status = ds(&checkout, &config, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(!git_ls_files(&checkout).contains(&"disabled.txt".to_owned()));

    let removed = ds_command(temp.path(), &config)
        .arg("remove")
        .arg(&checkout)
        .output()
        .unwrap();
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!checkout.exists());
}

#[tokio::test]
async fn plain_jj_checkout_never_gets_a_git_shim() {
    let temp = tempfile::tempdir().unwrap();
    let config = write_cli_config(temp.path());
    let checkout = temp.path().join("plain-jj");
    let initialized = ds_command(temp.path(), &config)
        .args(["git", "init", "--no-colocate"])
        .arg(&checkout)
        .output()
        .unwrap();
    assert!(initialized.status.success(), "{}", stderr(&initialized));
    assert!(!checkout.join(".git").exists());

    let status = ds(&checkout, &config, &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(!checkout.join(".git").exists());
}
