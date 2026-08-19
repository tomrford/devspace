use std::collections::BTreeSet;
use std::path::Path;
use std::thread;

use devspace_kernel::{ObjectKind, Oid, validate};
use devspace_machine::{
    CanonicalGitRemote, GitProcessEnvironment, MachineGitRepository, MachineId,
    delete_remote_ref, encode_lower_hex, gc_bare_remote, init_bare_remote,
};
use jj_lib::settings::UserSettings;

mod common;

use common::{oid_hex, read_raw, write_commit, write_raw};

fn settings() -> UserSettings {
    devspace_testutils::settings("Canonical Remote Test", "canonical@example.invalid", false)
}

fn machine_id(byte: u8) -> MachineId {
    MachineId::parse(format!("{byte:02x}").repeat(16)).unwrap()
}

async fn machine(path: &Path) -> MachineGitRepository {
    MachineGitRepository::init(path, &settings()).await.unwrap()
}

fn attach(path: &Path, id: u8) -> CanonicalGitRemote {
    init_bare_remote(path).unwrap();
    CanonicalGitRemote::new(
        path.to_string_lossy(),
        machine_id(id),
        GitProcessEnvironment::default(),
    )
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
    let signed = write_commit(
        repository,
        "Signer <signer@example.com>",
        tree,
        &[unknown],
        b"signed\n",
        &[
            (
                b"gpgsig",
                b"-----BEGIN PGP SIGNATURE-----\n abcdef\n -----END PGP SIGNATURE-----",
            ),
            (
                b"mergetag",
                b"object 5555555555555555555555555555555555555555\n type commit\n tag v1\n tagger Tagger <tagger@example.com> 1700000300 +0000\n \n signed tag message",
            ),
        ],
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
        &[signed],
        b"jj conflict\n",
        &[
            (b"change-id", b"zyxwvutsrqponmlkzyxwvutsrqponmlk"),
            (b"jj:trees", trees.as_bytes()),
            (b"jj:conflict-labels", b"left\n base\n right\n "),
        ],
    );
    BTreeSet::from([unknown, signed, conflicted])
}

fn assert_exact_objects(source: &MachineGitRepository, destination: &MachineGitRepository, ids: &BTreeSet<Oid>) {
    for id in ids {
        let expected = validate(ObjectKind::Commit, &read_raw(source, *id)).unwrap();
        assert_eq!(expected.id, *id);
        let actual = read_raw(destination, *id);
        assert_eq!(actual, read_raw(source, *id), "{}", oid_hex(*id));
        let closure = source.object_closure([*id]).unwrap();
        for object in closure.objects {
            assert_eq!(
                read_raw(destination, object.key.id),
                read_raw(source, object.key.id),
                "{}",
                encode_lower_hex(&object.key.id.0)
            );
        }
    }
}

