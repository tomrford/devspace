//! Best-effort, read-only Git administrative view for local checkout readers.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use blake2::{Blake2b512, Digest as _};
use devspace_machine::encode_lower_hex;
use futures::StreamExt as _;
use gix::bstr::{BStr, BString};
use jj_lib::backend::TreeValue;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _, StoreFactories};
use jj_lib::repo_path::RepoPath;
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;

const AFTER_HEAD_FAILPOINT: &str = "git_shim_after_head";
const DUMMY_CONFLICT_FILE: &str = ".jj-do-not-resolve-this-conflict";
const LOCK_FILE: &str = "devspace-git-shim.lock";
const STATE_FILE: &str = "devspace-git-shim.state";
const STATE_VERSION: &str = "git-shim-v2";
const UNBORN_ROOT_REF: &str = "refs/jj/root";

pub fn ensure(checkout_root: &Path, settings: &UserSettings) {
    if let Err(error) = ensure_inner(checkout_root, settings) {
        crate::boundary_sync::warn(&format!(
            "git shim refresh failed in {}: {error}",
            checkout_root.display()
        ));
    }
}

pub struct RemovalGuard {
    _lock: Option<CheckoutLock>,
}

pub fn remove_guard(checkout_root: &Path) -> RemovalGuard {
    match CheckoutLock::acquire(checkout_root).and_then(|lock| {
        make_git_dirs_writable(checkout_root)?;
        Ok(lock)
    }) {
        Ok(lock) => RemovalGuard { _lock: Some(lock) },
        Err(error) => {
            crate::boundary_sync::warn(&format!(
                "git shim unlock failed in {}: {error}",
                checkout_root.display()
            ));
            RemovalGuard { _lock: None }
        }
    }
}

fn ensure_inner(checkout_root: &Path, settings: &UserSettings) -> Result<(), String> {
    let _checkout_lock = CheckoutLock::acquire(checkout_root)?;
    let git_dir = checkout_root.join(".git");
    if git_dir.exists() && !git_dir.is_dir() {
        return Err(format!(
            "{} exists but is not a directory",
            git_dir.display()
        ));
    }

    // An interrupted process can leave the guard relaxed. Repair it before
    // resolving refresh inputs so errors cannot extend that window.
    make_git_dirs_read_only(checkout_root)?;

    let view = futures::executor::block_on(load_view(checkout_root, settings))?;
    let refresh_state = view.refresh_state();
    let previous_state = read_refresh_state(checkout_root);
    if shim_files_exist(&git_dir) && previous_state.as_deref() == Some(refresh_state.as_str()) {
        return Ok(());
    }

    let guard = GitDirGuard::acquire(checkout_root)?;
    let refresh = (|| {
        invalidate_refresh_state(checkout_root)?;
        if !previous_state.is_some_and(|state| state.starts_with(STATE_VERSION)) && git_dir.exists()
        {
            fs::remove_dir_all(&git_dir)
                .map_err(|error| format!("replace obsolete {}: {error}", git_dir.display()))?;
        }
        initialize_minimal_git(checkout_root, &view.canonical_objects)?;
        remove_stale_index_lock(&git_dir)?;
        write_head(&git_dir, view.head_oid.as_deref())?;
        if crate::failpoint::failpoint_enabled(AFTER_HEAD_FAILPOINT) {
            return Err(format!("injected failure at {AFTER_HEAD_FAILPOINT}"));
        }

        let git_repo =
            gix::open(checkout_root).map_err(|error| format!("open Git shim: {error}"))?;
        let mut index = futures::executor::block_on(build_index(
            view.repo.as_ref(),
            &git_repo,
            &view.parent_tree,
            &view.working_copy_tree,
        ))?;
        preserve_stat_data(&git_repo, &mut index)?;
        index
            .write(gix::index::write::Options::default())
            .map_err(|error| format!("write Git shim index: {error}"))?;
        write_refresh_state(checkout_root, &refresh_state)
    })();
    finish_guard(refresh, guard)
}

struct ShimView {
    canonical_objects: PathBuf,
    repo: std::sync::Arc<ReadonlyRepo>,
    head_oid: Option<String>,
    parent_tree: MergedTree,
    working_copy_tree: MergedTree,
}

