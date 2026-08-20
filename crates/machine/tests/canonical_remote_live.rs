//! Live Cloudflare Artifact compatibility. Ignored unless the environment
//! names an Artifact repository URL. Tokens stay in the process environment
//! and are never printed.

use std::collections::BTreeSet;
use std::thread;

use devspace_kernel::{ObjectKind, Oid};
use devspace_machine::{
    CanonicalGitRemote, GitProcessEnvironment, LeaseUpdate, MachineGitRepository, MachineId,
    PushErrorKind, QualifiedRef, encode_lower_hex, ls_remote_matching, push,
};
use jj_lib::settings::UserSettings;

mod common;

use common::{oid_hex, read_raw, write_commit, write_raw};

fn settings() -> UserSettings {
    devspace_testutils::settings(
        "Canonical Remote Live",
        "canonical-live@example.invalid",
        false,
    )
}

fn machine_id(byte: u8) -> MachineId {
    MachineId::parse(format!("{byte:02x}").repeat(16)).unwrap()
}

fn live_environment() -> GitProcessEnvironment {
    match std::env::var_os("DEVSPACE_CANONICAL_GIT_TOKEN") {
        Some(token) if !token.is_empty() => {
            GitProcessEnvironment::default().with_http_bearer(token)
        }
        _ => GitProcessEnvironment::default(),
    }
}

fn live_remote(id: u8) -> CanonicalGitRemote {
    let url = std::env::var("DEVSPACE_CANONICAL_GIT_REMOTE")
        .ok()
        .filter(|value| !value.is_empty())
        .expect("DEVSPACE_CANONICAL_GIT_REMOTE must name the disposable Artifact");
    CanonicalGitRemote::new(url, machine_id(id), live_environment())
}

