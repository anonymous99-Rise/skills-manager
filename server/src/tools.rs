//! Headless port of commands/tools.rs + commands/sync.rs (agent management,
//! skill→tool sync, per-preset tool toggles). Tray/refresh side effects are
//! dropped; everything else mirrors the desktop implementation.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::core::{
    error::AppError,
    scenario_service,
    skill_store::SkillStore,
    sync_engine,
    sync_metadata,
    tool_adapters::{self, CustomToolDef},
    tool_service::{
        self, get_custom_tool_paths, get_custom_tool_project_paths, get_custom_tools,
        get_disabled_tools, get_tool_order, normalize_project_relative_skills_dir_input,
        normalize_skills_dir_input, set_custom_tool_paths, set_custom_tool_project_paths,
        set_custom_tools, set_disabled_tools, set_tool_order,
    },
};

fn unsync_all_for_tool(store: &SkillStore, tool_key: &str) {
    let targets = store.get_all_targets().unwrap_or_default();
    for target in targets.iter().filter(|t| t.tool == tool_key) {
        // Tools can share one skills directory; another tool's live
        // deployment at this exact path is not ours to remove.
        let still_referenced = targets
            .iter()
            .any(|other| other.tool != tool_key && other.target_path == target.target_path);
        if !still_referenced {
            sync_engine::remove_recorded_target_or_warn(
                &PathBuf::from(&target.target_path),
                &target.mode,
            );
        }
        store.delete_target(&target.skill_id, tool_key).ok();
    }
}

fn sync_active_scenario_to_tool(store: &SkillStore, tool_key: &str) {
    scenario_service::sync_active_scenario_to_tool(store, tool_key)
}

fn reconcile_tool_sync_after_path_change(store: &SkillStore, tool_key: &str) {
    unsync_all_for_tool(store, tool_key);
    let disabled = get_disabled_tools(store);
    if !disabled.contains(&tool_key.to_string()) {
        sync_active_scenario_to_tool(store, tool_key);
    }
}

pub fn set_tool_enabled_internal(
    store: &SkillStore,
    key: &str,
    enabled: bool,
) -> Result<(), AppError> {
    if tool_adapters::find_adapter_with_store(store, key).is_none() {
        return Err(AppError::not_found(format!("Unknown agent: {key}")));
    }

    let mut disabled = get_disabled_tools(store);
    if enabled {
        disabled.retain(|item| item != key);
        set_disabled_tools(store, &disabled)?;
        sync_active_scenario_to_tool(store, key);
        Ok(())
    } else {
        if !disabled.iter().any(|item| item == key) {
            disabled.push(key.to_string());
        }
        unsync_all_for_tool(store, key);
        set_disabled_tools(store, &disabled)
    }
}

pub fn set_all_tools_enabled_internal(
    store: &SkillStore,
    enabled: bool,
) -> Result<(), AppError> {
    if enabled {
        set_disabled_tools(store, &[])?;
        if let Ok(Some(active_id)) = store.get_active_scenario_id() {
            match scenario_service::sync_scenario_skills(store, &active_id) {
                Ok(refusals) => {
                    for refusal in refusals {
                        log::warn!("enable-all-tools sync skipped a target: {refusal}");
                    }
                }
                Err(e) => log::warn!("enable-all-tools sync failed: {e}"),
            }
        }
        Ok(())
    } else {
        let adapters = tool_adapters::all_tool_adapters(store);
        let all_keys: Vec<String> = adapters.iter().map(|a| a.key.clone()).collect();
        for adapter in &adapters {
            unsync_all_for_tool(store, &adapter.key);
        }
        set_disabled_tools(store, &all_keys)
    }
}

fn is_builtin_key(key: &str) -> bool {
    tool_adapters::default_tool_adapters()
        .iter()
        .any(|a| a.key == key)
}

