//! Headless view of the shared core.
//!
//! The desktop crate owns `src-tauri/src/core/*`; this server crate includes
//! the same source files via `#[path]` so both binaries share one
//! implementation. Modules that only exist to feed the desktop UI (event
//! emission, tray, panic capture, the auto-updater scheduler, the bundled-CLI
//! bridge) are not included; where an included module references one of those,
//! a minimal shim keeps the API surface unchanged.
//!
//! `#[path]` targets resolve from this directory: `../../../` is the repo
//! root.

#[path = "../../../src-tauri/src/core/app_state.rs"]
pub mod app_state;
#[path = "../../../src-tauri/src/core/audit_log.rs"]
pub mod audit_log;
#[path = "../../../src-tauri/src/core/central_repo.rs"]
pub mod central_repo;
#[path = "../../../src-tauri/src/core/content_hash.rs"]
pub mod content_hash;
#[path = "../../../src-tauri/src/core/crypto.rs"]
pub mod crypto;
#[path = "../../../src-tauri/src/core/error.rs"]
pub mod error;
#[path = "../../../src-tauri/src/core/file_manager.rs"]
pub mod file_manager;
#[path = "../../../src-tauri/src/core/git2_engine.rs"]
pub mod git2_engine;
#[path = "../../../src-tauri/src/core/git_backup.rs"]
pub mod git_backup;
#[path = "../../../src-tauri/src/core/git_credentials.rs"]
pub mod git_credentials;
#[path = "../../../src-tauri/src/core/git_fetcher.rs"]
pub mod git_fetcher;
#[path = "../../../src-tauri/src/core/github_api.rs"]
pub mod github_api;
#[path = "../../../src-tauri/src/core/install_cancel.rs"]
pub mod install_cancel;
#[path = "../../../src-tauri/src/core/installer.rs"]
pub mod installer;
#[path = "../../../src-tauri/src/core/log_sanitize.rs"]
pub mod log_sanitize;
#[path = "../../../src-tauri/src/core/merge/mod.rs"]
pub mod merge;
#[path = "../../../src-tauri/src/core/migrations.rs"]
pub mod migrations;
#[path = "../../../src-tauri/src/core/path_guard.rs"]
pub mod path_guard;
#[path = "../../../src-tauri/src/core/project_scanner.rs"]
pub mod project_scanner;
#[path = "../../../src-tauri/src/core/removals.rs"]
pub mod removals;
#[path = "../../../src-tauri/src/core/repo_lock.rs"]
pub mod repo_lock;
#[path = "../../../src-tauri/src/core/scanner.rs"]
pub mod scanner;
#[path = "../../../src-tauri/src/core/scenario_service.rs"]
pub mod scenario_service;
#[path = "../../../src-tauri/src/core/skill_metadata.rs"]
pub mod skill_metadata;
#[path = "../../../src-tauri/src/core/skill_store.rs"]
pub mod skill_store;
#[path = "../../../src-tauri/src/core/skillssh_api.rs"]
pub mod skillssh_api;
#[path = "../../../src-tauri/src/core/sync_engine.rs"]
pub mod sync_engine;
#[path = "../../../src-tauri/src/core/sync_metadata.rs"]
pub mod sync_metadata;
#[path = "../../../src-tauri/src/core/timing.rs"]
pub mod timing;
#[path = "../../../src-tauri/src/core/tool_adapters.rs"]
pub mod tool_adapters;
#[path = "../../../src-tauri/src/core/tool_service.rs"]
pub mod tool_service;
#[path = "../../../src-tauri/src/core/well_known.rs"]
pub mod well_known;

/// Shim: desktop auto-backup scheduler. Only the settings key is shared.
pub mod auto_backup {
    pub const SETTING_LAST_ERROR: &str = "backup_last_auto_error";
}

/// Shim: the bundled-CLI bridge. The server has no CLI to copy, so the bridge
/// directory answers as "not managed here" — enough for the path filters in
/// `central_repo` that consult it.
pub mod cli_bridge {
    use std::path::PathBuf;

    pub fn bridge_dir() -> PathBuf {
        super::central_repo::home_base_dir().join("bin")
    }

    pub fn bridge_path() -> PathBuf {
        bridge_dir().join("skills-manager-cli")
    }

    pub fn is_bridge_file(_name: &str) -> bool {
        false
    }
}

/// Shim: desktop file watcher. `sync_engine` calls in only to avoid reacting
/// to its own writes; a no-op is correct for the server, which has no watcher
/// to mute.
pub mod file_watcher {
    use std::path::Path;

    pub fn mute_self_writes(_target: &Path) {}
}
