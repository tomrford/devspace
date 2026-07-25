use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use devspace_machine::{GitHttpTransport, ProjectionGitSnapshot, RepositoryName};

mod support;

use support::{
    TEST_MACHINE_ID, configure_machine_as as configure_machine, contains_bytes,
    ds_command_with_home as ds_command, ds_with_home as ds, git, git_command, git_output,
    machine_store, remote_ref, seal_commit, set_bookmark, stderr, stdout, unique_repository_name,
    write_cli_config,
};

const SECOND_MACHINE_ID: &str = "34343434343434343434343434343434";
const PRIVATE_SENTINEL: &[u8] = b"DEVSPACE_PRIVATE_SENTINEL\0\xff";

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn pushes_hidden_history_without_publishing_private_objects() {
    let fixture = LiveFixture::new("happy").await;
    fs::write(fixture.checkout_a.join(".dsprivate"), b"secret*\n").unwrap();
    fs::write(fixture.checkout_a.join("secret.bin"), PRIVATE_SENTINEL).unwrap();
    fs::write(fixture.checkout_a.join("visible.txt"), b"public one\n").unwrap();
    seal_commit(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "public main",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
        "@-",
    );
    fixture.add_origin(&fixture.checkout_a, &fixture.home_a, &fixture.config_a);
    let listed = ds(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        &["git", "remote", "list"],
    );
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(
        stdout(&listed).trim(),
        format!("origin {}", fixture.remote.display())
    );

    let pushed = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
    );
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    assert!(stderr(&pushed).contains("created"), "{}", stderr(&pushed));
    assert!(remote_ref(&fixture.remote, "main").is_some());
    assert_public_object_store(&fixture.remote, PRIVATE_SENTINEL);

    let before = fixture.snapshot(&fixture.home_a).await;
    let repeated = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
    );
    assert!(repeated.status.success(), "{}", stderr(&repeated));
    assert!(
        stderr(&repeated).contains("up to date"),
        "{}",
        stderr(&repeated)
    );
    let after = fixture.snapshot(&fixture.home_a).await;
    assert_eq!(after, before);

    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "release",
        "@-",
    );
    let second = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "release",
    );
    assert!(second.status.success(), "{}", stderr(&second));
    assert!(remote_ref(&fixture.remote, "release").is_some());
    assert_public_object_store(&fixture.remote, PRIVATE_SENTINEL);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn fresh_machine_claims_and_replays_a_push_left_pending_after_git_moved() {
    let fixture = LiveFixture::new("recovery").await;
    fs::write(fixture.checkout_a.join(".dsprivate"), b"secret*\n").unwrap();
    fs::write(fixture.checkout_a.join("secret.bin"), PRIVATE_SENTINEL).unwrap();
    fs::write(fixture.checkout_a.join("visible.txt"), b"before crash\n").unwrap();
    seal_commit(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "pending main",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
        "@-",
    );
    fixture.add_origin(&fixture.checkout_a, &fixture.home_a, &fixture.config_a);

    let crashed = ds_command(&fixture.checkout_a, &fixture.home_a, &fixture.config_a)
        .env("DEVSPACE_FAILPOINT", "after_git_push_before_finalize")
        .args(["git", "push", "-b", "main"])
        .output()
        .unwrap();
    assert_eq!(crashed.status.code(), Some(86), "{}", stderr(&crashed));
    let first_remote = remote_ref(&fixture.remote, "main").expect("Git push moved main");
    let pending = fixture.snapshot(&fixture.home_a).await;
    assert_eq!(pending.pending.len(), 1);

    fs::create_dir_all(&fixture.home_b).unwrap();
    configure_machine(
        &fixture.home_b,
        &fixture.base_url,
        SECOND_MACHINE_ID,
        &fixture.shared_secret,
    );
    let config_b = write_cli_config(&fixture.home_b);
    let checkout_b = fixture.home_b.join("checkout");
    let added = ds(
        &fixture.home_b,
        &fixture.home_b,
        &config_b,
        &[
            "add",
            &fixture.repository_name,
            "-r",
            "main",
            checkout_b.to_str().unwrap(),
        ],
    );
    assert!(added.status.success(), "{}", stderr(&added));
    let recovered = fixture.push(&checkout_b, &fixture.home_b, &config_b, "main");
    assert!(recovered.status.success(), "{}", stderr(&recovered));
    assert_eq!(remote_ref(&fixture.remote, "main"), Some(first_remote));
    let accepted = fixture.snapshot(&fixture.home_b).await;
    assert!(accepted.pending.is_empty());
    assert!(accepted.cursors.iter().any(|cursor| {
        cursor.remote == "origin"
            && cursor.bookmark == "main"
            && cursor.public_oid.0 == first_remote
    }));

    fs::write(checkout_b.join("visible.txt"), b"after recovery\n").unwrap();
    seal_commit(&checkout_b, &fixture.home_b, &config_b, "advanced main");
    set_bookmark(&checkout_b, &fixture.home_b, &config_b, "main", "@-");
    let advanced = fixture.push(&checkout_b, &fixture.home_b, &config_b, "main");
    assert!(advanced.status.success(), "{}", stderr(&advanced));
    let advanced_remote = remote_ref(&fixture.remote, "main").expect("advanced remote main");
    assert_ne!(advanced_remote, first_remote);
    let advanced_snapshot = fixture.snapshot(&fixture.home_b).await;
    assert!(advanced_snapshot.pending.is_empty());
    assert!(advanced_snapshot.cursors.iter().any(|cursor| {
        cursor.remote == "origin"
            && cursor.bookmark == "main"
            && cursor.public_oid.0 == advanced_remote
    }));
    assert_public_object_store(&fixture.remote, PRIVATE_SENTINEL);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn deleting_a_local_bookmark_deletes_the_journaled_remote_ref() {
    let fixture = LiveFixture::new("deletion").await;
    fs::write(fixture.checkout_a.join("visible.txt"), b"delete me\n").unwrap();
    seal_commit(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "deletion main",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
        "@-",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "feature",
        "@-",
    );
    fixture.add_origin(&fixture.checkout_a, &fixture.home_a, &fixture.config_a);
    for bookmark in ["main", "feature"] {
        let created = fixture.push(
            &fixture.checkout_a,
            &fixture.home_a,
            &fixture.config_a,
            bookmark,
        );
        assert!(created.status.success(), "{}", stderr(&created));
    }

    let deleted = ds(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        &["bookmark", "delete", "feature"],
    );
    assert!(deleted.status.success(), "{}", stderr(&deleted));
    let pushed = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "feature",
    );
    assert!(pushed.status.success(), "{}", stderr(&pushed));
    assert!(stderr(&pushed).contains("deleted"), "{}", stderr(&pushed));
    assert!(remote_ref(&fixture.remote, "feature").is_none());
    assert!(remote_ref(&fixture.remote, "main").is_some());
    assert!(
        !fixture
            .snapshot(&fixture.home_a)
            .await
            .cursors
            .iter()
            .any(|cursor| { cursor.remote == "origin" && cursor.bookmark == "feature" })
    );

    // Deleting the remote's current branch is refused remote-side; the journal
    // must abort without losing the cursor, and the CLI must surface the
    // remote's stated reason.
    let deleted = ds(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        &["bookmark", "delete", "main"],
    );
    assert!(deleted.status.success(), "{}", stderr(&deleted));
    let refused = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
    );
    assert!(!refused.status.success());
    let refusal = stderr(&refused);
    assert!(refusal.contains("delete the current branch"), "{refusal}");
    assert!(remote_ref(&fixture.remote, "main").is_some());
    assert!(
        fixture
            .snapshot(&fixture.home_a)
            .await
            .cursors
            .iter()
            .any(|cursor| { cursor.remote == "origin" && cursor.bookmark == "main" })
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn failed_git_transport_redacts_the_registered_remote_url() {
    let fixture = LiveFixture::new("redaction").await;
    fs::write(fixture.checkout_a.join("visible.txt"), b"redaction\n").unwrap();
    seal_commit(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "redaction main",
    );
    set_bookmark(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
        "@-",
    );
    let sentinel = "DO_NOT_PRINT_REMOTE_SENTINEL";
    let missing = fixture
        .temp
        .path()
        .join(format!("missing-{sentinel}/origin.git"));
    let full_url = missing.to_str().unwrap().to_owned();
    let added = ds(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        &["git", "remote", "add", "origin", &full_url],
    );
    assert!(added.status.success(), "{}", stderr(&added));

    let pushed = fixture.push(
        &fixture.checkout_a,
        &fixture.home_a,
        &fixture.config_a,
        "main",
    );
    assert_eq!(pushed.status.code(), Some(1));
    let diagnostics = format!("{}{}", stdout(&pushed), stderr(&pushed));
    assert!(!diagnostics.contains(sentinel), "{diagnostics}");
    assert!(!diagnostics.contains(&full_url), "{diagnostics}");
    let log = sync_log(&fixture.home_a, &fixture.repository_name);
    assert!(!log.contains(sentinel), "{log}");
    assert!(!log.contains(&full_url), "{log}");
}

struct LiveFixture {
    temp: tempfile::TempDir,
    base_url: String,
    shared_secret: String,
    repository_name: String,
    home_a: PathBuf,
    home_b: PathBuf,
    config_a: PathBuf,
    checkout_a: PathBuf,
    remote: PathBuf,
}

impl LiveFixture {
    async fn new(label: &str) -> Self {
        let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
        let shared_secret =
            std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
        let temp = tempfile::tempdir().unwrap();
        let home_a = temp.path().join("machine-a");
        let home_b = temp.path().join("machine-b");
        fs::create_dir_all(&home_a).unwrap();
        configure_machine(&home_a, &base_url, TEST_MACHINE_ID, &shared_secret);
        let config_a = write_cli_config(&home_a);
        let repository_name = unique_repository_name(temp.path(), &format!("git-push-{label}"));
        let created = ds(
            &home_a,
            &home_a,
            &config_a,
            &["repo", "new", &repository_name],
        );
        assert!(created.status.success(), "{}", stderr(&created));
        let checkout_a = home_a.join("checkout");
        let added = ds(
            &home_a,
            &home_a,
            &config_a,
            &[
                "add",
                &repository_name,
                "-r",
                "root()",
                checkout_a.to_str().unwrap(),
            ],
        );
        assert!(added.status.success(), "{}", stderr(&added));
        let remote = temp.path().join("origin.git");
        git(&["init", "--bare", remote.to_str().unwrap()], None);
        Self {
            temp,
            base_url,
            shared_secret,
            repository_name,
            home_a,
            home_b,
            config_a,
            checkout_a,
            remote,
        }
    }

    fn add_origin(&self, checkout: &Path, home: &Path, config: &Path) {
        let added = ds(
            checkout,
            home,
            config,
            &[
                "git",
                "remote",
                "add",
                "origin",
                self.remote.to_str().unwrap(),
            ],
        );
        assert!(added.status.success(), "{}", stderr(&added));
    }

    fn push(&self, checkout: &Path, home: &Path, config: &Path, bookmark: &str) -> Output {
        ds(checkout, home, config, &["git", "push", "-b", bookmark])
    }

    async fn snapshot(&self, home: &Path) -> ProjectionGitSnapshot {
        let store = machine_store(home);
        let entry = store
            .resolve(&RepositoryName::parse(&self.repository_name).unwrap())
            .unwrap()
            .unwrap();
        let config = store.load_config().unwrap();
        let transport = GitHttpTransport::new(
            config.base_url(),
            config.shared_secret().as_str(),
            config.machine_id().as_str(),
            entry.identity.repository_id.as_str(),
            entry.identity.incarnation.as_str(),
        )
        .unwrap();
        load_snapshot(&transport).await
    }
}

fn assert_public_object_store(remote: &Path, sentinel: &[u8]) {
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
        let output = git_command(&["cat-file", "-p", id], Some(remote))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(
            !contains_bytes(&output.stdout, sentinel),
            "private sentinel entered Git"
        );
        assert!(
            !contains_bytes(&output.stdout, b".dsprivate"),
            ".dsprivate entered Git"
        );
    }
}

fn sync_log(home: &Path, repository_name: &str) -> String {
    let store = machine_store(home);
    let entry = store
        .resolve(&RepositoryName::parse(repository_name).unwrap())
        .unwrap()
        .unwrap();
    fs::read_to_string(
        entry
            .native_repository_path
            .parent()
            .unwrap()
            .join("sync.log"),
    )
    .unwrap_or_default()
}

async fn load_snapshot(transport: &GitHttpTransport) -> ProjectionGitSnapshot {
    transport.projection_snapshot_all().await.unwrap()
}
