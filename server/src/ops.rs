//! Headless port of the skill operations from `src-tauri/src/commands/skills.rs`.
//! The logic is kept in step with the desktop implementation; only the Tauri
//! plumbing (State/AppHandle/emit) is replaced by plain function signatures.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::core::{
    audit_log::AuditDraft,
    central_repo,
    content_hash,
    error::AppError,
    git_fetcher,
    install_cancel::InstallCancelRegistry,
    installer,
    path_guard,
    repo_lock::RepoLock,
    scanner,
    scenario_service,
    skill_metadata::{self, is_valid_skill_dir},
    skill_store::{SkillRecord, SkillStore, SkillTargetRecord},
    sync_engine,
    sync_metadata,
    well_known,
};

// ── DTOs (wire format identical to the desktop commands) ──

#[derive(Debug, Serialize)]
pub struct ManagedSkillDto {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub source_type: String,
    pub source_ref: Option<String>,
    pub source_ref_resolved: Option<String>,
    pub source_subpath: Option<String>,
    pub source_branch: Option<String>,
    pub source_revision: Option<String>,
    pub remote_revision: Option<String>,
    pub update_status: String,
    pub last_checked_at: Option<i64>,
    pub last_check_error: Option<String>,
    pub central_path: String,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub status: String,
    pub targets: Vec<TargetDto>,
    pub preset_ids: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct TargetDto {
    pub id: String,
    pub skill_id: String,
    pub tool: String,
    pub target_path: String,
    pub mode: String,
    pub status: String,
    pub synced_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct SkillDocumentDto {
    pub skill_id: String,
    pub filename: String,
    pub content: String,
    pub central_path: String,
}

#[derive(Debug, Serialize)]
pub struct GitSkillPreview {
    pub rel_path: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GitPreviewResult {
    pub temp_dir: String,
    pub skills: Vec<GitSkillPreview>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SkillInstallItem {
    pub rel_path: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct UpdateSkillResult {
    pub skill: ManagedSkillDto,
    pub content_changed: bool,
    pub pending_removals: Vec<PendingRemoval>,
    pub removal_approval: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReimportSkillResult {
    pub skill: ManagedSkillDto,
    pub pending_removals: Vec<PendingRemoval>,
    pub removal_approval: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingRemoval {
    pub location: String,
    pub path: String,
}

pub const LIBRARY_LOCATION: &str = "library";

#[derive(Debug, Serialize)]
pub struct BatchUpdateSkillsResult {
    pub refreshed: usize,
    pub unchanged: usize,
    pub failed: Vec<String>,
    pub held_back: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchDeleteSkillsResult {
    pub deleted: usize,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct InstallSourceMetadata {
    pub source_type: String,
    pub source_ref: Option<String>,
    pub source_ref_resolved: Option<String>,
    pub source_subpath: Option<String>,
    pub source_branch: Option<String>,
    pub source_revision: Option<String>,
    pub remote_revision: Option<String>,
    pub update_status: String,
}

#[derive(Debug, Clone)]
pub struct GitSkillSource {
    pub clone_url: String,
    pub branch: Option<String>,
    pub subpath: Option<String>,
    pub locator_skill_id: Option<String>,
}

enum UpdateOutcome {
    Applied { content_changed: bool },
    Held { pending: Vec<PendingRemoval>, approval: String },
}

const REIMPORT_APPROVAL_DOMAIN: &str = "reimport";

// ── DTO mapping ──

pub fn managed_skill_to_dto(
    store: &SkillStore,
    skill: SkillRecord,
    all_targets: &[SkillTargetRecord],
    tags_map: &HashMap<String, Vec<String>>,
) -> ManagedSkillDto {
    let targets = all_targets
        .iter()
        .filter(|target| target.skill_id == skill.id)
        .map(|target| TargetDto {
            id: target.id.clone(),
            skill_id: target.skill_id.clone(),
            tool: target.tool.clone(),
            target_path: target.target_path.clone(),
            mode: target.mode.clone(),
            status: target.status.clone(),
            synced_at: target.synced_at,
        })
        .collect();

    let preset_ids = store.get_scenarios_for_skill(&skill.id).unwrap_or_default();
    let tags = tags_map.get(&skill.id).cloned().unwrap_or_default();

    let description = skill_metadata::parse_skill_md(Path::new(&skill.central_path))
        .description
        .filter(|s| !s.trim().is_empty())
        .or(skill.description);

    ManagedSkillDto {
        id: skill.id,
        name: skill.name,
        description,
        source_type: skill.source_type,
        source_ref: skill.source_ref,
        source_ref_resolved: skill.source_ref_resolved,
        source_subpath: skill.source_subpath,
        source_branch: skill.source_branch,
        source_revision: skill.source_revision,
        remote_revision: skill.remote_revision,
        update_status: skill.update_status,
        last_checked_at: skill.last_checked_at,
        last_check_error: skill.last_check_error,
        central_path: skill.central_path,
        enabled: skill.enabled,
        created_at: skill.created_at,
        updated_at: skill.updated_at,
        status: skill.status,
        targets,
        preset_ids,
        tags,
    }
}

pub fn managed_skill_by_id(store: &SkillStore, skill_id: &str) -> Result<ManagedSkillDto, AppError> {
    let skill = store
        .get_skill_by_id(skill_id)
        .map_err(AppError::db)?
        .ok_or_else(|| AppError::not_found("Skill not found"))?;
    let all_targets = store.get_all_targets().map_err(AppError::db)?;
    let tags_map = store.get_tags_map().map_err(AppError::db)?;
    Ok(managed_skill_to_dto(store, skill, &all_targets, &tags_map))
}

pub fn get_managed_skills(store: &SkillStore) -> Result<Vec<ManagedSkillDto>, AppError> {
    let skills = store.get_all_skills().map_err(AppError::db)?;
    let all_targets = store.get_all_targets().map_err(AppError::db)?;
    let tags_map = store.get_tags_map().map_err(AppError::db)?;
    Ok(skills
        .into_iter()
        .map(|skill| managed_skill_to_dto(store, skill, &all_targets, &tags_map))
        .collect())
}

pub fn get_skills_for_preset(store: &SkillStore, preset_id: &str) -> Result<Vec<ManagedSkillDto>, AppError> {
    let skills = store.get_skills_for_scenario(preset_id).map_err(AppError::db)?;
    let all_targets = store.get_all_targets().map_err(AppError::db)?;
    let tags_map = store.get_tags_map().map_err(AppError::db)?;
    Ok(skills
        .into_iter()
        .map(|skill| managed_skill_to_dto(store, skill, &all_targets, &tags_map))
        .collect())
}

pub fn get_skill_document(store: &SkillStore, skill_id: &str) -> Result<SkillDocumentDto, AppError> {
    let skill = store
        .get_skill_by_id(skill_id)
        .map_err(AppError::db)?
        .ok_or_else(|| AppError::not_found("Skill not found"))?;
    let (filename, content) = read_skill_document_from_dir(Path::new(&skill.central_path))?;
    Ok(SkillDocumentDto {
        skill_id: skill_id.to_string(),
        filename,
        content,
        central_path: skill.central_path,
    })
}

fn read_skill_document_from_dir(dir: &Path) -> Result<(String, String), AppError> {
    let candidates = [
        "SKILL.md",
        "skill.md",
        "CLAUDE.md",
        "claude.md",
        "README.md",
        "readme.md",
    ];

    for name in &candidates {
        let path = dir.join(name);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            return Ok((name.to_string(), content));
        }
    }

    for e in walkdir::WalkDir::new(dir).max_depth(4).into_iter().flatten() {
        let fname = e.file_name().to_string_lossy();
        if candidates.contains(&fname.as_ref()) {
            let content = std::fs::read_to_string(e.path())?;
            return Ok((fname.to_string(), content));
        }
    }

    Err(AppError::not_found("No documentation file found"))
}

pub fn source_label_for_skill(skill: &SkillRecord) -> String {
    match skill.source_type.as_str() {
        "skillssh" => "skills.sh".to_string(),
        "git" => "Git".to_string(),
        "local" => "Local".to_string(),
        "import" => "Imported".to_string(),
        other => other.to_string(),
    }
}

// ── Delete ──

pub fn delete_managed_skills_by_ids(
    store: &SkillStore,
    skill_ids: &[String],
) -> Result<BatchDeleteSkillsResult, AppError> {
    sync_metadata::with_repo_lock("delete skills", || {
        let mut deleted = 0;
        let mut failed = Vec::new();

        for skill_id in skill_ids {
            let Some(skill) = store.get_skill_by_id(skill_id)? else {
                store.log_audit(
                    AuditDraft::new("remove").skill(skill_id.clone(), "").fail("not found"),
                );
                failed.push(skill_id.clone());
                continue;
            };

            let targets = store.get_targets_for_skill(skill_id)?;
            for target in &targets {
                sync_engine::remove_recorded_target_or_warn(
                    &PathBuf::from(&target.target_path),
                    &target.mode,
                );
            }

            let central = PathBuf::from(&skill.central_path);
            if central.exists() {
                std::fs::remove_dir_all(&central).ok();
            }

            store.delete_skill(skill_id)?;
            store.log_audit(
                AuditDraft::new("remove").skill(skill_id.clone(), skill.name.clone()).ok(),
            );
            deleted += 1;
        }

        if deleted > 0 {
            sync_metadata::write_all_from_db_unlocked(store)?;
        }

        Ok(BatchDeleteSkillsResult { deleted, failed })
    })
    .map_err(AppError::db)
}

// ── Removal guards (copied from commands/skills.rs) ──

pub fn pending_removals_for(
    store: &SkillStore,
    skill: &SkillRecord,
    staged: Option<&Path>,
) -> Result<Vec<PendingRemoval>, AppError> {
    let library = Path::new(&skill.central_path);
    let mut pending = Vec::new();

    if let Some(staged) = staged {
        for path in crate::core::removals::removed_paths(library, staged)
            .map_err(AppError::io)?
        {
            pending.push(PendingRemoval {
                location: LIBRARY_LOCATION.to_string(),
                path,
            });
        }
    }

    let effective_new = staged.unwrap_or(library);
    for target in store
        .get_targets_for_skill(&skill.id)
        .map_err(AppError::db)?
    {
        if target.mode != "copy" {
            continue;
        }
        for path in crate::core::removals::removed_paths(
            Path::new(&target.target_path),
            effective_new,
        )
        .map_err(AppError::io)?
        {
            pending.push(PendingRemoval {
                location: target.tool.clone(),
                path,
            });
        }
    }
    Ok(pending)
}

fn removal_approval_token(revision: &str, pending: &[PendingRemoval]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(revision.as_bytes());
    let mut rows: Vec<String> = pending
        .iter()
        .map(|p| format!("{}\u{0}{}", p.location, p.path))
        .collect();
    rows.sort();
    for row in rows {
        hasher.update(row.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

struct StagedPathGuard<'a> {
    path: &'a Path,
    armed: std::cell::Cell<bool>,
}

impl<'a> StagedPathGuard<'a> {
    fn new(path: &'a Path, armed: bool) -> Self {
        Self { path, armed: std::cell::Cell::new(armed) }
    }

    fn release(&self) {
        self.armed.set(false);
    }
}

impl Drop for StagedPathGuard<'_> {
    fn drop(&mut self) {
        if self.armed.get() {
            let _ = remove_path_if_exists(self.path);
        }
    }
}

pub fn remove_path_if_exists(path: &Path) -> Result<(), AppError> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

// ── Install ──

fn log_install_outcome(
    store: &SkillStore,
    source_label: &str,
    outcome: &Result<(String, String), AppError>,
) {
    let draft = AuditDraft::new("install").detail(source_label);
    let draft = match outcome {
        Ok((id, name)) => draft.skill(id.clone(), name.clone()).ok(),
        Err(e) => draft.fail(e.to_string()),
    };
    store.log_audit(draft);
}

pub fn store_installed_skill_unlocked(
    store: &SkillStore,
    result: &installer::InstallResult,
    metadata: &InstallSourceMetadata,
    active_scenario_id: Option<&str>,
) -> Result<String, AppError> {
    let now = chrono::Utc::now().timestamp_millis();
    let central_path = result.central_path.to_string_lossy().to_string();

    if let Some(existing) = store
        .get_skill_by_central_path(&central_path)
        .map_err(AppError::db)?
    {
        store
            .update_skill_after_reinstall(
                &existing.id,
                &result.name,
                result.description.as_deref(),
                &metadata.source_type,
                metadata.source_ref.as_deref(),
                metadata.source_ref_resolved.as_deref(),
                metadata.source_subpath.as_deref(),
                metadata.source_branch.as_deref(),
                metadata.source_revision.as_deref(),
                metadata.remote_revision.as_deref(),
                Some(&result.content_hash),
                &metadata.update_status,
            )
            .map_err(AppError::db)?;
        if let Some(scenario_id) = active_scenario_id {
            store
                .add_skill_to_scenario(scenario_id, &existing.id)
                .map_err(AppError::db)?;
        }
        sync_metadata::write_all_from_db_unlocked(store).map_err(AppError::db)?;

        if let Some(scenario_id) = active_scenario_id {
            if let Err(e) = scenario_service::sync_skill_to_active_scenario(
                store,
                scenario_id,
                &existing.id,
            ) {
                log::warn!("Failed to sync reinstalled skill to preset: {e}");
            }
        }

        return Ok(existing.id);
    }

    let id = uuid::Uuid::new_v4().to_string();

    let record = SkillRecord {
        id: id.clone(),
        name: result.name.clone(),
        description: result.description.clone(),
        source_type: metadata.source_type.clone(),
        source_ref: metadata.source_ref.clone(),
        source_ref_resolved: metadata.source_ref_resolved.clone(),
        source_subpath: metadata.source_subpath.clone(),
        source_branch: metadata.source_branch.clone(),
        source_revision: metadata.source_revision.clone(),
        remote_revision: metadata.remote_revision.clone(),
        central_path,
        content_hash: Some(result.content_hash.clone()),
        enabled: true,
        created_at: now,
        updated_at: now,
        status: "ok".to_string(),
        update_status: metadata.update_status.clone(),
        last_checked_at: Some(now),
        last_check_error: None,
    };

    store.insert_skill(&record).map_err(AppError::db)?;
    if let Some(scenario_id) = active_scenario_id {
        store
            .add_skill_to_scenario(scenario_id, &id)
            .map_err(AppError::db)?;
    }
    sync_metadata::write_all_from_db_unlocked(store).map_err(AppError::db)?;

    if let Some(scenario_id) = active_scenario_id {
        if let Err(e) =
            scenario_service::sync_skill_to_active_scenario(store, scenario_id, &id)
        {
            log::warn!("Failed to sync newly installed skill to preset: {e}");
        }
    }

    Ok(id)
}

pub fn install_site_skill_dir(
    store: &SkillStore,
    site_url: &str,
    skill_dir: &Path,
    name: Option<&str>,
    resolved_url: &str,
    revision: Option<String>,
    active_scenario_id: Option<&str>,
) -> Result<(String, installer::InstallResult), AppError> {
    let site = well_known::parse_site_ref(site_url)
        .map_err(|error| AppError::invalid_input(error.to_string()))?;
    let source_ref = site.canonical_url();
    let base_name = match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(custom) => skill_metadata::sanitize_skill_name(custom)
            .ok_or_else(|| AppError::invalid_input(format!("Invalid skill name: '{custom}'")))?,
        None => site.skill_name,
    };
    let (install_name, destination) =
        resolve_install_target(store, "well-known", &source_ref, &base_name)?;
    let result = installer::install_skill_dir_to_destination(skill_dir, &install_name, &destination)
        .map_err(AppError::io)?;
    let metadata = InstallSourceMetadata {
        source_type: "well-known".to_string(),
        source_ref: Some(source_ref),
        source_ref_resolved: Some(resolved_url.to_string()),
        source_subpath: None,
        source_branch: None,
        source_revision: revision.clone(),
        remote_revision: revision,
        update_status: "local_only".to_string(),
    };
    let skill_id = store_installed_skill_unlocked(store, &result, &metadata, active_scenario_id)?;
    Ok((skill_id, result))
}

pub fn install_site_skill(
    store: &SkillStore,
    site_url: &str,
    name: Option<&str>,
    proxy_url: Option<&str>,
    cancel: &Arc<AtomicBool>,
) -> Result<(String, String), AppError> {
    let downloaded = well_known::download_site_skill(site_url, proxy_url)
        .map_err(AppError::network)?;
    let result = (|| -> Result<(String, String), AppError> {
        let _lock = RepoLock::acquire_foreground("install website skill").map_err(AppError::db)?;
        if cancel.load(Ordering::SeqCst) {
            return Err(AppError::cancelled("Installation cancelled by user"));
        }
        let (skill_id, result) = install_site_skill_dir(
            store,
            site_url,
            &downloaded.skill_dir,
            name,
            &downloaded.resolved_url,
            downloaded.revision.clone(),
            None,
        )?;
        Ok((skill_id, result.name))
    })();
    well_known::cleanup_temp(&downloaded.temp_dir);
    result
}

pub fn install_git_impl(
    store: &SkillStore,
    repo_url: &str,
    name: Option<&str>,
    proxy_url: Option<&str>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), AppError> {
    if well_known::is_site_ref(repo_url) {
        let outcome = install_site_skill(store, repo_url, name, proxy_url, cancel);
        log_install_outcome(store, "well-known", &outcome);
        return outcome.map(|_| ());
    }

    let outcome = (|| -> Result<(String, String), AppError> {
        git_fetcher::validate_git_url(repo_url).map_err(AppError::git)?;
        let parsed = git_fetcher::parse_git_source_resolved(repo_url, proxy_url);
        let temp_dir = git_fetcher::clone_repo_ref_scoped(
            &parsed.clone_url,
            parsed.branch.as_deref(),
            parsed.subpath.as_deref(),
            Some(cancel),
            proxy_url,
            None,
        )
        .map_err(AppError::classify_git_error)?;

        let install_result = (|| -> Result<(String, String), AppError> {
            let _lock = RepoLock::acquire_foreground("install git skill").map_err(AppError::db)?;
            let skill_dir = resolve_skill_dir(&temp_dir, parsed.subpath.as_deref(), None)?;
            let revision = git_fetcher::get_head_revision(&temp_dir).map_err(AppError::git)?;
            let result = installer::install_from_git_dir(&skill_dir, name)
                .map_err(AppError::io)?;
            let metadata = InstallSourceMetadata {
                source_type: "git".to_string(),
                source_ref: Some(parsed.original_url.clone()),
                source_ref_resolved: Some(parsed.clone_url.clone()),
                source_subpath: git_fetcher::relative_subpath(&temp_dir, &skill_dir),
                source_branch: parsed.branch.clone(),
                source_revision: Some(revision.clone()),
                remote_revision: Some(revision),
                update_status: "up_to_date".to_string(),
            };
            let skill_name = result.name.clone();
            let skill_id = store_installed_skill_unlocked(store, &result, &metadata, None)?;
            Ok((skill_id, skill_name))
        })();

        git_fetcher::cleanup_temp(&temp_dir);
        install_result
    })();

    log_install_outcome(store, "git", &outcome);
    outcome.map(|_| ())
}

pub fn install_from_skillssh_impl(
    store: &SkillStore,
    source: &str,
    skill_id: &str,
    proxy_url: Option<&str>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), AppError> {
    if !source.contains('/') {
        let site_url = format!("https://skills.sh/site/{source}/{skill_id}");
        let outcome = install_site_skill(store, &site_url, None, proxy_url, cancel);
        log_install_outcome(store, "well-known", &outcome);
        return outcome.map(|_| ());
    }

    let outcome = (|| -> Result<(String, String), AppError> {
        let repo_url = format!("https://github.com/{source}.git");
        let temp_dir = git_fetcher::clone_repo_ref_scoped(
            &repo_url,
            None,
            None,
            Some(cancel),
            proxy_url,
            None,
        )
        .map_err(AppError::classify_git_error)?;

        let install_result = (|| -> Result<(String, String), AppError> {
            let _lock =
                RepoLock::acquire_foreground("install skillssh skill").map_err(AppError::db)?;
            let skill_dir = resolve_skill_dir(&temp_dir, None, Some(skill_id))?;
            let revision = git_fetcher::get_head_revision(&temp_dir).map_err(AppError::git)?;
            let source_ref = format!("{source}/{skill_id}");
            let (install_name, destination) =
                resolve_skillssh_install_target(store, &source_ref, skill_id)?;
            let result = installer::install_skill_dir_to_destination(
                &skill_dir,
                &install_name,
                &destination,
            )
            .map_err(AppError::io)?;
            let metadata = InstallSourceMetadata {
                source_type: "skillssh".to_string(),
                source_ref: Some(source_ref),
                source_ref_resolved: Some(repo_url.clone()),
                source_subpath: git_fetcher::relative_subpath(&temp_dir, &skill_dir),
                source_branch: None,
                source_revision: Some(revision.clone()),
                remote_revision: Some(revision),
                update_status: "up_to_date".to_string(),
            };
            let skill_name = result.name.clone();
            let new_id = store_installed_skill_unlocked(store, &result, &metadata, None)?;
            Ok((new_id, skill_name))
        })();

        git_fetcher::cleanup_temp(&temp_dir);
        install_result
    })();

    log_install_outcome(store, "skillssh", &outcome);
    outcome.map(|_| ())
}

pub fn preview_git_install_impl(
    store: &SkillStore,
    repo_url: &str,
    proxy_url: Option<&str>,
    cancel: &Arc<AtomicBool>,
) -> Result<GitPreviewResult, AppError> {
    if well_known::is_site_ref(repo_url) {
        let downloaded = well_known::download_site_skill(repo_url, proxy_url)
            .map_err(AppError::network)?;
        let result = (|| -> Result<GitPreviewResult, AppError> {
            if cancel.load(Ordering::SeqCst) {
                return Err(AppError::cancelled("Installation cancelled by user"));
            }
            let dirs = collect_git_skill_dirs(&downloaded.skill_dir);
            if dirs.is_empty() {
                return Err(AppError::not_found("Downloaded source contains no skills"));
            }
            let skills = dirs
                .iter()
                .map(|dir| {
                    let meta = skill_metadata::parse_skill_md(dir);
                    let rel_path = skill_rel_key(&downloaded.skill_dir, dir);
                    let basename = dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| rel_path.clone());
                    let name = meta.name.filter(|s| !s.trim().is_empty()).unwrap_or(basename);
                    GitSkillPreview {
                        rel_path,
                        name,
                        description: meta.description,
                    }
                })
                .collect();
            Ok(GitPreviewResult {
                temp_dir: downloaded.temp_dir.to_string_lossy().to_string(),
                skills,
            })
        })();
        if result.is_err() {
            well_known::cleanup_temp(&downloaded.temp_dir);
        }
        return result;
    }

    let parsed = git_fetcher::parse_git_source_resolved(repo_url, proxy_url);
    let temp_dir = git_fetcher::clone_repo_ref_scoped(
        &parsed.clone_url,
        parsed.branch.as_deref(),
        parsed.subpath.as_deref(),
        Some(cancel),
        proxy_url,
        None,
    )
    .map_err(AppError::classify_git_error)?;

    let build_preview = || -> Result<GitPreviewResult, AppError> {
        let skill_dir = resolve_skill_dir(&temp_dir, parsed.subpath.as_deref(), None)?;
        let dirs = collect_git_skill_dirs(&skill_dir);

        let skills: Vec<GitSkillPreview> = dirs
            .iter()
            .map(|dir| {
                let meta = skill_metadata::parse_skill_md(dir);
                let rel_path = skill_rel_key(&skill_dir, dir);
                let basename = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| rel_path.clone());
                let name = meta
                    .name
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| basename.clone());
                GitSkillPreview {
                    rel_path,
                    name,
                    description: meta.description,
                }
            })
            .collect();

        Ok(GitPreviewResult {
            temp_dir: temp_dir.to_string_lossy().to_string(),
            skills,
        })
    };

    build_preview().inspect_err(|_e| {
        git_fetcher::cleanup_temp(&temp_dir);
    })
}

pub fn confirm_git_install_impl(
    store: &SkillStore,
    repo_url: &str,
    temp_dir: &str,
    items: &[SkillInstallItem],
    proxy_url: Option<&str>,
) -> Result<(), AppError> {
    if well_known::is_site_ref(repo_url) {
        let temp_path = validate_clone_temp_path(temp_dir)?;
        let result: Result<(), AppError> = (|| {
            if items.is_empty() {
                return Ok(());
            }
            let skill_dir = well_known::skill_dir_from_temp(&temp_path)
                .map_err(|error| AppError::invalid_input(error.to_string()))?;
            let all_dirs = collect_git_skill_dirs(&skill_dir);
            let _lock = RepoLock::acquire_foreground("confirm website skill install")
                .map_err(AppError::db)?;
            for dir in &all_dirs {
                let rel_key = skill_rel_key(&skill_dir, dir);
                let item = match items.iter().find(|item| item.rel_path == rel_key) {
                    Some(item) => item,
                    None => continue,
                };
                install_site_skill_dir(
                    store,
                    repo_url,
                    dir,
                    Some(item.name.as_str()),
                    repo_url,
                    None,
                    None,
                )?;
            }
            Ok(())
        })();
        well_known::cleanup_temp(&temp_path);
        return result;
    }
    let temp_path = validate_clone_temp_path(temp_dir)?;

    let result: Result<(), AppError> = (|| {
        if items.is_empty() {
            return Ok(());
        }

        let parsed = git_fetcher::parse_git_source_resolved(repo_url, proxy_url);
        let skill_dir = resolve_skill_dir(&temp_path, parsed.subpath.as_deref(), None)?;
        let all_dirs = collect_git_skill_dirs(&skill_dir);
        let revision = git_fetcher::get_head_revision(&temp_path).map_err(AppError::git)?;
        let _lock = RepoLock::acquire_foreground("confirm git install").map_err(AppError::db)?;

        for dir in &all_dirs {
            let rel_key = skill_rel_key(&skill_dir, dir);
            let item = match items.iter().find(|i| i.rel_path == rel_key) {
                Some(i) => i,
                None => continue,
            };
            let custom_name = item.name.trim();
            let install_name = if custom_name.is_empty() {
                None
            } else {
                Some(custom_name)
            };
            let result = installer::install_from_git_dir(dir, install_name)
                .map_err(AppError::io)?;
            let subpath = git_fetcher::relative_subpath(&temp_path, dir);
            let metadata = InstallSourceMetadata {
                source_type: "git".to_string(),
                source_ref: Some(repo_url.to_string()),
                source_ref_resolved: Some(parsed.clone_url.clone()),
                source_subpath: subpath,
                source_branch: parsed.branch.clone(),
                source_revision: Some(revision.clone()),
                remote_revision: Some(revision.clone()),
                update_status: "up_to_date".to_string(),
            };
            store_installed_skill_unlocked(store, &result, &metadata, None)?;
        }
        Ok(())
    })();

    git_fetcher::cleanup_temp(&temp_path);
    result
}

pub fn cancel_git_preview_impl(temp_dir: &str) -> Result<(), AppError> {
    if let Ok(temp_path) = validate_clone_temp_path(temp_dir) {
        git_fetcher::cleanup_temp(&temp_path);
    }
    Ok(())
}

// ── Skill directory helpers (copied) ──

pub fn collect_git_skill_dirs(skill_dir: &Path) -> Vec<PathBuf> {
    if is_valid_skill_dir(skill_dir) {
        return vec![skill_dir.to_path_buf()];
    }
    let mut dirs = scanner::collect_skill_dirs(skill_dir);
    dirs.sort();
    dirs
}

pub fn skill_rel_key(skill_dir: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(skill_dir).unwrap_or(dir);
    if rel.as_os_str().is_empty() {
        dir.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        rel.to_string_lossy().replace('\\', "/")
    }
}

pub fn validate_clone_temp_path(temp_dir: &str) -> Result<PathBuf, AppError> {
    let raw_path = PathBuf::from(temp_dir);
    if !raw_path.exists() {
        return Err(AppError::invalid_input(
            "Clone session expired, please try again",
        ));
    }
    let temp_path = raw_path
        .canonicalize()
        .map_err(|_| AppError::invalid_input("Invalid temp directory"))?;

    let expected_prefix = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    if temp_path.starts_with(&expected_prefix) {
        let dir_name_str = temp_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if dir_name_str.starts_with(git_fetcher::CLONE_TEMP_PREFIX)
            || dir_name_str.starts_with(well_known::TEMP_DIR_PREFIX)
        {
            return Ok(temp_path);
        }
    }

    Err(AppError::invalid_input("Invalid temp directory"))
}

pub fn resolve_skill_dir(
    repo_dir: &Path,
    subpath: Option<&str>,
    skill_id: Option<&str>,
) -> Result<PathBuf, AppError> {
    if let Some(subpath) = subpath {
        let candidate = repo_dir.join(subpath);
        if !path_guard::is_path_safe(repo_dir, &candidate) {
            return Err(AppError::invalid_input(format!(
                "Path '{subpath}' resolves outside the repository"
            )));
        }
        let usable = if skill_id.is_some() {
            is_valid_skill_dir(&candidate)
        } else {
            candidate.is_dir()
        };
        if usable {
            return Ok(candidate);
        }
        if skill_id.is_none() {
            return Err(AppError::not_found(format!(
                "Path '{subpath}' does not exist in the repository"
            )));
        }
    }

    let resolved = git_fetcher::find_skill_dir(repo_dir, skill_id).map_err(AppError::git)?;
    if !path_guard::is_path_safe(repo_dir, &resolved) {
        return Err(AppError::invalid_input(
            "Resolved skill directory is outside the repository",
        ));
    }
    Ok(resolved)
}

pub fn resolve_skillssh_install_target(
    store: &SkillStore,
    source_ref: &str,
    skill_id: &str,
) -> Result<(String, PathBuf), AppError> {
    resolve_install_target(store, "skillssh", source_ref, skill_id)
}

fn resolve_install_target(
    store: &SkillStore,
    source_type: &str,
    source_ref: &str,
    skill_id: &str,
) -> Result<(String, PathBuf), AppError> {
    if let Some(existing) = store
        .get_skill_by_source_ref(source_type, source_ref)
        .map_err(AppError::db)?
    {
        return Ok((existing.name, PathBuf::from(existing.central_path)));
    }

    let base_name = skill_id.trim();
    if base_name.is_empty() {
        return Err(AppError::invalid_input("Skill id is empty"));
    }

    let mut attempt = 1;
    loop {
        let candidate_name = if attempt == 1 {
            base_name.to_string()
        } else {
            format!("{base_name}-{attempt}")
        };
        let candidate_path = central_repo::skills_dir().join(&candidate_name);
        let candidate_path_str = candidate_path.to_string_lossy().to_string();
        let occupied = candidate_path.exists()
            || store
                .get_skill_by_central_path(&candidate_path_str)
                .map_err(AppError::db)?
                .is_some();

        if !occupied {
            return Ok((candidate_name, candidate_path));
        }

        attempt += 1;
    }
}

pub fn staged_path_for(central_path: &str) -> PathBuf {
    let path = PathBuf::from(central_path);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "skill".to_string());
    path.with_file_name(format!(".{file_name}.staged-{}", uuid::Uuid::new_v4()))
}

pub fn swap_skill_directory(staged_path: &Path, current_path: &Path) -> Result<(), AppError> {
    let backup_path = current_path.with_file_name(format!(
        ".{}.backup-{}",
        current_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "skill".to_string()),
        uuid::Uuid::new_v4()
    ));

    if current_path.exists() {
        std::fs::rename(current_path, &backup_path)?;
    }

    if let Err(err) = std::fs::rename(staged_path, current_path) {
        if backup_path.exists() {
            let _ = std::fs::rename(&backup_path, current_path);
        }
        let _ = remove_path_if_exists(staged_path);
        return Err(err.into());
    }

    remove_path_if_exists(&backup_path)?;
    Ok(())
}

pub fn resync_copy_targets(store: &SkillStore, skill_id: &str) -> Result<(), AppError> {
    let skill = store
        .get_skill_by_id(skill_id)
        .map_err(AppError::db)?
        .ok_or_else(|| AppError::not_found("Skill not found"))?;
    let source = PathBuf::from(&skill.central_path);
    let targets = store
        .get_targets_for_skill(skill_id)
        .map_err(AppError::db)?;

    for target in targets {
        if target.mode != "copy" {
            continue;
        }

        sync_engine::sync_skill(
            &source,
            Path::new(&target.target_path),
            sync_engine::SyncMode::Copy,
            sync_engine::ReplacePolicy::Recorded {
                mode: target.mode.as_str(),
            },
        )
        .map_err(AppError::io)?;

        let updated_target = SkillTargetRecord {
            synced_at: Some(chrono::Utc::now().timestamp_millis()),
            status: "ok".to_string(),
            last_error: None,
            source_hash: skill.content_hash.clone(),
            ..target
        };
        store.insert_target(&updated_target).map_err(AppError::db)?;
    }

    Ok(())
}

// ── Check & update ──

#[derive(Clone, PartialEq, Eq, Hash)]
struct RemoteKey {
    clone_url: String,
    branch: Option<String>,
}

impl From<GitSkillSource> for RemoteKey {
    fn from(source: GitSkillSource) -> Self {
        RemoteKey {
            clone_url: source.clone_url,
            branch: source.branch,
        }
    }
}

impl RemoteKey {
    fn matches(&self, source: &GitSkillSource) -> bool {
        self.clone_url == source.clone_url && self.branch == source.branch
    }
}

#[derive(Clone)]
pub struct PrefetchedRemote {
    key: RemoteKey,
    result: Result<String, String>,
}

pub fn git_source_from_skill(skill: &SkillRecord) -> Result<GitSkillSource, AppError> {
    if let Some(resolved) = &skill.source_ref_resolved {
        return Ok(GitSkillSource {
            clone_url: resolved.clone(),
            branch: skill.source_branch.clone(),
            subpath: skill.source_subpath.clone(),
            locator_skill_id: skill_ssh_id(skill),
        });
    }

    match skill.source_type.as_str() {
        "git" => {
            let source_ref = skill
                .source_ref
                .as_ref()
                .ok_or_else(|| AppError::invalid_input("Git skill is missing its source URL"))?;
            let parsed = git_fetcher::parse_git_source(source_ref);
            Ok(GitSkillSource {
                clone_url: parsed.clone_url,
                branch: skill.source_branch.clone().or(parsed.branch),
                subpath: skill.source_subpath.clone().or(parsed.subpath),
                locator_skill_id: None,
            })
        }
        "skillssh" => {
            let source_ref = skill.source_ref.as_ref().ok_or_else(|| {
                AppError::invalid_input("skills.sh skill is missing its source reference")
            })?;
            let (repo_source, fallback_skill_id) = source_ref
                .rsplit_once('/')
                .ok_or_else(|| AppError::invalid_input("Invalid skills.sh source reference"))?;
            Ok(GitSkillSource {
                clone_url: format!("https://github.com/{repo_source}.git"),
                branch: skill.source_branch.clone(),
                subpath: skill.source_subpath.clone(),
                locator_skill_id: Some(fallback_skill_id.to_string()),
            })
        }
        _ => Err(AppError::invalid_input(
            "Skill does not support git-based updates",
        )),
    }
}

fn skill_ssh_id(skill: &SkillRecord) -> Option<String> {
    if skill.source_type != "skillssh" {
        return None;
    }

    skill.source_ref.as_deref().and_then(|source_ref| {
        source_ref
            .rsplit_once('/')
            .map(|(_, skill_id)| skill_id.to_string())
    })
}

fn should_skip_update_check(
    store: &SkillStore,
    skill: &SkillRecord,
    force: bool,
) -> Result<bool, AppError> {
    if force {
        return Ok(false);
    }

    let ttl_minutes = store
        .get_setting("update_check_ttl_minutes")
        .map_err(AppError::db)?
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(60);
    let ttl_ms = ttl_minutes * 60 * 1000;
    let stable_status = !matches!(
        skill.update_status.as_str(),
        "unknown" | "checking" | "updating" | "error"
    );

    Ok(stable_status
        && skill
            .last_checked_at
            .map(|checked| chrono::Utc::now().timestamp_millis() - checked < ttl_ms)
            .unwrap_or(false))
}

pub fn prefetch_skill_remote(
    store: &SkillStore,
    skill_id: &str,
    force: bool,
    proxy_url: Option<&str>,
) -> Option<PrefetchedRemote> {
    let skill = store.get_skill_by_id(skill_id).ok().flatten()?;
    if !matches!(skill.source_type.as_str(), "git" | "skillssh") {
        return None;
    }
    if should_skip_update_check(store, &skill, force).unwrap_or(false) {
        return None;
    }
    let key = RemoteKey::from(git_source_from_skill(&skill).ok()?);
    let result = git_fetcher::resolve_remote_revision(
        &key.clone_url,
        key.branch.as_deref(),
        proxy_url,
    )
    .map_err(|err| err.to_string());
    Some(PrefetchedRemote { key, result })
}

pub fn check_skill_update_with_remote(
    store: &SkillStore,
    skill_id: &str,
    force: bool,
    prefetched: Option<PrefetchedRemote>,
) -> Result<ManagedSkillDto, AppError> {
    let skill = store
        .get_skill_by_id(skill_id)
        .map_err(AppError::db)?
        .ok_or_else(|| AppError::not_found("Skill not found"))?;

    if should_skip_update_check(store, &skill, force)? {
        return managed_skill_by_id(store, skill_id);
    }

    match skill.source_type.as_str() {
        "git" | "skillssh" => {
            let git_source = git_source_from_skill(&skill)?;
            let metadata_updated = skill.source_ref_resolved.as_deref()
                != Some(git_source.clone_url.as_str())
                || skill.source_subpath.as_deref() != git_source.subpath.as_deref()
                || skill.source_branch.as_deref() != git_source.branch.as_deref();
            if metadata_updated {
                store
                    .update_skill_source_metadata(
                        &skill.id,
                        Some(&git_source.clone_url),
                        git_source.subpath.as_deref(),
                        git_source.branch.as_deref(),
                        skill.source_revision.as_deref(),
                    )
                    .map_err(AppError::db)?;
            }

            let Some(remote_result) = prefetched
                .filter(|prefetched| prefetched.key.matches(&git_source))
                .map(|prefetched| prefetched.result)
            else {
                return managed_skill_by_id(store, skill_id);
            };
            match remote_result {
                Ok(remote_revision) => {
                    let update_status = match skill.source_revision.as_deref() {
                        Some(current) if current == remote_revision => "up_to_date",
                        Some(_) => "update_available",
                        None => "unknown",
                    };
                    store
                        .update_skill_check_state(
                            &skill.id,
                            Some(&remote_revision),
                            update_status,
                            None,
                        )
                        .map_err(AppError::db)?;
                }
                Err(message) => {
                    store
                        .update_skill_check_state(
                            &skill.id,
                            skill.remote_revision.as_deref(),
                            "error",
                            Some(&message),
                        )
                        .map_err(AppError::db)?;
                    return Err(AppError::git(message));
                }
            }
        }
        "local" | "import" => {
            let (status, error): (&str, Option<String>) = match skill.source_ref.as_deref() {
                Some(path) => {
                    let source_path = Path::new(path);
                    if !source_path.exists() {
                        (
                            "source_missing",
                            Some("Original source path no longer exists".to_string()),
                        )
                    } else {
                        match installer::hash_local_source(source_path) {
                            Ok(live_hash) => local_source_status(&skill, source_path, &live_hash),
                            Err(err) => ("error", Some(err.to_string())),
                        }
                    }
                }
                None => ("local_only", None),
            };
            store
                .update_skill_check_state(&skill.id, None, status, error.as_deref())
                .map_err(AppError::db)?;
        }
        "well-known" => {}
        _ => {
            store
                .update_skill_check_state(&skill.id, None, "unknown", None)
                .map_err(AppError::db)?;
        }
    }

    managed_skill_by_id(store, skill_id)
}

fn local_source_status(
    skill: &SkillRecord,
    source: &Path,
    live_hash: &str,
) -> (&'static str, Option<String>) {
    match skill.content_hash.as_deref() {
        None => ("local_only", None),
        Some(stored) if stored == live_hash => ("up_to_date", None),
        Some(_) if differs_only_by_line_endings(source, Path::new(&skill.central_path)) => {
            ("up_to_date", None)
        }
        Some(_) => ("update_available", None),
    }
}

fn differs_only_by_line_endings(source: &Path, central: &Path) -> bool {
    let (Ok(source_hash), Ok(central_hash)) = (
        installer::hash_local_source_eol_insensitive(source),
        content_hash::hash_directory_eol_insensitive(central),
    ) else {
        return false;
    };
    source_hash == central_hash
}

const MAX_CHECK_CONCURRENCY: usize = 8;

pub fn check_all_skill_updates_impl(
    store: &SkillStore,
    force_check: bool,
    proxy_url: Option<String>,
) -> Result<(), AppError> {
    let skills = store.get_all_skills().map_err(AppError::db)?;

    let mut remotes: HashSet<RemoteKey> = HashSet::new();
    for skill in &skills {
        if !matches!(skill.source_type.as_str(), "git" | "skillssh") {
            continue;
        }
        match should_skip_update_check(store, skill, force_check) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(err) => log::warn!(
                "check all: skip-decision for {} failed, checking anyway: {}",
                skill.id,
                err.message
            ),
        }
        if let Ok(source) = git_source_from_skill(skill) {
            remotes.insert(RemoteKey::from(source));
        }
    }
    let remote_revisions = if remotes.is_empty() {
        HashMap::new()
    } else {
        resolve_remotes_concurrent(remotes.into_iter().collect(), proxy_url.clone())
    };

    let mut failed = Vec::new();
    for skill in &skills {
        let prefetched = if matches!(skill.source_type.as_str(), "git" | "skillssh") {
            git_source_from_skill(skill).ok().and_then(|source| {
                let key = RemoteKey::from(source);
                remote_revisions
                    .get(&key)
                    .cloned()
                    .map(|result| PrefetchedRemote { key, result })
            })
        } else {
            None
        };
        let _lock = match RepoLock::acquire("check skill update") {
            Ok(lock) => lock,
            Err(err) => {
                failed.push(format!("{}: {}", skill.id, err));
                continue;
            }
        };
        if let Err(err) =
            check_skill_update_with_remote(store, &skill.id, force_check, prefetched)
        {
            log::warn!("check all: {} failed: {}", skill.id, err.message);
            failed.push(format!("{}: {}", skill.id, err));
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(AppError::internal(format!(
            "Failed to check {} skill(s): {}",
            failed.len(),
            failed.join("; ")
        )))
    }
}

fn resolve_remotes_concurrent(
    remotes: Vec<RemoteKey>,
    proxy_url: Option<String>,
) -> HashMap<RemoteKey, Result<String, String>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Mutex<HashMap<RemoteKey, Result<String, String>>> =
        Mutex::new(HashMap::with_capacity(remotes.len()));
    let worker_count = MAX_CHECK_CONCURRENCY.min(remotes.len().max(1));

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                let Some(key) = remotes.get(idx) else { break };

                let resolved = git_fetcher::resolve_remote_revision(
                    &key.clone_url,
                    key.branch.as_deref(),
                    proxy_url.as_deref(),
                )
                .map_err(|err| err.to_string());
                if let Ok(mut map) = results.lock() {
                    map.insert(key.clone(), resolved);
                }
            });
        }
    });

    results.into_inner().unwrap_or_default()
}

