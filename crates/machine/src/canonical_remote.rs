//! Canonical Git remote for Devspace object durability.
//!
//! Replaces the DSPK pack transport. Git objects move through standard Git
//! fetch and push. jj operation and view objects stay on the Durable Object.
//!
//! Mutation boundaries recorded from the previous pack path:
//! 1. local object write (Git ODB)
//! 2. inventory of cloud-known objects
//! 3. pack manifest upload
//! 4. pack chunk upload
//! 5. pack install transaction
//! 6. operation-object upload
//! 7. durable outbox write
//! 8. operation-head transaction
//! 9. outbox clear
//!
//! The Git remote collapses steps 2–5 into: advance a retention root, push it,
//! and verify a fresh fetch can read every required object. Extra immutable
//! objects after a crash are acceptable. A published jj head with missing Git
//! data is not.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use gix::objs::Write as _;
use thiserror::Error;

use crate::object_closure::{ObjectKey, to_git_kind};
use crate::{
    FetchError, GitProcessEnvironment, LeaseUpdate, MachineGitRepository, MachineId, ObjectKind,
    Oid, PushError, PushErrorKind, PushRefStatus, QualifiedRef, RemoteHeadsError, RemoteUrl,
    encode_lower_hex, fetch_refspecs, ls_remote_matching, push,
};
use devspace_kernel::{parse_commit, parse_tree};

const EMPTY_TREE: Oid = Oid([
    0x4b, 0x82, 0x5d, 0xc6, 0x42, 0xcb, 0x6e, 0xb9, 0xa0, 0x60, 0xe5, 0x4b, 0xf8, 0xd6, 0x92, 0x88,
    0xfb, 0xee, 0x49, 0x04,
]);
const RETENTION_AUTHOR: &str = "Devspace Retention <devspace@invalid>";
const LOCAL_RETENTION_PREFIX: &str = "refs/devspace/retention/";
const REMOTE_RETENTION_PREFIX: &str = "refs/heads/__devspace/";

/// Machine-owned retention: `refs/heads/__devspace/machines/<id>` points at a
/// synthetic commit whose parents are the required canonical heads.
///
/// Repository-wide compare-and-swap lost: the next writer must already hold
/// the previous synthetic commit, so unrelated machines serialize on one ref.
/// Direct machine refs lost: Git reachability does not follow `jj:trees`, so
/// those extra trees vanish unless a synthetic carry object names them.

#[derive(Clone)]
pub struct CanonicalGitRemote {
    url: RemoteUrl,
    environment: GitProcessEnvironment,
    machine_id: MachineId,
}