fn apply_tool_skills_dir(store: &SkillStore, key: &str, path: &str) -> Result<(), AppError> {
    let key = key.trim().to_string();
    let path = normalize_skills_dir_input(path)?;
    if key.is_empty() || path.is_empty() {
        return Err(AppError::invalid_input("Key and path are required"));
    }

    let old_adapter = tool_adapters::find_adapter_with_store(store, &key)
        .ok_or_else(|| AppError::not_found(format!("Unknown tool: {key}")))?;
    let old_skills_dir = old_adapter.skills_dir();

    let mut customs = get_custom_tools(store);
    match customs.iter_mut().find(|c| c.key == key) {
        Some(custom) if !is_builtin_key(&key) => {
            custom.skills_dir = path;
            set_custom_tools(store, &customs)?;
        }
        _ => {
            let mut paths = get_custom_tool_paths(store);
            paths.insert(key.clone(), path);
            set_custom_tool_paths(store, &paths)?;
        }
    }

    let new_adapter = tool_adapters::find_adapter_with_store(store, &key)
        .ok_or_else(|| AppError::not_found(format!("Unknown tool: {key}")))?;
    if old_skills_dir != new_adapter.skills_dir() {
        reconcile_tool_sync_after_path_change(store, &key);
    }
    Ok(())
}

fn apply_tool_project_skills_dir(
    store: &SkillStore,
    key: &str,
    project_relative_skills_dir: Option<&str>,
) -> Result<(), AppError> {
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(AppError::invalid_input("Key is required"));
    }
    let normalized =
        normalize_project_relative_skills_dir_input(project_relative_skills_dir.unwrap_or_default())?;

    let default_project_path = match tool_adapters::default_tool_adapters()
        .into_iter()
        .find(|a| a.key == key)
        .map(|a| a.project_relative_skills_dir().to_string())
    {
        Some(default_project_path) => default_project_path,
        None => {
            let mut customs = get_custom_tools(store);
            let custom = customs
                .iter_mut()
                .find(|c| c.key == key)
                .ok_or_else(|| AppError::not_found(format!("Unknown tool: {key}")))?;
            custom.project_relative_skills_dir = normalized;
            return set_custom_tools(store, &customs);
        }
    };
    let mut project_paths = get_custom_tool_project_paths(store);
    match normalized {
        Some(path) if path != default_project_path => {
            project_paths.insert(key, path);
        }
        _ => {
            project_paths.remove(&key);
        }
    }
    set_custom_tool_project_paths(store, &project_paths)
}

pub fn add_custom_tool_internal(
    store: &SkillStore,
    key: &str,
    display_name: &str,
    skills_dir: &str,
    project_relative_skills_dir: Option<&str>,
) -> Result<(), AppError> {
    let key = key.trim().to_string();
    let display_name = display_name.trim().to_string();
    let skills_dir = normalize_skills_dir_input(skills_dir)?;
    let project_relative_skills_dir =
        normalize_project_relative_skills_dir_input(project_relative_skills_dir.unwrap_or_default())?;
    if key.is_empty() || display_name.is_empty() || skills_dir.is_empty() {
        return Err(AppError::invalid_input(
            "Agent key, name and skills path are required",
        ));
    }

    let all = tool_adapters::all_tool_adapters(store);
    if all.iter().any(|a| a.key == key) {
        return Err(AppError::invalid_input(format!(
            "Agent key \"{key}\" already exists"
        )));
    }
    let mut customs = get_custom_tools(store);
    customs.push(CustomToolDef {
        key: key.clone(),
        display_name,
        skills_dir,
        project_relative_skills_dir,
        category: Default::default(),
    });
    set_custom_tools(store, &customs)?;
    reconcile_tool_sync_after_path_change(store, &key);
    Ok(())
}

pub fn remove_custom_tool_internal(store: &SkillStore, key: &str) -> Result<(), AppError> {
    unsync_all_for_tool(store, key);
    let mut customs = get_custom_tools(store);
    customs.retain(|c| c.key != key);
    set_custom_tools(store, &customs)?;
    let mut custom_paths = get_custom_tool_paths(store);
    custom_paths.remove(key);
    set_custom_tool_paths(store, &custom_paths)?;
    let mut disabled = get_disabled_tools(store);
    disabled.retain(|k| k != key);
    set_disabled_tools(store, &disabled)
}

// ── Skill ↔ tool sync (commands/sync.rs) ──

#[derive(Debug, Serialize)]
pub struct SkillToolToggleDto {
    pub tool: String,
    pub display_name: String,
    pub installed: bool,
    pub globally_enabled: bool,
    pub enabled: bool,
}

fn sync_skill_to_tool_internal(
    store: &SkillStore,
    skill_id: &str,
    tool: &str,
) -> Result<(), AppError> {
    scenario_service::sync_single_skill_to_tool(
        store,
        skill_id,
        tool,
        scenario_service::DeployIntent::Managed,
    )
}

