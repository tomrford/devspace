//! Test-only helpers shared by the `devspace-cli` and `devspace-machine` suites.
//!
//! This crate is a dev dependency only. Nothing here ships in the `ds` binary.

pub mod fake_worker;
pub mod stalling_server;

use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::settings::UserSettings;

/// Build the `UserSettings` a suite writes its fixture commits with.
///
/// `change_id_header` selects `git.write-change-id-header`, which changes the
/// canonical commit bytes; a suite that pins commit bytes must keep its own value.
pub fn settings(name: &str, email: &str, change_id_header: bool) -> UserSettings {
    let mut text = format!("[user]\nname = {name:?}\nemail = {email:?}\n");
    if change_id_header {
        text.push_str("\n[git]\nwrite-change-id-header = true\n");
    }
    let mut config = StackedConfig::with_defaults();
    config.add_layer(ConfigLayer::parse(ConfigSource::User, &text).unwrap());
    UserSettings::from_config(config).unwrap()
}
