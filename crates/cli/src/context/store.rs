//! Shared cache of remote clones and read-only snapshots, plus the gc-root
//! registry that lets `gc` find every project lockfile that still pins a
//! snapshot.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::ensure_dir_mode;
use super::git::Git;
use super::lock::FileLock;
use super::manifest::{LockEntry, Lockfile};
use anyhow::{Context as _, Result, bail};
use blake2::{Blake2b512, Digest as _};
use devspace_machine::encode_lower_hex;

pub struct Store {
    cache_root: PathBuf,
    state_root: PathBuf,
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub removed_snapshots: Vec<PathBuf>,
    pub removed_remotes: Vec<PathBuf>,
    pub removed_roots: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

impl Store {
    pub fn new(cache_root: PathBuf, state_root: PathBuf) -> Self {
        Self {
            cache_root,
            state_root,
        }
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.cache_root.join("snapshots")
    }

    fn remotes_dir(&self) -> PathBuf {
        self.cache_root.join("remotes")
    }

    fn roots_dir(&self) -> PathBuf {
        self.state_root.join("roots")
    }

    fn locks_dir(&self) -> PathBuf {
        self.state_root.join("locks")
    }

    pub fn prepare(&self) -> Result<()> {
        ensure_dir_mode(&self.cache_root, 0o700)?;
        ensure_dir_mode(&self.state_root, 0o700)?;
        ensure_dir_mode(&self.snapshots_dir(), 0o700)?;
        ensure_dir_mode(&self.remotes_dir(), 0o700)?;
        ensure_dir_mode(&self.roots_dir(), 0o700)?;
        ensure_dir_mode(&self.locks_dir(), 0o700)
    }

    pub fn lock_mutation(&self) -> Result<FileLock> {
        let path = self.locks_dir().join("store.lock");
        FileLock::try_acquire_or_err(&path, || {
            format!("context store is busy: {}", self.state_root.display())
        })
    }

    /// Serialize mutations of one project's `.repos/`, keyed like the gc
    /// roots. Keeping the lock file here instead of inside the project means
    /// checkouts never carry lock artifacts; the cost is that Grepo, which
    /// locks `.repos/.mutate.lock`, no longer excludes against `ds context`.
    pub fn lock_project_mutation(&self, project_dir: &Path) -> Result<FileLock> {
        let canonical = project_dir
            .canonicalize()
            .with_context(|| format!("canonicalize {}", project_dir.display()))?;
        let key = cache_key(&canonical.display().to_string());
        let path = self.locks_dir().join(format!("{key}.mutate.lock"));
        FileLock::try_acquire_or_err(&path, || {
            format!(
                "another mutation is in progress in {}",
                project_dir.display()
            )
        })
    }

    pub fn with_remote_cache<T>(
        &self,
        git: &Git,
        url: &str,
        f: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let remote_key = cache_key(url);
        self.with_remote_cache_for_key(git, url, &remote_key, f)
    }

    /// Where the snapshot for this (url, commit, subdir) lives, materialized
    /// or not.
    pub fn snapshot_path(&self, url: &str, commit: &str, subdir: Option<&str>) -> PathBuf {
        self.snapshot_dir_for_keys(&cache_key(url), &snapshot_key(url, commit, subdir))
    }

    pub fn ensure_snapshot_for_commit(
        &self,
        git: &Git,
        url: &str,
        commit: &str,
        subdir: Option<&str>,
    ) -> Result<PathBuf> {
        let remote_key = cache_key(url);
        let snapshot_key = snapshot_key(url, commit, subdir);
        let snapshot_dir = self.snapshot_dir_for_keys(&remote_key, &snapshot_key);
        self.with_remote_cache_for_key(git, url, &remote_key, |remote_dir| {
            if snapshot_dir.exists() {
                return Ok(snapshot_dir.clone());
            }
            git.ensure_commit_available(remote_dir, commit)?;
            git.materialize_snapshot(remote_dir, commit, &snapshot_dir, subdir)?;
            make_read_only(&snapshot_dir)?;
            Ok(snapshot_dir)
        })
    }

    /// Register `lock_path` as a gc root (a symlink back to the lockfile).
    pub fn refresh_root(&self, lock_path: &Path) -> Result<PathBuf> {
        let canonical = lock_path
            .canonicalize()
            .with_context(|| format!("canonicalize {}", lock_path.display()))?;
        let root_key = cache_key(&canonical.display().to_string());
        let root_link = self.roots_dir().join(format!("{root_key}.lock"));
        if root_link.exists() {
            fs::remove_file(&root_link)
                .with_context(|| format!("remove {}", root_link.display()))?;
        }
        symlink(&canonical, &root_link).with_context(|| {
            format!(
                "create symlink {} -> {}",
                root_link.display(),
                canonical.display()
            )
        })?;
        Ok(root_link)
    }

