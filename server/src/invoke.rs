//! POST /api/invoke/{cmd} — the invoke-over-HTTP bridge.
//!
//! The web frontend calls exactly the same command names with the same
//! camelCase argument objects the Tauri IPC uses; the dispatcher maps them to
//! the headless implementations in `ops` / `backup` / the core services.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::backup;
use crate::ops;
use crate::AppState;
use crate::tools::{tools_apply_project_dir, tools_apply_skills_dir, tools_reset_project_dir, tools_reset_skills_dir};

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl From<crate::core::error::AppError> for ApiError {
    fn from(e: crate::core::error::AppError) -> Self {
        use crate::core::error::ErrorKind;
        let status = match e.kind {
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
            ErrorKind::Cancelled => StatusCode::CONFLICT,
            ErrorKind::TargetConflict => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError { status, message: e.message }
    }
}

impl From<crate::auth::AuthError> for ApiError {
    fn from(_: crate::auth::AuthError) -> Self {
        ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "Not signed in".to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

/// Commands the server answers with an empty result.
const DESKTOP_ONLY: &[&str] = &[
    "scanProjectsCmd",
    "add_project",
    "add_linked_workspace",
    "remove_project",
    "reveal_project_folder",
    "open_central_repo_folder",
    "app_exit",
    "hide_to_tray",
    "restart_app",
    "update_install_blocker",
    "get_recent_log_excerpt",
    "export_logs_zip",
    "check_last_panic",
    "clear_last_panic",
    "get_project_skills",
    "get_project_skill_document",
    "get_project_agent_targets",
    "import_project_skill_to_center",
    "export_skill_to_project",
    "update_project_skill_to_center",
    "update_project_skill_from_center",
    "toggle_project_skill",
    "delete_project_skill",
    "slugify_skill_names",
    "get_global_local_skills",
    "get_global_local_skill_document",
    "import_global_local_skill_to_center",
    "update_global_local_skill_from_center",
    "delete_global_local_skill",
];

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/invoke/{cmd}", post(invoke_cmd)).route("/health", get(health))
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
struct NoArgs {}

fn args_to<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T, crate::core::error::AppError> {
    serde_json::from_value(args.clone())
        .map_err(|e| crate::core::error::AppError::invalid_input(format!("bad arguments: {e}")))
}

fn ok(value: Value) -> ApiResult {
    Ok(Json(value))
}

async fn invoke_cmd(
    State(state): State<Arc<AppState>>,
    Path(cmd): Path<String>,
    headers: HeaderMap,
    args: Option<Json<Value>>,
) -> Result<Json<Value>, ApiError> {
    state.auth.session_from_headers(&headers)?;

    let args = args.map(|Json(v)| v).unwrap_or(Value::Null);
    let store = state.store.clone();
    let cmd_owned = cmd.clone();
    let result = tokio::task::spawn_blocking(move || dispatch(&store, &cmd_owned, &args))
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("task join failed: {e}"),
    })??;
    Ok(Json(result))
}

fn dispatch(store: &crate::core::skill_store::SkillStore, cmd: &str, args: &Value) -> Result<Value, ApiError> {
    use crate::core::error::AppError;

    if DESKTOP_ONLY.contains(&cmd) {
        return Err(ApiError {
            status: StatusCode::NOT_IMPLEMENTED,
            message: format!(
                "'{cmd}' is a desktop-only feature in this deployment; use the desktop client"
            ),
        });
    }

    let null = Value::Null;
    let out: Value = match cmd {
        // ── Browse ──
        "fetch_leaderboard" => {
            #[derive(Deserialize)]
            struct A {
                board: String,
            }
            let a: A = args_to(args)?;
            fetch_leaderboard_impl(store, &a.board).map(serialize)
        }
        "search_skillssh" => {
            #[derive(Deserialize)]
            struct A {
                query: String,
                #[serde(default)]
                limit: Option<usize>,
            }
            let a: A = args_to(args)?;
            search_skillssh_impl(store, &a.query, a.limit).map(serialize)
        }

        // ── Skills read ──
        "get_managed_skills" => ops::get_managed_skills(store).map(serialize),
        "get_skills_for_preset" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "presetId")]
                preset_id: String,
            }
            let a: A = args_to(args)?;
            ops::get_skills_for_preset(store, &a.preset_id).map(serialize)
        }
        "get_skill_document" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
            }
            let a: A = args_to(args)?;
            ops::get_skill_document(store, &a.skill_id).map(serialize)
        }
        "get_all_tags" => store.get_all_tags().map_err(AppError::db).map(serialize),

        // ── Skills write ──
        "delete_managed_skill" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
            }
            let a: A = args_to(args)?;
            ops::delete_managed_skills_by_ids(store, std::slice::from_ref(&a.skill_id))
                .and_then(|r| {
                    if r.deleted == 0 {
                        Err(AppError::not_found("Skill not found"))
                    } else {
                        Ok(r)
                    }
                })
                .map(serialize)
        }
        "delete_managed_skills" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillIds")]
                skill_ids: Vec<String>,
            }
            let a: A = args_to(args)?;
            ops::delete_managed_skills_by_ids(store, &a.skill_ids).map(serialize)
        }
        "set_skill_tags" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                tags: Vec<String>,
            }
            let a: A = args_to(args)?;
            ops::set_skill_tags_internal(store, &a.skill_id, &a.tags).map(serialize_null)
        }
        "rename_tag" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "oldName")]
                old_name: String,
                #[serde(rename = "newName")]
                new_name: String,
            }
            let a: A = args_to(args)?;
            ops::rename_tag_internal(store, &a.old_name, &a.new_name)
                .map(|_| serialize_null(&null))
        }
        "delete_tag" => {
            #[derive(Deserialize)]
            struct A {
                name: String,
            }
            let a: A = args_to(args)?;
            ops::delete_tag_internal(store, &a.name).map(|_| serialize_null(&null))
        }

        // ── Install ──
        "install_local" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "sourcePath")]
                source_path: String,
                #[serde(default)]
                name: Option<String>,
            }
            let a: A = args_to(args)?;
            ops::install_local_impl(store, &a.source_path, a.name.as_deref())
                .map(serialize_null)
        }
        "batch_import_folder" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "folderPath")]
                folder_path: String,
            }
            let a: A = args_to(args)?;
            ops::batch_import_folder_impl(store, &a.folder_path).map(serialize)
        }
        "install_git" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "repoUrl")]
                repo_url: String,
                #[serde(default)]
                name: Option<String>,
            }
            let a: A = args_to(args)?;
            let cancel = Arc::new(AtomicBool::new(false));
            ops::install_git_impl(
                store,
                &a.repo_url,
                a.name.as_deref(),
                store.proxy_url().as_deref(),
                &cancel,
            )
            .map(serialize_null)
        }
        "install_from_skillssh" => {
            #[derive(Deserialize)]
            struct A {
                source: String,
                #[serde(rename = "skillId")]
                skill_id: String,
            }
            let a: A = args_to(args)?;
            let cancel = Arc::new(AtomicBool::new(false));
            ops::install_from_skillssh_impl(
                store,
                &a.source,
                &a.skill_id,
                store.proxy_url().as_deref(),
                &cancel,
            )
            .map(serialize_null)
        }
        "preview_git_install" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "repoUrl")]
                repo_url: String,
            }
            let a: A = args_to(args)?;
            let cancel = Arc::new(AtomicBool::new(false));
            ops::preview_git_install_impl(
                store,
                &a.repo_url,
                store
                    .get_setting("proxy_url")
                    .ok()
                    .flatten()
                    .as_deref(),
                &cancel,
            )
            .map(serialize)
        }
        "confirm_git_install" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "repoUrl")]
                repo_url: String,
                #[serde(rename = "tempDir")]
                temp_dir: String,
                items: Vec<ops::SkillInstallItem>,
            }
            let a: A = args_to(args)?;
            ops::confirm_git_install_impl(
                store,
                &a.repo_url,
                &a.temp_dir,
                &a.items,
                store.proxy_url().as_deref(),
            )
            .map(serialize_null)
        }
        "cancel_git_preview" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "tempDir")]
                temp_dir: String,
            }
            let a: A = args_to(args)?;
            ops::cancel_git_preview_impl(&a.temp_dir).map(serialize_null)
        }
        "cancel_install" => {
            // No in-flight registry across processes; nothing to cancel.
            let _ = args;
            Ok(serialize(&false))
        }

        // ── Update checks ──
        "check_skill_update" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                #[serde(default)]
                force: Option<bool>,
            }
            let a: A = args_to(args)?;
            let force = a.force.unwrap_or(false);
            let prefetched = ops::prefetch_skill_remote(store, &a.skill_id, force, store.proxy_url().as_deref());
            let _lock = crate::core::repo_lock::RepoLock::acquire_foreground(
                "check skill update",
            )
            .map_err(AppError::db)?;
            ops::check_skill_update_with_remote(store, &a.skill_id, force, prefetched)
                .map(serialize)
        }
        "check_all_skill_updates" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(default)]
                force: Option<bool>,
            }
            let a: A = args_to(args)?;
            ops::check_all_skill_updates_impl(store, a.force.unwrap_or(false), store.proxy_url())
                .map(serialize_null)
        }
        "update_skill" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                #[serde(rename = "approvedRemovals", default)]
                approved_removals: Option<String>,
            }
            let a: A = args_to(args)?;
            ops::update_git_skill_internal(
                store,
                &a.skill_id,
                store.proxy_url().as_deref(),
                None,
                a.approved_removals.as_deref(),
            )
            .map(serialize)
        }
        "batch_update_skills" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillIds")]
                skill_ids: Vec<String>,
            }
            let a: A = args_to(args)?;
            ops::batch_update_skills_impl(store, &a.skill_ids, store.proxy_url().as_deref())
                .map(serialize)
        }
        "reimport_local_skill" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                #[serde(rename = "approvedRemovals", default)]
                approved_removals: Option<String>,
            }
            let a: A = args_to(args)?;
            ops::reimport_local_skill_internal(store, &a.skill_id, a.approved_removals.as_deref())
                .map(serialize)
        }
        "relink_local_skill_source" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                #[serde(rename = "sourcePath")]
                source_path: String,
                #[serde(rename = "approvedRemovals", default)]
                approved_removals: Option<String>,
            }
            let a: A = args_to(args)?;
            ops::relink_local_skill_source_impl(
                store,
                &a.skill_id,
                &a.source_path,
                a.approved_removals.as_deref(),
            )
            .map(serialize)
        }
        "detach_local_skill_source" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
            }
            let a: A = args_to(args)?;
            ops::detach_local_skill_source_impl(store, &a.skill_id).map(serialize)
        }
        "get_source_skill_document" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
            }
            let a: A = args_to(args)?;
            ops::get_source_skill_document_impl(store, &a.skill_id, store.proxy_url().as_deref())
                .map(serialize)
        }
        "get_skill_source_diff" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
            }
            let a: A = args_to(args)?;
            ops::get_skill_source_diff_impl(store, &a.skill_id, store.proxy_url().as_deref())
                .map(serialize)
        }

        // ── Scan local agent dirs (server-side: /data/<agent>/skills etc.) ──
        "scan_local_skills" => ops::scan_local_skills_impl(store).map(serialize),
        "import_existing_skill" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "sourcePath")]
                source_path: String,
                #[serde(default)]
                name: Option<String>,
            }
            let a: A = args_to(args)?;
            ops::import_existing_skill_impl(store, &a.source_path, a.name.as_deref())
                .map(serialize_null)
        }
        "import_all_discovered" => {
            ops::import_all_discovered_impl(store).map(serialize_null)
        }

        // ── Presets ──
        "get_presets" => ops::get_presets(store).map(serialize),
        "get_active_preset" => ops::get_active_preset(store).map(serialize),
        "create_preset" => {
            #[derive(Deserialize)]
            struct A {
                name: String,
                #[serde(default)]
                description: Option<String>,
                #[serde(default)]
                icon: Option<String>,
            }
            let a: A = args_to(args)?;
            ops::create_preset_internal(store, &a.name, a.description.as_deref(), a.icon.as_deref())
                .map(serialize)
        }
        "update_preset" => {
            #[derive(Deserialize)]
            struct A {
                id: String,
                name: String,
                #[serde(default)]
                description: Option<String>,
                #[serde(default)]
                icon: Option<String>,
            }
            let a: A = args_to(args)?;
            ops::update_preset_internal(store, &a.id, &a.name, a.description.as_deref(), a.icon.as_deref())
                .map(serialize_null)
        }
        "delete_preset" => {
            #[derive(Deserialize)]
            struct A {
                id: String,
            }
            let a: A = args_to(args)?;
            ops::delete_preset_internal(store, &a.id).map(serialize_null)
        }
        "switch_preset" | "apply_preset_to_default" => {
            #[derive(Deserialize)]
            struct A {
                id: String,
            }
            let a: A = args_to(args)?;
            ops::apply_preset_to_default(store, &a.id).map(serialize_null)
        }
        "add_skill_to_preset" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                #[serde(rename = "presetId")]
                preset_id: String,
            }
            let a: A = args_to(args)?;
            ops::set_preset_skills_internal(store, &a.preset_id, std::slice::from_ref(&a.skill_id), true)
                .map(serialize_null)
        }
        "remove_skill_from_preset" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                #[serde(rename = "presetId")]
                preset_id: String,
            }
            let a: A = args_to(args)?;
            ops::set_preset_skills_internal(store, &a.preset_id, std::slice::from_ref(&a.skill_id), false)
                .map(serialize_null)
        }
        "reorder_presets" => {
            #[derive(Deserialize)]
            struct A {
                ids: Vec<String>,
            }
            let a: A = args_to(args)?;
            crate::core::sync_metadata::with_repo_lock("reorder scenarios", || {
                store.reorder_scenarios(&a.ids)?;
                crate::core::sync_metadata::write_all_from_db_unlocked(store)
            })
            .map_err(AppError::db)
            .map(serialize_null)
        }
        "get_preset_skill_order" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "presetId")]
                preset_id: String,
            }
            let a: A = args_to(args)?;
            store
                .get_skill_ids_for_scenario(&a.preset_id)
                .map_err(AppError::db)
                .map(serialize)
        }
        "reorder_preset_skills" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "presetId")]
                preset_id: String,
                #[serde(rename = "skillIds")]
                skill_ids: Vec<String>,
            }
            let a: A = args_to(args)?;
            crate::core::sync_metadata::with_repo_lock("reorder scenario skills", || {
                store.reorder_scenario_skills(&a.preset_id, &a.skill_ids)?;
                crate::core::sync_metadata::write_all_from_db_unlocked(store)
            })
            .map_err(AppError::db)
            .map(serialize_null)
        }

        // ── Sync / tool toggles ──
        "get_skill_tool_toggles" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                #[serde(rename = "presetId")]
                preset_id: String,
            }
            let a: A = args_to(args)?;
            crate::tools::get_skill_tool_toggles_impl(store, &a.skill_id, &a.preset_id)
                .map(serialize)
        }
        "set_skill_tool_toggle" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                #[serde(rename = "presetId")]
                preset_id: String,
                tool: String,
                enabled: bool,
            }
            let a: A = args_to(args)?;
            crate::tools::set_skill_tool_toggle_impl(
                store,
                &a.skill_id,
                &a.preset_id,
                &a.tool,
                a.enabled,
            )
            .map(serialize_null)
        }
        "sync_skill_to_tool" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                tool: String,
            }
            let a: A = args_to(args)?;
            crate::tools::sync_skill_to_tool_impl(store, &a.skill_id, &a.tool)
                .map(serialize_null)
        }
        "unsync_skill_from_tool" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                tool: String,
            }
            let a: A = args_to(args)?;
            crate::tools::unsync_skill_from_tool_impl(store, &a.skill_id, &a.tool)
                .map(serialize_null)
        }

        // ── Tool (agent) management ──
        "set_tool_enabled" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
                enabled: bool,
            }
            let a: A = args_to(args)?;
            crate::tools::set_tool_enabled_internal(store, &a.key, a.enabled).map(serialize_null)
        }
        "set_all_tools_enabled" => {
            #[derive(Deserialize)]
            struct A {
                enabled: bool,
            }
            let a: A = args_to(args)?;
            crate::tools::set_all_tools_enabled_internal(store, a.enabled).map(serialize_null)
        }
        "get_tool_order_cmd" => Ok(serialize(&crate::core::tool_service::get_tool_order(store))),
        "set_tool_order_cmd" => {
            #[derive(Deserialize)]
            struct A {
                order: Vec<String>,
            }
            let a: A = args_to(args)?;
            crate::core::tool_service::set_tool_order(store, &a.order).map(serialize_null)
        }
        "set_custom_tool_path" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
                path: String,
            }
            let a: A = args_to(args)?;
            tools_apply_skills_dir(store, &a.key, &a.path).map(serialize_null)
        }
        "reset_custom_tool_path" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
            }
            let a: A = args_to(args)?;
            tools_reset_skills_dir(store, &a.key).map(serialize_null)
        }
        "set_custom_tool_project_path" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
                #[serde(rename = "projectRelativeSkillsDir", default)]
                project_relative_skills_dir: Option<String>,
            }
            let a: A = args_to(args)?;
            tools_apply_project_dir(
                store,
                &a.key,
                a.project_relative_skills_dir.as_deref(),
            )
            .map(serialize_null)
        }
        "reset_custom_tool_project_path" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
            }
            let a: A = args_to(args)?;
            tools_reset_project_dir(store, &a.key).map(serialize_null)
        }
        "add_custom_tool" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
                #[serde(rename = "displayName")]
                display_name: String,
                #[serde(rename = "skillsDir")]
                skills_dir: String,
                #[serde(rename = "projectRelativeSkillsDir", default)]
                project_relative_skills_dir: Option<String>,
            }
            let a: A = args_to(args)?;
            crate::tools::add_custom_tool_internal(
                store,
                &a.key,
                &a.display_name,
                &a.skills_dir,
                a.project_relative_skills_dir.as_deref(),
            )
            .map(serialize_null)
        }
        "remove_custom_tool" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
            }
            let a: A = args_to(args)?;
            crate::tools::remove_custom_tool_internal(store, &a.key).map(serialize_null)
        }

        // ── Tools / projects (server-local, empty for the web console) ──
        "get_tool_status" => {
            let infos = crate::core::tool_service::list_tool_info(store);
            let out: Vec<Value> = infos
                .into_iter()
                .map(|info| {
                    serde_json::json!({
                        "key": info.key,
                        "display_name": info.display_name,
                        "installed": info.installed,
                        "skills_dir": info.skills_dir,
                        "enabled": info.enabled,
                        "is_custom": info.is_custom,
                        "has_path_override": info.has_path_override,
                        "project_relative_skills_dir": info.project_relative_skills_dir,
                        "has_project_path_override": info.has_project_path_override,
                        "category": info.category,
                    })
                })
                .collect();
            Ok(serialize(&out))
        }
        "get_projects" => Ok(serialize(&Vec::<Value>::new())),

        // ── Settings ──
        "get_settings" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
            }
            let a: A = args_to(args)?;
            store.get_setting(&a.key).map_err(AppError::db).map(serialize)
        }
        "set_settings" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
                value: String,
            }
            let a: A = args_to(args)?;
            store.set_setting(&a.key, &a.value).map_err(AppError::db).map(serialize_null)
        }
        "get_central_repo_path" => Ok(serialize(
            &crate::core::central_repo::base_dir().to_string_lossy().to_string(),
        )),
        "get_central_repo_path_override" => Ok(serialize(
            &crate::core::central_repo::configured_base_dir()
                .map(|p| p.to_string_lossy().to_string()),
        )),
        "get_central_repo_pending_path" => Ok(serialize(
            &crate::core::central_repo::pending_base_dir()
                .map(|p| p.to_string_lossy().to_string()),
        )),
        "set_central_repo_path" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(default)]
                path: Option<String>,
            }
            let a: A = args_to(args)?;
            crate::core::central_repo::set_base_dir_override(a.path)
                .map(|resolved| serialize(&resolved.to_string_lossy().to_string()))
                .map_err(crate::core::error::AppError::io)
        }
        "get_central_repo_warnings" => {
            Ok(serialize(&crate::core::central_repo::startup_warnings()))
        }
        "log_startup_event" => Ok(serialize_null(&null)),

        // ── App update check ──
        "check_app_update" => check_app_update_impl(store).map(serialize),
        "get_diagnostic_info" => {
            #[derive(serde::Serialize)]
            struct DiagnosticInfo {
                app_version: String,
                os: String,
                os_version: String,
                arch: String,
                central_repo_path: String,
                central_repo_path_overridden: bool,
            }
            let info = DiagnosticInfo {
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                os: std::env::consts::OS.to_string(),
                os_version: "server".to_string(),
                arch: std::env::consts::ARCH.to_string(),
                central_repo_path: crate::core::central_repo::base_dir()
                    .to_string_lossy()
                    .to_string(),
                central_repo_path_overridden: false,
            };
            Ok(serialize(&info))
        }

        // ── Git backup ──
        "git_backup_status" => backup::git_backup_status(store).map(serialize),
        "github_backup_connect" => {
            #[derive(Deserialize)]
            struct A {
                token: String,
                #[serde(rename = "repoName")]
                repo_name: String,
            }
            let a: A = args_to(args)?;
            backup::github_backup_connect_impl(store, &a.token, &a.repo_name).map(serialize)
        }
        "github_device_flow_start" => {
            backup::github_device_flow_start_impl(store).map(serialize)
        }
        "github_device_flow_poll" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "deviceCode")]
                device_code: String,
                #[serde(rename = "repoName")]
                repo_name: String,
            }
            let a: A = args_to(args)?;
            backup::github_device_flow_poll_impl(store, &a.device_code, &a.repo_name)
                .map(serialize)
        }
        "git_backup_clone" => {
            #[derive(Deserialize)]
            struct A {
                url: String,
                #[serde(rename = "setAside", default)]
                set_aside: Option<Vec<String>>,
            }
            let a: A = args_to(args)?;
            backup::git_backup_clone_impl(store, &a.url, a.set_aside).map(serialize)
        }
        "git_backup_reclone" => {
            #[derive(Deserialize)]
            struct A {
                url: String,
                #[serde(rename = "setAside", default)]
                set_aside: Option<Vec<String>>,
            }
            let a: A = args_to(args)?;
            backup::git_backup_reclone_impl(store, &a.url, a.set_aside).map(serialize)
        }
        "git_backup_migrate_credentials" => {
            backup::git_backup_migrate_credentials_impl(store).map(serialize)
        }
        "git_backup_fetch" => backup::git_backup_fetch(store).map(serialize_null),
        "git_backup_init" => backup::git_backup_init(store).map(serialize_null),
        "git_backup_set_remote" => {
            #[derive(Deserialize)]
            struct A {
                url: String,
            }
            let a: A = args_to(args)?;
            backup::git_backup_set_remote(store, &a.url).map(serialize)
        }
        "git_backup_sanitize_remote_url" => {
            #[derive(Deserialize)]
            struct A {
                url: String,
            }
            let a: A = args_to(args)?;
            backup::git_backup_sanitize_remote_url(&a.url).map(serialize)
        }
        "git_backup_commit" => {
            #[derive(Deserialize)]
            struct A {
                message: String,
            }
            let a: A = args_to(args)?;
            backup::git_backup_commit(store, &a.message).map(serialize_null)
        }
        "git_backup_push" => backup::git_backup_push(store).map(serialize_null),
        "git_backup_pull" => backup::git_backup_pull(store).map(serialize),
        "git_backup_sync" => {
            #[derive(Deserialize)]
            struct A {
                message: String,
            }
            let a: A = args_to(args)?;
            backup::git_backup_sync(store, &a.message).map(serialize)
        }
        "git_backup_pending_conflicts" => backup::git_backup_pending_conflicts(store).map(serialize),
        "git_backup_resolve_conflict" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(rename = "skillId")]
                skill_id: String,
                action: String,
            }
            let a: A = args_to(args)?;
            backup::git_backup_resolve_conflict(store, &a.skill_id, &a.action).map(serialize)
        }
        "git_backup_create_snapshot" => backup::git_backup_create_snapshot(store).map(serialize),
        "git_backup_list_versions" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(default)]
                limit: Option<u32>,
            }
            let a: A = args_to(args)?;
            backup::git_backup_list_versions(store, a.limit).map(serialize)
        }
        "git_backup_restore_version" => {
            #[derive(Deserialize)]
            struct A {
                tag: String,
            }
            let a: A = args_to(args)?;
            backup::git_backup_restore_version(store, &a.tag).map(serialize)
        }
        "git_backup_size_report" => backup::git_backup_size_report().map(serialize),
        "git_backup_remove_remote" => backup::git_backup_remove_remote(store).map(serialize_null),
        "backup_device_name" => Ok(serialize(&backup::effective_device_name(store))),
        "backup_set_device_name" => {
            #[derive(Deserialize)]
            struct A {
                name: String,
            }
            let a: A = args_to(args)?;
            backup::backup_set_device_name(store, &a.name).map(serialize)
        }

        _ => Err(AppError::not_found(format!("unknown command '{cmd}'"))),
    }?;

    Ok(out)
}