fn user_heads(git_dir: &Path) -> String {
    String::from_utf8(
        std::process::Command::new("git")
            .args(["--git-dir"])
            .arg(git_dir)
            .args(["for-each-ref", "--format=%(refname)", "refs/heads"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn machine_anchor_round_trips_exact_git_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let source = machine(&temp.path().join("source")).await;
    let heads = write_fixture_graph(&source);
    let remote = attach(&temp.path().join("remote.git"), 1);
    remote.push_commits(&source, heads.iter().copied()).unwrap();

    let destination = machine(&temp.path().join("destination")).await;
    remote.verify_commits(&destination, heads.iter().copied()).unwrap();
    assert_exact_objects(&source, &destination, &heads);
    assert!(
        !user_heads(destination.git_repo_path()).contains("__devspace"),
        "transport refs must stay out of refs/heads: {}",
        user_heads(destination.git_repo_path())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn machine_anchor_survives_forced_gc() {
    let temp = tempfile::tempdir().unwrap();
    let source = machine(&temp.path().join("source")).await;
    let heads = write_fixture_graph(&source);
    let remote_path = temp.path().join("remote.git");
    let remote = attach(&remote_path, 1);
    remote.push_commits(&source, heads.iter().copied()).unwrap();
    gc_bare_remote(&remote_path).unwrap();
    let after_gc = machine(&temp.path().join("gc")).await;
    remote.verify_commits(&after_gc, heads.iter().copied()).unwrap();
    assert_exact_objects(&source, &after_gc, &heads);
}

#[tokio::test(flavor = "current_thread")]
async fn two_machines_push_without_serializing_on_machine_refs() {
    let temp = tempfile::tempdir().unwrap();
    let remote_path = temp.path().join("remote.git");
    init_bare_remote(&remote_path).unwrap();
    let left = machine(&temp.path().join("left")).await;
    let right = machine(&temp.path().join("right")).await;
    let left_heads = write_fixture_graph(&left);
    let blob = write_raw(&right, ObjectKind::Blob, b"other machine\n");
    let mut tree = b"100644 other\0".to_vec();
    tree.extend_from_slice(&blob.0);
    let tree = write_raw(&right, ObjectKind::Tree, &tree);
    let right_head = write_commit(
        &right,
        "Other <other@example.com>",
        tree,
        &[],
        b"other\n",
        &[],
    );

    let left_remote = CanonicalGitRemote::new(
        remote_path.to_string_lossy(),
        machine_id(1),
        GitProcessEnvironment::default(),
    );
    let right_remote = CanonicalGitRemote::new(
        remote_path.to_string_lossy(),
        machine_id(2),
        GitProcessEnvironment::default(),
    );
    let left_push = thread::spawn({
        let remote = left_remote.clone();
        let heads = left_heads.clone();
        move || remote.push_commits(&left, heads)
    });
    right_remote.push_commits(&right, [right_head]).unwrap();
    left_push.join().unwrap().unwrap();

    let recovered = machine(&temp.path().join("recovered")).await;
    left_remote.verify_commits(&recovered, left_heads.iter().copied()).unwrap();
    right_remote.verify_commits(&recovered, [right_head]).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn deleted_machine_ref_can_be_recreated() {
    let temp = tempfile::tempdir().unwrap();
    let remote_path = temp.path().join("remote.git");
    init_bare_remote(&remote_path).unwrap();
    let source = machine(&temp.path().join("source")).await;
    let heads = write_fixture_graph(&source);
    let remote = CanonicalGitRemote::new(
        remote_path.to_string_lossy(),
        machine_id(9),
        GitProcessEnvironment::default(),
    );
    remote.push_commits(&source, heads.iter().copied()).unwrap();
    delete_remote_ref(
        &source,
        remote.url(),
        &format!("__devspace/machines/{}", machine_id(9).as_str()),
        &GitProcessEnvironment::default(),
    )
    .unwrap();
    remote.push_commits(&source, heads.iter().copied()).unwrap();
    let destination = machine(&temp.path().join("destination")).await;
    remote.verify_commits(&destination, heads.iter().copied()).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn stale_local_state_does_not_clobber_a_newer_machine_ref() {
    let temp = tempfile::tempdir().unwrap();
    let remote_path = temp.path().join("remote.git");
    init_bare_remote(&remote_path).unwrap();
    let source = machine(&temp.path().join("source")).await;
    let first = write_fixture_graph(&source);
    let remote = CanonicalGitRemote::new(
        remote_path.to_string_lossy(),
        machine_id(3),
        GitProcessEnvironment::default(),
    );
    remote.push_commits(&source, first.iter().copied()).unwrap();
    let later = write_commit(
        &source,
        "Later <later@example.com>",
        *source
            .object_closure(first.iter().copied())
            .unwrap()
            .objects
            .iter()
            .find(|object| object.key.kind == ObjectKind::Tree)
            .map(|object| &object.key.id)
            .unwrap(),
        &first.iter().copied().collect::<Vec<_>>(),
        b"later\n",
        &[],
    );
    remote.push_commits(&source, [later]).unwrap();
    let stale = CanonicalGitRemote::new(
        remote_path.to_string_lossy(),
        machine_id(3),
        GitProcessEnvironment::default(),
    );
    // A second handle still observes the current tip before pushing, so a
    // retry of the older set must fast-forward or no-op, not clobber.
    stale.push_commits(&source, first.iter().copied()).unwrap();
    let recovered = machine(&temp.path().join("recovered")).await;
    remote.verify_commits(&recovered, [later]).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn machine_anchor_retains_a_long_commit_chain() {
    let temp = tempfile::tempdir().unwrap();
    let source = machine(&temp.path().join("source")).await;
    let blob = write_raw(&source, ObjectKind::Blob, b"chain-0\n");
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(&blob.0);
    let mut tree = write_raw(&source, ObjectKind::Tree, &tree);
    let mut heads = Vec::new();
    let mut parent = None;
    for index in 0..80 {
        let commit = write_commit(
            &source,
            "Chain <chain@example.invalid>",
            tree,
            &parent.into_iter().collect::<Vec<_>>(),
            format!("chain {index}\n").as_bytes(),
            &[],
        );
        if index % 11 == 0 {
            let blob = write_raw(&source, ObjectKind::Blob, format!("chain-{index}\n").as_bytes());
            let mut bytes = b"100644 file\0".to_vec();
            bytes.extend_from_slice(&blob.0);
            tree = write_raw(&source, ObjectKind::Tree, &bytes);
        }
        parent = Some(commit);
        heads.push(commit);
    }
    let remote = attach(&temp.path().join("remote.git"), 4);
    remote
        .push_commits(&source, heads[..40].iter().copied())
        .unwrap();
    remote
        .push_commits(&source, heads[40..].iter().copied())
        .unwrap();

    let destination = machine(&temp.path().join("destination")).await;
    remote
        .verify_commits(&destination, heads.iter().copied())
        .unwrap();
    assert_exact_objects(&source, &destination, &heads.into_iter().collect());
}