// ── Update / reimport / batch ──

pub fn update_git_skill_internal(
    store: &SkillStore,
    skill_id: &str,
    proxy_url: Option<&str>,
    cancel: Option<&Arc<AtomicBool>>,
    approved_removals: Option<&str>,
) -> Result<UpdateSkillResult, AppError> {
    let skill = store
        .get_skill_by_id(skill_id)
        .map_err(AppError::db)?
        .ok_or_else(|| AppError::not_found("Skill not found"))?;

    if !matches!(skill.source_type.as_str(), "git" | "skillssh") {
        return Err(AppError::invalid_input(
            "Only git-based skills can be updated",
        ));
    }

    let git_source = git_source_from_skill(&skill)?;
    git_fetcher::validate_git_url(&git_source.clone_url).map_err(AppError::git)?;
    let remote_revision = git_fetcher::resolve_remote_revision(
        &git_source.clone_url,
        git_source.branch.as_deref(),
        proxy_url,
    )
    .map_err(|e| {
        let message = e.to_string();
        let _ = store.update_skill_check_state(
            skill_id,
            skill.remote_revision.as_deref(),
            "error",
            Some(&message),
        );
        AppError::git(message)
    })?;

    store
        .update_skill_update_status(skill_id, "updating")
        .map_err(AppError::db)?;

    let temp_dir = git_fetcher::clone_repo_ref_scoped(
        &git_source.clone_url,
        git_source.branch.as_deref(),
        git_source.subpath.as_deref(),
        cancel,
        proxy_url,
        None,
    )
    .map_err(AppError::classify_git_error)?;
    let update_result = (|| -> Result<UpdateOutcome, AppError> {
        git_fetcher::checkout_revision(&temp_dir, &remote_revision).map_err(AppError::git)?;
        let skill_dir = resolve_skill_dir(
            &temp_dir,
            git_source.subpath.as_deref(),
            git_source.locator_skill_id.as_deref(),
        )?;

        let new_hash = content_hash::hash_directory(&skill_dir).map_err(AppError::io)?;
        let content_changed = skill.content_hash.as_deref() != Some(new_hash.as_str());
        let source_subpath = git_fetcher::relative_subpath(&temp_dir, &skill_dir);
        let _lock = RepoLock::acquire_foreground("update installed skill")
            .map_err(AppError::db)?;

        let staged_path = staged_path_for(&skill.central_path);
        let install_result = if content_changed {
            Some(
                installer::install_skill_dir_to_destination(&skill_dir, &skill.name, &staged_path)
                    .inspect_err(|_| {
                        let _ = remove_path_if_exists(&staged_path);
                    })
                    .map_err(AppError::io)?,
            )
        } else {
            None
        };
        let staged_guard = StagedPathGuard::new(&staged_path, install_result.is_some());

        let pending = pending_removals_for(
            store,
            &skill,
            install_result.is_some().then_some(staged_path.as_path()),
        )?;

        let approval = removal_approval_token(&remote_revision, &pending);
        if !pending.is_empty() && approved_removals != Some(approval.as_str()) {
            store
                .update_skill_check_state(
                    &skill.id,
                    Some(&remote_revision),
                    "update_available",
                    None,
                )
                .map_err(AppError::db)?;
            return Ok(UpdateOutcome::Held { pending, approval });
        }

        if let Some(install_result) = install_result {
            swap_skill_directory(&staged_path, Path::new(&skill.central_path))?;
            staged_guard.release();

            store
                .update_skill_source_metadata(
                    &skill.id,
                    Some(&git_source.clone_url),
                    source_subpath.as_deref(),
                    git_source.branch.as_deref(),
                    Some(&remote_revision),
                )
                .map_err(AppError::db)?;
            store
                .update_skill_after_install(
                    &skill.id,
                    &skill.name,
                    install_result.description.as_deref(),
                    Some(&remote_revision),
                    Some(&remote_revision),
                    Some(&install_result.content_hash),
                    "up_to_date",
                )
                .map_err(AppError::db)?;
            resync_copy_targets(store, &skill.id)?;
            sync_metadata::write_all_from_db_unlocked(store).map_err(AppError::db)?;
        } else {
            store
                .update_skill_source_metadata(
                    &skill.id,
                    Some(&git_source.clone_url),
                    source_subpath.as_deref(),
                    git_source.branch.as_deref(),
                    Some(&remote_revision),
                )
                .map_err(AppError::db)?;
            store
                .update_skill_check_state(&skill.id, Some(&remote_revision), "up_to_date", None)
                .map_err(AppError::db)?;
            resync_copy_targets(store, &skill.id)?;
            sync_metadata::write_all_from_db_unlocked(store).map_err(AppError::db)?;
        }
        Ok(UpdateOutcome::Applied { content_changed })
    })();
    git_fetcher::cleanup_temp(&temp_dir);

    match update_result {
        Ok(outcome) => {
            let (content_changed, pending_removals, removal_approval) = match outcome {
                UpdateOutcome::Applied { content_changed } => (content_changed, Vec::new(), None),
                UpdateOutcome::Held { pending, approval } => (false, pending, Some(approval)),
            };
            let skill = managed_skill_by_id(store, skill_id)?;
            Ok(UpdateSkillResult {
                skill,
                content_changed,
                pending_removals,
                removal_approval,
            })
        }
        Err(e) => {
            let _ = store.update_skill_check_state(
                skill_id,
                Some(&remote_revision),
                "error",
                Some(&e.message),
            );
            Err(e)
        }
    }
}

