use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use devspace_machine::MachineGitRepository as MachineRepository;
use devspace_machine::{MachineStoreError, RepositoryName, encode_lower_hex};
use devspace_testutils::fake_worker::{create_server, respond};
use jj_lib::op_store::RemoteRef;
use jj_lib::ref_name::{RefName, RemoteName, RemoteRefSymbol};

mod support;

use support::worker::{cloud_paused_at_remote_list, create_push_server};
use support::{
    TEST_MACHINE_ID, configure_machine_as as configure_machine, ds_command_with_home as ds_command,
    ds_with_home as ds, git, git_command, git_output, machine_store, parse_git_oid,
    registered_repository, remote_ref, seal_commit, set_bookmark, settings, stderr, stdout,
    write_cli_config,
};

const DEVELOPMENT_SECRET: &str = "git-push-development-secret";

#[tokio::test]
async fn devspace_checkout_owns_fetch_and_fences_unowned_git_commands() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("machine");
    fs::create_dir_all(&home).unwrap();
    configure_machine(
        &home,
        "http://127.0.0.1:1",
        TEST_MACHINE_ID,
        DEVELOPMENT_SECRET,
    );
    let config = write_cli_config(&home);
    let checkout = local_checkout(&home, &config, "git-fence").await;

    let fetch = ds(&checkout, &home, &config, &["git", "fetch"]);
    assert_eq!(fetch.status.code(), Some(1));
    assert!(
        !stderr(&fetch).contains("not yet implemented"),
        "{}",
        stderr(&fetch)
    );

    let literal_fetch = ds(&checkout, &home, &config, &["git", "fetch", "-b", "a..b"]);
    assert_eq!(literal_fetch.status.code(), Some(1));
    assert!(
        stderr(&literal_fetch).contains("bookmark is not a valid Git branch name"),
        "{}",
        stderr(&literal_fetch)
    );

    let export = ds(&checkout, &home, &config, &["git", "export"]);
    assert_eq!(export.status.code(), Some(1));
    assert!(
        stderr(&export).contains("Devspace owns the Git boundary"),
        "{}",
        stderr(&export)
    );

    let broad_push = ds(&checkout, &home, &config, &["git", "push", "--all"]);
    assert_eq!(broad_push.status.code(), Some(1));
    assert!(
        stderr(&broad_push).contains("does not support `all`"),
        "{}",
        stderr(&broad_push)
    );

    let store = machine_store(&home);
    let entry = store
        .resolve(&RepositoryName::parse("git-fence").unwrap())
        .unwrap()
        .unwrap();
    store
        .unregister_repository(
            &RepositoryName::parse("git-fence").unwrap(),
            &entry.identity,
        )
        .unwrap();
    let unregistered = ds(&checkout, &home, &config, &["git", "fetch", "-b", "main"]);
    assert_eq!(unregistered.status.code(), Some(1));
    assert!(
        stderr(&unregistered).contains("repository-not-registered"),
        "{}",
        stderr(&unregistered)
    );
}