impl ShimView {
    fn refresh_state(&self) -> String {
        let mut hasher = Blake2b512::new();
        if let Some(head_oid) = &self.head_oid {
            hasher.update(head_oid.as_bytes());
        }
        for tree in [&self.parent_tree, &self.working_copy_tree] {
            for id in tree.tree_ids() {
                hasher.update((id.as_bytes().len() as u64).to_le_bytes());
                hasher.update(id.as_bytes());
            }
        }
        format!(
            "{STATE_VERSION}\n{}\n",
            encode_lower_hex(&hasher.finalize())
        )
    }
}

async fn load_view(checkout_root: &Path, settings: &UserSettings) -> Result<ShimView, String> {
    let workspace = Workspace::load(
        settings,
        checkout_root,
        &StoreFactories::default(),
        &crate::working_copy::devspace_working_copy_factories(),
    )
    .map_err(|error| format!("load checkout metadata: {error}"))?;
    let backend = jj_lib::git::get_git_backend(workspace.repo_loader().store())
        .map_err(|error| format!("load canonical Git backend: {error}"))?;
    let canonical_objects = backend.git_repo_path().join("objects");

    let operation = workspace
        .repo_loader()
        .load_operation(workspace.working_copy().operation_id())
        .await
        .map_err(|error| format!("load checkout operation: {error}"))?;
    let repo = workspace
        .repo_loader()
        .load_at(&operation)
        .await
        .map_err(|error| format!("load checkout repository: {error}"))?;
    let working_copy_id = repo
        .view()
        .get_wc_commit_id(workspace.workspace_name())
        .ok_or_else(|| "checkout has no working-copy commit".to_owned())?;
    let working_copy = repo
        .store()
        .get_commit_async(working_copy_id)
        .await
        .map_err(|error| format!("load working-copy commit: {error}"))?;
    let first_parent = &working_copy.parent_ids()[0];
    let head_oid = (first_parent != repo.store().root_commit_id()).then(|| first_parent.hex());
    let parent_tree = working_copy
        .parent_tree(repo.as_ref())
        .await
        .map_err(|error| format!("merge working-copy parent trees: {error}"))?;
    let working_copy_tree = working_copy.tree();

    Ok(ShimView {
        canonical_objects,
        repo,
        head_oid,
        parent_tree,
        working_copy_tree,
    })
}

fn initialize_minimal_git(checkout_root: &Path, canonical_objects: &Path) -> Result<(), String> {
    let git_dir = checkout_root.join(".git");
    if !git_dir.exists() {
        gix::init(checkout_root).map_err(|error| format!("initialize Git shim: {error}"))?;
        for path in [
            git_dir.join("hooks"),
            git_dir.join("info"),
            git_dir.join("description"),
        ] {
            remove_generated_path(&path)?;
        }
    }

    let alternates_dir = git_dir.join("objects").join("info");
    fs::create_dir_all(&alternates_dir)
        .map_err(|error| format!("create {}: {error}", alternates_dir.display()))?;
    fs::write(
        alternates_dir.join("alternates"),
        format!("{}\n", canonical_objects.display()),
    )
    .map_err(|error| format!("configure canonical Git object lookup: {error}"))
}

fn remove_generated_path(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path)
            .map_err(|error| format!("remove generated {}: {error}", path.display())),
        Ok(_) => fs::remove_file(path)
            .map_err(|error| format!("remove generated {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect generated {}: {error}", path.display())),
    }
}

fn write_head(git_dir: &Path, oid: Option<&str>) -> Result<(), String> {
    let value = match oid {
        Some(oid) => format!("{oid}\n"),
        None => format!("ref: {UNBORN_ROOT_REF}\n"),
    };
    fs::write(git_dir.join("HEAD"), value).map_err(|error| format!("write Git shim HEAD: {error}"))
}

async fn build_index(
    repo: &ReadonlyRepo,
    git_repo: &gix::Repository,
    parent_tree: &MergedTree,
    working_copy_tree: &MergedTree,
) -> Result<gix::index::File, String> {
    // Mirrors jj-lib 0.42's reset_index(), build_index_from_merged_tree(),
    // and update_intent_to_add_impl() without mutating jj's repository view.
    let mut index = if let Some(tree_id) = parent_tree.tree_ids().as_resolved() {
        if tree_id == repo.store().empty_tree_id() {
            empty_index(git_repo)
        } else {
            git_repo
                .index_from_tree(&gix::ObjectId::from_bytes_or_panic(tree_id.as_bytes()))
                .map_err(|error| format!("build Git index from parent tree: {error}"))?
        }
    } else {
        build_index_from_merged_tree(git_repo, parent_tree)?
    };
    update_intent_to_add(git_repo, &mut index, parent_tree, working_copy_tree).await?;
    Ok(index)
}