pub fn reimport_local_skill_internal(
    store: &SkillStore,
    skill_id: &str,
    approved_removals: Option<&str>,
) -> Result<ReimportSkillResult, AppError> {
    let skill = store
        .get_skill_by_id(skill_id)
        .map_err(AppError::db)?
        .ok_or_else(|| AppError::not_found("Skill not found"))?;

    if !matches!(skill.source_type.as_str(), "local" | "import") {
        return Err(AppError::invalid_input(
            "Only local skills can be reimported",
        ));
    }

    let source_path = skill
        .source_ref
        .clone()
        .ok_or_else(|| AppError::not_found("Local skill is missing its original source path"))?;
    let path = PathBuf::from(&source_path);
    if !path.exists() {
        store
            .update_skill_check_state(
                &skill.id,
                None,
                "source_missing",
                Some("Original source path no longer exists"),
            )
            .map_err(AppError::db)?;
        return Err(AppError::not_found("Original source path no longer exists"));
    }

    store
        .update_skill_update_status(skill_id, "updating")
        .map_err(AppError::db)?;

    let result = (|| -> Result<(Vec<PendingRemoval>, Option<String>), AppError> {
        let _lock = RepoLock::acquire_foreground("reimport local skill").map_err(AppError::db)?;
        let staged_path = staged_path_for(&skill.central_path);
        let install_result =
            installer::install_from_local_to_destination(&path, Some(&skill.name), &staged_path)
                .inspect_err(|_| {
                    let _ = remove_path_if_exists(&staged_path);
                })
                .map_err(AppError::io)?;
        let staged_guard = StagedPathGuard::new(&staged_path, true);

        let pending = pending_removals_for(store, &skill, Some(&staged_path))?;
        let approval = removal_approval_token(REIMPORT_APPROVAL_DOMAIN, &pending);
        if !pending.is_empty() && approved_removals != Some(approval.as_str()) {
            store
                .update_skill_update_status(&skill.id, &skill.update_status)
                .map_err(AppError::db)?;
            return Ok((pending, Some(approval)));
        }

        swap_skill_directory(&staged_path, Path::new(&skill.central_path))?;
        staged_guard.release();
        store
            .update_skill_after_install(
                &skill.id,
                &skill.name,
                install_result.description.as_deref(),
                None,
                None,
                Some(&install_result.content_hash),
                "local_only",
            )
            .map_err(AppError::db)?;
        resync_copy_targets(store, &skill.id)?;
        sync_metadata::write_all_from_db_unlocked(store).map_err(AppError::db)?;
        Ok((Vec::new(), None))
    })();

    match result {
        Ok((pending_removals, removal_approval)) => Ok(ReimportSkillResult {
            skill: managed_skill_by_id(store, skill_id)?,
            pending_removals,
            removal_approval,
        }),
        Err(e) => {
            let _ = store.update_skill_check_state(skill_id, None, "error", Some(&e.message));
            Err(e)
        }
    }
}

