use std::collections::BTreeMap;
use std::process::Command;

use devspace_kernel::{ObjectKind, Oid, parse_commit};
use devspace_machine::{
    CanonicalGitRemote, GitHttpTransport, GitProcessEnvironment, GitProcessMode, JournalFlowError,
    LeaseUpdate, MachineGitRepository, MachineId, PushErrorKind, PushFailpoint, PushHead,
    QualifiedRef, RemoteUrl, fetch_with_journal, init_bare_remote, push, push_with_journal,
};
use devspace_testutils::fake_worker::{create_server, respond};
use jj_lib::settings::UserSettings;

mod common;

use common::{create_live_repository, oid_hex, read_raw, write_commit, write_raw};

const COMMIT_IDENTITY: &str = "Journal <journal@example.invalid>";
/// The fixed `gpgsig` header the signed-identity fixtures carry.
const SIGNATURE: &[(&[u8], &[u8])] = &[(
    b"gpgsig",
    b"-----BEGIN PGP SIGNATURE-----\n fake\n -----END PGP SIGNATURE-----",
)];

fn settings() -> UserSettings {
    devspace_testutils::settings("Journal Test", "journal@example.invalid", true)
}

fn journal_remote(path: &std::path::Path) -> CanonicalGitRemote {
    init_bare_remote(path).unwrap();
    CanonicalGitRemote::new(
        path.to_string_lossy(),
        MachineId::parse("11".repeat(16)).unwrap(),
        GitProcessEnvironment::default(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn real_lease_push_preserves_signed_identity_bytes_and_observes_rejection() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MachineGitRepository::init(temp.path().join("machine"), &settings())
        .await
        .unwrap();
    let tree = write_tree(&repository, b"signed identity");
    let signed = write_commit(
        &repository,
        COMMIT_IDENTITY,
        tree,
        &[],
        b"signed\n",
        SIGNATURE,
    );
    let remote = temp.path().join("remote.git");
    let initialized = Command::new("git")
        .args(["init", "--bare", remote.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    let remote_url = RemoteUrl::new(remote.to_string_lossy());
    let reference = QualifiedRef::from_bookmark("main").unwrap();
    let environment = GitProcessEnvironment::new("git", GitProcessMode::Foreground);
    let report = push(
        repository.git_repo_path(),
        &remote_url,
        &BTreeMap::from([(
            reference.clone(),
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: Some(signed),
            },
        )]),
        &environment,
    )
    .unwrap();
    assert_eq!(report.refs[&reference].observed_oid, Some(signed));
    let remote_bytes = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args(["cat-file", "commit", &oid_hex(signed)])
        .output()
        .unwrap();
    assert!(remote_bytes.status.success());
    assert_eq!(remote_bytes.stdout, read_raw(&repository, signed));

    let next = write_commit(
        &repository,
        COMMIT_IDENTITY,
        tree,
        &[signed],
        b"next\n",
        &[],
    );
    let rejected = push(
        repository.git_repo_path(),
        &remote_url,
        &BTreeMap::from([(
            reference.clone(),
            LeaseUpdate {
                expected_old_oid: Some(Oid([0x22; 20])),
                new_oid: Some(next),
            },
        )]),
        &environment,
    )
    .unwrap_err();
    assert_eq!(rejected.kind, PushErrorKind::PushFailed);
    assert_eq!(rejected.report.refs[&reference].observed_oid, Some(signed));
    assert!(
        !rejected
            .report
            .diagnostic
            .command
            .contains(remote.to_str().unwrap())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn up_to_date_push_does_not_request_the_pack_catalog_or_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = journal_remote(&temp.path().join("canonical.git"));
    let repository = MachineGitRepository::init(temp.path().join("machine"), &settings())
        .await
        .unwrap();
    let head = write_commit(
        &repository,
        COMMIT_IDENTITY,
        write_tree(&repository, b"already current"),
        &[],
        b"already current\n",
        &[],
    );
    let head_hex = oid_hex(head);
    let (base_url, server) = create_server(move |_, request, stream| {
        let request_line = request.lines().next().unwrap();
        deny_retired_git_object_routes(request_line);
        if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
            respond(
                stream,
                "200 OK",
                r#"{"remotes":[{"name":"origin","url":"/tmp/unused.git"}]}"#,
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/git/projection?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "activationCursor": 1,
                    "cursors": [{
                        "remote": "origin",
                        "bookmark": "main",
                        "canonicalOid": head_hex,
                        "publicOid": head_hex,
                        "hiddenSetId": null,
                        "activationSequence": 1,
                    }],
                    "mappings": [],
                    "nextAfter": 0,
                    "through": 1,
                    "hasMore": false,
                    "pending": [],
                })
                .to_string(),
            );
            return true;
        } else {
            panic!("up-to-date push made an unnecessary request: {request_line}");
        }
        false
    });
    let transport = GitHttpTransport::new(
        &base_url,
        "up-to-date-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();

    let result = push_with_journal(
        &repository,
        &transport,
        &canonical,
        "origin",
        &[PushHead {
            bookmark: "main".to_owned(),
            canonical_oid: Some(head),
        }],
        [0x65; 16],
        &GitProcessEnvironment::new("git", GitProcessMode::Foreground),
        PushFailpoint::None,
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, "up-to-date");
    assert_eq!(result.public_heads["main"], Some(head));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        !request.contains("/packs?")
            && !request.contains("/packs/")
            && !request.contains("/chunks/")
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn non_noop_push_does_not_use_worker_git_object_routes() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = journal_remote(&temp.path().join("canonical.git"));
    let repository = MachineGitRepository::init(temp.path().join("machine"), &settings())
        .await
        .unwrap();
    let head = write_commit(
        &repository,
        COMMIT_IDENTITY,
        write_tree(&repository, b"known closure"),
        &[],
        b"known closure\n",
        &[],
    );
    let remote = temp.path().join("remote.git");
    let initialized = Command::new("git")
        .args(["init", "--bare", remote.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    let remote_url = remote.to_string_lossy().into_owned();
    let (base_url, server) = create_server(move |_, request, stream| {
        let request_line = request.lines().next().unwrap();
        deny_retired_git_object_routes(request_line);
        if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"remotes": [{"name": "origin", "url": remote_url}]})
                    .to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/git/projection?") {
            respond(
                stream,
                "200 OK",
                r#"{"activationCursor":0,"cursors":[],"mappings":[],"nextAfter":0,"through":0,"hasMore":false,"pending":[]}"#,
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes ")
        {
            respond(
                stream,
                "200 OK",
                r#"{"pending":true,"fence":1,"outcome":null}"#,
            );
        } else if request_line.starts_with("POST ") && request_line.contains("/recover ") {
            respond(
                stream,
                "200 OK",
                r#"{"pending":false,"fence":1,"outcome":"accepted"}"#,
            );
            return true;
        } else {
            panic!("unexpected projection push request: {request_line}");
        }
        false
    });
    let transport = GitHttpTransport::new(
        &base_url,
        "known-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();

    let result = push_with_journal(
        &repository,
        &transport,
        &canonical,
        "origin",
        &[PushHead {
            bookmark: "main".to_owned(),
            canonical_oid: Some(head),
        }],
        [0x66; 16],
        &GitProcessEnvironment::new("git", GitProcessMode::Foreground),
        PushFailpoint::None,
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, "accepted");
    let destination = MachineGitRepository::init(temp.path().join("fresh"), &settings())
        .await
        .unwrap();
    canonical
        .verify_commits(&destination, [head])
        .unwrap();
    let requests = server.join().unwrap();
    assert!(requests.iter().all(|request| {
        !request.contains("/packs") && !request.contains("/git/objects/")
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn hidden_paths_are_absent_from_the_publishable_remote_by_object_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let canonical_path = temp.path().join("canonical.git");
    let canonical = journal_remote(&canonical_path);
    let repository = MachineGitRepository::init(temp.path().join("machine"), &settings())
        .await
        .unwrap();
    let sentinel = b"private-sentinel-bytes\0hidden";
    let (head, _) = write_hidden_commit(&repository, None, sentinel);
    let remote = temp.path().join("publishable.git");
    let initialized = Command::new("git")
        .args(["init", "--bare", remote.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    let remote_url = remote.to_string_lossy().into_owned();
    let (base_url, server) = create_server(move |_, request, stream| {
        let request_line = request.lines().next().unwrap();
        deny_retired_git_object_routes(request_line);
        if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"remotes": [{"name": "origin", "url": remote_url}]})
                    .to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/git/projection?") {
            respond(
                stream,
                "200 OK",
                r#"{"activationCursor":0,"cursors":[],"mappings":[],"nextAfter":0,"through":0,"hasMore":false,"pending":[]}"#,
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes ")
        {
            respond(
                stream,
                "200 OK",
                r#"{"pending":true,"fence":1,"outcome":null}"#,
            );
        } else if request_line.starts_with("POST ") && request_line.contains("/recover ") {
            respond(
                stream,
                "200 OK",
                r#"{"pending":false,"fence":1,"outcome":"accepted"}"#,
            );
            return true;
        } else {
            panic!("unexpected hidden-path push request: {request_line}");
        }
        false
    });
    let transport = GitHttpTransport::new(
        &base_url,
        "hidden-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();

    let result = push_with_journal(
        &repository,
        &transport,
        &canonical,
        "origin",
        &[PushHead {
            bookmark: "main".to_owned(),
            canonical_oid: Some(head),
        }],
        [0x71; 16],
        &GitProcessEnvironment::new("git", GitProcessMode::Foreground),
        PushFailpoint::None,
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, "accepted");
    assert_remote_blobs_exclude(&remote, sentinel);
    assert_remote_trees_exclude_name(&remote, b".dsprivate");
    assert_remote_trees_exclude_name(&remote, b"secret.bin");
    assert_remote_blobs_include(&canonical_path, sentinel);
    drop(server.join().unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn identity_cursor_stops_clean_and_hidden_children_without_identity_states() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = journal_remote(&temp.path().join("canonical.git"));
    let repository = MachineGitRepository::init(temp.path().join("machine"), &settings())
        .await
        .unwrap();
    let tree = write_tree(&repository, b"public root");
    let identity = write_commit(
        &repository,
        COMMIT_IDENTITY,
        tree,
        &[],
        b"identity root\n",
        &[],
    );
    let clean = write_commit(
        &repository,
        COMMIT_IDENTITY,
        tree,
        &[identity],
        b"clean child\n",
        &[],
    );
    let (hidden, _) = write_hidden_commit(&repository, Some(identity), b"hidden child");
    let remote = temp.path().join("remote.git");
    let initialized = Command::new("git")
        .args(["init", "--bare", remote.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    let remote_url = remote.to_string_lossy().into_owned();
    let identity_hex = oid_hex(identity);
    let clean_hex = oid_hex(clean);
    let hidden_hex = oid_hex(hidden);
    let (base_url, server) = create_server(move |_, request, stream| {
        let request_line = request.lines().next().unwrap();
        deny_retired_git_object_routes(request_line);
        if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"remotes": [{"name": "origin", "url": remote_url}]})
                    .to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/git/projection?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "activationCursor": 1,
                    "cursors": [{
                        "remote": "origin",
                        "bookmark": "seed",
                        "canonicalOid": identity_hex,
                        "publicOid": identity_hex,
                        "hiddenSetId": null,
                        "activationSequence": 1,
                    }],
                    "mappings": [],
                    "nextAfter": 0,
                    "through": 1,
                    "hasMore": false,
                    "pending": [],
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes ")
        {
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            let updates = body["updates"].as_array().unwrap();
            let clean_update = updates
                .iter()
                .find(|update| update["bookmark"] == "clean")
                .unwrap();
            assert_eq!(clean_update["identityOid"], clean_hex);
            assert_eq!(clean_update["states"], serde_json::json!([]));
            let hidden_update = updates
                .iter()
                .find(|update| update["bookmark"] == "hidden")
                .unwrap();
            assert!(hidden_update["identityOid"].is_null());
            let states = hidden_update["states"].as_array().unwrap();
            assert!(states.iter().all(|state| {
                state["canonicalOid"] != identity_hex && state["canonicalOid"] != state["publicOid"]
            }));
            assert!(
                states
                    .iter()
                    .any(|state| state["canonicalOid"] == hidden_hex)
            );
            respond(
                stream,
                "200 OK",
                r#"{"pending":true,"fence":1,"outcome":null}"#,
            );
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes/")
            && request_line.contains("/recover ")
        {
            respond(
                stream,
                "200 OK",
                r#"{"pending":false,"fence":1,"outcome":"accepted"}"#,
            );
            return true;
        } else {
            panic!("unexpected identity-child request: {request_line}");
        }
        false
    });
    let transport = GitHttpTransport::new(
        &base_url,
        "identity-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();
    let result = push_with_journal(
        &repository,
        &transport,
        &canonical,
        "origin",
        &[
            PushHead {
                bookmark: "clean".to_owned(),
                canonical_oid: Some(clean),
            },
            PushHead {
                bookmark: "hidden".to_owned(),
                canonical_oid: Some(hidden),
            },
        ],
        [0x67; 16],
        &GitProcessEnvironment::new("git", GitProcessMode::Foreground),
        PushFailpoint::None,
    )
    .await
    .unwrap();
    assert_eq!(result.public_heads["clean"], Some(clean));
    assert_ne!(result.public_heads["hidden"], Some(hidden));
    server.join().unwrap();
}
#[tokio::test(flavor = "current_thread")]
async fn settled_aborted_claim_refreshes_without_requesting_replay() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = journal_remote(&temp.path().join("canonical.git"));
    let repository = MachineGitRepository::init(temp.path().join("machine"), &settings())
        .await
        .unwrap();
    let tree = write_tree(&repository, b"claim race");
    let head = write_commit(
        &repository,
        COMMIT_IDENTITY,
        tree,
        &[],
        b"claim race\n",
        &[],
    );
    let remote = temp.path().join("remote.git");
    let initialized = Command::new("git")
        .args(["init", "--bare", remote.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    let remote_url = remote.to_string_lossy().into_owned();
    let head_hex = oid_hex(head);
    let batch_id = "68".repeat(16);
    let mut snapshots = 0_usize;
    let (base_url, server) = create_server(move |_, request, stream| {
        let request_line = request.lines().next().unwrap();
        deny_retired_git_object_routes(request_line);
        if request_line.starts_with("GET ") && request_line.contains("/remotes?") {
            respond(
                stream,
                "200 OK",
                &serde_json::json!({"remotes": [{"name": "origin", "url": remote_url}]})
                    .to_string(),
            );
        } else if request_line.starts_with("GET ") && request_line.contains("/git/projection?") {
            snapshots += 1;
            respond(
                stream,
                "200 OK",
                &serde_json::json!({
                    "activationCursor": 0,
                    "cursors": [],
                    "mappings": [],
                    "nextAfter": 0,
                    "through": 0,
                    "hasMore": false,
                    "pending": if snapshots == 1 {
                        vec![serde_json::json!({
                            "batchId": batch_id,
                            "remote": "origin",
                            "ownerMachine": "22".repeat(16),
                            "fence": 1,
                            "refs": [{
                                "bookmark": "main",
                                "expectedOldOid": null,
                                "proposedPublicOid": head_hex,
                                "identityOid": head_hex,
                            }],
                        })]
                    } else {
                        Vec::new()
                    },
                })
                .to_string(),
            );
        } else if request_line.starts_with("POST ") && request_line.contains("/claim ") {
            respond(
                stream,
                "200 OK",
                r#"{"pending":false,"fence":1,"outcome":"aborted"}"#,
            );
        } else if request_line.contains("/replay?") {
            panic!("settled aborted claim must not request replay");
        } else if request_line.starts_with("POST ")
            && request_line.contains("/git/projection/pushes ")
        {
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["updates"][0]["identityOid"], head_hex);
            assert_eq!(body["updates"][0]["states"], serde_json::json!([]));
            respond(
                stream,
                "200 OK",
                r#"{"pending":true,"fence":2,"outcome":null}"#,
            );
        } else if request_line.starts_with("POST ") && request_line.contains("/recover ") {
            respond(
                stream,
                "200 OK",
                r#"{"pending":false,"fence":2,"outcome":"accepted"}"#,
            );
            return true;
        } else {
            panic!("unexpected claim-race request: {request_line}");
        }
        false
    });
    let transport = GitHttpTransport::new(
        &base_url,
        "claim-secret",
        &"11".repeat(16),
        &"ab".repeat(32),
        &"cd".repeat(16),
    )
    .unwrap();
    let result = push_with_journal(
        &repository,
        &transport,
        &canonical,
        "origin",
        &[PushHead {
            bookmark: "main".to_owned(),
            canonical_oid: Some(head),
        }],
        [0x69; 16],
        &GitProcessEnvironment::new("git", GitProcessMode::Foreground),
        PushFailpoint::None,
    )
    .await
    .unwrap();
    assert!(result.recovered_batches.is_empty());
    assert_eq!(result.public_heads["main"], Some(head));
    assert_eq!(remote_ref(&remote, "main"), head);
    server.join().unwrap();
}
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_URL and DEVSPACE_SHARED_SECRET for a live Worker"]
async fn live_journal_push_recovery_and_fetch_proofs() {
    let total = std::time::Instant::now();
    let base_url = std::env::var("DEVSPACE_URL").expect("set DEVSPACE_URL");
    let shared_secret =
        std::env::var("DEVSPACE_SHARED_SECRET").expect("set DEVSPACE_SHARED_SECRET");
    if std::env::var_os("DEVSPACE_JOURNAL_CRASH_CHILD").is_some() {
        let repository_id = std::env::var("DEVSPACE_JOURNAL_REPOSITORY_ID").unwrap();
        let incarnation = std::env::var("DEVSPACE_JOURNAL_INCARNATION").unwrap();
        let repository_path = std::env::var_os("DEVSPACE_JOURNAL_MACHINE_PATH").unwrap();
        let crash_head = Oid::from_hex(
            std::env::var("DEVSPACE_JOURNAL_CRASH_OID")
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let repository = MachineGitRepository::open(repository_path, &settings())
            .await
            .unwrap();
        let transport = GitHttpTransport::new(
            &base_url,
            &shared_secret,
            &"11".repeat(16),
            &repository_id,
            &incarnation,
        )
        .unwrap();
        let canonical = CanonicalGitRemote::from_env(MachineId::parse("11".repeat(16)).unwrap())
            .expect("crash child needs DEVSPACE_CANONICAL_GIT_REMOTE");
        let result = push_with_journal(
            &repository,
            &transport,
            &canonical,
            "origin",
            &[PushHead {
                bookmark: "crash".to_owned(),
                canonical_oid: Some(crash_head),
            }],
            [0x33; 16],
            &GitProcessEnvironment::new("git", GitProcessMode::Foreground),
            PushFailpoint::AfterGitPush,
        )
        .await;
        if matches!(
            result,
            Err(JournalFlowError::AfterPushFailpoint { batch_id }) if batch_id == [0x33; 16]
        ) {
            std::process::exit(86);
        }
        panic!("crash child did not reach AFTER_PUSH: {result:?}");
    }
    let (repository_id, incarnation) =
        create_live_repository(&base_url, &shared_secret, "git-journal-spike").await;
    let machine_a = "11".repeat(16);
    let machine_b = "22".repeat(16);
    let transport_a = GitHttpTransport::new(
        &base_url,
        &shared_secret,
        &machine_a,
        &repository_id,
        &incarnation,
    )
    .unwrap();
    let transport_b = GitHttpTransport::new(
        &base_url,
        &shared_secret,
        &machine_b,
        &repository_id,
        &incarnation,
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let canonical = journal_remote(&temp.path().join("canonical.git"));
    let remote = temp.path().join("remote.git");
    let initialized = Command::new("git")
        .args(["init", "--bare", remote.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(initialized.status.success());
    transport_a
        .set_remote("origin", remote.to_str().unwrap())
        .await
        .unwrap();
    let a = MachineGitRepository::init(temp.path().join("machine-a"), &settings())
        .await
        .unwrap();
    let environment = GitProcessEnvironment::new("git", GitProcessMode::Foreground);

    // (a) Hidden-bearing canonical history becomes a public-only remote graph.
    let started = std::time::Instant::now();
    let (hidden_head, secret) = write_hidden_commit(&a, None, b"private-live-sentinel\0\xff");
    let pushed_hidden = push_with_journal(
        &a,
        &transport_a,
        &canonical,
        "origin",
        &[PushHead {
            bookmark: "main".to_owned(),
            canonical_oid: Some(hidden_head),
        }],
        [0x31; 16],
        &environment,
        PushFailpoint::None,
    )
    .await
    .unwrap();
    let public_main = pushed_hidden.public_heads["main"].unwrap();
    assert_ne!(public_main, hidden_head);
    assert_eq!(remote_ref(&remote, "main"), public_main);
    assert_remote_blobs_exclude(&remote, &secret);
    let snapshot = transport_a.projection_snapshot_all().await.unwrap();
    let cursor = snapshot
        .cursors
        .iter()
        .find(|cursor| cursor.remote == "origin" && cursor.bookmark == "main")
        .unwrap();
    assert_eq!(cursor.canonical_oid, hidden_head);
    assert_eq!(cursor.public_oid, public_main);
    eprintln!("LIVE_PROOF a passed in {:?}", started.elapsed());

    // (b) An identity-projected signed commit crosses a real push byte-for-byte.
    let started = std::time::Instant::now();
    let signed_tree = write_tree(&a, b"signed live identity");
    let signed = write_commit(
        &a,
        COMMIT_IDENTITY,
        signed_tree,
        &[],
        b"signed live\n",
        SIGNATURE,
    );
    let signed_bytes = read_raw(&a, signed);
    let signed_result = push_with_journal(
        &a,
        &transport_a,
        &canonical,
        "origin",
        &[PushHead {
            bookmark: "signed".to_owned(),
            canonical_oid: Some(signed),
        }],
        [0x32; 16],
        &environment,
        PushFailpoint::None,
    )
    .await
    .unwrap();
    assert_eq!(signed_result.public_heads["signed"], Some(signed));
    assert_eq!(remote_commit(&remote, signed), signed_bytes);
    let identity_snapshot = transport_a.projection_snapshot_all().await.unwrap();
    assert!(
        identity_snapshot
            .mappings
            .iter()
            .all(|mapping| { mapping.canonical_oid != signed && mapping.public_oid != signed })
    );
    assert!(identity_snapshot.cursors.iter().any(|cursor| {
        cursor.remote == "origin"
            && cursor.bookmark == "signed"
            && cursor.canonical_oid == signed
            && cursor.public_oid == signed
    }));
    eprintln!("LIVE_PROOF b passed in {:?}", started.elapsed());

    // (c) A stops after Git push; fresh B claims and recovers from the Git remote.
    let started = std::time::Instant::now();
    let (crash_head, _) = write_hidden_commit(&a, Some(hidden_head), b"crash-private\0\xfe");
    let crashed = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "live_journal_push_recovery_and_fetch_proofs",
            "--ignored",
            "--nocapture",
        ])
        .env("DEVSPACE_JOURNAL_CRASH_CHILD", "1")
        .env("DEVSPACE_JOURNAL_REPOSITORY_ID", &repository_id)
        .env("DEVSPACE_JOURNAL_INCARNATION", &incarnation)
        .env("DEVSPACE_JOURNAL_MACHINE_PATH", a.path())
        .env("DEVSPACE_JOURNAL_CRASH_OID", oid_hex(crash_head))
        .env(
            "DEVSPACE_CANONICAL_GIT_REMOTE",
            temp.path().join("canonical.git"),
        )
        .output()
        .unwrap();
    assert_eq!(
        crashed.status.code(),
        Some(86),
        "crash child failed:\n{}{}",
        String::from_utf8_lossy(&crashed.stdout),
        String::from_utf8_lossy(&crashed.stderr)
    );
    let pending = transport_a.projection_snapshot_all().await.unwrap();
    let pending_public = pending
        .pending
        .iter()
        .find(|batch| batch.batch_id == [0x33; 16])
        .unwrap()
        .refs[0]
        .proposed_public_oid
        .unwrap();
    assert_eq!(remote_ref(&remote, "crash"), pending_public);
    let b = MachineGitRepository::init(temp.path().join("machine-b"), &settings())
        .await
        .unwrap();
    assert!(!b.git_repo_path().join("refs/devspace").exists());
    let recovered = push_with_journal(
        &b,
        &transport_b,
        &canonical,
        "origin",
        &[PushHead {
            bookmark: "crash".to_owned(),
            canonical_oid: Some(crash_head),
        }],
        [0x34; 16],
        &environment,
        PushFailpoint::None,
    )
    .await
    .unwrap();
    assert_eq!(recovered.recovered_batches, vec![[0x33; 16]]);
    assert_eq!(recovered.outcome, "up-to-date");
    assert_eq!(read_raw(&b, crash_head), read_raw(&a, crash_head));
    assert_eq!(read_raw(&b, pending_public), read_raw(&a, pending_public));
    let recovered_snapshot = transport_b.projection_snapshot_all().await.unwrap();
    assert!(recovered_snapshot.pending.is_empty());
    assert_eq!(
        recovered_snapshot
            .cursors
            .iter()
            .find(|cursor| cursor.bookmark == "crash")
            .unwrap()
            .public_oid,
        pending_public
    );
    eprintln!("LIVE_PROOF c passed in {:?}", started.elapsed());

    // (d) A foreign child of rewritten public P is fetched, hidden state is
    // replayed onto it, recordFetch stores L(F)->F, and the next push reuses it.
    let started = std::time::Instant::now();
    let public_tree = parse_commit(&read_raw(&a, public_main)).unwrap().tree;
    let foreign = write_commit(
        &a,
        COMMIT_IDENTITY,
        public_tree,
        &[public_main],
        b"foreign after projected history\n",
        &[],
    );
    let direct = push(
        a.git_repo_path(),
        &RemoteUrl::new(remote.to_string_lossy()),
        &BTreeMap::from([(
            QualifiedRef::from_bookmark("main").unwrap(),
            LeaseUpdate {
                expected_old_oid: Some(public_main),
                new_oid: Some(foreign),
            },
        )]),
        &environment,
    )
    .unwrap();
    assert_eq!(
        direct.refs[&QualifiedRef::from_bookmark("main").unwrap()].observed_oid,
        Some(foreign)
    );
    let fetched = fetch_with_journal(
        &b,
        &transport_b,
        &canonical,
        "origin",
        &["main".to_owned()],
        [0x35; 16],
        &environment,
    )
    .await
    .unwrap();
    assert_eq!(fetched.public_heads["main"], foreign);
    assert_ne!(fetched.canonical_heads["main"], foreign);
    assert_eq!(fetched.mirrors.len(), 1);
    assert_eq!(fetched.mirrors[0].public_parents, vec![public_main]);
    assert_eq!(fetched.mirrors[0].canonical_parents, vec![hidden_head]);
    assert!(fetched.disclosure_warnings.is_empty());
    let canonical_foreign_bytes = read_raw(&b, fetched.canonical_heads["main"]);
    let canonical_foreign = parse_commit(&canonical_foreign_bytes).unwrap();
    assert_eq!(canonical_foreign.parents, vec![hidden_head]);
    assert!(tree_has_entry(&b, canonical_foreign.tree, b".dsprivate"));
    assert!(tree_has_entry(&b, canonical_foreign.tree, b"secret.bin"));
    let final_snapshot = transport_b.projection_snapshot_all().await.unwrap();
    let main_cursor = final_snapshot
        .cursors
        .iter()
        .find(|cursor| cursor.remote == "origin" && cursor.bookmark == "main")
        .unwrap();
    assert_eq!(main_cursor.public_oid, foreign);
    assert_eq!(main_cursor.canonical_oid, fetched.canonical_heads["main"]);
    assert!(final_snapshot.mappings.iter().any(|mapping| {
        mapping.canonical_oid == fetched.canonical_heads["main"] && mapping.public_oid == foreign
    }));

    let (local_after_fetch, _) = write_hidden_commit(
        &b,
        Some(fetched.canonical_heads["main"]),
        b"local-after-lift\0\xfd",
    );
    let pushed_after_fetch = push_with_journal(
        &b,
        &transport_b,
        &canonical,
        "origin",
        &[PushHead {
            bookmark: "main".to_owned(),
            canonical_oid: Some(local_after_fetch),
        }],
        [0x36; 16],
        &environment,
        PushFailpoint::None,
    )
    .await
    .unwrap();
    let public_after_fetch = pushed_after_fetch.public_heads["main"].unwrap();
    assert_eq!(
        parse_commit(&read_raw(&b, public_after_fetch))
            .unwrap()
            .parents,
        vec![foreign]
    );
    assert_eq!(remote_ref(&remote, "main"), public_after_fetch);
    eprintln!("LIVE_PROOF d passed in {:?}", started.elapsed());

    // (e) Two hidden-divergent canonical roots can share one public OID. Each
    // fetched bookmark uses its cursor to select the correct private lineage.
    let started = std::time::Instant::now();
    let (shared_a, _) = write_hidden_commit(&a, None, b"shared-a-private");
    let (shared_b, _) = write_hidden_commit(&a, None, b"shared-b-private");
    assert_ne!(shared_a, shared_b);
    let shared_push = push_with_journal(
        &a,
        &transport_a,
        &canonical,
        "origin",
        &[
            PushHead {
                bookmark: "shared-a".to_owned(),
                canonical_oid: Some(shared_a),
            },
            PushHead {
                bookmark: "shared-b".to_owned(),
                canonical_oid: Some(shared_b),
            },
        ],
        [0x37; 16],
        &environment,
        PushFailpoint::None,
    )
    .await
    .unwrap();
    let shared_public = shared_push.public_heads["shared-a"].unwrap();
    assert_eq!(shared_push.public_heads["shared-b"], Some(shared_public));
    let c = MachineGitRepository::init(temp.path().join("machine-c"), &settings())
        .await
        .unwrap();
    let fetched_shared = fetch_with_journal(
        &c,
        &transport_b,
        &canonical,
        "origin",
        &["shared-a".to_owned(), "shared-b".to_owned()],
        [0x38; 16],
        &environment,
    )
    .await
    .unwrap();
    assert_eq!(fetched_shared.public_heads["shared-a"], shared_public);
    assert_eq!(fetched_shared.public_heads["shared-b"], shared_public);
    assert_eq!(fetched_shared.canonical_heads["shared-a"], shared_a);
    assert_eq!(fetched_shared.canonical_heads["shared-b"], shared_b);
    assert!(fetched_shared.mirrors.is_empty());
    eprintln!("LIVE_PROOF e passed in {:?}", started.elapsed());
    eprintln!("LIVE_PROOF total {:?}", total.elapsed());
}

fn write_tree(repository: &MachineGitRepository, contents: &[u8]) -> Oid {
    let blob = write_raw(repository, ObjectKind::Blob, contents);
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(&blob.0);
    write_raw(repository, ObjectKind::Tree, &tree)
}

fn write_hidden_commit(
    repository: &MachineGitRepository,
    parent: Option<Oid>,
    secret: &[u8],
) -> (Oid, Vec<u8>) {
    let policy = write_raw(repository, ObjectKind::Blob, b"secret.bin\n");
    let public = write_raw(repository, ObjectKind::Blob, b"public live bytes\n");
    let secret_oid = write_raw(repository, ObjectKind::Blob, secret);
    let mut tree = Vec::new();
    for (name, oid) in [
        (b".dsprivate".as_slice(), policy),
        (b"public.txt".as_slice(), public),
        (b"secret.bin".as_slice(), secret_oid),
    ] {
        tree.extend_from_slice(b"100644 ");
        tree.extend_from_slice(name);
        tree.push(0);
        tree.extend_from_slice(&oid.0);
    }
    let tree = write_raw(repository, ObjectKind::Tree, &tree);
    (
        write_commit(
            repository,
            COMMIT_IDENTITY,
            tree,
            &parent.into_iter().collect::<Vec<_>>(),
            b"hidden live\n",
            &[],
        ),
        secret.to_vec(),
    )
}

fn tree_has_entry(repository: &MachineGitRepository, tree: Oid, name: &[u8]) -> bool {
    devspace_kernel::parse_tree(&read_raw(repository, tree))
        .unwrap()
        .entries
        .iter()
        .any(|entry| entry.name == name)
}

fn remote_ref(remote: &std::path::Path, bookmark: &str) -> Oid {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args(["rev-parse", &format!("refs/heads/{bookmark}")])
        .output()
        .unwrap();
    assert!(output.status.success());
    Oid::from_hex(String::from_utf8(output.stdout).unwrap().trim().as_bytes()).unwrap()
}

fn remote_commit(remote: &std::path::Path, oid: Oid) -> Vec<u8> {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args(["cat-file", "commit", &oid_hex(oid)])
        .output()
        .unwrap();
    assert!(output.status.success());
    output.stdout
}

fn deny_retired_git_object_routes(request_line: &str) {
    if request_line.contains("/packs") || request_line.contains("/git/objects/") {
        panic!("retired Git object route: {request_line}");
    }
}

fn assert_remote_trees_exclude_name(remote: &std::path::Path, name: &[u8]) {
    let listed = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ])
        .output()
        .unwrap();
    assert!(listed.status.success());
    for line in String::from_utf8(listed.stdout).unwrap().lines() {
        let (oid, kind) = line.split_once(' ').unwrap();
        if kind != "tree" {
            continue;
        }
        let tree = Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["cat-file", "tree", oid])
            .output()
            .unwrap();
        assert!(tree.status.success());
        let parsed = devspace_kernel::parse_tree(&tree.stdout).unwrap();
        assert!(
            parsed.entries.iter().all(|entry| entry.name != name),
            "publishable tree {oid} still names {}",
            String::from_utf8_lossy(name)
        );
    }
}

fn assert_remote_blobs_include(remote: &std::path::Path, sentinel: &[u8]) {
    let listed = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let found = String::from_utf8(listed.stdout).unwrap().lines().any(|line| {
        let (oid, kind) = line.split_once(' ').unwrap();
        if kind != "blob" {
            return false;
        }
        let blob = Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["cat-file", "blob", oid])
            .output()
            .unwrap();
        blob.status.success()
            && blob
                .stdout
                .windows(sentinel.len())
                .any(|window| window == sentinel)
    });
    assert!(found, "canonical remote is missing the private sentinel");
}

fn assert_remote_blobs_exclude(remote: &std::path::Path, sentinel: &[u8]) {
    let listed = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args([
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname) %(objecttype)",
        ])
        .output()
        .unwrap();
    assert!(listed.status.success());
    for line in String::from_utf8(listed.stdout).unwrap().lines() {
        let (oid, kind) = line.split_once(' ').unwrap();
        if kind != "blob" {
            continue;
        }
        let blob = Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["cat-file", "blob", oid])
            .output()
            .unwrap();
        assert!(blob.status.success());
        assert!(
            !blob
                .stdout
                .windows(sentinel.len())
                .any(|window| window == sentinel),
            "private sentinel reached remote blob {oid}"
        );
    }
}