fn write_fixture_graph(repository: &MachineGitRepository) -> BTreeSet<Oid> {
    let blob = write_raw(repository, ObjectKind::Blob, b"hello\0world\n");
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(&blob.0);
    let tree = write_raw(repository, ObjectKind::Tree, &tree);
    let unknown = write_commit(
        repository,
        "Alice Example <alice@example.com>",
        tree,
        &[],
        b"message\xffbytes\n",
        &[(b"encoding", b"ISO-8859-1"), (b"x-vendor", b"opaque value")],
    );
    let left = write_raw(repository, ObjectKind::Tree, &[]);
    let base_blob = write_raw(repository, ObjectKind::Blob, b"base side\n");
    let mut base_tree = b"100644 file\0".to_vec();
    base_tree.extend_from_slice(&base_blob.0);
    let base = write_raw(repository, ObjectKind::Tree, &base_tree);
    let trees = format!("{} {} {}", oid_hex(left), oid_hex(base), oid_hex(tree));
    let conflicted = write_commit(
        repository,
        "JJ User <jj@example.com>",
        tree,
        &[unknown],
        b"jj conflict\n",
        &[
            (b"change-id", b"zyxwvutsrqponmlkzyxwvutsrqponmlk"),
            (b"jj:trees", trees.as_bytes()),
            (b"jj:conflict-labels", b"left\n base\n right\n "),
        ],
    );
    BTreeSet::from([unknown, conflicted])
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_CANONICAL_GIT_REMOTE against a disposable Artifact"]
async fn live_artifact_round_trips_exact_git_bytes() {
    let remote = live_remote(1);
    let temp = tempfile::tempdir().unwrap();
    let source = MachineGitRepository::init(temp.path().join("source"), &settings())
        .await
        .unwrap();
    let heads = write_fixture_graph(&source);
    remote.push_commits(&source, heads.iter().copied()).unwrap();

    let advertised =
        ls_remote_matching(remote.url(), "refs/heads/__devspace/*", &live_environment()).unwrap();
    assert!(
        advertised
            .keys()
            .any(|name| name.contains("__devspace/machines/")),
        "Artifact must advertise the machine retention ref"
    );

    let destination = MachineGitRepository::init(temp.path().join("destination"), &settings())
        .await
        .unwrap();
    remote
        .verify_commits(&destination, heads.iter().copied())
        .unwrap();
    for head in &heads {
        assert_eq!(
            read_raw(&destination, *head),
            read_raw(&source, *head),
            "{}",
            oid_hex(*head)
        );
        let closure = source.object_closure([*head]).unwrap();
        for object in closure.objects {
            assert_eq!(
                read_raw(&destination, object.key.id),
                read_raw(&source, object.key.id),
                "{}",
                encode_lower_hex(&object.key.id.0)
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_CANONICAL_GIT_REMOTE against a disposable Artifact"]
async fn live_artifact_two_machines_do_not_serialize() {
    let temp = tempfile::tempdir().unwrap();
    let left = MachineGitRepository::init(temp.path().join("left"), &settings())
        .await
        .unwrap();
    let right = MachineGitRepository::init(temp.path().join("right"), &settings())
        .await
        .unwrap();
    let left_heads = write_fixture_graph(&left);
    let blob = write_raw(&right, ObjectKind::Blob, b"other machine\n");
    let mut tree = b"100644 other\0".to_vec();
    tree.extend_from_slice(&blob.0);
    let tree = write_raw(&right, ObjectKind::Tree, &tree);
    let right_head = write_commit(
        &right,
        "Other <other@example.invalid>",
        tree,
        &[],
        b"other\n",
        &[],
    );
    let left_remote = live_remote(2);
    let right_remote = live_remote(3);
    let left_push = thread::spawn({
        let remote = left_remote.clone();
        let heads = left_heads.clone();
        move || remote.push_commits(&left, heads)
    });
    right_remote.push_commits(&right, [right_head]).unwrap();
    left_push.join().unwrap().unwrap();

    let recovered = MachineGitRepository::init(temp.path().join("recovered"), &settings())
        .await
        .unwrap();
    left_remote
        .verify_commits(&recovered, left_heads.iter().copied())
        .unwrap();
    right_remote
        .verify_commits(&recovered, [right_head])
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_CANONICAL_GIT_REMOTE against a disposable Artifact"]
async fn live_artifact_rejects_a_stale_retention_lease() {
    let remote = live_remote(4);
    let temp = tempfile::tempdir().unwrap();
    let source = MachineGitRepository::init(temp.path().join("source"), &settings())
        .await
        .unwrap();
    let heads = write_fixture_graph(&source);
    remote.push_commits(&source, heads.iter().copied()).unwrap();
    let bookmark = format!("__devspace/machines/{}", machine_id(4).as_str());
    let reference = QualifiedRef::from_bookmark(&bookmark).unwrap();
    let error = push(
        source.git_repo_path(),
        remote.url(),
        &[(
            reference,
            LeaseUpdate {
                expected_old_oid: None,
                new_oid: heads.iter().next().copied(),
            },
        )]
        .into_iter()
        .collect(),
        &live_environment(),
    )
    .expect_err("creating an existing retention ref must not clobber");
    assert_eq!(error.kind, PushErrorKind::PushFailed);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_CANONICAL_GIT_REMOTE against a disposable Artifact"]
async fn live_artifact_retains_a_bounded_commit_chain() {
    let remote = live_remote(5);
    let temp = tempfile::tempdir().unwrap();
    let source = MachineGitRepository::init(temp.path().join("source"), &settings())
        .await
        .unwrap();
    let blob = write_raw(&source, ObjectKind::Blob, b"live-chain-0\n");
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(&blob.0);
    let tree = write_raw(&source, ObjectKind::Tree, &tree);
    let mut parent = None;
    let mut heads = Vec::new();
    for index in 0..24 {
        let commit = write_commit(
            &source,
            "Chain <chain@example.invalid>",
            tree,
            &parent.into_iter().collect::<Vec<_>>(),
            format!("live chain {index}\n").as_bytes(),
            &[],
        );
        parent = Some(commit);
        heads.push(commit);
    }
    remote
        .push_commits(&source, heads[..12].iter().copied())
        .unwrap();
    remote
        .push_commits(&source, heads[12..].iter().copied())
        .unwrap();
    let destination = MachineGitRepository::init(temp.path().join("destination"), &settings())
        .await
        .unwrap();
    remote
        .verify_commits(&destination, heads.iter().copied())
        .unwrap();
}
