//! Test-only execution seams. Each one is inert unless the test harness names
//! it in the matching environment variable.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

const CHECKOUT_ENV: &str = "DEVSPACE_TEST_CHECKOUT_FAILPOINT";
const CHECKOUT_READY_ENV: &str = "DEVSPACE_TEST_CHECKOUT_FAILPOINT_READY";
const CHECKOUT_CONTINUE_ENV: &str = "DEVSPACE_TEST_CHECKOUT_FAILPOINT_CONTINUE";
const FAILPOINT_ENV: &str = "DEVSPACE_FAILPOINT";

/// Announce that the checkout mutation reached `name`, then either wait for the
/// test to release it or park forever so the test can kill the process
/// mid-flight.
pub(crate) fn checkout_failpoint(name: &str) {
    if std::env::var_os(CHECKOUT_ENV).as_deref() != Some(OsStr::new(name)) {
        return;
    }
    if let Some(path) = std::env::var_os(CHECKOUT_READY_ENV) {
        fs::write(path, name).ok();
    }
    if let Some(path) = std::env::var_os(CHECKOUT_CONTINUE_ENV) {
        while !Path::new(&path).exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        return;
    }
    loop {
        std::thread::park();
    }
}

/// Abort at `name` with an exit code no command produces on its own.
pub(crate) fn abort_failpoint(name: &str) {
    if failpoint_enabled(name) {
        std::process::exit(86);
    }
}

/// For call sites that must unwind or report rather than abort.
pub(crate) fn failpoint_enabled(name: &str) -> bool {
    std::env::var_os(FAILPOINT_ENV).as_deref() == Some(OsStr::new(name))
}