fn unsync_skill_for_tool_in_preset(
    store: &SkillStore,
    skill_id: &str,
    tool: &str,
) -> Result<(), AppError> {
    let targets = store.get_targets_for_skill(skill_id).map_err(AppError::db)?;
    if let Some(target) = targets.iter().find(|target| target.tool == tool) {
        let still_referenced = targets
            .iter()
            .any(|other| other.tool != tool && other.target_path == target.target_path);
        if still_referenced {
            return store.delete_target(skill_id, tool).map_err(AppError::db);
        }
        sync_engine::remove_recorded_target_or_warn(
            &PathBuf::from(&target.target_path),
            &target.mode,
        );
    }
    store.delete_target(skill_id, tool).map_err(AppError::db)
}

pub fn sync_skill_to_tool_impl(
    store: &SkillStore,
    skill_id: &str,
    tool: &str,
) -> Result<(), AppError> {
    sync_skill_to_tool_internal(store, skill_id, tool)?;

    if let Ok(Some(active_id)) = store.get_active_scenario_id() {
        let skill_ids = store
            .get_skill_ids_for_scenario(&active_id)
            .map_err(AppError::db)?;
        if skill_ids.contains(&skill_id.to_string()) {
            let adapter_keys: Vec<String> = tool_adapters::enabled_installed_adapters(store)
                .iter()
                .map(|a| a.key.clone())
                .collect();
            store
                .ensure_scenario_skill_tool_defaults(&active_id, skill_id, &adapter_keys)
                .map_err(AppError::db)?;
            store
                .set_scenario_skill_tool_enabled(&active_id, skill_id, tool, true)
                .map_err(AppError::db)?;
        }
    }
    Ok(())
}

pub fn unsync_skill_from_tool_impl(
    store: &SkillStore,
    skill_id: &str,
    tool: &str,
) -> Result<(), AppError> {
    let targets = store.get_targets_for_skill(skill_id).map_err(AppError::db)?;

    if let Some(target) = targets.iter().find(|t| t.tool == tool) {
        let target_path = PathBuf::from(&target.target_path);
        let still_referenced = store
            .get_all_targets()
            .map_err(AppError::db)?
            .into_iter()
            .any(|other| {
                other.target_path == target.target_path
                    && !(other.skill_id == skill_id && other.tool == tool)
            });
        if still_referenced {
            log::debug!(
                "unsync: keeping {} (still referenced by another target)",
                target_path.display()
            );
        } else {
            match sync_engine::remove_recorded_target(&target_path, &target.mode) {
                Ok(true) => {}
                Ok(false) => log::warn!(
                    "unsync: preserving {} — no longer matches its recorded {} deployment; \
                     removing the record only",
                    target_path.display(),
                    target.mode
                ),
                Err(e) => log::warn!("unsync: failed to remove {}: {e}", target_path.display()),
            }
        }
    }

    store.delete_target(skill_id, tool).map_err(AppError::db)?;

    if let Ok(Some(active_id)) = store.get_active_scenario_id() {
        let skill_ids = store
            .get_skill_ids_for_scenario(&active_id)
            .map_err(AppError::db)?;
        if skill_ids.contains(&skill_id.to_string()) {
            let adapter_keys: Vec<String> = tool_adapters::enabled_installed_adapters(store)
                .iter()
                .map(|a| a.key.clone())
                .collect();
            store
                .ensure_scenario_skill_tool_defaults(&active_id, skill_id, &adapter_keys)
                .map_err(AppError::db)?;
            store
                .set_scenario_skill_tool_enabled(&active_id, skill_id, tool, false)
                .map_err(AppError::db)?;
        }
    }
    Ok(())
}

