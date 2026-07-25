use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;

use devspace_machine::{CatalogEntry, MachineConfigError, MachineStore, RepositoryName};
use jj_cli::command_error::{CommandError, user_error};
use jj_lib::settings::UserSettings;

const BOUNDARY_SYNC_ENV: &str = "DEVSPACE_BOUNDARY_SYNC";
const DAEMON_ENV: &str = "DEVSPACE_DAEMON";

const DAEMON_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(5),
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(40),
];

#[derive(Default)]
struct BoundarySyncState {
    suppressed: bool,
    repository_sync_suppressed: bool,
    repositories: BTreeMap<RepositoryName, PathBuf>,
    checkouts: BTreeSet<PathBuf>,
    moved_checkouts: BTreeMap<PathBuf, Vec<crate::context::SyncMessage>>,
    context_auto_sync: bool,
    git_shim: Option<(bool, UserSettings)>,
}

pub(crate) fn configure_checkout_hooks(settings: &UserSettings) -> Result<(), CommandError> {
    let store = MachineStore::platform_default().map_err(|error| user_error(error.to_string()))?;
    let (git_shim, context_auto_sync) = match store.load_config() {
        Ok(config) => (config.git_shim(), config.context_auto_sync()),
        Err(MachineConfigError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            (false, false)
        }
        Err(error) => return Err(user_error(error.to_string())),
    };
    let mut state = state();
    state.git_shim = Some((git_shim, settings.clone()));
    state.context_auto_sync = context_auto_sync;
    Ok(())
}

pub(crate) fn record_checkout(path: &Path) {
    state().checkouts.insert(path.to_owned());
}

pub(crate) fn context_auto_sync_enabled() -> bool {
    state().context_auto_sync
}

pub(crate) fn record_checkout_movement(
    path: &Path,
    clear_messages: Vec<crate::context::SyncMessage>,
) {
    let mut state = state();
    state.checkouts.insert(path.to_owned());
    state
        .moved_checkouts
        .entry(path.to_owned())
        .or_default()
        .extend(clear_messages);
}

pub(crate) fn relocate_checkout(from: &Path, to: &Path) {
    let mut state = state();
    if state.checkouts.remove(from) {
        state.checkouts.insert(to.to_owned());
    }
    if let Some(clear_messages) = state.moved_checkouts.remove(from) {
        state
            .moved_checkouts
            .entry(to.to_owned())
            .or_default()
            .extend(clear_messages);
    }
}