    /// Sweep snapshots and remote caches unreachable from any rooted
    /// lockfile. Foreign entries hold nothing in this store, so they
    /// contribute no reachability.
    pub fn gc(&self) -> Result<GcReport> {
        let mut report = GcReport::default();
        let mut reachable_snapshots = BTreeSet::new();
        let mut reachable_remotes = BTreeSet::new();

        for entry in read_dir_paths(&self.roots_dir())? {
            let metadata = fs::symlink_metadata(&entry)
                .with_context(|| format!("inspect {}", entry.display()))?;
            if !metadata.file_type().is_symlink() {
                continue;
            }
            let Ok(lock_path) = fs::canonicalize(&entry) else {
                fs::remove_file(&entry).with_context(|| format!("remove {}", entry.display()))?;
                report.removed_roots.push(entry);
                continue;
            };
            let lockfile = match Lockfile::load(&lock_path) {
                Ok(lockfile) => lockfile,
                Err(error) => {
                    report.warnings.push(format!(
                        "skipped rooted lockfile {}: {error:#}",
                        lock_path.display()
                    ));
                    continue;
                }
            };
            for entry in lockfile.entries() {
                let LockEntry::Git(git_entry) = entry else {
                    continue;
                };
                let Some(commit) = &git_entry.commit else {
                    continue;
                };
                let remote_key = cache_key(&git_entry.url);
                let snapshot_key =
                    snapshot_key(&git_entry.url, commit, git_entry.subdir.as_deref());
                reachable_snapshots.insert(self.snapshot_dir_for_keys(&remote_key, &snapshot_key));
                reachable_remotes.insert(self.remote_dir_for_key(&remote_key));
            }
        }

        for url_dir in read_dir_paths(&self.snapshots_dir())? {
            if !url_dir.is_dir() {
                continue;
            }
            let mut has_remaining_entries = false;
            for snapshot_dir in read_dir_paths(&url_dir)? {
                if !snapshot_dir.is_dir() || reachable_snapshots.contains(&snapshot_dir) {
                    has_remaining_entries = true;
                    continue;
                }
                make_writable(&snapshot_dir)?;
                fs::remove_dir_all(&snapshot_dir)
                    .with_context(|| format!("remove {}", snapshot_dir.display()))?;
                report.removed_snapshots.push(snapshot_dir);
            }
            if !has_remaining_entries {
                fs::remove_dir(&url_dir)
                    .with_context(|| format!("remove {}", url_dir.display()))?;
            }
        }

        for remote_dir in read_dir_paths(&self.remotes_dir())? {
            if !remote_dir.is_dir() || reachable_remotes.contains(&remote_dir) {
                continue;
            }
            fs::remove_dir_all(&remote_dir)
                .with_context(|| format!("remove {}", remote_dir.display()))?;
            report.removed_remotes.push(remote_dir);
        }

        Ok(report)
    }

    fn remote_dir_for_key(&self, remote_key: &str) -> PathBuf {
        self.remotes_dir().join(format!("{remote_key}.git"))
    }

    fn snapshot_dir_for_keys(&self, remote_key: &str, snapshot_key: &str) -> PathBuf {
        self.snapshots_dir().join(remote_key).join(snapshot_key)
    }

    fn with_remote_cache_for_key<T>(
        &self,
        git: &Git,
        url: &str,
        remote_key: &str,
        f: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let _lock = FileLock::acquire(&self.locks_dir().join(format!("remote-{remote_key}.lock")))?;
        let remote_dir = self.remote_dir_for_key(remote_key);
        git.ensure_remote_cache(&remote_dir, url)?;
        f(&remote_dir)
    }
}

/// Names a cache directory after its input. Changing this changes where every
/// existing snapshot and remote cache lives.
fn cache_key(value: &str) -> String {
    encode_lower_hex(&Blake2b512::digest(value.as_bytes()))
}

fn snapshot_key(url: &str, commit: &str, subdir: Option<&str>) -> String {
    cache_key(&format!("{url}\n{commit}\n{}", subdir.unwrap_or("")))
}

pub fn replace_symlink(link_path: &Path, target: &Path) -> Result<()> {
    if let Some(metadata) = symlink_metadata_if_exists(link_path)? {
        if !metadata.file_type().is_symlink() {
            bail!(
                "{} exists and is not a devspace-managed symlink",
                link_path.display()
            );
        }
        fs::remove_file(link_path).with_context(|| format!("remove {}", link_path.display()))?;
    }
    let parent = link_path
        .parent()
        .with_context(|| format!("link path has no parent: {}", link_path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    symlink(target, link_path).with_context(|| {
        format!(
            "create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })
}

pub fn remove_managed_symlink(path: &Path) -> Result<()> {
    let Some(metadata) = symlink_metadata_if_exists(path)? else {
        return Ok(());
    };
    if !metadata.file_type().is_symlink() {
        bail!(
            "{} exists and is not a devspace-managed symlink",
            path.display()
        );
    }
    fs::remove_file(path).with_context(|| format!("remove {}", path.display()))
}

fn make_read_only(root: &Path) -> Result<()> {
    crate::tree_modes::rewrite(root, |is_dir, mode| {
        Some(if is_dir || mode & 0o100 != 0 {
            0o500
        } else {
            0o400
        })
    })
    .with_context(|| format!("make {} read-only", root.display()))
}

fn make_writable(root: &Path) -> Result<()> {
    crate::tree_modes::rewrite(root, |is_dir, mode| {
        Some(if is_dir || mode & 0o100 != 0 {
            0o700
        } else {
            0o600
        })
    })
    .with_context(|| format!("make {} writable", root.display()))
}

pub fn read_dir_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        entries.push(
            entry
                .with_context(|| format!("read {}", path.display()))?
                .path(),
        );
    }
    entries.sort();
    Ok(entries)
}

pub fn symlink_metadata_if_exists(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("inspect {}", path.display())),
    }
}