impl std::fmt::Debug for CanonicalGitRemote {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CanonicalGitRemote")
            .field("url", &self.url)
            .field("machine_id", &self.machine_id)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionPush {
    pub refs: BTreeMap<String, Oid>,
    pub required: BTreeSet<Oid>,
}

#[derive(Debug, Error)]
pub enum CanonicalRemoteError {
    #[error("retention bookmark `{0}` is not a valid Git branch name")]
    InvalidRef(String),
    #[error("Git remote rejected a retention update")]
    PushRejected(#[source] PushError),
    #[error("Git remote fetch failed")]
    Fetch(#[source] FetchError),
    #[error("Git remote observation failed")]
    Observe(#[source] RemoteHeadsError),
    #[error("failed to write retention object: {0}")]
    WriteObject(String),
    #[error("retention object {0} is missing after fetch")]
    MissingAfterFetch(String),
    #[error("retention object {expected} fetched as {actual}")]
    ObjectMismatch { expected: String, actual: String },
    #[error("canonical Git remote is not configured")]
    MissingRemote,
}

impl CanonicalGitRemote {
    pub fn new(
        url: impl Into<String>,
        machine_id: MachineId,
        environment: GitProcessEnvironment,
    ) -> Self {
        Self {
            url: RemoteUrl::new(url.into()),
            environment,
            machine_id,
        }
    }

    /// Build a remote from `DEVSPACE_CANONICAL_GIT_REMOTE` and an optional
    /// `DEVSPACE_CANONICAL_GIT_TOKEN`. The token is never logged.
    pub fn from_env(machine_id: MachineId) -> Result<Self, CanonicalRemoteError> {
        let url = std::env::var("DEVSPACE_CANONICAL_GIT_REMOTE")
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or(CanonicalRemoteError::MissingRemote)?;
        let remote = Self::new(url, machine_id, GitProcessEnvironment::default());
        match std::env::var("DEVSPACE_CANONICAL_GIT_TOKEN") {
            Ok(token) if !token.is_empty() => remote.with_token(&token),
            _ => Ok(remote),
        }
    }

    /// Embed a repository-scoped token in the Git URL without exposing it.
    pub fn with_token(self, token: &str) -> Result<Self, CanonicalRemoteError> {
        let url = authenticated_git_url(self.url.expose(), token);
        Ok(Self {
            url: RemoteUrl::new(url),
            ..self
        })
    }

    pub fn url(&self) -> &RemoteUrl {
        &self.url
    }

    pub fn push_commits(
        &self,
        repository: &MachineGitRepository,
        commits: impl IntoIterator<Item = Oid>,
    ) -> Result<RetentionPush, CanonicalRemoteError> {
        let required = commits
            .into_iter()
            .filter(|oid| oid.0 != [0; 20])
            .collect::<BTreeSet<_>>();
        if required.is_empty() {
            return Ok(RetentionPush {
                refs: BTreeMap::new(),
                required,
            });
        }
        ensure_empty_tree(repository)?;
        let updates = self.machine_anchor_updates(repository, &required)?;
        let report = push(
            repository.git_repo_path(),
            &self.url,
            &updates,
            &self.environment,
        )
        .map_err(map_push_error)?;
        for (reference, status) in &report.refs {
            if !matches!(
                status.status,
                PushRefStatus::Updated | PushRefStatus::UpToDate
            ) {
                return Err(CanonicalRemoteError::PushRejected(PushError {
                    kind: PushErrorKind::PushFailed,
                    report,
                }));
            }
            let Some(observed) = status.observed_oid else {
                return Err(CanonicalRemoteError::PushRejected(PushError {
                    kind: PushErrorKind::ObservationFailed,
                    report,
                }));
            };
            let expected = updates[reference]
                .new_oid
                .expect("retention push always names a new OID");
            if observed != expected {
                return Err(CanonicalRemoteError::PushRejected(PushError {
                    kind: PushErrorKind::ObservationFailed,
                    report,
                }));
            }
        }
        Ok(RetentionPush {
            refs: report
                .refs
                .into_iter()
                .map(|(reference, status)| {
                    (
                        reference.to_string(),
                        status.observed_oid.expect("observed after a successful push"),
                    )
                })
                .collect(),
            required,
        })
    }

    pub fn fetch_retention(
        &self,
        repository: &MachineGitRepository,
    ) -> Result<BTreeMap<String, Oid>, CanonicalRemoteError> {
        fetch_refspecs(
            repository.git_repo_path(),
            &self.url,
            &[format!("+{REMOTE_RETENTION_PREFIX}*:{LOCAL_RETENTION_PREFIX}*")],
            &self.environment,
        )
        .map_err(CanonicalRemoteError::Fetch)?;
        let remote = ls_remote_matching(
            &self.url,
            &format!("{REMOTE_RETENTION_PREFIX}*"),
            &self.environment,
        )
        .map_err(CanonicalRemoteError::Observe)?;
        Ok(remote
            .into_iter()
            .filter_map(|(name, oid)| {
                name.strip_prefix(REMOTE_RETENTION_PREFIX)
                    .map(|suffix| (format!("{LOCAL_RETENTION_PREFIX}{suffix}"), oid))
            })
            .collect())
    }

    pub fn verify_commits(
        &self,
        repository: &MachineGitRepository,
        commits: impl IntoIterator<Item = Oid>,
    ) -> Result<(), CanonicalRemoteError> {
        let required = commits
            .into_iter()
            .filter(|oid| oid.0 != [0; 20])
            .collect::<BTreeSet<_>>();
        if required.is_empty() {
            return Ok(());
        }
        self.fetch_retention(repository)?;
        for oid in required {
            require_object(repository, ObjectKind::Commit, oid)?;
            let closure = repository
                .object_closure([oid])
                .map_err(|error| CanonicalRemoteError::WriteObject(error.to_string()))?;
            for object in closure.objects {
                require_object(repository, object.key.kind, object.key.id)?;
            }
        }
        Ok(())
    }

    fn machine_anchor_updates(
        &self,
        repository: &MachineGitRepository,
        required: &BTreeSet<Oid>,
    ) -> Result<BTreeMap<QualifiedRef, LeaseUpdate>, CanonicalRemoteError> {
        let bookmark = format!("__devspace/machines/{}", self.machine_id.as_str());
        let reference = retention_ref(&bookmark)?;
        let previous = remote_oid(&self.url, reference.as_str(), &self.environment)?;
        let mut parents = BTreeSet::new();
        if let Some(previous) = previous {
            parents.insert(previous);
        }
        parents.extend(required.iter().copied());
        let tree = carry_tree(repository, required)?;
        let anchor = write_retention_commit(repository, tree, parents)?;
        Ok(BTreeMap::from([(
            reference,
            LeaseUpdate {
                expected_old_oid: previous,
                new_oid: Some(anchor),
            },
        )]))
    }
}

fn retention_ref(bookmark: &str) -> Result<QualifiedRef, CanonicalRemoteError> {
    QualifiedRef::from_bookmark(bookmark)
        .map_err(|_| CanonicalRemoteError::InvalidRef(bookmark.to_owned()))
}

fn remote_oid(
    url: &RemoteUrl,
    reference: &str,
    environment: &GitProcessEnvironment,
) -> Result<Option<Oid>, CanonicalRemoteError> {
    let refs = ls_remote_matching(url, reference, environment).map_err(CanonicalRemoteError::Observe)?;
    Ok(refs.get(reference).copied())
}

fn ensure_empty_tree(repository: &MachineGitRepository) -> Result<(), CanonicalRemoteError> {
    if object_exists(repository, EMPTY_TREE) {
        return Ok(());
    }
    write_object(repository, ObjectKind::Tree, &[])?;
    Ok(())
}

fn carry_tree(
    repository: &MachineGitRepository,
    required: &BTreeSet<Oid>,
) -> Result<Oid, CanonicalRemoteError> {
    if required.is_empty() {
        return Ok(EMPTY_TREE);
    }
    let closure = repository
        .object_closure(required.iter().copied())
        .map_err(|error| CanonicalRemoteError::WriteObject(error.to_string()))?;
    let git_reachable = git_reachable_keys(repository, required)?;
    let extras = closure
        .objects
        .iter()
        .map(|object| object.key)
        .filter(|key| !git_reachable.contains(key))
        .collect::<BTreeSet<_>>();
    if extras.is_empty() {
        return Ok(EMPTY_TREE);
    }
    let mut entries = Vec::new();
    for key in extras {
        let prefix: &[u8] = match key.kind {
            ObjectKind::Blob => b"100644",
            ObjectKind::Tree => b"40000",
            ObjectKind::Commit => continue,
        };
        let mut name = match key.kind {
            ObjectKind::Blob => b"b/".to_vec(),
            ObjectKind::Tree => b"t/".to_vec(),
            ObjectKind::Commit => continue,
        };
        name.extend_from_slice(encode_lower_hex(&key.id.0).as_bytes());
        entries.push((name, prefix, key.id));
    }
    entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut bytes = Vec::new();
    for (name, mode, id) in entries {
        bytes.extend_from_slice(mode);
        bytes.push(b' ');
        bytes.extend_from_slice(&name);
        bytes.push(0);
        bytes.extend_from_slice(&id.0);
    }
    write_object(repository, ObjectKind::Tree, &bytes)
}

fn git_reachable_keys(
    repository: &MachineGitRepository,
    heads: &BTreeSet<Oid>,
) -> Result<BTreeSet<ObjectKey>, CanonicalRemoteError> {
    let mut pending = heads
        .iter()
        .copied()
        .map(|id| ObjectKey {
            kind: ObjectKind::Commit,
            id,
        })
        .collect::<BTreeSet<_>>();
    let mut visited = BTreeSet::new();
    while let Some(key) = pending.pop_first() {
        if !visited.insert(key) {
            continue;
        }
        let bytes = repository
            .read_object(key)
            .map_err(|error| CanonicalRemoteError::WriteObject(error.to_string()))?;
        match key.kind {
            ObjectKind::Blob => {}
            ObjectKind::Tree => {
                let tree = parse_tree(&bytes)
                    .map_err(|error| CanonicalRemoteError::WriteObject(error.to_string()))?;
                for entry in tree.entries {
                    let kind = match entry.kind {
                        devspace_kernel::TreeEntryKind::Tree => ObjectKind::Tree,
                        devspace_kernel::TreeEntryKind::Gitlink => continue,
                        _ => ObjectKind::Blob,
                    };
                    pending.insert(ObjectKey { kind, id: entry.oid });
                }
            }
            ObjectKind::Commit => {
                let commit = parse_commit(&bytes)
                    .map_err(|error| CanonicalRemoteError::WriteObject(error.to_string()))?;
                pending.insert(ObjectKey {
                    kind: ObjectKind::Tree,
                    id: commit.tree,
                });
                for parent in commit.parents {
                    pending.insert(ObjectKey {
                        kind: ObjectKind::Commit,
                        id: parent,
                    });
                }
            }
        }
    }
    Ok(visited)
}

fn write_retention_commit(
    repository: &MachineGitRepository,
    tree: Oid,
    parents: BTreeSet<Oid>,
) -> Result<Oid, CanonicalRemoteError> {
    let mut bytes = format!("tree {}\n", encode_lower_hex(&tree.0)).into_bytes();
    for parent in &parents {
        bytes.extend_from_slice(format!("parent {}\n", encode_lower_hex(&parent.0)).as_bytes());
    }
    bytes.extend_from_slice(format!("author {RETENTION_AUTHOR} 0 +0000\n").as_bytes());
    bytes.extend_from_slice(format!("committer {RETENTION_AUTHOR} 0 +0000\n").as_bytes());
    bytes.push(b'\n');
    write_object(repository, ObjectKind::Commit, &bytes)
}

fn write_object(
    repository: &MachineGitRepository,
    kind: ObjectKind,
    bytes: &[u8],
) -> Result<Oid, CanonicalRemoteError> {
    let git = repository.git_repo();
    let actual = git
        .objects
        .write_buf(to_git_kind(kind), bytes)
        .map_err(|source| CanonicalRemoteError::WriteObject(source.to_string()))?;
    Oid::from_bytes(actual.as_bytes()).ok_or_else(|| {
        CanonicalRemoteError::WriteObject("Git object ID is not 20 bytes".to_owned())
    })
}

fn object_exists(repository: &MachineGitRepository, id: Oid) -> bool {
    let git_id = gix::ObjectId::from_bytes_or_panic(&id.0);
    repository.git_repo().try_find_object(git_id).ok().flatten().is_some()
}

fn require_object(
    repository: &MachineGitRepository,
    kind: ObjectKind,
    id: Oid,
) -> Result<Vec<u8>, CanonicalRemoteError> {
    let key = ObjectKey { kind, id };
    repository
        .read_object(key)
        .map_err(|_| CanonicalRemoteError::MissingAfterFetch(encode_lower_hex(&id.0)))
}

fn map_push_error(error: PushError) -> CanonicalRemoteError {
    CanonicalRemoteError::PushRejected(error)
}

fn authenticated_git_url(url: &str, token: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let rest = rest.split_once('@').map_or(rest, |(_, host)| host);
        return format!("{scheme}://x:{}@{rest}", encode_userinfo(token));
    }
    url.to_owned()
}

fn encode_userinfo(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub fn init_bare_remote(path: impl AsRef<Path>) -> Result<RemoteUrl, CanonicalRemoteError> {
    let path = path.as_ref();
    std::fs::create_dir_all(path).map_err(|source| CanonicalRemoteError::WriteObject(source.to_string()))?;
    let status = std::process::Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(path)
        .status()
        .map_err(|source| CanonicalRemoteError::WriteObject(source.to_string()))?;
    if !status.success() {
        return Err(CanonicalRemoteError::WriteObject(format!(
            "git init --bare failed with {status}"
        )));
    }
    Ok(RemoteUrl::new(path.to_string_lossy().into_owned()))
}

pub fn gc_bare_remote(path: impl AsRef<Path>) -> Result<(), CanonicalRemoteError> {
    let status = std::process::Command::new("git")
        .args(["-C"])
        .arg(path.as_ref())
        .args(["gc", "--prune=now", "--quiet"])
        .status()
        .map_err(|source| CanonicalRemoteError::WriteObject(source.to_string()))?;
    if !status.success() {
        return Err(CanonicalRemoteError::WriteObject(format!(
            "git gc failed with {status}"
        )));
    }
    Ok(())
}

pub fn delete_remote_ref(
    repository: &MachineGitRepository,
    url: &RemoteUrl,
    bookmark: &str,
    environment: &GitProcessEnvironment,
) -> Result<(), CanonicalRemoteError> {
    let reference = retention_ref(bookmark)?;
    let previous = remote_oid(url, reference.as_str(), environment)?;
    let Some(previous) = previous else {
        return Ok(());
    };
    push(
        repository.git_repo_path(),
        url,
        &BTreeMap::from([(
            reference,
            LeaseUpdate {
                expected_old_oid: Some(previous),
                new_oid: None,
            },
        )]),
        environment,
    )
    .map_err(map_push_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticated_url_inserts_token_and_strips_existing_userinfo() {
        let url = authenticated_git_url(
            "https://old:secret@example.invalid/git/ns/repo.git",
            "art_v1_token?expires=1760000000",
        );
        assert_eq!(
            url,
            "https://x:art_v1_token%3Fexpires%3D1760000000@example.invalid/git/ns/repo.git"
        );
        assert_eq!(format!("{:?}", RemoteUrl::new(url)), "<remote-url>");
    }
}
