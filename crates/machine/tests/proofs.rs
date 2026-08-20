use std::collections::BTreeSet;
use std::fs;

use devspace_kernel::{ObjectKind, Oid};
use devspace_machine::MachineGitRepository;
use futures::executor::block_on;
use jj_lib::settings::UserSettings;

mod common;

use common::{oid_hex, run_git, write_commit, write_raw};

const COMMIT_IDENTITY: &str = "Foreign <foreign@example.invalid>";

fn settings() -> UserSettings {
    devspace_testutils::settings("Machine Git Test", "machine@example.invalid", true)
}

#[test]
fn initializes_git_odb_with_stock_operation_stores() {
    let temp = tempfile::tempdir().unwrap();
    let repository = block_on(MachineGitRepository::init(temp.path(), &settings())).unwrap();

    assert_eq!(
        repository.git_repo_path(),
        fs::canonicalize(temp.path().join("store/git")).unwrap()
    );
    assert!(repository.git_repo_path().join("objects").is_dir());
    assert!(repository.path().join("store/extra").is_dir());
    assert!(repository.operation_store_path().join("type").is_file());
    assert!(repository.operation_heads_path().join("type").is_file());
    assert!(
        gix::open(repository.git_repo_path())
            .unwrap()
            .workdir()
            .is_none()
    );
}

#[test]
fn discovers_packed_foreign_history_equal_to_git_rev_list() {
    let temp = tempfile::tempdir().unwrap();
    let repository = block_on(MachineGitRepository::init(temp.path(), &settings())).unwrap();
    let foreign = write_foreign_commit(&repository, b"packed closure\n");
    run_git(
        &repository,
        &["update-ref", "refs/heads/foreign", &oid_hex(foreign)],
    );
    run_git(&repository, &["repack", "-ad"]);
    let pack_directory = repository.git_repo_path().join("objects/pack");
    assert!(fs::read_dir(&pack_directory).unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|ext| ext == "pack")
    }));
    let foreign_hex = oid_hex(foreign);
    assert!(
        !repository
            .git_repo_path()
            .join("objects")
            .join(&foreign_hex[..2])
            .join(&foreign_hex[2..])
            .exists()
    );

    let closure = repository.object_closure([foreign]).unwrap();
    let output = run_git(&repository, &["rev-list", "--objects", &oid_hex(foreign)]);
    let expected = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| Oid::from_hex(line.split_whitespace().next().unwrap().as_bytes()).unwrap())
        .collect::<BTreeSet<_>>();
    let actual = closure
        .objects
        .iter()
        .map(|object| object.key.id)
        .collect::<BTreeSet<_>>();

    assert_eq!(actual, expected);
    assert_eq!(
        closure
            .objects
            .iter()
            .map(|object| object.key.kind)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([ObjectKind::Blob, ObjectKind::Tree, ObjectKind::Commit])
    );
}

fn write_foreign_commit(repository: &MachineGitRepository, contents: &[u8]) -> Oid {
    let blob = write_raw(repository, ObjectKind::Blob, contents);
    let mut tree = b"100644 foreign.txt\0".to_vec();
    tree.extend_from_slice(&blob.0);
    let tree = write_raw(repository, ObjectKind::Tree, &tree);
    write_commit(
        repository,
        COMMIT_IDENTITY,
        tree,
        &[],
        b"foreign commit\n",
        &[],
    )
}
