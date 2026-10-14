//! Headless port of the git backup operations from `commands/git_backup.rs`.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::{
    central_repo, error::AppError, git2_engine, git_backup, git_credentials, git_fetcher,
    skill_metadata, skill_store::SkillStore, sync_metadata,
};
use walkdir::WalkDir;

pub fn classify_git_chain(e: anyhow::Error) -> AppError {
    AppError::classify_git_error(format!("{e:#}"))
}

pub fn sync_engine_pref(store: &SkillStore) {
    let git2_enabled = store
        .get_setting("git_backup_engine")
        .ok()
        .flatten()
        .map(|v| v.trim() == "git2")
        .unwrap_or(false);
    git2_engine::set_preference(git2_enabled, store.proxy_url());
}

pub fn effective_device_name(store: &SkillStore) -> String {
    let saved = store
        .get_setting("backup_device_name")
        .ok()
        .flatten()
        .map(|v| git_backup::sanitize_device_name(&v))
        .filter(|v| !v.is_empty());
    if let Some(name) = saved {
        return name;
    }
    let name = git_backup::default_device_name();
    if let Err(e) = store.set_setting("backup_device_name", &name) {
        log::warn!("device name: failed to persist default: {e:#}");
    }
    name
}

pub fn apply_device_identity(store: &SkillStore, skills_dir: &Path) {
    let name = effective_device_name(store);
    if let Err(e) = git_backup::configure_device_identity(skills_dir, &name) {
        log::warn!("device name: failed to configure git identity: {e:#}");
    }
}

/// Move credentials embedded in `url` into the credential store and return the
/// sanitized URL. Falls back to the original URL when storing fails (e.g. a
/// headless server without a keychain keeps the legacy embedded-credential
/// behavior).
pub fn sanitize_url_to_keychain(url: &str) -> String {
    let Some((cred, sanitized)) = git_credentials::split_credentials_from_url(url) else {
        return url.to_string();
    };
    let Some(host) = git_credentials::https_host(&sanitized) else {
        return url.to_string();
    };
    match git_credentials::store_credential(&host, &cred) {
        Ok(()) => sanitized,
        Err(e) => {
            log::warn!(
                "git credentials: keychain unavailable, keeping embedded credentials: {e:#}"
            );
            url.to_string()
        }
    }
}

pub fn git_backup_init(store: &SkillStore) -> Result<(), AppError> {
    let skills_dir = central_repo::skills_dir();
    git_backup::with_repo_lock("git init", || {
        sync_metadata::write_all_from_db_unlocked(store)?;
        git_backup::init_repo_unlocked(&skills_dir, &effective_device_name(store))
    })
    .map_err(AppError::git)
}

pub fn git_backup_set_remote(store: &SkillStore, url: &str) -> Result<String, AppError> {
    sync_engine_pref(store);
    git_fetcher::validate_git_url(url).map_err(AppError::git)?;
    let skills_dir = central_repo::skills_dir();
    let effective = sanitize_url_to_keychain(url.trim());
    git_backup::set_remote(&skills_dir, &effective).map_err(classify_git_chain)?;
    Ok(effective)
}

pub fn git_backup_commit(store: &SkillStore, message: &str) -> Result<(), AppError> {
    let skills_dir = central_repo::skills_dir();
    git_backup::with_repo_lock("git commit", || {
        apply_device_identity(store, &skills_dir);
        sync_metadata::write_all_from_db_unlocked(store)?;
        git_backup::commit_all_unlocked(&skills_dir, message)
    })
    .map_err(AppError::git)
}