pub fn batch_update_skills_impl(
    store: &SkillStore,
    skill_ids: &[String],
    proxy_url: Option<&str>,
) -> Result<BatchUpdateSkillsResult, AppError> {
    let mut refreshed = 0usize;
    let mut unchanged = 0usize;
    let mut failed = Vec::new();
    let mut held_back = Vec::new();

    for skill_id in skill_ids {
        let skill = match store.get_skill_by_id(skill_id).map_err(AppError::db)? {
            Some(skill) => skill,
            None => {
                failed.push(format!("{skill_id}: Skill not found"));
                continue;
            }
        };

        match skill.source_type.as_str() {
            "git" | "skillssh" => {
                let outcome = update_git_skill_internal(
                    store,
                    skill_id,
                    proxy_url,
                    None,
                    None,
                );
                match outcome {
                    Ok(result) if !result.pending_removals.is_empty() => {
                        held_back.push(skill.name.clone());
                    }
                    Ok(result) => {
                        if result.content_changed {
                            refreshed += 1;
                        } else {
                            unchanged += 1;
                        }
                    }
                    Err(err) => failed.push(format!("{}: {}", skill.name, err.message)),
                }
            }
            "local" | "import" => {
                let outcome = reimport_local_skill_internal(store, skill_id, None);
                match outcome {
                    Ok(result) if !result.pending_removals.is_empty() => {
                        held_back.push(skill.name.clone());
                    }
                    Ok(_) => refreshed += 1,
                    Err(err) => failed.push(format!("{}: {}", skill.name, err.message)),
                }
            }
            _ => failed.push(format!("{}: Source type cannot be refreshed", skill.name)),
        }
    }

    Ok(BatchUpdateSkillsResult {
        refreshed,
        unchanged,
        failed,
        held_back,
    })
}