fn empty_index(git_repo: &gix::Repository) -> gix::index::File {
    gix::index::File::from_state(
        gix::index::State::new(git_repo.object_hash()),
        git_repo.index_path(),
    )
}

fn build_index_from_merged_tree(
    git_repo: &gix::Repository,
    merged_tree: &MergedTree,
) -> Result<gix::index::File, String> {
    let mut index = empty_index(git_repo);
    let mut push = |path: &RepoPath, value: &Option<TreeValue>, stage: gix::index::entry::Stage| {
        let Some(value) = value else {
            return;
        };
        let (id, mode) = match value {
            TreeValue::File { id, executable, .. } => (
                id.as_bytes(),
                if *executable {
                    gix::index::entry::Mode::FILE_EXECUTABLE
                } else {
                    gix::index::entry::Mode::FILE
                },
            ),
            TreeValue::Symlink(id) => (id.as_bytes(), gix::index::entry::Mode::SYMLINK),
            TreeValue::Tree(_) => return,
            TreeValue::GitSubmodule(id) => (id.as_bytes(), gix::index::entry::Mode::COMMIT),
        };
        index.dangerously_push_entry(
            gix::index::entry::Stat::default(),
            gix::ObjectId::from_bytes_or_panic(id),
            gix::index::entry::Flags::from_stage(stage),
            mode,
            BStr::new(path.as_internal_file_string()),
        );
    };

    let mut has_many_sided_conflict = false;
    for (path, value) in merged_tree.entries() {
        let value = value.map_err(|error| format!("read merged parent tree: {error}"))?;
        if let Some(resolved) = value.as_resolved() {
            push(&path, resolved, gix::index::entry::Stage::Unconflicted);
            continue;
        }

        let conflict = value.simplify();
        if let [left, base, right] = conflict.as_slice() {
            push(&path, left, gix::index::entry::Stage::Ours);
            push(&path, base, gix::index::entry::Stage::Base);
            push(&path, right, gix::index::entry::Stage::Theirs);
        } else {
            has_many_sided_conflict = true;
            push(
                &path,
                conflict.first(),
                gix::index::entry::Stage::Unconflicted,
            );
        }
    }
    index.sort_entries();

    if has_many_sided_conflict
        && index
            .entry_index_by_path(DUMMY_CONFLICT_FILE.into())
            .is_err()
    {
        let blob = git_repo
            .write_blob(
                b"The working copy commit contains conflicts which cannot be resolved using Git.\n",
            )
            .map_err(|error| format!("write Git shim conflict guard: {error}"))?;
        index.dangerously_push_entry(
            gix::index::entry::Stat::default(),
            blob.detach(),
            gix::index::entry::Flags::from_stage(gix::index::entry::Stage::Ours),
            gix::index::entry::Mode::FILE,
            DUMMY_CONFLICT_FILE.into(),
        );
        index.sort_entries();
    }
    Ok(index)
}

async fn update_intent_to_add(
    git_repo: &gix::Repository,
    index: &mut gix::index::File,
    parent_tree: &MergedTree,
    working_copy_tree: &MergedTree,
) -> Result<(), String> {
    let mut diff = parent_tree.diff_stream(working_copy_tree, &EverythingMatcher);
    let mut added = Vec::new();
    while let Some(entry) = diff.next().await {
        let values = entry
            .values
            .map_err(|error| format!("diff working-copy tree: {error}"))?;
        if !values.before.is_absent() {
            continue;
        }
        let executable = match values.after.as_normal() {
            Some(TreeValue::File { executable, .. }) => *executable,
            Some(TreeValue::Symlink(_)) => false,
            _ => continue,
        };
        if index
            .entry_index_by_path(BStr::new(entry.path.as_internal_file_string()))
            .is_err()
        {
            added.push((BString::from(entry.path.into_internal_string()), executable));
        }
    }

    if added.is_empty() {
        return Ok(());
    }
    let empty_blob = git_repo
        .write_blob(b"")
        .map_err(|error| format!("write Git shim intent-to-add blob: {error}"))?
        .detach();
    for (path, executable) in added {
        index.dangerously_push_entry(
            gix::index::entry::Stat::default(),
            empty_blob,
            gix::index::entry::Flags::INTENT_TO_ADD | gix::index::entry::Flags::EXTENDED,
            if executable {
                gix::index::entry::Mode::FILE_EXECUTABLE
            } else {
                gix::index::entry::Mode::FILE
            },
            path.as_ref(),
        );
    }
    index.sort_entries();
    Ok(())
}