#[tokio::test]
async fn git_push_waits_for_the_repository_sync_lock_then_proceeds() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("machine");
    fs::create_dir_all(&home).unwrap();
    configure_machine(
        &home,
        "http://127.0.0.1:1",
        TEST_MACHINE_ID,
        DEVELOPMENT_SECRET,
    );
    let config = write_cli_config(&home);
    let checkout = local_checkout(&home, &config, "locked-push").await;
    let store = machine_store(&home);
    let entry = store
        .resolve(&RepositoryName::parse("locked-push").unwrap())
        .unwrap()
        .unwrap();
    let guard = store.try_lock_repository_sync(&entry.identity).unwrap();
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        drop(guard);
    });

    let started = Instant::now();
    let output = ds(&checkout, &home, &config, &["git", "push", "-b", "main"]);
    let elapsed = started.elapsed();
    release.join().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        elapsed >= Duration::from_millis(200),
        "push did not wait for the held lock: {elapsed:?}"
    );
    let diagnostic = stderr(&output);
    assert_eq!(
        diagnostic
            .matches("Waiting for an in-flight operation")
            .count(),
        1,
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("already being synchronized"),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn git_push_resolves_bookmarks_after_waiting_for_the_sync_lock() {
    let fixture = FakePushFixture::new("post-sync-resolution").await;
    fixture.commit("main", "first\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.commit("main", "second\n");

    let store = machine_store(&fixture.home);
    let guard = store
        .try_lock_repository_sync(&fixture.entry().identity)
        .unwrap();
    let child = ds_command(&fixture.checkout, &fixture.home, &fixture.config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["git", "push", "-b", "main"])
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(200));
    fixture.commit("main", "third\n");
    drop(guard);

    let pushed = child.wait_with_output().unwrap();
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    let view = fixture.bookmark_list();
    assert_eq!(
        bookmark_target(&view, "main"),
        bookmark_target(&view, "main@origin"),
        "{view}"
    );
}

#[tokio::test]
async fn git_push_holds_the_repository_sync_lock_after_sync_completes() {
    let (base_url, push_reached, release_push, server) = cloud_paused_at_remote_list();
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("machine");
    fs::create_dir_all(&home).unwrap();
    configure_machine(&home, &base_url, TEST_MACHINE_ID, DEVELOPMENT_SECRET);
    let config = write_cli_config(&home);
    let checkout = local_checkout(&home, &config, "lock-lifetime").await;
    let store = machine_store(&home);
    let entry = store
        .resolve(&RepositoryName::parse("lock-lifetime").unwrap())
        .unwrap()
        .unwrap();
    let child = ds_command(&checkout, &home, &config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(["git", "push", "-b", "main"])
        .spawn()
        .unwrap();

    push_reached
        .recv_timeout(Duration::from_secs(10))
        .expect("push did not reach the post-sync projection request");
    assert!(matches!(
        store.try_lock_repository_sync(&entry.identity),
        Err(MachineStoreError::RepositorySyncAlreadyLocked { .. })
    ));
    release_push.send(()).unwrap();

    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no such Git remote `origin`"));
    assert!(stderr(&output).contains("remote-not-found"));
    drop(store.try_lock_repository_sync(&entry.identity).unwrap());
}

#[tokio::test]
async fn remote_add_prints_the_workers_kebab_case_error_code_without_the_url() {
    let (base_url, server) = create_server(|_, _, stream| {
        let body = r#"{"error":"remote URL must not contain userinfo credentials","code":"credentials-in-remote-url"}"#;
        respond(stream, "400 Bad Request", body);
        true
    });
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("machine");
    fs::create_dir_all(&home).unwrap();
    configure_machine(&home, &base_url, TEST_MACHINE_ID, DEVELOPMENT_SECRET);
    let config = write_cli_config(&home);
    let checkout = local_checkout(&home, &config, "remote-error").await;
    let sentinel = "REMOTE_PASSWORD_SENTINEL";
    let url = format!("https://user:{sentinel}@example.invalid/repo.git");

    let output = ds(
        &checkout,
        &home,
        &config,
        &["git", "remote", "add", "origin", &url],
    );
    let diagnostic = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        !server.join().unwrap().is_empty(),
        "CLI never contacted the Worker: {diagnostic}"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        diagnostic.contains("credentials-in-remote-url"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains(sentinel), "{diagnostic}");
    assert!(!diagnostic.contains(&url), "{diagnostic}");
}

#[tokio::test]
async fn push_moves_the_remote_tracking_bookmark_to_the_local_target() {
    let fixture = FakePushFixture::new("view-move").await;
    fixture.commit("main", "first\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let first_view = fixture.bookmark_list();
    assert!(
        first_view.contains("main@origin|true|true|"),
        "{first_view}"
    );

    fixture.commit("main", "second\n");
    let updated = fixture.push(&["-b", "main"]);
    assert!(updated.status.success(), "{}", stderr(&updated));

    let updated_view = fixture.bookmark_list();
    assert!(
        updated_view.contains("main@origin|true|true|"),
        "{updated_view}"
    );
    assert_ne!(updated_view, first_view);
}

#[tokio::test]
async fn push_records_a_jj_operation_with_the_stock_description() {
    let fixture = FakePushFixture::new("operation").await;
    fixture.commit("main", "operation\n");
    let pushed = fixture.push(&["-b", "main"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));

    assert_eq!(
        fixture.operation_description(),
        "push bookmark main to git remote origin"
    );
}

#[tokio::test]
async fn creation_push_tracks_the_new_remote_bookmark() {
    let fixture = FakePushFixture::new("auto-track").await;
    fixture.commit("main", "created\n");

    let pushed = fixture.push(&["-b", "main"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));

    let view = fixture.bookmark_list();
    assert!(view.contains("main@origin|true|true|"), "{view}");
}

#[tokio::test]
async fn deletion_push_removes_the_remote_tracking_bookmark() {
    let fixture = FakePushFixture::new("view-delete").await;
    fixture.commit("feature", "created\n");
    let created = fixture.push(&["-b", "feature"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.delete_bookmark("feature");

    let deleted = fixture.push(&["-b", "feature"]);
    assert!(deleted.status.success(), "{}", stderr(&deleted));

    let view = fixture.bookmark_list();
    assert!(!view.contains("feature@origin|"), "{view}");
    assert_eq!(
        fixture.operation_description(),
        "push bookmark feature to git remote origin"
    );
    assert!(remote_ref(&fixture.remote, "feature").is_none());
}

#[tokio::test]
async fn recovered_pending_deletion_is_reported_as_success() {
    let fixture = FakePushFixture::new("recovered-deletion").await;
    fixture.commit("feature", "created\n");
    let created = fixture.push(&["-b", "feature"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.delete_bookmark("feature");

    let crashed = ds_command(&fixture.checkout, &fixture.home, &fixture.config)
        .env("DEVSPACE_FAILPOINT", "after_git_push_before_finalize")
        .args(["git", "push", "-b", "feature"])
        .output()
        .unwrap();
    assert_eq!(crashed.status.code(), Some(86), "{}", stderr(&crashed));
    assert!(remote_ref(&fixture.remote, "feature").is_none());

    let recovered = fixture.push(&["-b", "feature"]);
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    assert!(
        stderr(&recovered).contains("pushed feature to origin: deleted"),
        "{}",
        stderr(&recovered)
    );
    let view = fixture.bookmark_list();
    assert!(!view.contains("feature@origin|"), "{view}");
}

#[tokio::test]
async fn deleted_combines_with_explicit_bookmarks() {
    let fixture = FakePushFixture::new("deleted-selection").await;
    fixture.commit("feature", "created\n");
    let created = fixture.push(&["-b", "feature"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.delete_bookmark("feature");
    fixture.commit("main", "main\n");

    let deleted = fixture.push(&["-b", "main", "--deleted"]);
    assert!(deleted.status.success(), "{}", stderr(&deleted));
    assert!(stderr(&deleted).contains("pushed feature to origin: deleted"));
    assert!(stderr(&deleted).contains("pushed main to origin: created"));
    assert!(!fixture.remote_bookmark("feature").await.is_present());
    assert!(remote_ref(&fixture.remote, "feature").is_none());
    assert!(remote_ref(&fixture.remote, "main").is_some());
}

#[tokio::test]
async fn up_to_date_push_repairs_a_stale_remote_tracking_bookmark() {
    let fixture = FakePushFixture::new("self-heal").await;
    fixture.commit("main", "created\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    fixture.remove_remote_bookmark("main").await;

    let repaired = fixture.push(&["-b", "main"]);
    assert!(repaired.status.success(), "{}", stderr(&repaired));
    assert!(stderr(&repaired).contains("up to date"));

    let view = fixture.bookmark_list();
    assert!(view.contains("main@origin|true|true|"), "{view}");
    assert_eq!(
        fixture.operation_description(),
        "push bookmark main to git remote origin"
    );
}

#[tokio::test]
async fn multiple_explicit_bookmarks_use_sorted_operation_description() {
    let fixture = FakePushFixture::new("operation-sorted").await;
    fixture.commit("zeta", "zeta\n");
    fixture.commit("alpha", "alpha\n");

    let pushed = fixture.push(&["-b", "zeta", "-b", "alpha"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    assert_eq!(
        fixture.operation_description(),
        "push bookmarks alpha, zeta to git remote origin"
    );
}

#[tokio::test]
async fn explicit_push_refuses_a_present_untracked_remote_bookmark() {
    let fixture = FakePushFixture::new("untracked-explicit").await;
    fixture.commit("main", "first\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let untracked = fixture.ds(&["bookmark", "untrack", "main@origin"]);
    assert!(untracked.status.success(), "{}", stderr(&untracked));
    fixture.commit("main", "second\n");

    let refused = fixture.push(&["-b", "main"]);
    assert_eq!(refused.status.code(), Some(1));
    let diagnostic = stderr(&refused);
    assert!(
        diagnostic.contains("Non-tracking remote bookmark main@origin exists"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("ds bookmark track main --remote=origin"),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn deleted_selects_only_absent_local_tracked_bookmarks_on_the_remote() {
    let fixture = FakePushFixture::new("deleted-matrix").await;

    fixture.commit("deleted", "deleted\n");
    assert!(fixture.push(&["-b", "deleted"]).status.success());
    fixture.delete_bookmark("deleted");

    fixture.commit("untracked", "untracked\n");
    assert!(fixture.push(&["-b", "untracked"]).status.success());
    let forgotten = fixture.ds(&["bookmark", "forget", "untracked"]);
    assert!(forgotten.status.success(), "{}", stderr(&forgotten));

    fixture.commit("local", "local\n");
    assert!(fixture.push(&["-b", "local"]).status.success());

    fixture.add_remote("backup");
    fixture.commit("elsewhere", "elsewhere\n");
    assert!(
        fixture
            .push(&["-b", "elsewhere", "--remote", "backup"])
            .status
            .success()
    );
    fixture.delete_bookmark("elsewhere");

    let pushed = fixture.push(&["--deleted"]);
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    let diagnostic = stderr(&pushed);
    assert!(diagnostic.contains("pushed deleted to origin: deleted"));
    assert!(!diagnostic.contains("pushed untracked"), "{diagnostic}");
    assert!(!diagnostic.contains("pushed local"), "{diagnostic}");
    assert!(!diagnostic.contains("pushed elsewhere"), "{diagnostic}");
    assert!(remote_ref(&fixture.remote, "deleted").is_none());
    assert!(remote_ref(&fixture.remote, "untracked").is_some());
    assert!(remote_ref(&fixture.remote, "local").is_some());
    assert!(remote_ref(&fixture.remote, "elsewhere").is_some());
    assert_eq!(
        fixture.operation_description(),
        "push all deleted bookmarks to git remote origin"
    );
}

#[tokio::test]
async fn push_fails_loudly_when_a_mapped_object_is_missing() {
    let fixture = FakePushFixture::new("missing-mapped-object").await;
    fs::write(fixture.checkout.join(".dsprivate"), b"/secret.txt\n").unwrap();
    fs::write(fixture.checkout.join("secret.txt"), b"private\n").unwrap();
    fixture.commit("main", "first\n");
    let created = fixture.push(&["-b", "main"]);
    assert!(created.status.success(), "{}", stderr(&created));
    let oid = remote_ref(&fixture.remote, "main").unwrap();
    let hex = encode_lower_hex(&oid);
    let repository = fixture.repository().await;
    let object_path = repository
        .git_repo_path()
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    drop(repository);
    fs::remove_file(&object_path).unwrap();
    fixture.commit("main", "second\n");

    let pushed = fixture.push(&["-b", "main"]);
    assert_eq!(pushed.status.code(), Some(1));
    let diagnostic = stderr(&pushed);
    assert!(diagnostic.contains("seeded public commit"), "{diagnostic}");
    assert!(diagnostic.contains("is unavailable"), "{diagnostic}");
}

#[tokio::test]
async fn new_bookmark_reuses_the_git_oid_of_imported_history() {
    let fixture = FakePushFixture::new("imported-history").await;
    let imported_oid = signed_commit(&fixture.remote, "main");

    let fetched = fixture.fetch(&["-b", "main"]);
    assert!(fetched.status.success(), "{}", stderr(&fetched));
    let tracked = ds(
        &fixture.checkout,
        &fixture.home,
        &fixture.config,
        &["bookmark", "track", "main@origin"],
    );
    assert!(tracked.status.success(), "{}", stderr(&tracked));
    let checked_out = ds(
        &fixture.checkout,
        &fixture.home,
        &fixture.config,
        &["new", "main"],
    );
    assert!(checked_out.status.success(), "{}", stderr(&checked_out));

    fixture.commit("main", "descendant\n");
    let main = fixture.push(&["-b", "main"]);
    assert!(main.status.success(), "{}", stderr(&main));
    let main_oid = remote_ref(&fixture.remote, "main").unwrap();
    assert_eq!(
        parse_git_oid(git_output(&["rev-parse", "main^"], Some(&fixture.remote)).trim()),
        imported_oid
    );

    set_bookmark(
        &fixture.checkout,
        &fixture.home,
        &fixture.config,
        "release",
        "main",
    );
    let release = fixture.push(&["-b", "release"]);
    assert!(release.status.success(), "{}", stderr(&release));

    assert_eq!(remote_ref(&fixture.remote, "release"), Some(main_oid));
}

struct FakePushFixture {
    _temp: tempfile::TempDir,
    _server: JoinHandle<Vec<String>>,
    home: PathBuf,
    config: PathBuf,
    checkout: PathBuf,
    remote: PathBuf,
    repository_name: String,
}

impl FakePushFixture {
    async fn new(label: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("origin.git");
        git(&["init", "--bare", remote.to_str().unwrap()], None);
        let (base_url, server) = create_push_server(remote.to_str().unwrap().to_owned());
        let home = temp.path().join("machine");
        fs::create_dir_all(&home).unwrap();
        configure_machine(&home, &base_url, TEST_MACHINE_ID, DEVELOPMENT_SECRET);
        let config = write_cli_config(&home);
        let repository_name = format!("git-push-{label}");
        let checkout = local_checkout(&home, &config, &repository_name).await;
        let added = ds(
            &checkout,
            &home,
            &config,
            &["git", "remote", "add", "origin", remote.to_str().unwrap()],
        );
        assert!(added.status.success(), "{}", stderr(&added));
        Self {
            _temp: temp,
            _server: server,
            home,
            config,
            checkout,
            remote,
            repository_name,
        }
    }

    fn commit(&self, bookmark: &str, contents: &str) {
        fs::write(self.checkout.join("visible.txt"), contents).unwrap();
        seal_commit(
            &self.checkout,
            &self.home,
            &self.config,
            &format!("{bookmark} commit"),
        );
        set_bookmark(&self.checkout, &self.home, &self.config, bookmark, "@-");
    }

    fn delete_bookmark(&self, bookmark: &str) {
        let deleted = ds(
            &self.checkout,
            &self.home,
            &self.config,
            &["bookmark", "delete", bookmark],
        );
        assert!(deleted.status.success(), "{}", stderr(&deleted));
    }

    fn push(&self, args: &[&str]) -> Output {
        let mut command = ds_command(&self.checkout, &self.home, &self.config);
        command.arg("git").arg("push").args(args).output().unwrap()
    }

    fn ds(&self, args: &[&str]) -> Output {
        ds(&self.checkout, &self.home, &self.config, args)
    }

    fn bookmark_list(&self) -> String {
        let output = self.ds(&[
            "bookmark",
            "list",
            "--all-remotes",
            "--ignore-working-copy",
            "-T",
            r#"name ++ if(remote, "@" ++ remote) ++ "|" ++ present ++ "|" ++ tracked ++ "|" ++ if(present, normal_target.commit_id().short(12)) ++ "\n""#,
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    }

    fn operation_description(&self) -> String {
        let output = self.ds(&[
            "operation",
            "log",
            "--ignore-working-copy",
            "--no-graph",
            "--limit",
            "1",
            "-T",
            "description",
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    }

    fn add_remote(&self, name: &str) {
        let added = self.ds(&["git", "remote", "add", name, self.remote.to_str().unwrap()]);
        assert!(added.status.success(), "{}", stderr(&added));
    }

    fn fetch(&self, args: &[&str]) -> Output {
        let mut command = ds_command(&self.checkout, &self.home, &self.config);
        command.arg("git").arg("fetch").args(args).output().unwrap()
    }

    fn entry(&self) -> devspace_machine::CatalogEntry {
        machine_store(&self.home)
            .resolve(&RepositoryName::parse(&self.repository_name).unwrap())
            .unwrap()
            .unwrap()
    }

    async fn repository(&self) -> MachineRepository {
        MachineRepository::open(&self.entry().native_repository_path, &settings())
            .await
            .unwrap()
    }

    async fn remote_bookmark(&self, name: &str) -> RemoteRef {
        self.repository()
            .await
            .repo()
            .view()
            .get_remote_bookmark(RemoteRefSymbol {
                name: RefName::new(name),
                remote: RemoteName::new("origin"),
            })
            .clone()
    }

    async fn remove_remote_bookmark(&self, name: &str) {
        let repository = self.repository().await;
        let mut transaction = repository.repo().start_transaction();
        transaction.repo_mut().set_remote_bookmark(
            RemoteRefSymbol {
                name: RefName::new(name),
                remote: RemoteName::new("origin"),
            },
            RemoteRef::absent(),
        );
        transaction
            .commit("remove fixture remote bookmark")
            .await
            .unwrap();
    }
}

fn bookmark_target<'a>(view: &'a str, name: &str) -> &'a str {
    view.lines()
        .find_map(|line| {
            let (symbol, rest) = line.split_once('|')?;
            (symbol == name).then(|| rest.rsplit('|').next().unwrap())
        })
        .unwrap_or_else(|| panic!("bookmark `{name}` is missing from:\n{view}"))
}

async fn local_checkout(home: &Path, config: &Path, name: &str) -> PathBuf {
    registered_repository(home, name).await;
    let checkout = home.join("checkout");
    let added = ds(
        home,
        home,
        config,
        &["add", name, "-r", "root()", checkout.to_str().unwrap()],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    checkout
}

fn signed_commit(remote: &Path, bookmark: &str) -> [u8; 20] {
    let tree = git_output(&["mktree"], Some(remote));
    let raw = format!(
        "tree {}\nauthor Imported <imported@example.invalid> 0 +0000\ncommitter Imported <imported@example.invalid> 0 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n dummy\n -----END PGP SIGNATURE-----\n\nimported history\n",
        tree.trim()
    );
    let mut hash = git_command(
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        Some(remote),
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
    hash.stdin
        .as_mut()
        .unwrap()
        .write_all(raw.as_bytes())
        .unwrap();
    let output = hash.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let oid = String::from_utf8(output.stdout).unwrap();
    git(
        &["update-ref", &format!("refs/heads/{bookmark}"), oid.trim()],
        Some(remote),
    );
    parse_git_oid(oid.trim())
}