// ── Scan local agent dirs (headless port of commands/scan.rs) ──

#[derive(Debug, Serialize)]
pub struct ScanResultDto {
    pub tools_scanned: usize,
    pub skills_found: usize,
    pub groups: Vec<crate::core::scanner::DiscoveredGroup>,
}

fn canonicalize_lossy(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

fn match_imported_skill_id(
    rec: &crate::core::skill_store::DiscoveredSkillRecord,
    managed_skills: &[SkillRecord],
) -> Option<String> {
    let found_path = canonicalize_lossy(&rec.found_path);
    if let Some(existing) = managed_skills.iter().find(|skill| {
        skill.source_ref.as_deref().map(|s| canonicalize_lossy(s)).as_ref() == Some(&found_path)
            || skill
                .source_ref_resolved
                .as_deref()
                .map(|s| canonicalize_lossy(s))
                .as_ref()
                == Some(&found_path)
    }) {
        return Some(existing.id.clone());
    }

    if let Some(fingerprint) = rec.fingerprint.as_deref() {
        if let Some(existing) = managed_skills
            .iter()
            .find(|skill| skill.content_hash.as_deref() == Some(fingerprint))
        {
            return Some(existing.id.clone());
        }
    }

    None
}

pub fn scan_local_skills_impl(store: &SkillStore) -> Result<ScanResultDto, AppError> {
    let all_targets = store.get_all_targets().map_err(AppError::db)?;
    let managed_paths: Vec<String> = all_targets.iter().map(|t| t.target_path.clone()).collect();
    let managed_skills = store.get_all_skills().map_err(AppError::db)?;

    let adapters = crate::core::tool_adapters::all_tool_adapters(store);
    let mut plan = crate::core::scanner::scan_local_skills_with_adapters(
        &managed_paths,
        &adapters,
    )
    .map_err(AppError::io)?;

    for rec in &mut plan.discovered {
        rec.imported_skill_id = match_imported_skill_id(rec, &managed_skills);
    }

    store.clear_discovered().map_err(AppError::db)?;
    for rec in &plan.discovered {
        store.insert_discovered(rec).map_err(AppError::db)?;
    }

    let all_discovered = store.get_all_discovered().map_err(AppError::db)?;
    let groups = crate::core::scanner::group_discovered(&all_discovered);

    Ok(ScanResultDto {
        tools_scanned: plan.tools_scanned,
        skills_found: plan.skills_found,
        groups,
    })
}

pub fn import_existing_skill_impl(
    store: &SkillStore,
    source_path: &str,
    name: Option<&str>,
) -> Result<(), AppError> {
    sync_metadata::with_repo_lock("import existing skill", || {
        let path = PathBuf::from(source_path);
        let resolved_name =
            crate::core::installer::resolve_local_skill_name(&path, name)?;

        let result = crate::core::installer::install_from_local(&path, Some(&resolved_name))?;

        if store
            .get_skill_by_central_path(&result.central_path.to_string_lossy())?
            .is_some()
        {
            return Ok(());
        }

        let now = chrono::Utc::now().timestamp_millis();
        let id = uuid::Uuid::new_v4().to_string();

        let record = SkillRecord {
            id: id.clone(),
            name: result.name,
            description: result.description,
            source_type: "import".to_string(),
            source_ref: Some(source_path.to_string()),
            source_ref_resolved: None,
            source_subpath: None,
            source_branch: None,
            source_revision: None,
            remote_revision: None,
            central_path: result.central_path.to_string_lossy().to_string(),
            content_hash: Some(result.content_hash),
            enabled: true,
            created_at: now,
            updated_at: now,
            status: "ok".to_string(),
            update_status: "local_only".to_string(),
            last_checked_at: None,
            last_check_error: None,
        };

        store.insert_skill(&record)?;

        sync_metadata::write_all_from_db_unlocked(store)
    })
    .map_err(AppError::io)
}

pub fn import_all_discovered_impl(store: &SkillStore) -> Result<(), AppError> {
    sync_metadata::with_repo_lock("import all discovered skills", || {
        let discovered = store.get_all_discovered()?;
        let groups = crate::core::scanner::group_discovered(&discovered);

        let mut changed = false;

        for group in groups {
            if group.imported {
                continue;
            }
            if let Some(first) = group.locations.first() {
                let path = PathBuf::from(&first.found_path);

                if let Ok(result) =
                    crate::core::installer::install_from_local(&path, Some(&group.name))
                {
                    if store
                        .get_skill_by_central_path(&result.central_path.to_string_lossy())?
                        .is_some()
                    {
                        continue;
                    }

                    let now = chrono::Utc::now().timestamp_millis();
                    let id = uuid::Uuid::new_v4().to_string();
                    let record = SkillRecord {
                        id,
                        name: result.name,
                        description: result.description,
                        source_type: "import".to_string(),
                        source_ref: Some(first.found_path.clone()),
                        source_ref_resolved: None,
                        source_subpath: None,
                        source_branch: None,
                        source_revision: None,
                        remote_revision: None,
                        central_path: result.central_path.to_string_lossy().to_string(),
                        content_hash: Some(result.content_hash),
                        enabled: true,
                        created_at: now,
                        updated_at: now,
                        status: "ok".to_string(),
                        update_status: "local_only".to_string(),
                        last_checked_at: None,
                        last_check_error: None,
                    };
                    store.insert_skill(&record)?;
                    changed = true;
                }
            }
        }

        if changed {
            sync_metadata::write_all_from_db_unlocked(store)?;
        }

        Ok(())
    })
    .map_err(AppError::io)
}

// ── Tags ──

pub fn set_skill_tags_internal(
    store: &SkillStore,
    skill_id: &str,
    tags: &[String],
) -> Result<(), AppError> {
    let mut normalized = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if !tag.is_empty() && !normalized.iter().any(|existing| existing == tag) {
            normalized.push(tag.to_string());
        }
    }

    sync_metadata::with_repo_lock("set skill tags", || {
        store.set_tags_for_skill(skill_id, &normalized)?;
        sync_metadata::ensure_skill_metadata_unlocked(store, skill_id)
    })
    .map_err(AppError::db)
}