fn preserve_stat_data(
    git_repo: &gix::Repository,
    index: &mut gix::index::File,
) -> Result<(), String> {
    let old_index = match git_repo.open_index() {
        Ok(index) => index,
        Err(gix::worktree::open_index::Error::IndexFile(gix::index::file::init::Error::Io(
            error,
        ))) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read previous Git shim index: {error}")),
    };
    for (entry, path) in index.entries_mut_with_paths() {
        let Some(old_entry) = old_index.entry_by_path_and_stage(path, entry.stage()) else {
            continue;
        };
        if entry.id == old_entry.id && entry.mode == old_entry.mode {
            entry.stat = old_entry.stat;
        }
    }
    Ok(())
}

fn shim_files_exist(git_dir: &Path) -> bool {
    [
        git_dir.join("HEAD"),
        git_dir.join("config"),
        git_dir.join("index"),
        git_dir.join("objects/info/alternates"),
    ]
    .iter()
    .all(|path| path.is_file())
}

fn finish_guard(result: Result<(), String>, guard: GitDirGuard<'_>) -> Result<(), String> {
    match (result, guard.finish()) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(guard_error)) => Err(format!(
            "{error}; also failed to restore Git guard: {guard_error}"
        )),
    }
}

fn read_refresh_state(checkout_root: &Path) -> Option<String> {
    fs::read_to_string(checkout_root.join(".jj").join(STATE_FILE)).ok()
}

fn write_refresh_state(checkout_root: &Path, state: &str) -> Result<(), String> {
    let path = checkout_root.join(".jj").join(STATE_FILE);
    fs::write(&path, state).map_err(|error| format!("write {}: {error}", path.display()))
}

fn invalidate_refresh_state(checkout_root: &Path) -> Result<(), String> {
    let path = checkout_root.join(".jj").join(STATE_FILE);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {}: {error}", path.display())),
    }
}

fn remove_stale_index_lock(git_dir: &Path) -> Result<(), String> {
    let path = git_dir.join("index.lock");
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove stale {}: {error}", path.display())),
    }
}

fn make_git_dirs_writable(checkout_root: &Path) -> Result<(), String> {
    update_git_dir_modes(checkout_root, |mode| mode | 0o700)
}

fn make_git_dirs_read_only(checkout_root: &Path) -> Result<(), String> {
    update_git_dir_modes(checkout_root, |mode| mode & !0o222)
}

fn update_git_dir_modes(checkout_root: &Path, f: impl Fn(u32) -> u32 + Copy) -> Result<(), String> {
    let git_dir = checkout_root.join(".git");
    if !git_dir.exists() {
        return Ok(());
    }
    crate::tree_modes::rewrite(&git_dir, |is_dir, mode| is_dir.then(|| f(mode)))
        .map_err(|error| format!("update modes under {}: {error}", git_dir.display()))
}

struct CheckoutLock {
    file: fs::File,
}

impl CheckoutLock {
    fn acquire(checkout_root: &Path) -> Result<Self, String> {
        let path = checkout_root.join(".jj").join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        file.lock()
            .map_err(|error| format!("lock {}: {error}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for CheckoutLock {
    fn drop(&mut self) {
        self.file.unlock().ok();
    }
}

struct GitDirGuard<'a> {
    checkout_root: &'a Path,
    active: bool,
}

impl<'a> GitDirGuard<'a> {
    fn acquire(checkout_root: &'a Path) -> Result<Self, String> {
        let guard = Self {
            checkout_root,
            active: true,
        };
        make_git_dirs_writable(checkout_root)?;
        Ok(guard)
    }

    fn finish(mut self) -> Result<(), String> {
        make_git_dirs_read_only(self.checkout_root)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for GitDirGuard<'_> {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = make_git_dirs_read_only(self.checkout_root)
        {
            crate::boundary_sync::warn(&format!(
                "failed to restore Git shim guard in {}: {error}",
                self.checkout_root.display()
            ));
        }
    }
}