/// Poison is recovered: a panicked command must not disable the checkout
/// hooks for the rest of the process.
fn state() -> MutexGuard<'static, BoundarySyncState> {
    static STATE: OnceLock<Mutex<BoundarySyncState>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(BoundarySyncState::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn record(entry: &CatalogEntry) {
    let Some(repository_directory) = entry.native_repository_path.parent() else {
        return;
    };
    state()
        .repositories
        .insert(entry.name.clone(), repository_directory.to_owned());
}

pub(crate) fn record_repository_path(path: &Path) {
    let Ok(store) = MachineStore::platform_default() else {
        return;
    };
    let Ok(entries) = store.entries() else {
        return;
    };
    let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    if let Some(entry) = entries.into_iter().find(|entry| {
        dunce::canonicalize(&entry.native_repository_path)
            .unwrap_or_else(|_| entry.native_repository_path.clone())
            == path
    }) {
        record(&entry);
    }
}

pub(crate) fn suppress() {
    let mut state = state();
    state.suppressed = true;
    state.repositories.clear();
    state.checkouts.clear();
    state.moved_checkouts.clear();
}

pub(crate) fn suppress_repository_sync() {
    let mut state = state();
    state.repository_sync_suppressed = true;
    state.repositories.clear();
}

pub(crate) fn spawn_recorded(command_succeeded: bool) {
    let (
        suppressed,
        repository_sync_suppressed,
        repositories,
        checkouts,
        moved_checkouts,
        context_auto_sync,
        git_shim,
    ) = {
        let mut state = state();
        (
            state.suppressed,
            state.repository_sync_suppressed,
            std::mem::take(&mut state.repositories),
            std::mem::take(&mut state.checkouts),
            std::mem::take(&mut state.moved_checkouts),
            state.context_auto_sync,
            state.git_shim.take(),
        )
    };
    if suppressed {
        return;
    }
    if context_auto_sync {
        for (checkout, clear_messages) in moved_checkouts {
            auto_sync_context(&checkout, clear_messages);
        }
    }
    if !command_succeeded {
        return;
    }
    if let Some((true, settings)) = git_shim {
        for checkout in checkouts {
            crate::git_shim::ensure(&checkout, &settings);
        }
    }
    if repository_sync_suppressed
        || std::env::var_os(BOUNDARY_SYNC_ENV).is_some_and(|value| value == "0")
    {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let daemon_enabled = !std::env::var_os(DAEMON_ENV).is_some_and(|value| value == "0");
    let store = daemon_enabled
        .then(MachineStore::platform_default)
        .and_then(Result::ok);
    for (name, repository_directory) in repositories {
        if let Some(store) = &store {
            if crate::daemon::notify_sync(store, &name) {
                continue;
            }
            spawn_daemon(&executable, store.root());
            if DAEMON_RETRY_DELAYS.iter().any(|delay| {
                thread::sleep(*delay);
                crate::daemon::notify_sync(store, &name)
            }) {
                continue;
            }
        }
        spawn_one_shot(&executable, &name, &repository_directory);
    }
}

fn auto_sync_context(checkout_root: &Path, clear_messages: Vec<crate::context::SyncMessage>) {
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    for message in clear_messages {
        write_context_message(&mut stderr, message);
    }
    if !crate::context::has_project_lock(checkout_root) {
        return;
    }
    let result = {
        let mut emit = |message| {
            write_context_message(&mut stderr, message);
            Ok(())
        };
        crate::context::sync_at(checkout_root, &mut emit)
    };
    match result {
        Ok(report) => {
            match crate::context::aliases_not_ignored_at(checkout_root) {
                Ok(true) => {
                    writeln!(
                        stderr,
                        "Warning: {}",
                        crate::context::ALIASES_NOT_IGNORED_WARNING
                    )
                    .ok();
                }
                Ok(false) => {}
                Err(error) => {
                    let error = crate::context::redact_url_userinfo(&format!("{error:#}"));
                    writeln!(
                        stderr,
                        "Warning: could not check context ignore policy in {} after working-copy movement ({error})",
                        checkout_root.display()
                    ).ok();
                }
            }
            if report.warned {
                writeln!(
                    stderr,
                    "Warning: `ds context sync` completed with warnings in {} after working-copy movement; run it manually to retry",
                    checkout_root.display()
                ).ok();
            }
        }
        Err(error) => {
            let error = crate::context::redact_url_userinfo(&format!("{error:#}"));
            writeln!(
                stderr,
                "Warning: could not sync context in {} after working-copy movement ({error}); the context state may be inconsistent, so rewrite or rebuild it manually",
                checkout_root.display()
            ).ok();
        }
    }
}

/// The user-facing channel for a post-command hook that failed. jj's default
/// tracing subscriber discards anything below ERROR, so a hook that only logs
/// is silent.
pub(crate) fn warn(text: &str) {
    writeln!(io::stderr().lock(), "Warning: {text}").ok();
}

fn write_context_message(output: &mut impl Write, message: crate::context::SyncMessage) {
    let prefix = match message.kind {
        crate::context::SyncMessageKind::Warning => "context auto-sync: warning: ",
        crate::context::SyncMessageKind::Output | crate::context::SyncMessageKind::Diagnostic => {
            "context auto-sync: "
        }
    };
    let text = crate::context::redact_url_userinfo(&message.text);
    writeln!(output, "{prefix}{text}").ok();
}

fn spawn_daemon(executable: &Path, machine_store_root: &Path) {
    let mut command = Command::new(executable);
    command
        .args(["daemon", "run"])
        .current_dir(machine_store_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    command.spawn().ok();
}

fn spawn_one_shot(executable: &Path, name: &RepositoryName, repository_directory: &Path) {
    let Ok(log) = File::create(repository_directory.join("sync.log")) else {
        return;
    };
    let Ok(error_log) = log.try_clone() else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .args(["sync", "run", "--repository-name", name.as_str()])
        // The parent's cwd may no longer exist (`ds remove .` deletes it);
        // give the child a directory that outlives the command.
        .current_dir(repository_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log));
    detach(&mut command);
    command.spawn().ok();
}

fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}