pub fn rename_tag_internal(
    store: &SkillStore,
    old_name: &str,
    new_name: &str,
) -> Result<Vec<String>, AppError> {
    let old_name = old_name.trim();
    let new_name = new_name.trim();
    if old_name.is_empty() || new_name.is_empty() {
        return Err(AppError::invalid_input("Tag name cannot be empty"));
    }
    if new_name == old_name {
        return Ok(Vec::new());
    }
    sync_metadata::with_repo_lock("rename tag", || {
        let affected = store.rename_tag(old_name, new_name)?;
        for skill_id in &affected {
            sync_metadata::ensure_skill_metadata_unlocked(store, skill_id)?;
        }
        Ok(affected)
    })
    .map_err(AppError::db)
}

pub fn delete_tag_internal(store: &SkillStore, name: &str) -> Result<Vec<String>, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::invalid_input("Tag name cannot be empty"));
    }
    sync_metadata::with_repo_lock("delete tag", || {
        let affected = store.delete_tag(name)?;
        for skill_id in &affected {
            sync_metadata::ensure_skill_metadata_unlocked(store, skill_id)?;
        }
        Ok(affected)
    })
    .map_err(AppError::db)
}

// ── Presets ──

#[derive(Debug, Serialize)]
pub struct PresetDto {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub sort_order: i32,
    pub skill_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

fn preset_dto(store: &SkillStore, scenario: crate::core::skill_store::ScenarioRecord) -> PresetDto {
    let skill_count = store.count_skills_for_scenario(&scenario.id).unwrap_or(0);
    PresetDto {
        id: scenario.id,
        name: scenario.name,
        description: scenario.description,
        icon: scenario.icon,
        sort_order: scenario.sort_order,
        skill_count,
        created_at: scenario.created_at,
        updated_at: scenario.updated_at,
    }
}

pub fn get_presets(store: &SkillStore) -> Result<Vec<PresetDto>, AppError> {
    let scenarios = store.get_all_scenarios().map_err(AppError::db)?;
    Ok(scenarios.into_iter().map(|s| preset_dto(store, s)).collect())
}

pub fn get_active_preset(store: &SkillStore) -> Result<Option<PresetDto>, AppError> {
    let active_id = store.get_active_scenario_id().map_err(AppError::db)?;
    if let Some(id) = active_id {
        let scenarios = store.get_all_scenarios().map_err(AppError::db)?;
        if let Some(s) = scenarios.into_iter().find(|s| s.id == id) {
            return Ok(Some(preset_dto(store, s)));
        }
    }
    Ok(None)
}

pub fn create_preset_internal(
    store: &SkillStore,
    name: &str,
    description: Option<&str>,
    icon: Option<&str>,
) -> Result<PresetDto, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::invalid_input("Preset name cannot be empty"));
    }

    let now = chrono::Utc::now().timestamp_millis();
    let id = uuid::Uuid::new_v4().to_string();
    let record = crate::core::skill_store::ScenarioRecord {
        id: id.clone(),
        name: name.to_string(),
        description: description
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        icon: icon
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        sort_order: 999,
        created_at: now,
        updated_at: now,
    };

    sync_metadata::with_repo_lock("create preset", || {
        store.insert_scenario(&record)?;
        sync_metadata::write_all_from_db_unlocked(store)
    })
    .map_err(AppError::db)?;
    Ok(preset_dto(store, record))
}