pub fn git_backup_push(store: &SkillStore) -> Result<(), AppError> {
    sync_engine_pref(store);
    let skills_dir = central_repo::skills_dir();
    git_backup::push(&skills_dir).map_err(classify_git_chain)?;
    let _ = store.set_setting(crate::core::auto_backup::SETTING_LAST_ERROR, "");
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncOutcome {
    pub committed: bool,
    pub merge: Option<crate::core::merge::MergeSummary>,
    pub pushed: bool,
    pub snapshot_tag: Option<String>,
}

pub fn git_backup_sync(store: &SkillStore, message: &str) -> Result<SyncOutcome, AppError> {
    sync_engine_pref(store);
    let skills_dir = central_repo::skills_dir();
    git_backup::with_repo_lock("git sync", || run_sync_blocking(store, &skills_dir, message))
        .map_err(classify_git_chain)
}

const SYNC_PUSH_ATTEMPTS: usize = 3;

fn run_sync_blocking(
    store: &SkillStore,
    skills_dir: &Path,
    message: &str,
) -> anyhow::Result<SyncOutcome> {
    apply_device_identity(store, skills_dir);

    sync_metadata::write_all_from_db_unlocked(store)?;
    if let Err(e) =
        git_backup::apply_oversized_exclusions(skills_dir, git_backup::SKILL_SIZE_LIMIT_BYTES)
    {
        log::warn!("backup size: exclusion scan failed (continuing): {e:#}");
    }
    let mut committed = false;
    if git_backup::has_uncommitted_changes(skills_dir)? {
        git_backup::commit_all_unlocked(skills_dir, message)?;
        committed = true;
    }

    let branch = git_backup::current_branch(skills_dir);
    if let Err(e) = git_backup::fetch_branch(skills_dir, &branch) {
        log::info!("git sync: initial fetch failed (continuing): {e:#}");
    }

    let mut merge_summary: Option<crate::core::merge::MergeSummary> = None;
    let mut pushed = false;
    let mut snapshot_tag: Option<String> = None;
    for attempt in 0..SYNC_PUSH_ATTEMPTS {
        let status = git_backup::get_status(skills_dir)?;
        if status.behind > 0 {
            let summary = crate::core::merge::gated_pull_unlocked(store, skills_dir)?;
            reconcile_skills_index_unlocked(store)?;
            merge_summary = Some(summary);
        }

        let status = git_backup::get_status(skills_dir)?;
        let needs_push =
            committed || status.ahead > 0 || status.upstream_health == "no_upstream";
        if !needs_push {
            break;
        }
        snapshot_tag = Some(git_backup::create_snapshot_tag_unlocked(skills_dir)?);
        match git_backup::push_unlocked(skills_dir) {
            Ok(()) => {
                pushed = true;
                break;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let rejected = msg.contains("non-fast-forward")
                    || msg.contains("fetch first")
                    || msg.contains("[rejected]")
                    || msg.contains("failed to push some refs");
                if !rejected || attempt + 1 == SYNC_PUSH_ATTEMPTS {
                    return Err(e);
                }
                log::info!("git sync: push rejected (attempt {}), refetching", attempt + 1);
                git_backup::fetch_branch(skills_dir, &branch)?;
            }
        }
    }

    if pushed {
        let _ = store.set_setting(crate::core::auto_backup::SETTING_LAST_ERROR, "");
    }
    store.log_audit(
        crate::core::audit_log::AuditDraft::new("sync")
            .detail(format!(
                "committed={} merged={} pushed={}",
                committed,
                merge_summary.is_some(),
                pushed
            ))
            .ok(),
    );
    Ok(SyncOutcome { committed, merge: merge_summary, pushed, snapshot_tag })
}

pub fn reconcile_skills_index_unlocked(store: &SkillStore) -> anyhow::Result<()> {
    sync_metadata::cleanup_temporary_files()?;
    if sync_metadata::has_complete_skill_snapshot() {
        sync_metadata::reindex_from_metadata_unlocked(store)?;
        return Ok(());
    }

    let skills_dir = central_repo::skills_dir();
    std::fs::create_dir_all(&skills_dir)?;

    let existing = store.get_all_skills()?;
    for skill in existing {
        if !Path::new(&skill.central_path).exists() {
            store.delete_skill(&skill.id)?;
        }
    }

    for entry in WalkDir::new(&skills_dir)
        .min_depth(1)
        .max_depth(6)
        .into_iter()
        .filter_entry(|e| e.file_name().to_string_lossy() != ".git")
        .flatten()
    {
        let path = entry.path().to_path_buf();
        if !entry.file_type().is_dir() || !skill_metadata::is_valid_skill_dir(&path) {
            continue;
        }

        let central_path = path.to_string_lossy().to_string();
        if store.get_skill_by_central_path(&central_path)?.is_some() {
            continue;
        }

        let meta = skill_metadata::parse_skill_md(&path);
        let inferred_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown-skill".to_string());
        let name = meta
            .name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(inferred_name);
        let now = chrono::Utc::now().timestamp_millis();

        let record = crate::core::skill_store::SkillRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            description: meta.description,
            source_type: "import".to_string(),
            source_ref: Some(central_path.clone()),
            source_ref_resolved: None,
            source_subpath: None,
            source_branch: None,
            source_revision: None,
            remote_revision: None,
            central_path,
            content_hash: crate::core::content_hash::hash_directory(&path).ok(),
            enabled: true,
            created_at: now,
            updated_at: now,
            status: "ok".to_string(),
            update_status: "local_only".to_string(),
            last_checked_at: Some(now),
            last_check_error: None,
        };

        store.insert_skill(&record)?;
    }

    sync_metadata::write_all_from_db_unlocked(store)
}

pub fn git_backup_pull(store: &SkillStore) -> Result<crate::core::merge::MergeSummary, AppError> {
    sync_engine_pref(store);
    let skills_dir = central_repo::skills_dir();
    git_backup::with_repo_lock("git pull", || {
        apply_device_identity(store, &skills_dir);
        let summary = crate::core::merge::gated_pull_unlocked(store, &skills_dir)?;
        reconcile_skills_index_unlocked(store)?;
        store.log_audit(
            crate::core::audit_log::AuditDraft::new("sync_merge")
                .detail(format!(
                    "engine={} updated={} kept_local={} conflicts={} pending={}",
                    summary.engine,
                    summary.updated.len(),
                    summary.kept_local.len(),
                    summary.new_conflicts.len(),
                    summary.pending_total
                ))
                .ok(),
        );
        Ok(summary)
    })
    .map_err(classify_git_chain)
}

