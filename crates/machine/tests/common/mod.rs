#![allow(dead_code)]

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use devspace_kernel::{ObjectKind, Oid, validate};
use devspace_machine::{
    ControlPlaneClient, MachineConfig, MachineGitRepository, MachineId, RepositoryName,
    SharedSecret, encode_lower_hex,
};
use gix::objs::{Kind as GitObjectKind, Write as _};
use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;

pub fn settings() -> UserSettings {
    devspace_testutils::settings("Devspace Test", "devspace@example.invalid", false)
}

pub fn oid_hex(id: Oid) -> String {
    encode_lower_hex(&id.0)
}

pub fn write_raw(repository: &MachineGitRepository, kind: ObjectKind, bytes: &[u8]) -> Oid {
    let expected = validate(kind, bytes).unwrap().id;
    let git = gix::open(repository.git_repo_path()).unwrap();
    let git_kind = match kind {
        ObjectKind::Blob => GitObjectKind::Blob,
        ObjectKind::Tree => GitObjectKind::Tree,
        ObjectKind::Commit => GitObjectKind::Commit,
    };
    let actual = git.objects.write_buf(git_kind, bytes).unwrap();
    assert_eq!(actual.as_bytes(), expected.0);
    expected
}

/// Write a canonical commit signed off by `identity`, formatted `Name <email>`.
///
/// `identity` is part of the commit bytes, so each suite passes its own and the
/// fixture object IDs stay stable.
pub fn write_commit(
    repository: &MachineGitRepository,
    identity: &str,
    tree: Oid,
    parents: &[Oid],
    message: &[u8],
    extras: &[(&[u8], &[u8])],
) -> Oid {
    let mut bytes = format!("tree {}\n", oid_hex(tree)).into_bytes();
    for parent in parents {
        bytes.extend_from_slice(format!("parent {}\n", oid_hex(*parent)).as_bytes());
    }
    bytes.extend_from_slice(format!("author {identity} 1700000000 +0000\n").as_bytes());
    bytes.extend_from_slice(format!("committer {identity} 1700000000 +0000\n").as_bytes());
    for (name, value) in extras {
        bytes.extend_from_slice(name);
        bytes.push(b' ');
        bytes.extend_from_slice(value);
        bytes.push(b'\n');
    }
    bytes.push(b'\n');
    bytes.extend_from_slice(message);
    write_raw(repository, ObjectKind::Commit, &bytes)
}

pub fn read_raw(repository: &MachineGitRepository, id: Oid) -> Vec<u8> {
    gix::open(repository.git_repo_path())
        .unwrap()
        .find_object(gix::ObjectId::from_bytes_or_panic(&id.0))
        .unwrap()
        .data
        .clone()
}

pub fn run_git(repository: &MachineGitRepository, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(repository.git_repo_path())
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

pub fn all_objects(repository: &MachineGitRepository) -> BTreeSet<String> {
    String::from_utf8(run_git(
        repository,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    ))
    .unwrap()
    .lines()
    .map(str::to_owned)
    .collect()
}

/// Create a repository named `{label}-{nanos}` on the live Worker at `base_url`.
///
/// The `#[ignore]`d live suites call this. It goes through the shipping control-plane
/// client, so the test path carries the product's timeouts and error handling.
pub async fn create_live_repository(
    base_url: &str,
    shared_secret: &str,
    label: &str,
) -> (String, String) {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let config = MachineConfig::new(
        base_url,
        MachineId::parse("11".repeat(16)).unwrap(),
        SharedSecret::new(shared_secret).unwrap(),
    )
    .unwrap();
    let name = RepositoryName::parse(format!("{label}-{suffix}")).unwrap();
    let created = ControlPlaneClient::new(&config)
        .unwrap()
        .create_repository(&name, suffix.to_be_bytes())
        .await
        .unwrap_or_else(|error| panic!("creating `{name}` on {base_url} failed: {error}"));
    (
        created.identity.repository_id.as_str().to_owned(),
        created.identity.incarnation.as_str().to_owned(),
    )
}

pub async fn tree_with_file(store: &Arc<Store>, path: &RepoPathBuf, contents: &[u8]) -> MergedTree {
    let mut reader = contents;
    let file_id = store.write_file(path, &mut reader).await.unwrap();
    let mut builder = MergedTreeBuilder::new(store.empty_merged_tree());
    builder.set_or_remove(
        path.clone(),
        Merge::normal(TreeValue::File {
            id: file_id,
            executable: false,
            copy_id: CopyId::placeholder(),
        }),
    );
    builder.write_tree().await.unwrap()
}