pub fn update_preset_internal(
    store: &SkillStore,
    id: &str,
    name: &str,
    description: Option<&str>,
    icon: Option<&str>,
) -> Result<(), AppError> {
    scenario_service::ensure_scenario_exists(store, id)?;
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::invalid_input("Preset name cannot be empty"));
    }
    let description = description.map(str::trim).filter(|value| !value.is_empty());
    let icon = icon.map(str::trim).filter(|value| !value.is_empty());
    sync_metadata::with_repo_lock("update preset", || {
        store.update_scenario(id, name, description, icon)?;
        sync_metadata::write_all_from_db_unlocked(store)
    })
    .map_err(AppError::db)
}

pub fn delete_preset_internal(store: &SkillStore, id: &str) -> Result<(), AppError> {
    scenario_service::ensure_scenario_exists(store, id)?;
    let was_active = store
        .get_active_scenario_id()
        .map_err(AppError::db)?
        .as_deref()
        == Some(id);

    sync_metadata::with_repo_lock("delete preset", || {
        if was_active {
            store.clear_active_scenario()?;
        }
        store.delete_scenario(id)?;
        sync_metadata::write_all_from_db_unlocked(store)
    })
    .map_err(AppError::db)
}

pub fn apply_preset_to_default(store: &SkillStore, id: &str) -> Result<(), AppError> {
    scenario_service::apply_scenario_to_default(store, id)
        .and_then(scenario_service::refusals_to_error)
}

pub fn set_preset_skills_internal(
    store: &SkillStore,
    preset_id: &str,
    skill_ids: &[String],
    add: bool,
) -> Result<(), AppError> {
    scenario_service::ensure_scenario_exists(store, preset_id)?;
    sync_metadata::with_repo_lock(
        if add {
            "add skills to preset"
        } else {
            "remove skills from preset"
        },
        || {
            for skill_id in skill_ids {
                if store.get_skill_by_id(skill_id)?.is_none() {
                    return Err(anyhow::anyhow!("Skill not found: {skill_id}"));
                }
                if add {
                    store.add_skill_to_scenario(preset_id, skill_id)?;
                } else {
                    store.remove_skill_from_scenario(preset_id, skill_id)?;
                }
            }
            sync_metadata::write_all_from_db_unlocked(store)
        },
    )
    .map_err(AppError::db)
}