pub fn git_backup_pending_conflicts(
    store: &SkillStore,
) -> Result<Vec<crate::core::skill_store::PendingConflictRow>, AppError> {
    store.list_pending_conflicts().map_err(AppError::db)
}

pub fn git_backup_resolve_conflict(
    store: &SkillStore,
    skill_id: &str,
    action: &str,
) -> Result<String, AppError> {
    use crate::core::merge::resolve::ResolveAction;
    let Some(action) = ResolveAction::parse(action) else {
        return Err(AppError::invalid_input("Unknown resolve action"));
    };
    let skills_dir = central_repo::skills_dir();
    git_backup::with_repo_lock("resolve conflict", || {
        apply_device_identity(store, &skills_dir);
        let safety_tag = crate::core::merge::resolve::resolve_conflict_unlocked(
            store, &skills_dir, skill_id, action,
        )?;
        reconcile_skills_index_unlocked(store)?;
        store.log_audit(
            crate::core::audit_log::AuditDraft::new("resolve_conflict")
                .skill(skill_id.to_string(), skill_id.to_string())
                .ok(),
        );
        Ok(safety_tag)
    })
    .map_err(classify_git_chain)
}

pub fn git_backup_create_snapshot(store: &SkillStore) -> Result<String, AppError> {
    let _ = store;
    let skills_dir = central_repo::skills_dir();
    git_backup::create_snapshot_tag(&skills_dir).map_err(AppError::git)
}

pub fn git_backup_list_versions(
    store: &SkillStore,
    limit: Option<u32>,
) -> Result<Vec<git_backup::GitBackupVersion>, AppError> {
    let _ = store;
    let skills_dir = central_repo::skills_dir();
    git_backup::list_snapshot_versions(&skills_dir, limit.map(|v| v as usize))
        .map_err(AppError::git)
}

pub fn git_backup_restore_version(store: &SkillStore, tag: &str) -> Result<String, AppError> {
    let skills_dir = central_repo::skills_dir();
    git_backup::with_repo_lock("git restore snapshot", || {
        apply_device_identity(store, &skills_dir);
        let safety_tag =
            git_backup::restore_snapshot_version_unlocked(&skills_dir, tag)?;
        reconcile_skills_index_unlocked(store)?;
        Ok(safety_tag)
    })
    .map_err(classify_git_chain)
}

pub fn git_backup_size_report() -> Result<git_backup::BackupSizeReport, AppError> {
    let skills_dir = central_repo::skills_dir();
    git_backup::size_report(&skills_dir).map_err(AppError::io)
}

pub fn git_backup_sanitize_remote_url(url: &str) -> Result<String, AppError> {
    git_fetcher::validate_git_url(url).map_err(AppError::git)?;
    Ok(sanitize_url_to_keychain(url.trim()))
}

pub fn git_backup_status(_store: &SkillStore) -> Result<git_backup::GitBackupStatus, AppError> {
    let skills_dir = central_repo::skills_dir();
    git_backup::get_status(&skills_dir).map_err(AppError::git)
}

pub fn git_backup_fetch(_store: &SkillStore) -> Result<(), AppError> {
    let skills_dir = central_repo::skills_dir();
    git_backup::fetch_remote(&skills_dir).map_err(AppError::git)
}

pub fn git_backup_remove_remote(store: &SkillStore) -> Result<(), AppError> {
    let skills_dir = central_repo::skills_dir();

    let mut hosts = std::collections::HashSet::new();
    if let Some(url) = git_backup::raw_remote_url(&skills_dir) {
        hosts.extend(git_credentials::https_host(&url));
    }
    if let Ok(Some(url)) = store.get_setting("git_backup_remote_url") {
        hosts.extend(git_credentials::https_host(&url));
    }

    git_backup::remove_remote(&skills_dir).map_err(AppError::git)?;
    store
        .set_setting("git_backup_remote_url", "")
        .map_err(AppError::db)?;
    let _ = store.set_setting("github_auth_method", "");

    for host in hosts {
        if let Err(e) = git_credentials::delete_credential(&host) {
            log::warn!("git disconnect: failed to delete keychain credential: {e:#}");
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub fn backup_device_name(store: &SkillStore) -> String {
    effective_device_name(store)
}

#[allow(dead_code)]
pub fn backup_set_device_name(store: &SkillStore, name: &str) -> Result<String, AppError> {
    let sanitized = git_backup::sanitize_device_name(name);
    if sanitized.is_empty() {
        return Err(AppError::invalid_input("Device name is empty"));
    }
    store
        .set_setting("backup_device_name", &sanitized)
        .map_err(AppError::db)?;
    let skills_dir = central_repo::skills_dir();
    if skills_dir.join(".git").exists() {
        let _ = git_backup::configure_device_identity(&skills_dir, &sanitized);
    }
    Ok(sanitized)
}
