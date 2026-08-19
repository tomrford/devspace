//! Live Cloudflare Artifact compatibility. Ignored unless the environment
//! names an Artifact repository URL. Tokens stay in the process environment
//! and are never printed.

use std::collections::BTreeSet;

use devspace_kernel::ObjectKind;
use devspace_machine::{
    CanonicalGitRemote, GitProcessEnvironment, MachineGitRepository, MachineId,
};
use jj_lib::settings::UserSettings;

mod common;

use common::{oid_hex, write_commit, write_raw};

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

fn live_remote(id: u8) -> Option<CanonicalGitRemote> {
    let url = std::env::var("DEVSPACE_CANONICAL_GIT_REMOTE").ok()?;
    if url.is_empty() {
        return None;
    }
    Some(
        CanonicalGitRemote::from_env(machine_id(id))
            .unwrap_or_else(|_| CanonicalGitRemote::new(url, machine_id(id), GitProcessEnvironment::default())),
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires DEVSPACE_CANONICAL_GIT_REMOTE against a disposable Artifact"]
async fn live_artifact_round_trips_exact_git_bytes() {
    let Some(remote) = live_remote(1) else {
        panic!("DEVSPACE_CANONICAL_GIT_REMOTE must name the disposable Artifact");
    };
    let temp = tempfile::tempdir().unwrap();
    let source = MachineGitRepository::init(temp.path().join("source"), &settings())
        .await
        .unwrap();
    let blob = write_raw(&source, ObjectKind::Blob, b"artifact live\n");
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(&blob.0);
    let tree = write_raw(&source, ObjectKind::Tree, &tree);
    let head = write_commit(
        &source,
        "Live <live@example.invalid>",
        tree,
        &[],
        b"live\n",
        &[(b"x-vendor", b"opaque")],
    );
    remote.push_commits(&source, BTreeSet::from([head])).unwrap();

    let destination = MachineGitRepository::init(temp.path().join("destination"), &settings())
        .await
        .unwrap();
    remote
        .verify_commits(&destination, BTreeSet::from([head]))
        .unwrap();
    assert_eq!(
        common::read_raw(&destination, head),
        common::read_raw(&source, head),
        "{}",
        oid_hex(head)
    );
}