pub fn get_skill_tool_toggles_impl(
    store: &SkillStore,
    skill_id: &str,
    preset_id: &str,
) -> Result<Vec<SkillToolToggleDto>, AppError> {
    let skill_ids = store
        .get_skill_ids_for_scenario(preset_id)
        .map_err(AppError::db)?;
    if !skill_ids.contains(&skill_id.to_string()) {
        return Err(AppError::not_found("Skill is not enabled in this preset"));
    }

    let disabled = get_disabled_tools(store);
    let all_adapters = tool_adapters::all_tool_adapters(store);
    let default_enabled_keys: Vec<String> = all_adapters
        .iter()
        .filter(|adapter| adapter.is_installed() && !disabled.contains(&adapter.key))
        .map(|adapter| adapter.key.clone())
        .collect();
    store
        .ensure_scenario_skill_tool_defaults(preset_id, skill_id, &default_enabled_keys)
        .map_err(AppError::db)?;

    let toggles = store
        .get_scenario_skill_tool_toggles(preset_id, skill_id)
        .map_err(AppError::db)?;
    let enabled_map: HashMap<String, bool> = toggles
        .into_iter()
        .map(|toggle| (toggle.tool, toggle.enabled))
        .collect();

    Ok(all_adapters
        .into_iter()
        .map(|adapter| {
            let globally_enabled = !disabled.contains(&adapter.key);
            let available = adapter.is_installed() && globally_enabled;
            SkillToolToggleDto {
                enabled: if available {
                    enabled_map.get(&adapter.key).copied().unwrap_or(false)
                } else {
                    false
                },
                tool: adapter.key.clone(),
                display_name: adapter.display_name.clone(),
                installed: adapter.is_installed(),
                globally_enabled,
            }
        })
        .collect())
}

pub fn set_skill_tool_toggle_impl(
    store: &SkillStore,
    skill_id: &str,
    preset_id: &str,
    tool: &str,
    enabled: bool,
) -> Result<(), AppError> {
    let skill_ids = store
        .get_skill_ids_for_scenario(preset_id)
        .map_err(AppError::db)?;
    if !skill_ids.contains(&skill_id.to_string()) {
        return Err(AppError::not_found("Skill is not enabled in this preset"));
    }

    let adapter = tool_adapters::find_adapter_with_store(store, tool)
        .ok_or_else(|| AppError::not_found(format!("Unknown tool: {tool}")))?;
    let disabled = get_disabled_tools(store);
    let globally_enabled = !disabled.contains(&tool.to_string());

    if enabled {
        if !adapter.is_installed() {
            return Err(AppError::not_found(format!(
                "{} is not installed",
                adapter.display_name
            )));
        }
        if !globally_enabled {
            return Err(AppError::invalid_input(format!(
                "{} is disabled",
                adapter.display_name
            )));
        }
    }

    sync_metadata::with_repo_lock("set skill tool toggle", || {
        store.set_scenario_skill_tool_enabled(preset_id, skill_id, tool, enabled)?;
        sync_metadata::write_all_from_db_unlocked(store)
    })
    .map_err(AppError::db)?;

    let is_active = store
        .get_active_scenario_id()
        .map_err(AppError::db)?
        .as_deref()
        == Some(preset_id);
    if is_active {
        if enabled {
            sync_skill_to_tool_internal(store, skill_id, tool)?;
        } else {
            unsync_skill_for_tool_in_preset(store, skill_id, tool)?;
        }
    }

    Ok(())
}

// ── Public wrappers used by the dispatcher ──

pub fn tools_apply_skills_dir(store: &SkillStore, key: &str, path: &str) -> Result<(), AppError> {
    apply_tool_skills_dir(store, key, path)
}

pub fn tools_reset_skills_dir(store: &SkillStore, key: &str) -> Result<(), AppError> {
    let old_adapter = tool_adapters::find_adapter_with_store(store, key)
        .ok_or_else(|| AppError::not_found(format!("Unknown tool: {key}")))?;
    let old_skills_dir = old_adapter.skills_dir();

    let mut paths = get_custom_tool_paths(store);
    paths.remove(key);
    set_custom_tool_paths(store, &paths)?;

    let new_adapter = tool_adapters::find_adapter_with_store(store, key)
        .ok_or_else(|| AppError::not_found(format!("Unknown tool: {key}")))?;
    if old_skills_dir != new_adapter.skills_dir() {
        reconcile_tool_sync_after_path_change(store, key);
    }
    Ok(())
}

pub fn tools_apply_project_dir(
    store: &SkillStore,
    key: &str,
    project_relative_skills_dir: Option<&str>,
) -> Result<(), AppError> {
    apply_tool_project_skills_dir(store, key, project_relative_skills_dir)
}

pub fn tools_reset_project_dir(store: &SkillStore, key: &str) -> Result<(), AppError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(AppError::invalid_input("Key is required"));
    }
    if tool_adapters::find_adapter_with_store(store, key).is_none() {
        return Err(AppError::not_found(format!("Unknown tool: {key}")));
    }
    let mut project_paths = get_custom_tool_project_paths(store);
    if project_paths.remove(key).is_some() {
        set_custom_tool_project_paths(store, &project_paths)?;
    }
    Ok(())
}