fn serialize(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn serialize_null<T>(_unused: T) -> Value {
    Value::Null
}

fn fetch_leaderboard_impl(
    store: &crate::core::skill_store::SkillStore,
    board: &str,
) -> Result<Vec<crate::core::skillssh_api::SkillsShSkill>, crate::core::error::AppError> {
    use crate::core::skillssh_api::{self, LeaderboardType};

    const LEADERBOARD_CACHE_TTL: i64 = 300;
    let cache_key = format!("leaderboard_{board}");

    if let Ok(Some(cached)) = store.get_cache(&cache_key, LEADERBOARD_CACHE_TTL) {
        if let Ok(skills) = serde_json::from_str::<Vec<skillssh_api::SkillsShSkill>>(&cached) {
            return Ok(skills);
        }
    }

    let proxy_url = store.proxy_url();
    let board_type = LeaderboardType::from_str(board);
    let skills = skillssh_api::fetch_leaderboard(board_type, proxy_url.as_deref())
        .map_err(crate::core::error::AppError::network)?;

    if let Ok(json) = serde_json::to_string(&skills) {
        store.set_cache(&cache_key, &json).ok();
    }

    Ok(skills)
}

fn search_skillssh_impl(
    store: &crate::core::skill_store::SkillStore,
    query: &str,
    limit: Option<usize>,
) -> Result<Vec<crate::core::skillssh_api::SkillsShSkill>, crate::core::error::AppError> {
    let proxy_url = store.proxy_url();
    let requested = limit.unwrap_or(60);
    let bounded = requested.clamp(1, 300);
    crate::core::skillssh_api::search_skills(query, bounded, proxy_url.as_deref())
        .map_err(crate::core::error::AppError::network)
}

fn check_app_update_impl(
    store: &crate::core::skill_store::SkillStore,
) -> Result<Value, crate::core::error::AppError> {
    use crate::core::error::AppError;
    use crate::core::skillssh_api;

    let proxy_url = store.proxy_url();
    let client = skillssh_api::build_http_client(proxy_url.as_deref(), 15);

    // Shared egress IPs (e.g. a clash proxy) routinely blow through the
    // unauthenticated GitHub API limit. The web frontend tracks its own
    // deployment anyway — degrade to "no update" like the desktop app's
    // silent startup check instead of surfacing an error.
    let fallback = || {
        log::warn!("check_app_update: upstream check failed; reporting no update");
        serde_json::json!({
            "has_update": false,
            "current_version": env!("CARGO_PKG_VERSION"),
            "latest_version": "",
            "release_url": "https://github.com/anonymous99-Rise/skills-manager/releases",
        })
    };

    let resp: serde_json::Value = match client
        .get("https://api.github.com/repos/anonymous99-Rise/skills-manager/releases/latest")
        .header("User-Agent", "skills-manager-server")
        .send()
    {
        Ok(r) => match r.json() {
            Ok(v) => v,
            Err(e) => {
                log::warn!("check_app_update: parse failed: {e}");
                return Ok(fallback());
            }
        },
        Err(e) => {
            log::warn!("check_app_update: network failed: {e}");
            return Ok(fallback());
        }
    };

    let Some(tag) = resp["tag_name"].as_str() else {
        log::warn!(
            "check_app_update: unexpected response: {}",
            serde_json::to_string(&resp).unwrap_or_default()
        );
        return Ok(fallback());
    };
    let latest_version = tag.strip_prefix('v').unwrap_or(tag).to_string();
    let release_url = resp["html_url"]
        .as_str()
        .unwrap_or("https://github.com/anonymous99-Rise/skills-manager/releases")
        .to_string();

    Ok(serde_json::json!({
        "has_update": false,
        "current_version": env!("CARGO_PKG_VERSION"),
        "latest_version": latest_version,
        "release_url": release_url,
    }))
}
