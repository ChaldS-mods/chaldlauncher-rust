//! Chaldi AI agent.
//!
//! The launcher keeps the conversation and executes tools locally, but it never talks to an AI
//! provider directly: every model call goes through the authenticated Chald Friends server
//! (`/chaldfriends/v1/ai/chat`). The server owns the key, the model, the system prompt and the
//! per-account quota; a turn id binds all tool rounds of one user message to the same account.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use sysinfo::System;
use tauri::{AppHandle, Manager};

use crate::content;
use crate::curseforge;
use crate::dispatch;
use crate::errors::{AppError, AppResult};
use crate::ftbquests;
use crate::game;
use crate::instances::{self, InstanceView};
use crate::mod_deps;
use crate::modrinth;
use crate::paths;
use crate::state::AppState;
use crate::undo::{self, UndoKind};
use crate::versions;

const MAX_ROUNDS: usize = 14;
const MAX_HISTORY_MESSAGES: usize = 30;
const OLD_TOOL_RESULT_CHARS: usize = 500;
const MAX_RESULT_CHARS: usize = 5_500;
const LOADERS: &[&str] = &["vanilla", "fabric", "quilt", "forge", "neoforge"];
const L4: &[&str] = &["fabric", "quilt", "forge", "neoforge"];

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": properties, "required": required },
        }
    })
}

/// Tool schemas are sent with every model call, so descriptions are intentionally short.
fn tool_defs() -> Value {
    let instance = json!({ "type": "string", "description": "Instance id or name" });
    let project = json!({ "type": "string", "description": "Modrinth id or slug from search" });
    let kind = json!({ "type": "string", "enum": ["mod", "resourcepack"] });
    let loader4 = json!({ "type": "string", "enum": L4 });
    let mc = json!({ "type": "string", "description": "Minecraft version, e.g. 1.21.1" });
    Value::Array(vec![
        tool("ask_user", "Poll: show the user a question with clickable options and END your turn. Use whenever the user must choose (version, loader, which mod, confirm deletion/update). Call it alone, max once per turn.", json!({
            "question": { "type": "string" },
            "options": { "type": "array", "items": { "type": "string" }, "description": "2-8 short options" },
            "multiple": { "type": "boolean", "description": "Allow several answers" },
        }), &["question", "options"]),
        tool("get_system_info", "Launcher version, OS, RAM, memory settings, newest Minecraft release.", json!({}), &[]),
        tool("list_instances", "All instances: id, name, Minecraft/loader versions, state.", json!({}), &[]),
        tool("get_instance", "Details and settings of one instance.", json!({ "instance": instance.clone() }), &["instance"]),
        tool("get_instance_status", "Install state/progress. waitSeconds (0-45) waits for the install to finish.", json!({
            "instance": instance.clone(), "waitSeconds": { "type": "integer" },
        }), &["instance"]),
        tool("list_minecraft_versions", "Real Minecraft versions, newest first. Never guess versions.", json!({
            "type": { "type": "string", "enum": ["release", "snapshot", "all"] },
            "query": { "type": "string", "description": "Prefix, e.g. 1.20" },
            "limit": { "type": "integer" },
        }), &[]),
        tool("get_loader_versions", "Loader versions for a Minecraft version. Without loader: which loaders support it.", json!({
            "mcVersion": mc.clone(), "loader": loader4.clone(), "limit": { "type": "integer" },
        }), &["mcVersion"]),
        tool("create_instance", "Create an instance; loader version is chosen automatically. install=true starts the download.", json!({
            "name": { "type": "string" }, "mcVersion": mc.clone(),
            "loader": { "type": "string", "enum": LOADERS },
            "loaderVersion": { "type": "string", "description": "Leave empty unless the user named one" },
            "description": { "type": "string" }, "install": { "type": "boolean" },
        }), &["name", "mcVersion", "loader"]),
        tool("install_instance", "Download Minecraft + loader (background; also repairs).", json!({
            "instance": instance.clone(), "repair": { "type": "boolean" },
        }), &["instance"]),
        tool("get_logs", "Game logs + automatic findings about the crash cause.", json!({
            "instance": instance.clone(),
            "source": { "type": "string", "enum": ["auto", "session", "latest", "crash"] },
            "errorsOnly": { "type": "boolean" },
            "grep": { "type": "string", "description": "a|b filter" },
            "maxLines": { "type": "integer" },
        }), &["instance"]),
        tool("list_mods", "Installed mods/resource packs with names, versions, projectId.", json!({
            "instance": instance.clone(), "kind": { "type": "string", "enum": ["mods", "resourcepacks"] },
        }), &["instance"]),
        tool("get_installed_mod_info", "Description, authors, dependencies of an installed mod (from Modrinth).", json!({
            "instance": instance.clone(), "file": { "type": "string", "description": "File or mod name" },
        }), &["instance", "file"]),
        tool("search_modrinth", "Search Modrinth. Pass instance to filter by its version and loader.", json!({
            "query": { "type": "string" },
            "type": { "type": "string", "enum": ["mod", "modpack", "resourcepack"] },
            "instance": { "type": "string" }, "mcVersion": mc.clone(), "loader": loader4.clone(),
            "sort": { "type": "string", "enum": ["relevance", "downloads", "follows", "newest", "updated"] },
            "limit": { "type": "integer" }, "offset": { "type": "integer" },
        }), &["query"]),
        tool("get_mod_info", "Full description of a Modrinth project (paged: use offset=nextOffset). Read before installing.", json!({
            "projectId": project.clone(), "offset": { "type": "integer" }, "maxChars": { "type": "integer" },
        }), &["projectId"]),
        tool("get_mod_versions", "Versions of a project with loaders, dependencies, changelog.", json!({
            "projectId": project.clone(), "instance": { "type": "string" },
            "mcVersion": mc.clone(), "loader": loader4.clone(),
            "limit": { "type": "integer" }, "changelog": { "type": "boolean" },
        }), &["projectId"]),
        tool("check_mod_compatibility", "Dry run: will it install into the instance, and with which version. Call before install_mod.", json!({
            "instance": instance.clone(), "projectId": project.clone(), "kind": kind.clone(), "versionId": { "type": "string" },
        }), &["instance", "projectId"]),
        tool("install_mod", "Install a mod/resource pack with required dependencies.", json!({
            "instance": instance.clone(), "projectId": project.clone(), "kind": kind.clone(), "versionId": { "type": "string" },
        }), &["instance", "projectId"]),
        tool("install_modpack", "Install an existing Modrinth modpack as a NEW instance (background).", json!({
            "projectId": project.clone(), "versionId": { "type": "string" },
        }), &["projectId"]),
        tool("search_curseforge", "Search CurseForge. Pass instance to filter by its Minecraft version and loader.", json!({
            "query": { "type": "string" },
            "classType": { "type": "string", "enum": ["mod", "modpack", "resourcepack", "shader", "datapack"] },
            "instance": { "type": "string" }, "mcVersion": mc.clone(), "loader": loader4.clone(),
            "sort": { "type": "string", "enum": ["featured", "popularity", "lastUpdated"] },
            "limit": { "type": "integer" }, "offset": { "type": "integer" },
        }), &["query"]),
        tool("get_curseforge_mod_info", "Full CurseForge project info: description, supported versions/loaders. Call before installing.", json!({
            "modId": { "type": "integer", "description": "Numeric CurseForge project id from search" },
        }), &["modId"]),
        tool("get_curseforge_files", "CurseForge files for a project. Each file includes dependencies and requiredDependencies.", json!({
            "modId": { "type": "integer" }, "instance": { "type": "string" },
            "mcVersion": mc.clone(), "loader": loader4.clone(), "limit": { "type": "integer" },
        }), &["modId"]),
        tool("check_curseforge_compatibility", "Dry run: which CurseForge files fit the instance and which required mods each file declares. Call before install_curseforge_mod.", json!({
            "instance": instance.clone(), "modId": { "type": "integer" }, "fileId": { "type": "integer" },
        }), &["instance", "modId"]),
        tool("install_curseforge_mod", "Install a CurseForge mod AND all its required dependencies recursively. Always follow with validate_dependencies.", json!({
            "instance": instance.clone(), "modId": { "type": "integer" },
            "fileId": { "type": "integer", "description": "Optional exact file id" },
            "kind": { "type": "string", "enum": ["mod", "resourcepack", "shader", "datapack"] },
        }), &["instance", "modId"]),
        tool("validate_dependencies", "Deep dependency audit: reads every installed jar manifest plus CurseForge and Modrinth metadata. With fix=true it tries to install missing required mods. MUST be called after any install and before saying the modpack is ready.", json!({
            "instance": instance.clone(),
            "fix": { "type": "boolean", "description": "Default true: try to auto-install missing required mods" },
        }), &["instance"]),
        tool("build_modpack", "CREATE a custom modpack inside an existing instance: finds compatible mods for a theme plus essentials and installs them with dependencies.", json!({
            "instance": instance.clone(),
            "theme": { "type": "string", "description": "Theme, e.g. Create" },
            "count": { "type": "integer", "description": "Target number of mods (3-12, default 8)" },
        }), &["instance", "theme"]),
        tool("validate_modpack", "Audit installed mods: find required dependencies that are missing or incompatible. Call after build_modpack and after manual installs.", json!({
            "instance": instance.clone(),
        }), &["instance"]),
        tool("remove_mod", "Delete a mod/resource pack file.", json!({
            "instance": instance.clone(), "file": { "type": "string" },
            "kind": { "type": "string", "enum": ["mods", "resourcepacks"] },
        }), &["instance", "file"]),
        tool("set_mod_enabled", "Enable or disable a mod.", json!({
            "instance": instance.clone(), "file": { "type": "string" }, "enabled": { "type": "boolean" },
        }), &["instance", "file", "enabled"]),
        tool("check_mod_updates", "Which installed mods have updates.", json!({ "instance": instance.clone() }), &["instance"]),
        tool("update_mod", "Update one mod; without file updates ALL.", json!({
            "instance": instance.clone(), "file": { "type": "string" },
        }), &["instance"]),
        tool("apply_jvm_preset", "Apply a JVM preset to an instance (empty instance = global).", json!({
            "preset": { "type": "string", "enum": ["none", "balanced", "performance", "lowLatency", "compatibility"] },
            "instance": { "type": "string" },
        }), &["preset"]),
        tool("list_instance_files", "List files of an instance. Paths are relative to .minecraft; use prefix instance/ for instance.json/backups.", json!({
            "instance": instance.clone(),
            "path": { "type": "string", "description": "Folder inside the instance, default .minecraft" },
            "maxDepth": { "type": "integer", "description": "Default 2" },
            "pattern": { "type": "string", "description": "Substring filter for names" },
            "limit": { "type": "integer" },
        }), &["instance"]),
        tool("read_instance_file", "Read a UTF-8 text file of an instance (configs, quests, logs).", json!({
            "instance": instance.clone(), "path": { "type": "string" },
            "maxChars": { "type": "integer" }, "offsetChars": { "type": "integer" },
        }), &["instance", "path"]),
        tool("write_instance_file", "Create or replace a text file inside an instance. Automatically undoable.", json!({
            "instance": instance.clone(), "path": { "type": "string" },
            "content": { "type": "string" }, "append": { "type": "boolean" },
        }), &["instance", "path", "content"]),
        tool("delete_instance_file", "Delete a file or folder inside an instance. Needs confirm=true. Automatically undoable.", json!({
            "instance": instance.clone(), "path": { "type": "string" }, "confirm": { "type": "boolean" },
        }), &["instance", "path", "confirm"]),
        tool("list_ftb_quests", "List FTB Quests chapters, quest ids and titles of an instance. Call before editing quests.", json!({
            "instance": instance.clone(),
        }), &["instance"]),
        tool("create_ftb_quests_chapter", "Create a new FTB Quests chapter (SNBT) with a title and optional icon.", json!({
            "instance": instance.clone(), "title": { "type": "string" },
            "icon": { "type": "string", "description": "Optional item id, e.g. minecraft:book" },
        }), &["instance", "title"]),
        tool("add_ftb_quest", "Add a quest to an FTB Quests chapter: task, rewards, description and dependencies. Automatically undoable.", json!({
            "instance": instance.clone(),
            "chapter": { "type": "string", "description": "Chapter id or title from list_ftb_quests" },
            "title": { "type": "string" },
            "taskType": { "type": "string", "enum": ["checkmark", "item", "kill", "advancement", "dimension", "stage"] },
            "item": { "type": "string", "description": "Item id for item tasks/rewards" },
            "entity": { "type": "string", "description": "Entity id for kill tasks" },
            "advancement": { "type": "string" }, "dimension": { "type": "string" }, "stage": { "type": "string" },
            "count": { "type": "integer", "description": "Required amount / XP / reward count" },
            "subtitle": { "type": "string" },
            "description": { "description": "String or array of lines" },
            "dependencies": { "type": "array", "items": { "type": "string" } },
            "rewards": { "type": "array", "items": {
                "type": "object", "properties": {
                    "type": { "type": "string", "enum": ["xp", "xp_levels", "item", "command", "toast"] },
                    "xp": { "type": "integer" }, "xpLevels": { "type": "integer" },
                    "item": { "type": "string" }, "count": { "type": "integer" },
                    "command": { "type": "string" }, "description": { "type": "string" },
                }, "required": ["type"],
            } },
            "x": { "type": "number" }, "y": { "type": "number" },
            "locale": { "type": "string", "description": "Default en_us" },
        }), &["instance", "chapter", "title"]),
        tool("edit_ftb_quest", "Edit an existing FTB Quests quest by id: title, description, task, rewards, dependencies, position. Automatically undoable.", json!({
            "instance": instance.clone(), "questId": { "type": "string" },
            "title": { "type": "string" }, "subtitle": { "type": "string" },
            "description": { "description": "String or array of lines" },
            "taskType": { "type": "string", "enum": ["checkmark", "item", "kill", "advancement", "dimension", "stage"] },
            "item": { "type": "string" }, "entity": { "type": "string" },
            "advancement": { "type": "string" }, "dimension": { "type": "string" }, "stage": { "type": "string" },
            "count": { "type": "integer" },
            "dependencies": { "type": "array", "items": { "type": "string" } },
            "rewards": { "type": "array", "items": { "type": "object" } },
            "x": { "type": "number" }, "y": { "type": "number" },
            "locale": { "type": "string" },
        }), &["instance", "questId"]),
        tool("launch_instance", "Launch the game to test a modpack. Use get_logs afterwards to check for crashes.", json!({
            "instance": instance.clone(), "waitSeconds": { "type": "integer", "description": "0-30, optional" },
        }), &["instance"]),
        tool("stop_instance", "Stop a running game. Undo starts it again.", json!({ "instance": instance.clone() }), &["instance"]),
        tool("rename_instance", "Rename an instance. Automatically undoable.", json!({
            "instance": instance.clone(), "name": { "type": "string" },
        }), &["instance", "name"]),
        tool("duplicate_instance", "Duplicate an instance with all its files. Automatically undoable.", json!({
            "instance": instance.clone(), "name": { "type": "string" },
        }), &["instance", "name"]),
        tool("delete_instance", "Delete an instance with all files. Requires confirm=true. Automatically undoable.", json!({
            "instance": instance.clone(), "confirm": { "type": "boolean" },
        }), &["instance", "confirm"]),
        tool("set_instance_settings", "Change instance memory (MB), JVM arguments, window size or fullscreen.", json!({
            "instance": instance.clone(),
            "memoryMinMb": { "type": "integer" }, "memoryMaxMb": { "type": "integer" },
            "jvmArgs": { "type": "string" }, "width": { "type": "integer" }, "height": { "type": "integer" },
            "fullscreen": { "type": "boolean" }, "javaPath": { "type": "string" },
        }), &["instance"]),
        tool("undo_last", "Undo the last change made by you in this or a previous session. Returns what was restored.", json!({}), &[]),
        tool("list_undo", "Show the changelog of your last actions, newest first, for undo_last.", json!({}), &[]),
    ])
}

// ---------------------------------------------------------------- argument helpers

fn arg_str(args: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = args.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

fn arg_bool(args: &Value, keys: &[&str], default: bool) -> bool {
    for key in keys {
        match args.get(*key) {
            Some(Value::Bool(v)) => return *v,
            Some(Value::String(v)) if v == "true" => return true,
            Some(Value::String(v)) if v == "false" => return false,
            _ => {}
        }
    }
    default
}

fn arg_int(args: &Value, keys: &[&str], default: i64, min: i64, max: i64) -> i64 {
    for key in keys {
        let number = match args.get(*key) {
            Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|v| v.round() as i64)),
            Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
            _ => None,
        };
        if let Some(number) = number {
            return number.clamp(min, max);
        }
    }
    default
}

fn need(value: String, label: &str) -> AppResult<String> {
    if value.is_empty() {
        Err(AppError::new("AI_ARGS", format!("Missing required argument \"{label}\".")))
    } else {
        Ok(value)
    }
}

fn normalize_loader(raw: &str) -> AppResult<String> {
    let value = raw.trim().to_lowercase().replace([' ', '_', '-'], "");
    let result = match value.as_str() {
        "vanilla" | "none" | "ванилла" | "чистый" | "без" | "" => "vanilla",
        "fabric" | "фабрик" => "fabric",
        "quilt" => "quilt",
        "forge" | "форж" => "forge",
        "neoforge" | "neo" => "neoforge",
        _ => return Err(AppError::new("AI_ARGS", format!("Unknown loader \"{raw}\". Allowed: {}.", LOADERS.join(", ")))),
    };
    Ok(result.to_string())
}

fn loader_title(loader: &str) -> String {
    if loader == "neoforge" {
        "NeoForge".to_string()
    } else {
        let mut chars = loader.chars();
        match chars.next() {
            Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
            None => loader.to_string(),
        }
    }
}

fn running_ids(state: &AppState) -> HashSet<String> {
    state.games.running_ids().into_iter().collect()
}

fn find_instance(state: &AppState, reference: &str) -> AppResult<InstanceView> {
    let reference = need(reference.to_string(), "instance")?;
    let settings = state.settings.get()?;
    let all = instances::list(&state.data_dir, &settings, &running_ids(state))?;
    let low = reference.to_lowercase();
    if let Some(found) = all.iter().find(|item| item.meta.id == reference || item.meta.id.to_lowercase() == low || item.meta.name.to_lowercase() == low) {
        return Ok(found.clone());
    }
    let partial: Vec<&InstanceView> = all
        .iter()
        .filter(|item| item.meta.name.to_lowercase().contains(&low) || (item.meta.name.len() > 2 && low.contains(&item.meta.name.to_lowercase())))
        .collect();
    if partial.len() == 1 {
        return Ok(partial[0].clone());
    }
    let fmt = |item: &InstanceView| format!("{} («{}»)", item.meta.id, item.meta.name);
    if partial.len() > 1 {
        return Err(AppError::new(
            "INSTANCE_AMBIGUOUS",
            format!("Several instances match «{reference}»: {}. Use the exact id.", partial.iter().map(|i| fmt(i)).collect::<Vec<_>>().join(", ")),
        ));
    }
    Err(AppError::new(
        "INSTANCE_NOT_FOUND",
        format!("Instance «{reference}» not found. Existing: {}.", all.iter().map(|i| fmt(i)).collect::<Vec<_>>().join(", ")),
    ))
}

async fn resolve_mc(state: &AppState, raw: &str) -> AppResult<String> {
    let list = versions::minecraft(&state.data_dir, false).await?;
    let latest = list.iter().find(|v| v.kind == "release").map(|v| v.id.clone()).unwrap_or_default();
    let wanted = raw.trim().trim_start_matches('v');
    if wanted.is_empty() || wanted.eq_ignore_ascii_case("latest") {
        return if latest.is_empty() { Err(AppError::new("VERSION_UNKNOWN", "Не удалось получить список версий Minecraft")) } else { Ok(latest) };
    }
    if list.iter().any(|v| v.id == wanted) {
        return Ok(wanted.to_string());
    }
    let similar: Vec<String> = list
        .iter()
        .filter(|v| v.kind == "release" && v.id.starts_with(wanted))
        .take(8)
        .map(|v| v.id.clone())
        .collect();
    Err(AppError::new(
        "VERSION_UNKNOWN",
        format!(
            "Minecraft version \"{wanted}\" does not exist.{}. Newest release: {latest}. Use list_minecraft_versions.",
            if similar.is_empty() { String::new() } else { format!(" Similar releases: {}", similar.join(", ")) }
        ),
    ))
}

async fn resolve_loader_version(loader: &str, mc: &str, wanted: &str) -> AppResult<String> {
    let list = versions::loader_versions(loader, mc).await?;
    if list.is_empty() {
        return Err(AppError::new(
            "LOADER_INCOMPATIBLE",
            format!("{} does not support Minecraft {mc}.", loader_title(loader)),
        ));
    }
    let best = list.iter().find(|v| v.recommended).or_else(|| list.iter().find(|v| v.stable)).unwrap_or(&list[0]);
    if wanted.is_empty() || ["latest", "auto", "recommended"].contains(&wanted.to_lowercase().as_str()) {
        return Ok(best.version.clone());
    }
    if list.iter().any(|v| v.version == wanted) {
        return Ok(wanted.to_string());
    }
    let available: Vec<String> = list.iter().take(8).map(|v| v.version.clone()).collect();
    Err(AppError::new(
        "LOADER_INCOMPATIBLE",
        format!("{} {wanted} does not exist for Minecraft {mc}. Available: {}.", loader_title(loader), available.join(", ")),
    ))
}

/// Keeps the search input alive across the await (a borrowed temporary does not live long enough inline).
async fn mr_search(query: &str, kind: &str, sort: &str, offset: u64, mc: Option<&str>, loader: Option<&str>) -> AppResult<Value> {
    let input = json!({
        "query": query, "type": kind, "mcVersion": mc, "loader": loader,
        "sort": sort, "offset": offset,
    });
    modrinth::search(&input).await
}

async fn cf_search(query: &str, class_type: &str, sort: &str, offset: u64, mc: Option<&str>, loader: Option<&str>) -> AppResult<Value> {
    curseforge::search(&json!({
        "query": query, "classType": class_type, "mcVersion": mc, "loader": loader,
        "sort": sort, "offset": offset,
    })).await
}

fn normalized_id(value: &str) -> String {
    value.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// Only accept a search hit when its id/slug/title really matches the missing mod id.
fn confident_hit(hits: &[Value], wanted: &str) -> Option<Value> {
    let wanted = normalized_id(wanted);
    if wanted.is_empty() {
        return None;
    }
    hits.iter().find(|hit| {
        ["id", "slug", "title"].iter().any(|key| {
            hit.get(*key)
                .map(|value| match value {
                    Value::Number(_) => value.to_string() == wanted,
                    Value::String(text) => {
                        let found = normalized_id(text);
                        found == wanted || (found.len() > 3 && wanted.len() > 3 && (found.contains(&wanted) || wanted.contains(&found)))
                    }
                    _ => false,
                })
                .unwrap_or(false)
        })
    }).cloned()
}

/// Try to install one missing required mod by its id, first from CurseForge, then Modrinth.
async fn install_dependency_by_id(app: &AppHandle, state: &AppState, instance: &str, dep_id: &str) -> AppResult<Value> {
    let item = find_instance(state, instance)?;
    let settings = state.settings.get()?;
    let loader = normalize_loader(&item.meta.loader.r#type)?;
    let mc = &item.meta.mc_version;
    let loader_arg = if loader == "vanilla" { None } else { Some(loader.as_str()) };

    // First search with instance filters; retry without them because platforms sometimes
    // omit a dependency from a filtered search even though a compatible file exists.
    for (mc_arg, loader_arg) in [(Some(mc.as_str()), loader_arg), (None, None)] {
        if let Ok(result) = cf_search(dep_id, "mod", "featured", 0, mc_arg, loader_arg).await {
            let hits = result.get("hits").and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(hit) = confident_hit(&hits, dep_id) {
                if let Some(mod_id) = hit.get("id").and_then(Value::as_i64) {
                    if let Ok(installed) = curseforge::install_with_dependencies(
                        &state.data_dir, &settings, app, &item.meta.id, mod_id, "mod", None,
                    ).await {
                        if installed.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                            return Ok(json!({
                                "ok": true, "modId": dep_id, "source": "curseforge",
                                "projectId": mod_id, "installed": installed.get("installed"),
                            }));
                        }
                    }
                }
            }
        }
    }
    for (mc_arg, loader_arg) in [(Some(mc.as_str()), loader_arg), (None, None)] {
        if let Ok(result) = mr_search(dep_id, "mod", "relevance", 0, mc_arg, loader_arg).await {
            let hits = result.get("hits").and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(hit) = confident_hit(&hits, dep_id) {
                if let Some(project_id) = hit.get("id").and_then(Value::as_str) {
                    let files = modrinth::install(
                        &state.data_dir, &settings, app, &item.meta.id, project_id, "mod", None, None,
                    ).await?;
                    return Ok(json!({
                        "ok": true, "modId": dep_id, "source": "modrinth",
                        "projectId": project_id, "files": files,
                    }));
                }
            }
        }
    }
    Err(AppError::new(
        "DEPENDENCY_NOT_FOUND",
        format!("Не удалось надёжно найти мод «{dep_id}» ни на CurseForge, ни на Modrinth. Установи его вручную и повтори проверку."),
    ))
}

/// Combines local jar-manifest declarations with Modrinth and CurseForge file metadata.
/// A missing id from any of the three sources makes the audit fail.
async fn audit_dependencies(game_dir: &Path) -> AppResult<Value> {
    let manifest = mod_deps::scan(game_dir)?;
    let mut missing: Vec<Value> = manifest.get("missing").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in &missing {
        let dep = arg_str(entry, &["modId", "id"]);
        if !dep.is_empty() {
            seen.insert(dep.to_lowercase());
        }
    }
    let mut modrinth_errors = 0usize;
    for (project_id, file, version_id) in modrinth::installed_info(game_dir) {
        if version_id.is_empty() {
            continue;
        }
        match modrinth::required_dependencies(&version_id).await {
            Ok(deps) => {
                for dep in deps {
                    if seen.insert(dep.to_lowercase()) {
                        missing.push(json!({
                            "modId": dep, "requiredBy": project_id, "requiredByFile": file,
                            "source": "modrinth", "hint": "Install this project, then validate again.",
                        }));
                    }
                }
            }
            Err(error) => {
                modrinth_errors += 1;
                missing.push(json!({
                    "modId": project_id, "file": file, "source": "modrinth-metadata",
                    "error": error.message, "hint": "Dependency metadata could not be read; check manually.",
                }));
            }
        }
    }
    let mut cf_errors = 0usize;
    for (project_id, file, file_id) in curseforge::installed_info(game_dir) {
        match curseforge::required_dependencies(project_id, file_id).await {
            Ok(deps) => {
                for dep in deps {
                    let dep = dep.to_string();
                    if seen.insert(dep.to_lowercase()) {
                        missing.push(json!({
                            "modId": dep, "requiredBy": project_id.to_string(), "requiredByFile": file,
                            "source": "curseforge", "hint": "Install this CurseForge project, then validate again.",
                        }));
                    }
                }
            }
            Err(error) => {
                cf_errors += 1;
                missing.push(json!({
                    "modId": project_id.to_string(), "file": file, "source": "curseforge-metadata",
                    "error": error.message, "hint": "Dependency metadata could not be read; check manually.",
                }));
            }
        }
    }
    Ok(json!({
        "ok": missing.is_empty(),
        "missing": missing,
        "manifest": manifest,
        "metadataErrors": { "modrinth": modrinth_errors, "curseforge": cf_errors },
        "note": if missing.is_empty() {
            "Обязательные зависимости из манифестов JAR и метаданных платформ присутствуют."
        } else {
            "ОБНАРУЖЕНЫ НЕДОСТАЮЩИЕ ОБЯЗАТЕЛЬНЫЕ ЗАВИСИМОСТИ. Не говори, что сборка готова, пока они не установлены."
        },
    }))
}

/// `modrinth::versions` returns a JSON array; the agent prefers a typed Vec for mapping.
async fn mod_versions(project_id: &str, mc: Option<&str>, loader: Option<&str>) -> AppResult<Vec<Value>> {
    Ok(modrinth::versions(project_id, mc, loader)
        .await?
        .as_array()
        .cloned()
        .unwrap_or_default())
}

fn start_install(app: &AppHandle, state: &AppState, id: &str) -> AppResult<()> {
    if state.installs.lock().unwrap().contains_key(id) {
        return Ok(());
    }
    let app_handle = app.clone();
    let instance_id = id.to_string();
    tauri::async_runtime::spawn(async move {
        let state = app_handle.state::<AppState>();
        if let Err(error) = dispatch::install_instance(&app_handle, &state, &instance_id, None).await {
            game::emit(&app_handle, "op:progress", json!({
                "opId": format!("install:{instance_id}"),
                "kind": "install",
                "instanceId": instance_id,
                "label": "Установка не удалась",
                "fraction": null,
                "status": "error",
                "message": error.message,
            }));
        }
    });
    Ok(())
}

fn start_modpack(app: &AppHandle, state: &AppState, project_id: &str, version_id: Option<String>) -> AppResult<()> {
    {
        let mut packs = state.modrinth_packs.lock().unwrap();
        if packs.contains_key(project_id) {
            return Ok(());
        }
        packs.insert(project_id.to_string(), Arc::new(AtomicBool::new(false)));
    }
    let cancel = state.modrinth_packs.lock().unwrap().get(project_id).cloned().unwrap();
    let app_handle = app.clone();
    let pid = project_id.to_string();
    tauri::async_runtime::spawn(async move {
        let state = app_handle.state::<AppState>();
        let _ = modrinth::install_pack(&app_handle, &state, &pid, version_id.as_deref(), cancel).await;
    });
    Ok(())
}

fn pack(value: &Value, max_chars: usize) -> String {
    let text = value.to_string();
    if text.chars().count() <= max_chars {
        return text;
    }
    let preview: String = text.chars().take(max_chars.saturating_sub(220)).collect();
    json!({
        "truncated": true,
        "note": "Result was too long and is cut. Ask for less (limit/filters).",
        "preview": preview,
    })
    .to_string()
}

fn clean_description(markdown: &str) -> String {
    let mut out = String::new();
    let mut skip = false;
    let chars: Vec<char> = markdown.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if skip {
            if chars[i] == '>' {
                skip = false;
            }
            i += 1;
            continue;
        }
        if chars[i] == '<' {
            skip = true;
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .split('\n')
        .map(|line| line.trim_start_matches(['#', ' ', '*']).trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn slice_chars(text: &str, offset: usize, max: usize) -> (String, usize, Option<usize>) {
    let chars: Vec<char> = text.chars().collect();
    let from = offset.min(chars.len());
    let to = (from + max).min(chars.len());
    let part: String = chars[from..to].iter().collect();
    let next = if to < chars.len() { Some(to) } else { None };
    (part, to, next)
}

fn resolve_content_file(game_dir: &Path, kind: &str, reference: &str) -> AppResult<String> {
    let reference = need(reference.to_string(), "file")?;
    let list = content::list(game_dir, kind)?;
    let low = reference.to_lowercase();
    if let Some(found) = list.iter().find(|item| {
        let file = item.get("file").and_then(Value::as_str).unwrap_or("");
        file == reference || file.trim_end_matches(".disabled") == reference || file.to_lowercase() == low
    }) {
        return Ok(found.get("file").and_then(Value::as_str).unwrap_or("").to_string());
    }
    let partial: Vec<&Value> = list
        .iter()
        .filter(|item| {
            let file = item.get("file").and_then(Value::as_str).unwrap_or("").to_lowercase();
            let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase();
            file.contains(&low) || name.contains(&low)
        })
        .collect();
    if partial.len() == 1 {
        return Ok(partial[0].get("file").and_then(Value::as_str).unwrap_or("").to_string());
    }
    if partial.len() > 1 {
        let names: Vec<String> = partial
            .iter()
            .take(8)
            .map(|item| item.get("file").and_then(Value::as_str).unwrap_or("").to_string())
            .collect();
        return Err(AppError::new("AI_ARGS", format!("Several files match «{reference}»: {}. Use the exact file name.", names.join(", "))));
    }
    Err(AppError::new("FILE_MISSING", format!("No file matches «{reference}» in {kind}. Call list_mods to see the real names.")))
}

/// Resolves a tool path to a file inside one instance. Relative paths are relative to
/// `.minecraft`; `instance/` addresses the folder with instance.json.
fn instance_file_target(state: &AppState, args: &Value) -> AppResult<(PathBuf, PathBuf, String)> {
    let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
    let settings = state.settings.get()?;
    let instance_dir = instances::instance_dir(&state.data_dir, &settings, &item.meta.id)?;
    let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
    let raw = arg_str(args, &["path", "file", "target"]);
    if raw.is_empty() {
        return Err(AppError::new("AI_ARGS", "Missing required argument \"path\"."));
    }
    let raw = raw.replace('\\', "/");
    let (base, rest) = if let Some(rest) = raw.strip_prefix("instance/") {
        (instance_dir.clone(), rest)
    } else if let Some(rest) = raw.strip_prefix(".minecraft/") {
        (game_dir.clone(), rest)
    } else {
        (game_dir.clone(), raw.as_str())
    };
    let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
    if parts.is_empty() {
        return Err(AppError::new("AI_ARGS", "Path must point to a file inside the instance, not to its root."));
    }
    let target = paths::resolve_inside(&base, &parts)?;
    if target == base || target == instance_dir {
        return Err(AppError::new("AI_ARGS", "Path must point to a file inside the instance, not to its root."));
    }
    let display = if target.starts_with(&instance_dir) {
        format!("instance/{}", target.strip_prefix(&instance_dir).unwrap_or(&target).to_string_lossy().replace('\\', "/"))
    } else {
        target.to_string_lossy().to_string()
    };
    Ok((game_dir, target, display))
}

fn no_running_game(state: &AppState) -> AppResult<()> {
    if !running_ids(state).is_empty() {
        return Err(AppError::new("BUSY", "Останови игру перед изменением файлов инстанса."));
    }
    Ok(())
}

fn read_tail(file: &Path, max_chars: usize) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return Some(text);
    }
    Some(chars[chars.len() - max_chars..].iter().collect())
}

fn newest_crash_report(game_dir: &Path) -> Option<(String, PathBuf)> {
    let entries = std::fs::read_dir(game_dir.join("crash-reports")).ok()?;
    let mut best: Option<(SystemTime, String, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().map(|(time, _, _)| modified > *time).unwrap_or(true) {
            best = Some((modified, path.file_name()?.to_string_lossy().to_string(), path));
        }
    }
    best.map(|(_, name, path)| (name, path))
}

const LOG_FINDINGS: &[(&str, &str)] = &[
    ("unsupportedclassversionerror", "Wrong Java version for this Minecraft/mod (class file is newer than Java). Check the instance Java setting."),
    ("outofmemoryerror", "Out of memory: raise the maximum RAM of the instance (or lower it if the machine has too little)."),
    ("gc overhead limit", "Out of memory: raise the maximum RAM of the instance."),
    ("mixin apply failed", "Mixin conflict: a mod is incompatible with the game or another mod. Update or disable it."),
    ("missing or unsupported mandatory dependencies", "A required dependency is missing or has the wrong version (often Fabric API, Cloth Config, Architectury). Install it or update the mod."),
    ("modresultexception", "A mod dependency is missing or has the wrong version."),
    ("duplicate mod", "Duplicate or conflicting mods: look for two files of the same mod in list_mods."),
    ("nosuchmethoderror", "A mod does not match the game/loader/other mod version. Run check_mod_updates."),
    ("noclassdeffounderror", "A class is missing: usually a mod dependency. Install it or update the mod."),
    ("glfw error", "Graphics driver / OpenGL problem: update the GPU driver."),
    ("unknownhostexception", "Network problem while downloading/connecting."),
];

// ---------------------------------------------------------------- tools

/// Restores the newest snapshot. Also used by the launcher's own "Undo" button.
pub async fn undo_last(app: &AppHandle, state: &AppState) -> AppResult<Value> {
    let data = state.data_dir.clone();
    let Some(entry) = undo::last(&data)? else {
        return Ok(json!({ "ok": false, "note": "Нечего откатывать — список действий пуст." }));
    };
    let label = entry.label.clone();
    let undone = match &entry.kind {
        UndoKind::Files { .. } => undo::undo(&data, |_| Ok(()), |_| {})?,
        UndoKind::StopGame { .. } => undo::undo(&data, |_| Ok(()), |id| state.games.stop(id))?,
        UndoKind::StartGame { instance } => {
            let id = instance.clone();
            let settings = state.settings.get()?;
            let auth = state.auth.launch_auth(&settings).await?;
            let app = app.clone();
            let launch_data = data.clone();
            undo::undo(&data, move |_| state.games.launch(&app, &launch_data, &settings, &id, auth), |_| {})?
        }
    };
    Ok(json!({ "ok": true, "undone": label, "detail": undone, "note": "Восстановлено последнее действие Чалди." }))
}

pub fn undo_log(state: &AppState) -> Value {
    json!({
        "ok": true,
        "entries": undo::list(&state.data_dir),
        "note": "Newest first. Call undo_last to restore the newest one.",
    })
}

async fn run_tool(app: &AppHandle, state: &AppState, name: &str, args: &Value) -> AppResult<Value> {
    match name {
        "ask_user" => Ok(json!({ "status": "poll_shown", "note": "The poll is shown to the user; their answer arrives as the next message." })),

        "get_system_info" => {
            let settings = state.settings.get()?;
            let mut sys = System::new_all();
            sys.refresh_all();
            let latest = tokio::time::timeout(Duration::from_secs(3), versions::minecraft(&state.data_dir, false))
                .await
                .ok()
                .and_then(|result| result.ok())
                .and_then(|list| list.into_iter().find(|v| v.kind == "release").map(|v| v.id));
            Ok(json!({
                "launcherVersion": env!("CARGO_PKG_VERSION"),
                "platform": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
                "totalRamMb": sys.total_memory() / 1024,
                "freeRamMb": sys.available_memory() / 1024,
                "cpuCores": sys.physical_core_count().unwrap_or(0),
                "memoryAuto": settings.memory_auto,
                "memoryMinMb": settings.memory_min_mb,
                "memoryMaxMb": settings.memory_max_mb,
                "jvmArgs": settings.jvm_args,
                "language": settings.language,
                "latestMinecraftRelease": latest,
            }))
        }

        "list_instances" => {
            let settings = state.settings.get()?;
            let list = instances::list(&state.data_dir, &settings, &running_ids(state))?;
            Ok(Value::Array(list.iter().map(|item| json!({
                "id": item.meta.id, "name": item.meta.name, "mcVersion": item.meta.mc_version,
                "loader": item.meta.loader.r#type, "loaderVersion": item.meta.loader.version,
                "state": item.state, "stateReason": item.state_reason,
                "running": item.running, "modCount": item.mod_count, "lastPlayedAt": item.meta.last_played_at,
            })).collect()))
        }

        "get_instance" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let mut view = serde_json::to_value(&item)?;
            if let Some(map) = view.as_object_mut() {
                map.remove("coverDataUrl");
                map.remove("path");
            }
            Ok(view)
        }

        "get_instance_status" => {
            let reference = arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]);
            let wait = arg_int(args, &["waitSeconds"], 0, 0, 45) as u64;
            let mut item = find_instance(state, &reference)?;
            let deadline = std::time::Instant::now() + Duration::from_secs(wait);
            while state.installs.lock().unwrap().contains_key(&item.meta.id) && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                item = find_instance(state, &item.meta.id)?;
            }
            let installing = state.installs.lock().unwrap().contains_key(&item.meta.id);
            Ok(json!({
                "id": item.meta.id, "name": item.meta.name, "state": item.state, "stateReason": item.state_reason,
                "installing": installing, "running": item.running,
                "next": if installing {
                    "Still installing. Call get_instance_status again with waitSeconds=30, or tell the user it is in progress."
                } else if item.state == "ready" {
                    "Ready to play."
                } else {
                    "Installation did not finish. Explain the state to the user."
                },
            }))
        }

        "list_minecraft_versions" => {
            let kind = arg_str(args, &["type"]);
            let kind = if ["release", "snapshot", "all"].contains(&kind.as_str()) { kind } else { "release".to_string() };
            let query = arg_str(args, &["query"]).to_lowercase();
            let limit = arg_int(args, &["limit"], 15, 1, 50) as usize;
            let list = versions::minecraft(&state.data_dir, false).await?;
            let latest_release = list.iter().find(|v| v.kind == "release").map(|v| v.id.clone());
            let latest_snapshot = list.iter().find(|v| v.kind == "snapshot").map(|v| v.id.clone());
            let matched: Vec<&versions::McVersionEntry> = list
                .iter()
                .filter(|v| (kind == "all" && (v.kind == "release" || v.kind == "snapshot")) || v.kind == kind)
                .filter(|v| query.is_empty() || v.id.to_lowercase().starts_with(&query))
                .collect();
            Ok(json!({
                "latestRelease": latest_release,
                "latestSnapshot": latest_snapshot,
                "matched": matched.len(),
                "versions": matched.iter().take(limit).map(|v| json!({
                    "id": v.id, "type": v.kind, "released": v.release_time.chars().take(10).collect::<String>(),
                })).collect::<Vec<_>>(),
            }))
        }

        "get_loader_versions" => {
            let mc = resolve_mc(state, &arg_str(args, &["mcVersion", "minecraft", "version"])).await?;
            let limit = arg_int(args, &["limit"], 8, 1, 30) as usize;
            let raw_loader = arg_str(args, &["loader"]);
            if !raw_loader.is_empty() {
                let loader = normalize_loader(&raw_loader)?;
                if loader == "vanilla" {
                    return Ok(json!({ "mcVersion": mc, "note": "Vanilla has no loader versions." }));
                }
                let list = versions::loader_versions(&loader, &mc).await?;
                let best = list.iter().find(|v| v.recommended).or_else(|| list.iter().find(|v| v.stable)).or_else(|| list.first());
                return Ok(json!({
                    "mcVersion": mc, "loader": loader, "supported": !list.is_empty(),
                    "best": best.map(|v| v.version.clone()),
                    "versions": list.iter().take(limit).map(|v| json!({
                        "version": v.version, "stable": v.stable, "recommended": v.recommended,
                    })).collect::<Vec<_>>(),
                    "note": if list.is_empty() { format!("{} does not support Minecraft {mc}.", loader_title(&loader)) } else { String::new() },
                }));
            }
            let mut loaders = Vec::new();
            for loader in L4 {
                let list = versions::loader_versions(loader, &mc).await.unwrap_or_default();
                let best = list.iter().find(|v| v.recommended).or_else(|| list.iter().find(|v| v.stable)).or_else(|| list.first());
                loaders.push(json!({
                    "loader": loader, "supported": !list.is_empty(),
                    "best": best.map(|v| v.version.clone()), "versionsCount": list.len(),
                }));
            }
            Ok(json!({ "mcVersion": mc, "loaders": loaders }))
        }

        "create_instance" => {
            let name = need(arg_str(args, &["name"]), "name")?.chars().take(48).collect::<String>();
            let loader = normalize_loader(&arg_str(args, &["loader"]))?;
            let mc = resolve_mc(state, &arg_str(args, &["mcVersion", "version"])).await?;
            let loader_version = if loader == "vanilla" {
                None
            } else {
                Some(resolve_loader_version(&loader, &mc, &arg_str(args, &["loaderVersion"])).await?)
            };
            let settings = state.settings.get()?;
            let existing = instances::list(&state.data_dir, &settings, &running_ids(state))?
                .into_iter()
                .find(|item| item.meta.name.to_lowercase() == name.to_lowercase() && item.meta.mc_version == mc && item.meta.loader.r#type == loader);
            let reused = existing.is_some();
            let item = match existing {
                Some(item) => item,
                None => instances::create(&state.data_dir, &settings, &running_ids(state), &json!({
                    "name": name,
                    "description": arg_str(args, &["description"]).chars().take(120).collect::<String>(),
                    "mcVersion": mc,
                    "loader": { "type": loader, "version": loader_version },
                    "kind": if loader == "vanilla" { "official" } else { "modded" },
                    "coverColor": 0,
                    "settings": { "javaPath": "", "memoryMinMb": null, "memoryMaxMb": null, "jvmArgs": settings.jvm_args, "width": null, "height": null, "fullscreen": null },
                }))?,
            };
            let install_started = arg_bool(args, &["install"], false)
                && item.state != "ready"
                && !state.installs.lock().unwrap().contains_key(&item.meta.id);
            if install_started {
                start_install(app, state, &item.meta.id)?;
            }
            Ok(json!({
                "ok": true, "id": item.meta.id, "name": item.meta.name, "mcVersion": mc,
                "loader": { "type": loader, "version": loader_version.or(item.meta.loader.version) },
                "reusedExisting": reused, "installStarted": install_started, "gameInstalled": item.state == "ready",
                "next": if install_started {
                    "The instance profile is created but the game is NOT installed yet (download runs in background). Do NOT say it is installed: call get_instance_status with waitSeconds=30 until state is ready."
                } else if item.state == "ready" {
                    "Instance is ready."
                } else {
                    "The game is NOT installed. Call install_instance to download it."
                },
            }))
        }

        "install_instance" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if state.installs.lock().unwrap().contains_key(&item.meta.id) {
                return Ok(json!({ "ok": true, "alreadyRunning": true, "next": "Installation is already running: use get_instance_status." }));
            }
            if item.state == "ready" && !arg_bool(args, &["repair"], false) {
                return Ok(json!({ "ok": true, "alreadyInstalled": true, "id": item.meta.id, "note": "Already installed. Pass repair=true only to reinstall." }));
            }
            start_install(app, state, &item.meta.id)?;
            Ok(json!({
                "ok": true, "id": item.meta.id, "installStarted": true, "gameInstalled": false,
                "next": "Download just STARTED, the game is NOT installed yet. Do NOT say it is installed: call get_instance_status with waitSeconds=30 until state is ready.",
            }))
        }

        "get_logs" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let max_lines = arg_int(args, &["maxLines"], 120, 20, 300) as usize;
            let source = arg_str(args, &["source"]);
            let session = state.games.logs(&item.meta.id);
            let mut crash_name: Option<String> = None;
            let (used_source, raw): (String, String) = if source == "crash" {
                match newest_crash_report(&game_dir) {
                    Some((name, path)) => {
                        crash_name = Some(name);
                        ("crash".to_string(), std::fs::read_to_string(path).unwrap_or_default().chars().take(200_000).collect())
                    }
                    None => return Ok(json!({ "id": item.meta.id, "source": "crash", "lines": [], "note": "No crash reports found for this instance." })),
                }
            } else if source == "session" || (source != "latest" && !session.is_empty()) {
                ("session".to_string(), session.join("\n"))
            } else {
                match read_tail(&game_dir.join("logs").join("latest.log"), 600_000) {
                    Some(text) => ("latest".to_string(), text),
                    None if !session.is_empty() => ("session".to_string(), session.join("\n")),
                    None => return Ok(json!({ "id": item.meta.id, "source": "latest", "lines": [], "note": "No logs yet: the game has not been started." })),
                }
            };
            let lines: Vec<String> = raw.lines().map(|line| line.trim_end().to_string()).filter(|line| !line.trim().is_empty()).collect();
            let scan = raw.to_lowercase();
            let findings: Vec<Value> = LOG_FINDINGS
                .iter()
                .filter_map(|(needle, cause)| {
                    let evidence = lines.iter().find(|line| line.to_lowercase().contains(needle))?;
                    Some(json!({ "cause": cause, "evidence": evidence.chars().take(300).collect::<String>() }))
                })
                .collect();
            let mut shown: Vec<String> = if arg_bool(args, &["errorsOnly"], false) {
                let keywords = ["error", "fatal", "exception", "caused by", "crash", "mixin apply failed", "failed to", "could not", "missing or unsupported"];
                lines.iter().filter(|line| keywords.iter().any(|word| line.to_lowercase().contains(word))).cloned().collect()
            } else {
                lines.clone()
            };
            let filter = arg_str(args, &["grep"]).to_lowercase();
            if !filter.is_empty() {
                let parts: Vec<&str> = filter.split('|').map(str::trim).filter(|p| !p.is_empty()).collect();
                shown.retain(|line| parts.iter().any(|part| line.to_lowercase().contains(part)));
            }
            let chosen: Vec<&String> = if used_source == "crash" {
                shown.iter().take(max_lines).collect()
            } else {
                shown.iter().skip(shown.len().saturating_sub(max_lines)).collect()
            };
            let picked: Vec<String> = chosen
                .into_iter()
                .map(|line| if line.chars().count() > 400 { format!("{}…", line.chars().take(400).collect::<String>()) } else { line.clone() })
                .collect();
            let _ = scan;
            Ok(json!({
                "id": item.meta.id, "source": used_source, "crashReport": crash_name,
                "totalLines": lines.len(), "shown": picked.len(), "findings": findings, "lines": picked,
            }))
        }

        "list_mods" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let kind = if arg_str(args, &["kind"]) == "resourcepacks" { "resourcepacks" } else { "mods" };
            let list = content::list(&game_dir, kind)?;
            let by_file: HashMap<String, String> = modrinth::installed_info(&game_dir)
                .into_iter()
                .map(|(project, file, _)| (file, project))
                .collect();
            let items: Vec<Value> = list.iter().take(150).map(|entry| {
                let file = entry.get("file").and_then(Value::as_str).unwrap_or("");
                let base = file.trim_end_matches(".disabled");
                json!({
                    "file": file,
                    "name": entry.get("name").and_then(Value::as_str).unwrap_or(""),
                    "version": entry.get("version").and_then(Value::as_str).unwrap_or(""),
                    "enabled": entry.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                    "sizeKb": entry.get("size").and_then(Value::as_u64).unwrap_or(0) / 1024,
                    "projectId": by_file.get(base).cloned(),
                })
            }).collect();
            Ok(json!({ "instance": item.meta.id, "kind": kind, "total": list.len(), "items": items }))
        }

        "get_installed_mod_info" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let file = resolve_content_file(&game_dir, "mods", &arg_str(args, &["file", "mod", "name"]))?;
            let by_file: HashMap<String, String> = modrinth::installed_info(&game_dir)
                .into_iter()
                .map(|(project, installed_file, _)| (installed_file, project))
                .collect();
            let base = file.trim_end_matches(".disabled").to_string();
            let project_id = by_file.get(&base).cloned();
            let info = match project_id.as_deref() {
                Some(id) => modrinth::project(id).await?,
                None => Value::Null,
            };
            Ok(json!({
                "file": file, "projectId": project_id,
                "title": info.get("title").and_then(Value::as_str).unwrap_or(""),
                "description": info.get("description").and_then(Value::as_str).unwrap_or(""),
                "note": if info.is_null() { "The jar has no Modrinth id. Use search_modrinth by the mod name." } else { "" },
            }))
        }

        "search_modrinth" => {
            let query = need(arg_str(args, &["query", "q"]), "query")?;
            let mut kind = arg_str(args, &["type"]);
            if !["mod", "modpack", "resourcepack"].contains(&kind.as_str()) {
                kind = "mod".to_string();
            }
            let mut sort = arg_str(args, &["sort"]);
            if !["relevance", "downloads", "follows", "newest", "updated"].contains(&sort.as_str()) {
                sort = "relevance".to_string();
            }
            let limit = arg_int(args, &["limit"], 8, 1, 20) as usize;
            let offset = arg_int(args, &["offset"], 0, 0, 1000) as u64;
            let reference = arg_str(args, &["instance", "instanceId", "id", "name"]);
            let mut mc = arg_str(args, &["mcVersion"]);
            let mut loader = arg_str(args, &["loader"]);
            if !reference.is_empty() {
                let item = find_instance(state, &reference)?;
                if mc.is_empty() {
                    mc = item.meta.mc_version.clone();
                }
                if loader.is_empty() && item.meta.loader.r#type != "vanilla" {
                    loader = item.meta.loader.r#type.clone();
                }
            }
            let loader = if loader.is_empty() { None } else { Some(normalize_loader(&loader)?) };
            let loader = loader.filter(|value| value != "vanilla");
            let mut result = mr_search(&query, &kind, &sort, offset, Some(&mc), loader.as_deref()).await?;
            let mut dropped = false;
            if result.get("hits").and_then(Value::as_array).map(Vec::is_empty).unwrap_or(true) && (!mc.is_empty() || loader.is_some()) {
                result = mr_search(&query, &kind, &sort, offset, None, None).await?;
                dropped = result.get("hits").and_then(Value::as_array).map(|hits| !hits.is_empty()).unwrap_or(false);
            }
            let empty_hits = Vec::new();
            let hits: Vec<Value> = result
                .get("hits")
                .and_then(Value::as_array)
                .unwrap_or(&empty_hits)
                .iter()
                .take(limit)
                .map(|hit| json!({
                    "projectId": hit.get("id").and_then(Value::as_str).unwrap_or(""),
                    "slug": hit.get("slug").and_then(Value::as_str).unwrap_or(""),
                    "title": hit.get("title").and_then(Value::as_str).unwrap_or(""),
                    "summary": hit.get("description").and_then(Value::as_str).unwrap_or("").chars().take(200).collect::<String>(),
                    "author": hit.get("author").and_then(Value::as_str).unwrap_or(""),
                    "downloads": hit.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                    "categories": hit.get("categories").cloned().unwrap_or(Value::Null),
                }))
                .collect();
            Ok(json!({
                "total": result.get("total").and_then(Value::as_u64).unwrap_or(0),
                "filters": { "mcVersion": if dropped { Value::Null } else { json!(mc) }, "loader": if dropped { Value::Null } else { json!(loader) } },
                "warning": if dropped { format!("Nothing matched Minecraft {} / {}; these results are UNFILTERED and may not be installable. Verify with check_mod_compatibility.", mc, loader.clone().unwrap_or_default()) } else { String::new() },
                "hits": hits,
            }))
        }

        "get_mod_info" => {
            let id = need(arg_str(args, &["projectId", "project_id", "slug", "id", "project"]), "projectId")?;
            let info = modrinth::project(&id).await?;
            let body = clean_description(info.get("body").and_then(Value::as_str).unwrap_or(""));
            let offset = arg_int(args, &["offset"], 0, 0, 1_000_000) as usize;
            let max_chars = arg_int(args, &["maxChars"], 3500, 500, 8000) as usize;
            let (description, end, next_offset) = slice_chars(&body, offset, max_chars);
            Ok(json!({
                "projectId": info.get("id").and_then(Value::as_str).unwrap_or(&id),
                "slug": info.get("slug").and_then(Value::as_str).unwrap_or(""),
                "title": info.get("title").and_then(Value::as_str).unwrap_or(""),
                "type": info.get("type").and_then(Value::as_str).unwrap_or(""),
                "summary": info.get("description").and_then(Value::as_str).unwrap_or(""),
                "downloads": info.get("downloads").and_then(Value::as_u64).unwrap_or(0),
                "follows": info.get("follows").and_then(Value::as_u64).unwrap_or(0),
                "categories": info.get("categories").cloned().unwrap_or(Value::Null),
                "loaders": info.get("loaders").cloned().unwrap_or(Value::Null),
                "minecraftVersions": info.get("gameVersions").cloned().unwrap_or(Value::Null),
                "license": info.get("license").cloned().unwrap_or(Value::Null),
                "updated": info.get("updated").and_then(Value::as_str).map(|s| s.chars().take(10).collect::<String>()),
                "description": if description.is_empty() { info.get("description").and_then(Value::as_str).unwrap_or("").to_string() } else { description },
                "descriptionLength": body.chars().count(), "offset": offset, "nextOffset": next_offset,
                "sliceEnd": end,
            }))
        }

        "get_mod_versions" => {
            let id = need(arg_str(args, &["projectId", "project_id", "slug", "id", "project"]), "projectId")?;
            let mut mc = arg_str(args, &["mcVersion"]);
            let mut loader = arg_str(args, &["loader"]);
            let reference = arg_str(args, &["instance", "instanceId", "id", "name"]);
            if !reference.is_empty() {
                let item = find_instance(state, &reference)?;
                if mc.is_empty() {
                    mc = item.meta.mc_version.clone();
                }
                if loader.is_empty() && item.meta.loader.r#type != "vanilla" {
                    loader = item.meta.loader.r#type.clone();
                }
            }
            let loader = if loader.is_empty() { None } else { Some(normalize_loader(&loader)?) };
            let limit = arg_int(args, &["limit"], 6, 1, 15) as usize;
            let list = mod_versions(&id, if mc.is_empty() { None } else { Some(&mc) }, loader.as_deref()).await?;
            let with_changelog = arg_bool(args, &["changelog"], false);
            let versions: Vec<Value> = list.iter().take(limit).map(|version| json!({
                "versionId": version.get("id").and_then(Value::as_str).unwrap_or(""),
                "versionNumber": version.get("versionNumber").and_then(Value::as_str).unwrap_or(""),
                "name": version.get("name").and_then(Value::as_str).unwrap_or(""),
                "type": version.get("type").and_then(Value::as_str).unwrap_or(""),
                "published": version.get("published").and_then(Value::as_str).map(|s| s.chars().take(10).collect::<String>()),
                "minecraftVersions": version.get("gameVersions").cloned().unwrap_or(Value::Null),
                "loaders": version.get("loaders").cloned().unwrap_or(Value::Null),
                "file": version.get("fileName").and_then(Value::as_str).unwrap_or(""),
                "requiredDeps": version.get("requiredDeps").and_then(Value::as_u64).unwrap_or(0),
                "changelog": if with_changelog { version.get("changelog").and_then(Value::as_str).unwrap_or("").chars().take(500).collect::<String>() } else { String::new() },
            })).collect();
            Ok(json!({
                "projectId": id, "filters": { "mcVersion": mc, "loader": loader }, "count": versions.len(),
                "versions": versions,
                "note": if versions.is_empty() { "No versions for these filters. Call again without instance/mcVersion/loader to see everything the project offers." } else { "" },
            }))
        }

        "check_mod_compatibility" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let id = need(arg_str(args, &["projectId", "project_id", "slug", "id", "project"]), "projectId")?;
            let settings = state.settings.get()?;
            let _game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let list = mod_versions(&id, Some(&item.meta.mc_version), if item.meta.loader.r#type == "vanilla" { None } else { Some(&item.meta.loader.r#type) }).await?;
            if list.is_empty() {
                let project = modrinth::project(&id).await.ok();
                return Ok(json!({
                    "compatible": false, "instance": item.meta.id, "minecraft": item.meta.mc_version, "loader": item.meta.loader.r#type,
                    "reason": "This project has no build for the instance version/loader.",
                    "projectSupports": project.map(|p| json!({
                        "loaders": p.get("loaders").cloned().unwrap_or(Value::Null),
                        "minecraftVersions": p.get("gameVersions").cloned().unwrap_or(Value::Null),
                    })),
                }));
            }
            Ok(json!({
                "compatible": true, "instance": item.meta.id, "minecraft": item.meta.mc_version, "loader": item.meta.loader.r#type,
                "willInstall": list.iter().take(10).map(|version| json!({
                    "versionId": version.get("id").and_then(Value::as_str).unwrap_or(""),
                    "versionNumber": version.get("versionNumber").and_then(Value::as_str).unwrap_or(""),
                    "type": version.get("type").and_then(Value::as_str).unwrap_or(""),
                    "file": version.get("fileName").and_then(Value::as_str).unwrap_or(""),
                    "requiredDeps": version.get("requiredDeps").and_then(Value::as_u64).unwrap_or(0),
                })).collect::<Vec<_>>(),
                "note": "The first compatible version will be installed with its required dependencies.",
            }))
        }

        "install_mod" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let id = need(arg_str(args, &["projectId", "project_id", "slug", "id", "project"]), "projectId")?;
            let mut kind = arg_str(args, &["kind"]);
            if kind != "resourcepack" {
                kind = "mod".to_string();
            }
            let settings = state.settings.get()?;
            let version_id = arg_str(args, &["versionId"]);
            let files = modrinth::install(
                &state.data_dir, &settings, app, &item.meta.id, &id, &kind,
                if version_id.is_empty() { None } else { Some(&version_id) }, None,
            ).await?;
            Ok(json!({
                "ok": true, "instance": item.meta.id, "files": files,
                "note": if files.len() > 1 { "The first file is the requested mod, the others are its required dependencies." } else { "" },
            }))
        }

        "install_modpack" => {
            let id = need(arg_str(args, &["projectId", "project_id", "slug", "id", "project"]), "projectId")?;
            let version_id = arg_str(args, &["versionId"]);
            start_modpack(app, state, &id, if version_id.is_empty() { None } else { Some(version_id) })?;
            Ok(json!({
                "ok": true, "started": true, "completed": false,
                "message": "Modpack download STARTED in the background; it is NOT installed yet. The new instance appears in list_instances; follow it with get_instance_status. Do not say it is installed before state is ready.",
            }))
        }

        "search_curseforge" => {
            let query = need(arg_str(args, &["query", "q", "name", "mod"]), "query")?;
            let mut class_type = arg_str(args, &["classType", "class", "type"]);
            if !["mod", "modpack", "resourcepack", "shader", "datapack"].contains(&class_type.as_str()) {
                class_type = "mod".to_string();
            }
            let mut sort = arg_str(args, &["sort"]);
            if !["featured", "popularity", "lastUpdated"].contains(&sort.as_str()) {
                sort = "featured".to_string();
            }
            let mut mc = arg_str(args, &["mcVersion", "minecraft", "version"]);
            let mut loader = arg_str(args, &["loader"]);
            let reference = arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]);
            if !reference.is_empty() {
                let item = find_instance(state, &reference)?;
                if mc.is_empty() { mc = item.meta.mc_version.clone(); }
                if loader.is_empty() { loader = item.meta.loader.r#type.clone(); }
            }
            if loader == "vanilla" { loader.clear(); }
            let limit = arg_int(args, &["limit"], 10, 1, 20) as usize;
            let offset = arg_int(args, &["offset"], 0, 0, 10_000) as u64;
            let result = cf_search(
                &query, &class_type, &sort, offset,
                if mc.is_empty() { None } else { Some(&mc) },
                if loader.is_empty() { None } else { Some(&loader) },
            ).await?;
            let total = result.get("total").and_then(Value::as_u64).unwrap_or(0);
            let hits: Vec<Value> = result.get("hits").and_then(Value::as_array)
                .map(|list| list.iter().take(limit).cloned().collect())
                .unwrap_or_default();
            Ok(json!({
                "hits": hits, "total": total, "offset": offset,
                "note": "Используй числовой modId в get_curseforge_mod_info / get_curseforge_files / install_curseforge_mod.",
            }))
        }

        "get_curseforge_mod_info" => {
            let mod_id = arg_int(args, &["modId", "projectId", "id"], 0, 1, i64::MAX);
            curseforge::project(mod_id).await
        }

        "get_curseforge_files" => {
            let mod_id = arg_int(args, &["modId", "projectId", "id"], 0, 1, i64::MAX);
            let mut mc = arg_str(args, &["mcVersion", "minecraft", "version"]);
            let mut loader = arg_str(args, &["loader"]);
            let reference = arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]);
            if !reference.is_empty() {
                let item = find_instance(state, &reference)?;
                if mc.is_empty() { mc = item.meta.mc_version.clone(); }
                if loader.is_empty() { loader = item.meta.loader.r#type.clone(); }
            }
            if loader == "vanilla" { loader.clear(); }
            let limit = arg_int(args, &["limit"], 10, 1, 50) as usize;
            let files = curseforge::files(
                mod_id,
                if mc.is_empty() { None } else { Some(&mc) },
                if loader.is_empty() { None } else { Some(&loader) },
            ).await?;
            let all = files.as_array().cloned().unwrap_or_default();
            Ok(json!({
                "modId": mod_id, "count": all.len(),
                "files": all.iter().take(limit).cloned().collect::<Vec<_>>(),
                "note": "requiredDependencies — это id модов CurseForge, которые ОБЯЗАТЕЛЬНО нужны. install_curseforge_mod установит их автоматически.",
            }))
        }

        "check_curseforge_compatibility" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let mod_id = arg_int(args, &["modId", "projectId", "id"], 0, 1, i64::MAX);
            let file_id = arg_int(args, &["fileId"], 0, 0, i64::MAX);
            let loader = if item.meta.loader.r#type == "vanilla" { None } else { Some(item.meta.loader.r#type.as_str()) };
            let files = curseforge::files(mod_id, Some(&item.meta.mc_version), loader).await?;
            let all = files.as_array().cloned().unwrap_or_default();
            let selected: Vec<Value> = if file_id > 0 {
                all.into_iter().filter(|file| file.get("id").and_then(Value::as_i64) == Some(file_id)).collect()
            } else {
                all.into_iter().take(10).collect()
            };
            Ok(json!({
                "compatible": !selected.is_empty(),
                "instance": item.meta.id, "minecraft": item.meta.mc_version, "loader": item.meta.loader.r#type,
                "willInstall": selected.iter().map(|file| json!({
                    "fileId": file.get("id"), "fileName": file.get("fileName"),
                    "releaseType": file.get("releaseType"),
                    "requiredDependencies": file.get("requiredDependencies"),
                })).collect::<Vec<_>>(),
                "note": if selected.is_empty() {
                    "Нет файла под эту версию Minecraft / загрузчик. Выбери другой мод или спроси пользователя."
                } else {
                    "Обязательные зависимости скачает install_curseforge_mod автоматически."
                },
            }))
        }

        "install_curseforge_mod" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if running_ids(state).contains(&item.meta.id) {
                return Err(AppError::new("BUSY", "Останови игру, чтобы менять моды"));
            }
            let mod_id = arg_int(args, &["modId", "projectId", "id"], 0, 1, i64::MAX);
            let mut kind = arg_str(args, &["kind"]);
            if !["mod", "resourcepack", "shader", "datapack"].contains(&kind.as_str()) {
                kind = "mod".to_string();
            }
            let file_id = arg_int(args, &["fileId"], 0, 0, i64::MAX);
            let settings = state.settings.get()?;
            curseforge::install_with_dependencies(
                &state.data_dir, &settings, app, &item.meta.id, mod_id, &kind,
                if file_id > 0 { Some(file_id) } else { None },
            ).await
        }

        "validate_dependencies" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let fix = arg_bool(args, &["fix", "autoInstall", "autoFix"], true);
            let initial = audit_dependencies(&game_dir).await?;
            let missing_initial = initial.get("missing").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut auto_installed: Vec<Value> = Vec::new();
            let mut failures: Vec<Value> = Vec::new();
            if fix {
                let mut wanted: Vec<String> = Vec::new();
                for entry in &missing_initial {
                    let dep = arg_str(entry, &["modId", "id"]);
                    if !dep.is_empty() && !wanted.iter().any(|value| value.eq_ignore_ascii_case(&dep)) {
                        wanted.push(dep);
                    }
                }
                for dep in wanted {
                    match install_dependency_by_id(app, state, &item.meta.id, &dep).await {
                        Ok(value) => auto_installed.push(value),
                        Err(error) => failures.push(json!({
                            "modId": dep, "code": error.code, "error": error.message,
                        })),
                    }
                }
            }
            let final_audit = audit_dependencies(&game_dir).await?;
            Ok(json!({
                "ok": final_audit.get("ok").and_then(Value::as_bool).unwrap_or(false),
                "instance": item.meta.id,
                "initialMissing": missing_initial,
                "autoInstalled": auto_installed,
                "failed": failures,
                "missingDependencies": final_audit.get("missing").cloned().unwrap_or(Value::Null),
                "manifestAudit": final_audit.get("manifest").cloned().unwrap_or(Value::Null),
                "note": if final_audit.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    "Все обязательные моды из манифестов JAR и метаданных платформ на месте. Для проверки конфликтов всё равно запусти игру."
                } else {
                    "ОСТАЛИСЬ НЕДОСТАЮЩИЕ ОБЯЗАТЕЛЬНЫЕ МОДЫ. Установи их или назови пользователю точные имена; не говори, что сборка готова."
                },
            }))
        }

        "build_modpack" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if running_ids(state).contains(&item.meta.id) {
                return Err(AppError::new("BUSY", "Останови игру, чтобы менять моды"));
            }
            let theme = need(arg_str(args, &["theme", "query", "name", "mod"]), "theme")?;
            let target = arg_int(args, &["count", "limit"], 8, 3, 12) as usize;
            let settings = state.settings.get()?;
            let mc = item.meta.mc_version.clone();
            let loader = if item.meta.loader.r#type == "vanilla" {
                None
            } else {
                Some(normalize_loader(&item.meta.loader.r#type)?)
            };

            let mut queries = vec![theme.clone(), format!("{theme} addon")];
            for essential in ["JEI", "Jade", "Sodium", "Iris", "JourneyMap", "Inventory HUD+"] {
                queries.push(essential.to_string());
            }

            let mut installed: Vec<Value> = Vec::new();
            let mut failed: Vec<Value> = Vec::new();
            let mut seen: Vec<String> = Vec::new();

            for query in queries {
                if installed.len() >= target { break; }
                let result = match mr_search(&query, "mod", "relevance", 0, Some(&mc), loader.as_deref()).await {
                    Ok(value) => value,
                    Err(error) => {
                        failed.push(json!({ "query": query, "code": error.code, "error": error.message }));
                        continue;
                    }
                };
                let hits = result.get("hits").and_then(Value::as_array).cloned().unwrap_or_default();
                for hit in hits.into_iter().take(4) {
                    let id = hit.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    if id.is_empty() || seen.contains(&id) { continue; }
                    seen.push(id.clone());
                    let title = hit.get("title").and_then(Value::as_str).unwrap_or("").to_string();
                    match modrinth::install(
                        &state.data_dir, &settings, app, &item.meta.id, &id, "mod", None, None,
                    ).await {
                        Ok(files) => installed.push(json!({
                            "projectId": id, "title": title, "files": files,
                        })),
                        Err(error) => failed.push(json!({
                            "projectId": id, "title": title, "code": error.code, "error": error.message,
                        })),
                    }
                    if installed.len() >= target { break; }
                }
            }

            // Second pass: fetch the dependency list of every version we actually installed.
            // Modrinth metadata is sometimes incomplete, so catch anything still missing here.
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let present: Vec<String> = modrinth::installed_info(&game_dir)
                .into_iter().map(|(project_id, _, _)| project_id).collect();
            let mut auto_deps: Vec<Value> = Vec::new();
            let mut missing: Vec<Value> = Vec::new();
            for (project_id, _file, version_id) in modrinth::installed_info(&game_dir) {
                if version_id.is_empty() { continue; }
                let deps = match modrinth::required_dependencies(&version_id).await {
                    Ok(deps) => deps,
                    Err(error) => {
                        missing.push(json!({ "projectId": project_id, "code": error.code, "error": error.message }));
                        continue;
                    }
                };
                for dep in deps {
                    if present.contains(&dep)
                        || auto_deps.iter().any(|value| value.get("projectId").and_then(Value::as_str) == Some(dep.as_str()))
                    { continue; }
                    match modrinth::install(
                        &state.data_dir, &settings, app, &item.meta.id, &dep, "mod", None, None,
                    ).await {
                        Ok(files) => auto_deps.push(json!({ "projectId": dep, "installedFor": project_id, "files": files })),
                        Err(error) => missing.push(json!({
                            "projectId": dep, "installedFor": project_id, "code": error.code, "error": error.message,
                        })),
                    }
                }
            }

            // Local jar manifests catch what platform metadata misses (e.g. Waystonesable -> Waystones).
            let audit = audit_dependencies(&game_dir).await?;
            if let Some(items) = audit.get("missing").and_then(Value::as_array) {
                for entry in items {
                    let dep = arg_str(entry, &["modId", "id"]);
                    if dep.is_empty() || missing.iter().any(|value| {
                        arg_str(value, &["modId", "projectId", "id"]).eq_ignore_ascii_case(&dep)
                    }) {
                        continue;
                    }
                    missing.push(entry.clone());
                }
            }
            let ok = missing.is_empty();
            Ok(json!({
                "ok": ok,
                "instance": item.meta.id,
                "theme": theme,
                "target": target,
                "installed": installed,
                "autoDependencies": auto_deps,
                "missingDependencies": missing,
                "manifestAudit": audit.get("manifest").cloned().unwrap_or(Value::Null),
                "failed": failed,
                "note": if ok {
                    "Это САМОСТОЯТЕЛЬНО собранная сборка, а не готовый модпак. Аудит зависимостей пройден; перед ответом всё равно вызови validate_dependencies и list_mods."
                } else {
                    "В этой сборке не хватает обязательных зависимостей. Вызови validate_dependencies(fix=true), повтори аудит и только потом говори, что сборка готова."
                },
            }))
        }

        "validate_modpack" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let audit = audit_dependencies(&game_dir).await?;
            let mod_count = modrinth::installed_info(&game_dir).len() + curseforge::installed_info(&game_dir).len();
            let ok = audit.get("ok").and_then(Value::as_bool).unwrap_or(false);
            Ok(json!({
                "ok": ok,
                "instance": item.meta.id,
                "modCount": mod_count,
                "missingDependencies": audit.get("missing").cloned().unwrap_or(Value::Null),
                "manifestAudit": audit.get("manifest").cloned().unwrap_or(Value::Null),
                "note": if ok {
                    "Modrinth, CurseForge и манифесты JAR не нашли недостающих зависимостей. Это не заменяет запуск игры: конфликты модов всё ещё возможны."
                } else {
                    "Найдены недостающие обязательные зависимости (в том числе в манифестах JAR, например Waystonesable → Waystones). Установи их и повтори проверку."
                },
            }))
        }

        "remove_mod" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if running_ids(state).contains(&item.meta.id) {
                return Err(AppError::new("BUSY", "Останови игру, чтобы менять файлы"));
            }
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let kind = if arg_str(args, &["kind"]) == "resourcepacks" { "resourcepacks" } else { "mods" };
            let file = resolve_content_file(&game_dir, kind, &arg_str(args, &["file", "mod", "name"]))?;
            content::remove(&game_dir, kind, &file, true)?;
            Ok(json!({ "ok": true, "removed": file }))
        }

        "set_mod_enabled" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if running_ids(state).contains(&item.meta.id) {
                return Err(AppError::new("BUSY", "Останови игру, чтобы менять файлы"));
            }
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let file = resolve_content_file(&game_dir, "mods", &arg_str(args, &["file", "mod", "name"]))?;
            let enabled = arg_bool(args, &["enabled"], true);
            let is_enabled = !file.ends_with(".disabled");
            if is_enabled != enabled {
                content::toggle(&game_dir, "mods", &file)?;
            }
            Ok(json!({
                "ok": true,
                "file": if enabled { file.trim_end_matches(".disabled").to_string() } else if file.ends_with(".disabled") { file } else { format!("{file}.disabled") },
                "enabled": enabled, "changed": is_enabled != enabled,
            }))
        }

        "check_mod_updates" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if item.meta.loader.r#type == "vanilla" {
                return Err(AppError::new("LOADER_REQUIRED", "Instance is vanilla: it has no mods."));
            }
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let installed = modrinth::installed_info(&game_dir);
            let mut updates = Vec::new();
            for (project_id, file, version_id) in installed.into_iter().take(40) {
                let versions = match mod_versions(&project_id, Some(&item.meta.mc_version), Some(&item.meta.loader.r#type)).await {
                    Ok(list) => list,
                    Err(_) => continue,
                };
                let Some(newest) = versions.first() else { continue };
                let new_id = newest.get("id").and_then(Value::as_str).unwrap_or("");
                if new_id.is_empty() || new_id == version_id {
                    continue;
                }
                updates.push(json!({
                    "projectId": project_id, "file": file,
                    "current": version_id, "newVersion": newest.get("versionNumber").and_then(Value::as_str).unwrap_or(""),
                    "type": newest.get("type").and_then(Value::as_str).unwrap_or(""),
                    "versionId": new_id,
                }));
            }
            Ok(json!({
                "instance": item.meta.id, "totalMods": modrinth::installed_info(&game_dir).len(),
                "updatesAvailable": updates.len(), "updates": updates.iter().take(40).cloned().collect::<Vec<_>>(),
            }))
        }

        "update_mod" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if running_ids(state).contains(&item.meta.id) || state.installs.lock().unwrap().contains_key(&item.meta.id) {
                return Err(AppError::new("BUSY", "Идёт установка или игра запущена: изменение файлов недоступно"));
            }
            if item.meta.loader.r#type == "vanilla" {
                return Err(AppError::new("LOADER_REQUIRED", "Instance is vanilla: it has no mods."));
            }
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let reference = arg_str(args, &["file", "mod"]);
            let installed = modrinth::installed_info(&game_dir);
            let targets: Vec<(String, String)> = if reference.is_empty() {
                let mut out = Vec::new();
                for (project_id, file, version_id) in installed.iter().take(30) {
                    let versions = mod_versions(project_id, Some(&item.meta.mc_version), Some(&item.meta.loader.r#type)).await.unwrap_or_default();
                    if let Some(newest) = versions.first() {
                        let new_id = newest.get("id").and_then(Value::as_str).unwrap_or("");
                        if !new_id.is_empty() && new_id != version_id {
                            out.push((file.clone(), project_id.clone()));
                        }
                    }
                }
                out
            } else {
                let file = resolve_content_file(&game_dir, "mods", &reference)?;
                let base = file.trim_end_matches(".disabled").to_string();
                match installed.iter().find(|(_, installed_file, _)| installed_file == &base) {
                    Some((project_id, _, _)) => vec![(file, project_id.clone())],
                    None => return Err(AppError::new("NOT_ON_MODRINTH", "This file is not known to Modrinth, so it cannot be updated automatically.")),
                }
            };
            let mut updated = Vec::new();
            for (file, project_id) in targets {
                let result: AppResult<Vec<String>> = async {
                    content::remove(&game_dir, "mods", &file, true)?;
                    modrinth::install(&state.data_dir, &settings, app, &item.meta.id, &project_id, "mod", None, None).await
                }.await;
                updated.push(match result {
                    Ok(files) => json!({ "file": file, "ok": true, "newFiles": files }),
                    Err(error) => json!({ "file": file, "ok": false, "error": error.message }),
                });
            }
            Ok(json!({
                "ok": true,
                "updatedCount": updated.iter().filter(|item| item.get("ok").and_then(Value::as_bool).unwrap_or(false)).count(),
                "updated": updated,
            }))
        }

        "apply_jvm_preset" => {
            let preset = arg_str(args, &["preset"]);
            if !jvm_preset_exists(&preset) {
                return Err(AppError::new("AI_ARGS", format!("Unknown preset \"{preset}\". Allowed: none, balanced, performance, lowLatency, compatibility.")));
            }
            let args_text = match preset.as_str() {
                "none" => "",
                "balanced" | "performance" | "lowLatency" | "compatibility" => jvm_preset(&preset),
                _ => return Err(AppError::new("AI_ARGS", format!("Unknown preset \"{preset}\". Allowed: none, balanced, performance, lowLatency, compatibility."))),
            };
            let reference = arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]);
            if !reference.is_empty() {
                let item = find_instance(state, &reference)?;
                let mut instance_settings = serde_json::to_value(&item.meta.settings)?;
                instance_settings["jvmArgs"] = json!(args_text);
                instances::update(&state.data_dir, &state.settings.get()?, &running_ids(state), &json!({
                    "id": item.meta.id, "patch": { "settings": instance_settings },
                }))?;
                return Ok(json!({ "ok": true, "scope": "instance", "id": item.meta.id, "preset": preset }));
            }
            state.settings.set(&json!({ "jvmArgs": args_text }))?;
            Ok(json!({ "ok": true, "scope": "global", "preset": preset }))
        }

        "list_instance_files" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let instance_dir = instances::instance_dir(&state.data_dir, &settings, &item.meta.id)?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let raw = arg_str(args, &["path", "dir", "folder"]);
            let (base, rest): (PathBuf, &str) = if raw == "instance" || raw.starts_with("instance/") {
                (instance_dir.clone(), raw.strip_prefix("instance/").unwrap_or(""))
            } else if raw.is_empty() || raw == ".minecraft" {
                (game_dir.clone(), "")
            } else if let Some(rest) = raw.strip_prefix(".minecraft/") {
                (game_dir.clone(), rest)
            } else {
                (game_dir.clone(), raw.as_str())
            };
            let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty() && *part != ".").collect();
            let target = paths::resolve_inside(&base, &parts)?;
            if !target.is_dir() {
                return Err(AppError::new("FILE_MISSING", format!("Папки «{raw}» нет в инстансе.")));
            }
            let max_depth = arg_int(args, &["maxDepth", "depth"], 2, 1, 5) as usize;
            let limit = arg_int(args, &["limit"], 120, 1, 400) as usize;
            let pattern = arg_str(args, &["pattern"]).to_lowercase();
            let display = |path: &Path| -> String {
                let rel = path.strip_prefix(&instance_dir).unwrap_or(path).to_string_lossy().replace('\\', "/");
                if path.starts_with(&game_dir) && !rel.starts_with(".minecraft/") {
                    format!(".minecraft/{rel}")
                } else {
                    rel
                }
            };
            let target_display = display(&target);
            let mut stack = vec![(target, 0usize)];
            let mut found: Vec<Value> = Vec::new();
            while let Some((dir, depth)) = stack.pop() {
                let mut entries: Vec<_> = fs::read_dir(&dir)?.flatten().collect();
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    if found.len() >= limit { break; }
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !pattern.is_empty() && !name.to_lowercase().contains(&pattern) { continue; }
                    let is_dir = path.is_dir();
                    found.push(json!({
                        "path": display(&path),
                        "type": if is_dir { "dir" } else { "file" },
                        "size": fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                    }));
                    if is_dir && depth < max_depth { stack.push((path, depth + 1)); }
                }
            }
            Ok(json!({
                "ok": true, "instance": item.meta.id, "path": target_display,
                "count": found.len(), "entries": found,
                "note": "Relative paths in read/write/delete tools use the same form (prefix instance/ for instance.json).",
            }))
        }

        "read_instance_file" => {
            let (_, target, display) = instance_file_target(state, args)?;
            if target.is_dir() {
                return Err(AppError::new("AI_ARGS", "Это папка. Используй list_instance_files."));
            }
            let bytes = fs::read(&target)?;
            let text = String::from_utf8(bytes)
                .map_err(|_| AppError::new("BINARY_FILE", "Файл не является текстовым UTF-8."))?;
            let max = arg_int(args, &["maxChars", "maxChars"], 40_000, 500, 200_000) as usize;
            let offset = arg_int(args, &["offsetChars"], 0, 0, 5_000_000) as usize;
            let total = text.chars().count();
            let (part, end, next) = slice_chars(&text, offset, max);
            Ok(json!({
                "ok": true, "path": display,
                "content": part, "chars": end, "totalChars": total,
                "nextOffset": next, "truncated": next.is_some(),
            }))
        }

        "write_instance_file" => {
            no_running_game(state)?;
            let (_, target, display) = instance_file_target(state, args)?;
            if target.is_dir() {
                return Err(AppError::new("AI_ARGS", "Нельзя перезаписать папку как файл."));
            }
            let content = args.get("content").and_then(Value::as_str)
                .ok_or_else(|| AppError::new("AI_ARGS", "Missing required argument \"content\"."))?;
            let append = arg_bool(args, &["append"], false);
            let content = if append && target.is_file() {
                format!("{}{}", fs::read_to_string(&target)?, content)
            } else {
                content.to_string()
            };
            if let Some(parent) = target.parent() { fs::create_dir_all(parent)?; }
            let temp = target.with_extension("chald-ai.tmp");
            fs::write(&temp, content.as_bytes())?;
            fs::rename(&temp, &target)?;
            Ok(json!({ "ok": true, "path": display, "bytes": content.len(), "note": "File written; undo_last restores the previous content." }))
        }

        "delete_instance_file" => {
            no_running_game(state)?;
            let (base, target, display) = instance_file_target(state, args)?;
            if !arg_bool(args, &["confirm"], false) {
                return Err(AppError::new("CONFIRM_REQUIRED", "Удаление нужно подтвердить confirm=true."));
            }
            if target == base || target == base.join(".minecraft") {
                return Err(AppError::new("AI_ARGS", "Нельзя удалить корень инстанса этим инструментом."));
            }
            if target.is_dir() { fs::remove_dir_all(&target)?; }
            else if target.exists() { fs::remove_file(&target)?; }
            else { return Err(AppError::new("FILE_MISSING", format!("«{display}» не существует."))); }
            Ok(json!({ "ok": true, "deleted": display, "note": "File deleted; undo_last restores it." }))
        }

        "list_ftb_quests" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            ftbquests::list(&game_dir)
        }

        "create_ftb_quests_chapter" => {
            no_running_game(state)?;
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            let icon = arg_str(args, &["icon"]);
            ftbquests::create_chapter(&game_dir, &arg_str(args, &["title", "name"]), if icon.is_empty() { None } else { Some(&icon) })
        }

        "add_ftb_quest" => {
            no_running_game(state)?;
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            ftbquests::add_quest(&game_dir, args)
        }

        "edit_ftb_quest" => {
            no_running_game(state)?;
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let settings = state.settings.get()?;
            let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
            ftbquests::edit_quest(&game_dir, args)
        }

        "launch_instance" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if state.installs.lock().unwrap().contains_key(&item.meta.id) {
                return Err(AppError::new("BUSY", "Инстанс ещё устанавливается."));
            }
            if item.state != "ready" {
                return Err(AppError::new("INSTALL_REQUIRED", format!("Инстанс «{}» не установлен ({}). Сначала install_instance.", item.meta.name, item.state)));
            }
            let settings = state.settings.get()?;
            let auth = state.auth.launch_auth(&settings).await?;
            state.games.launch(app, &state.data_dir, &settings, &item.meta.id, auth)?;
            let wait = arg_int(args, &["waitSeconds", "wait"], 0, 0, 30) as u64;
            if wait > 0 { tokio::time::sleep(Duration::from_secs(wait)).await; }
            let recent: Vec<String> = state.games.logs(&item.meta.id).into_iter().rev().take(14).collect();
            Ok(json!({
                "ok": true, "id": item.meta.id, "name": item.meta.name,
                "launched": true, "waitSeconds": wait, "recentLog": recent,
                "note": "Call get_logs to check for errors. undo_last stops the game.",
            }))
        }

        "stop_instance" => {
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if !running_ids(state).contains(&item.meta.id) {
                return Ok(json!({ "ok": true, "changed": false, "note": "Игра и так не запущена." }));
            }
            state.games.stop(&item.meta.id);
            Ok(json!({ "ok": true, "id": item.meta.id, "changed": true, "note": "undo_last запустит игру снова." }))
        }

        "rename_instance" => {
            no_running_game(state)?;
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let name = arg_str(args, &["name", "newName", "title"]);
            if name.is_empty() {
                return Err(AppError::new("AI_ARGS", "Missing required argument \"name\"."));
            }
            let settings = state.settings.get()?;
            let view = instances::update(&state.data_dir, &settings, &running_ids(state), &json!({
                "id": item.meta.id, "patch": { "name": name },
            }))?;
            Ok(json!({ "ok": true, "id": view.meta.id, "name": view.meta.name }))
        }

        "duplicate_instance" => {
            no_running_game(state)?;
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let name = arg_str(args, &["name", "newName", "title"]);
            if name.is_empty() {
                return Err(AppError::new("AI_ARGS", "Missing required argument \"name\"."));
            }
            let settings = state.settings.get()?;
            let view = instances::duplicate(&state.data_dir, &settings, &running_ids(state), &json!({
                "id": item.meta.id, "name": name,
            }))?;
            Ok(json!({ "ok": true, "source": item.meta.id, "id": view.meta.id, "name": view.meta.name }))
        }

        "delete_instance" => {
            no_running_game(state)?;
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            if !arg_bool(args, &["confirm"], false) {
                return Err(AppError::new("CONFIRM_REQUIRED", "Удаление инстанса нужно подтвердить confirm=true."));
            }
            let settings = state.settings.get()?;
            instances::remove(&state.data_dir, &settings, &json!({ "id": item.meta.id, "confirm": true }))?;
            Ok(json!({ "ok": true, "deleted": item.meta.id, "name": item.meta.name, "note": "undo_last восстановит инстанс." }))
        }

        "set_instance_settings" => {
            no_running_game(state)?;
            let item = find_instance(state, &arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]))?;
            let mut updated = serde_json::to_value(&item.meta.settings)?;
            for (key, aliases) in [
                ("memoryMinMb", vec!["memoryMinMb", "minMemoryMb", "minRam"]),
                ("memoryMaxMb", vec!["memoryMaxMb", "maxMemoryMb", "maxRam"]),
                ("width", vec!["width"]),
                ("height", vec!["height"]),
            ] {
                if args.get(key).is_some() || aliases.iter().skip(1).any(|alias| args.get(*alias).is_some()) {
                    let value = arg_int(args, &aliases, 0, 0, 65536);
                    updated[key] = json!(value as u32);
                }
            }
            if let Some(value) = args.get("jvmArgs").and_then(Value::as_str) { updated["jvmArgs"] = json!(value); }
            if let Some(value) = args.get("fullscreen").and_then(Value::as_bool) { updated["fullscreen"] = json!(value); }
            if let Some(value) = args.get("javaPath").and_then(Value::as_str) { updated["javaPath"] = json!(value); }
            let settings = state.settings.get()?;
            let view = instances::update(&state.data_dir, &settings, &running_ids(state), &json!({
                "id": item.meta.id, "patch": { "settings": updated },
            }))?;
            Ok(json!({ "ok": true, "id": view.meta.id, "settings": view.meta.settings }))
        }

        "undo_last" => undo_last(app, state).await,

        "list_undo" => Ok(undo_log(state)),

        _ => Ok(json!({
            "error": format!("Unknown tool: {name}"),
            "hint": format!("Available tools: {}.", [
                "ask_user", "get_system_info", "list_instances", "get_instance", "get_instance_status",
                "list_minecraft_versions", "get_loader_versions", "create_instance", "install_instance", "get_logs",
                "list_mods", "get_installed_mod_info", "search_modrinth", "get_mod_info", "get_mod_versions",
                "check_mod_compatibility", "install_mod", "install_modpack", "remove_mod", "set_mod_enabled",
                "check_mod_updates", "update_mod", "apply_jvm_preset",
                "list_instance_files", "read_instance_file", "write_instance_file", "delete_instance_file",
                "list_ftb_quests", "create_ftb_quests_chapter", "add_ftb_quest", "edit_ftb_quest",
                "launch_instance", "stop_instance", "rename_instance", "duplicate_instance",
                "delete_instance", "set_instance_settings", "undo_last", "list_undo",
            ].join(", ")),
        })),
    }
}

fn jvm_preset(id: &str) -> &'static str {
    match id {
        "balanced" => "-XX:+UseG1GC -XX:+ParallelRefProcEnabled -XX:MaxGCPauseMillis=200 -XX:+UnlockExperimentalVMOptions -XX:+DisableExplicitGC -XX:+AlwaysPreTouch -XX:G1NewSizePercent=30 -XX:G1MaxNewSizePercent=40 -XX:G1HeapRegionSize=8M -XX:G1ReservePercent=20 -XX:G1HeapWastePercent=5 -XX:G1MixedGCCountTarget=4 -XX:InitiatingHeapOccupancyPercent=15 -XX:G1MixedGCLiveThresholdPercent=90 -XX:G1RSetUpdatingPauseTimePercent=5 -XX:SurvivorRatio=32 -XX:+PerfDisableSharedMem -XX:MaxTenuringThreshold=1",
        "performance" => "-XX:+UseG1GC -XX:+UnlockExperimentalVMOptions -XX:+ParallelRefProcEnabled -XX:+DisableExplicitGC -XX:+AlwaysPreTouch -XX:MaxGCPauseMillis=100 -XX:InitiatingHeapOccupancyPercent=35 -XX:+OmitStackTraceInFastThrow",
        "lowLatency" => "-XX:+UseZGC -XX:+UnlockExperimentalVMOptions -XX:+ZUncommit -XX:ZUncommitDelay=300 -XX:MaxGCPauseMillis=50",
        "compatibility" => "-XX:+UseSerialGC -XX:+DisableExplicitGC",
        _ => "",
    }
}

fn jvm_preset_exists(id: &str) -> bool {
    ["none", "balanced", "performance", "lowLatency", "compatibility"].contains(&id)
}

// ---------------------------------------------------------------- agent loop

fn normalize_client_message(message: &Value) -> Option<Value> {
    let role = message.get("role")?.as_str()?;
    if !["user", "assistant", "tool"].contains(&role) {
        return None;
    }
    let content = message.get("content").and_then(Value::as_str).unwrap_or("");
    let mut out = json!({ "role": role, "content": content });
    let calls = message.get("toolCalls").or_else(|| message.get("tool_calls")).and_then(Value::as_array);
    if let Some(calls) = calls {
        let converted: Vec<Value> = calls.iter().filter_map(|call| {
            let id = call.get("id").and_then(Value::as_str)?;
            let function = call.get("function");
            let name = function.and_then(|f| f.get("name")).and_then(Value::as_str)
                .or_else(|| call.get("name").and_then(Value::as_str))?;
            let arguments = function.and_then(|f| f.get("arguments")).and_then(Value::as_str)
                .or_else(|| call.get("arguments").and_then(Value::as_str)).unwrap_or("{}");
            // Gemini 3 requires the thought_signature returned with a function call to be
            // sent back unchanged on the next turn. Keep every provider-specific field
            // (notably `extra_content`) instead of rebuilding the call from scratch.
            let mut normalized = call.clone();
            normalized["id"] = json!(id);
            normalized["type"] = json!("function");
            let mut normalized_function = function.cloned().unwrap_or_else(|| json!({}));
            normalized_function["name"] = json!(name);
            normalized_function["arguments"] = json!(arguments);
            normalized["function"] = normalized_function;
            Some(normalized)
        }).collect();
        if !converted.is_empty() {
            out["tool_calls"] = json!(converted);
        }
    }
    if let Some(id) = message.get("toolCallId").or_else(|| message.get("tool_call_id")).and_then(Value::as_str) {
        out["tool_call_id"] = json!(id);
    }
    if let Some(name) = message.get("toolName").or_else(|| message.get("name")).and_then(Value::as_str) {
        out["name"] = json!(name);
    }
    Some(out)
}

fn prepare_history(messages: Vec<Value>) -> Vec<Value> {
    let mut messages: Vec<Value> = messages.into_iter().filter_map(|m| normalize_client_message(&m)).collect();
    if messages.len() > MAX_HISTORY_MESSAGES {
        let count = messages.len();
        messages = messages.split_off(count - MAX_HISTORY_MESSAGES);
        if let Some(index) = messages.iter().position(|m| m.get("role").and_then(Value::as_str) == Some("user")) {
            if index > 0 {
                messages = messages.split_off(index);
            }
        }
    }
    let last_user = messages.iter().rposition(|m| m.get("role").and_then(Value::as_str) == Some("user")).unwrap_or(0);
    for (index, message) in messages.iter_mut().enumerate() {
        if index >= last_user || message.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        if let Some(content) = message.get("content").and_then(Value::as_str) {
            if content.chars().count() > OLD_TOOL_RESULT_CHARS {
                let preview: String = content.chars().take(OLD_TOOL_RESULT_CHARS).collect();
                message["content"] = json!(format!("{preview}… [обрезано]"));
            }
        }
    }
    messages
}

async fn context_message(state: &AppState) -> String {
    let mut lines = vec![format!(
        "Сегодня: {}. Лаунчер {}.",
        chrono::Utc::now().format("%Y-%m-%d"),
        env!("CARGO_PKG_VERSION")
    )];
    if let Ok(Ok(list)) = tokio::time::timeout(Duration::from_secs(3), versions::minecraft(&state.data_dir, false)).await {
        if let Some(latest) = list.iter().find(|v| v.kind == "release") {
            lines.push(format!("Последний релиз Minecraft: {}.", latest.id));
        }
    }
    match state.settings.get().and_then(|settings| {
        Ok(instances::list(&state.data_dir, &settings, &running_ids(state))?)
    }) {
        Ok(list) if list.is_empty() => lines.push("Инстансов пока нет.".to_string()),
        Ok(list) => {
            lines.push("Инстансы:".to_string());
            for item in list.iter().take(10) {
                let loader = if item.meta.loader.r#type == "vanilla" {
                    "vanilla".to_string()
                } else {
                    format!("{} {}", item.meta.loader.r#type, item.meta.loader.version.clone().unwrap_or_else(|| "?".to_string()))
                };
                lines.push(format!(
                    "- id={} «{}» — Minecraft {}, {}, {}, модов: {}{}",
                    item.meta.id, item.meta.name, item.meta.mc_version, loader, item.state, item.mod_count,
                    if item.running { ", игра запущена" } else { "" },
                ));
            }
            if list.len() > 10 {
                lines.push(format!("…и ещё {} (list_instances).", list.len() - 10));
            }
        }
        Err(_) => {}
    }
    format!("Контекст (на момент сообщения):\n{}", lines.join("\n"))
}

fn progress(app: &AppHandle, event: Value) {
    game::emit(app, "ai:progress", event);
}

fn tool_label(name: &str) -> String {
    let labels = [
        ("ask_user", "Задаю вопрос"),
        ("get_system_info", "Смотрю систему"),
        ("list_instances", "Смотрю инстансы"),
        ("get_instance", "Читаю данные инстанса"),
        ("get_instance_status", "Проверяю установку"),
        ("list_minecraft_versions", "Ищу версии Minecraft"),
        ("get_loader_versions", "Ищу версии загрузчика"),
        ("create_instance", "Создаю инстанс"),
        ("install_instance", "Устанавливаю игру"),
        ("get_logs", "Читаю логи"),
        ("list_mods", "Смотрю моды"),
        ("get_installed_mod_info", "Читаю описание установленного мода"),
        ("search_modrinth", "Ищу на Modrinth"),
        ("get_mod_info", "Читаю описание мода"),
        ("get_mod_versions", "Смотрю версии мода"),
        ("check_mod_compatibility", "Проверяю совместимость мода"),
        ("install_mod", "Ставлю мод"),
        ("install_modpack", "Ставлю сборку"),
        ("search_curseforge", "Ищу на CurseForge"),
        ("get_curseforge_mod_info", "Читаю описание CurseForge"),
        ("get_curseforge_files", "Смотрю файлы CurseForge"),
        ("check_curseforge_compatibility", "Проверяю файлы CurseForge"),
        ("install_curseforge_mod", "Ставлю мод с CurseForge и зависимости"),
        ("validate_dependencies", "Проверяю зависимости модов"),
        ("remove_mod", "Удаляю мод"),
        ("set_mod_enabled", "Включаю/отключаю мод"),
        ("check_mod_updates", "Ищу обновления модов"),
        ("update_mod", "Обновляю мод"),
        ("apply_jvm_preset", "Ставлю пресет JVM"),
        ("list_instance_files", "Смотрю файлы инстанса"),
        ("read_instance_file", "Читаю файл инстанса"),
        ("write_instance_file", "Пишу файл инстанса"),
        ("delete_instance_file", "Удаляю файл инстанса"),
        ("list_ftb_quests", "Смотрю квесты FTB"),
        ("create_ftb_quests_chapter", "Создаю главу квестов"),
        ("add_ftb_quest", "Добавляю квест"),
        ("edit_ftb_quest", "Редактирую квест"),
        ("launch_instance", "Запускаю игру"),
        ("stop_instance", "Останавливаю игру"),
        ("rename_instance", "Переименовываю инстанс"),
        ("duplicate_instance", "Дублирую инстанс"),
        ("delete_instance", "Удаляю инстанс"),
        ("set_instance_settings", "Меняю настройки инстанса"),
        ("undo_last", "Откатываю действие"),
        ("list_undo", "Смотрю историю изменений"),
    ];
    labels.iter().find(|(name_, _)| *name_ == name).map(|(_, label)| label.to_string()).unwrap_or_else(|| name.to_string())
}

fn parse_poll(args_text: &str) -> Result<Value, String> {
    let args: Value = serde_json::from_str(args_text).unwrap_or(Value::Null);
    let question = args.get("question").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let mut options: Vec<String> = Vec::new();
    if let Some(list) = args.get("options").and_then(Value::as_array) {
        for value in list {
            if let Some(text) = value.as_str() {
                let text = text.trim();
                if !text.is_empty() && !options.iter().any(|item| item == text) {
                    options.push(text.to_string());
                }
            }
        }
    }
    options.truncate(8);
    if question.is_empty() {
        return Err("Argument \"question\" is required.".to_string());
    }
    if options.len() < 2 {
        return Err("Argument \"options\" must contain at least 2 different non-empty strings.".to_string());
    }
    let multiple = matches!(args.get("multiple"), Some(Value::Bool(true))) || args.get("multiple").and_then(Value::as_str) == Some("true");
    Ok(json!({
        "id": format!("{:016x}", rand::random::<u64>()),
        "question": question,
        "options": options,
        "multiple": multiple,
    }))
}

const MUTATING: &[&str] = &[
    "create_instance", "install_instance", "install_mod", "install_modpack", "install_curseforge_mod", "validate_dependencies",
    "remove_mod", "set_mod_enabled", "update_mod", "apply_jvm_preset",
    "write_instance_file", "delete_instance_file", "create_ftb_quests_chapter", "add_ftb_quest", "edit_ftb_quest",
    "launch_instance", "stop_instance", "rename_instance", "duplicate_instance", "delete_instance", "set_instance_settings", "undo_last",
];

/// The old generic server prompt made the model introduce itself after doing tools.
fn is_generic_intro(text: &str) -> bool {
    let low = text.to_lowercase();
    let introduces = low.contains("я чалди") || low.contains("я — чалди") || low.contains("я - чалди");
    let filler = low.contains("спрашивай") || low.contains("чем помочь") || low.contains("помощник сообщества");
    introduces && filler && text.chars().count() < 220
}

fn has_claim(text: &str) -> bool {
    let words = [
        "установил", "установила", "установлено", "установлены", "поставил", "поставлено", "создал", "создана", "скачал", "скачано",
        "удалил", "обновил", "отключил", "включил", "добавил", "применил", "готово", "готов", "всё ок", "все ок", "всё работает",
        "installed", "downloaded", "done", "all ok", "ready", "success",
    ];
    let low = text.to_lowercase();
    words.iter().any(|word| low.contains(word))
}

/// Human-readable blockers that must be shown even if the model ignores the correction.
fn blocker_lines(
    tool_failures: &[Value],
    audit_issues: &[Value],
    install_issues: &[Value],
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for issue in audit_issues {
        let dep = arg_str(issue, &["modId", "id", "projectId"]);
        let by = arg_str(issue, &["requiredBy", "requiredByFile"]);
        let text = if dep.is_empty() {
            "проверка зависимостей не прошла".to_string()
        } else if by.is_empty() {
            format!("отсутствует обязательная зависимость «{dep}»")
        } else {
            format!("«{dep}» нужен для «{by}» и не установлен")
        };
        if seen.insert(text.clone()) {
            lines.push(format!("⚠️ {text}."));
        }
    }
    for issue in tool_failures {
        let tool = arg_str(issue, &["tool"]);
        let detail = arg_str(issue, &["detail", "error"]);
        let text = if tool.is_empty() {
            detail.clone()
        } else {
            format!("инструмент {tool}: {detail}")
        };
        if !text.is_empty() && seen.insert(text.clone()) {
            lines.push(format!("⚠️ Ошибка: {text}."));
        }
    }
    for issue in install_issues {
        let name = arg_str(issue, &["name"]);
        let text = format!("«{name}» ещё устанавливается или не готов");
        if seen.insert(text.clone()) {
            lines.push(format!("⏳ {text}."));
        }
    }
    lines
}

async fn install_issues(state: &AppState, ids: &HashSet<String>) -> Vec<Value> {
    let settings = match state.settings.get() { Ok(value) => value, Err(_) => return Vec::new() };
    let list = instances::list(&state.data_dir, &settings, &running_ids(state)).unwrap_or_default();
    let mut out = Vec::new();
    for id in ids {
        let Some(item) = list.iter().find(|instance| &instance.meta.id == id) else { continue };
        if state.installs.lock().unwrap().contains_key(id) {
            out.push(json!({ "id": id, "name": item.meta.name, "kind": "installing", "detail": item.state_reason.clone().unwrap_or_else(|| "идёт загрузка".to_string()) }));
        } else if item.state != "ready" {
            out.push(json!({
                "id": id, "name": item.meta.name, "kind": "missing",
                "detail": item.state_reason.clone().unwrap_or_else(|| "игра не установлена".to_string()),
            }));
        }
    }
    out
}

/// Paths a tool may change, used to build an undo snapshot before it runs.
fn undo_roots(state: &AppState, name: &str, args: &Value) -> AppResult<Vec<PathBuf>> {
    let settings = state.settings.get()?;
    let reference = arg_str(args, &["instance", "instanceId", "instance_id", "id", "name"]);
    if name == "create_instance" || name == "install_modpack" || name == "duplicate_instance" {
        return Ok(vec![paths::instances_root(&state.data_dir, &settings.instances_dir)]);
    }
    if name == "apply_jvm_preset" && reference.is_empty() {
        return Ok(vec![state.data_dir.join("settings.json")]);
    }
    if name == "launch_instance" || name == "stop_instance" {
        return Ok(Vec::new());
    }
    let item = find_instance(state, &reference)?;
    let instance_dir = instances::instance_dir(&state.data_dir, &settings, &item.meta.id)?;
    let game_dir = instances::game_dir_of(&state.data_dir, &settings, &item.meta.id)?;
    let roots = match name {
        "write_instance_file" | "delete_instance_file" => {
            vec![instance_file_target(state, args)?.1]
        }
        "create_ftb_quests_chapter" | "add_ftb_quest" | "edit_ftb_quest" => {
            vec![game_dir.join("config").join("ftbquests").join("quests")]
        }
        "rename_instance" | "set_instance_settings" | "delete_instance" => vec![instance_dir],
        _ => vec![game_dir],
    };
    Ok(roots)
}

/// Runs one user turn. Quota is charged by the server once, on the first call of the turn.
pub async fn chat(app: &AppHandle, state: &AppState, arg: &Value) -> AppResult<Value> {
    let client_messages = arg.get("messages").and_then(Value::as_array)
        .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректное поле messages"))?;
    let history = prepare_history(client_messages.clone());
    if history.is_empty() {
        return Err(AppError::new("INVALID_ARGS", "messages пустой"));
    }
    let mut wire: Vec<Value> = vec![json!({ "role": "user", "content": context_message(state).await })];
    wire.extend(history);

    let tools = tool_defs();
    let mut turn_id: Option<String> = None;
    let mut steps: Vec<Value> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut started_installs: HashSet<String> = HashSet::new();
    let mut did_mutate = false;
    let mut nudged_claim = false;
    let mut nudged_intro = false;
    let mut nudged_audit = false;
    let mut forced_stop = false;
    let mut tool_failures: Vec<Value> = Vec::new();
    let mut audit_issues: Vec<Value> = Vec::new();
    let mut poll: Option<Value> = None;
    let mut last_limit: Value = json!({ "limit": null, "remaining": null, "resetInSec": null });
    let mut last_reply = String::new();

    for round in 0..MAX_ROUNDS {
        progress(app, json!({ "kind": "thinking", "label": if round == 0 { "Чалди думает…" } else { "Чалди проверяет…" } }));
        let response = state.friends.ai_chat(&json!(wire), Some(&tools), turn_id.as_deref()).await?;
        if let Some(id) = response.get("turnId").and_then(Value::as_str) {
            if !id.is_empty() {
                turn_id = Some(id.to_string());
            }
        }
        for key in ["limit", "remaining", "resetInSec"] {
            if let Some(value) = response.get(key) {
                last_limit[key] = value.clone();
            }
        }
        let message = response.get("message").cloned().unwrap_or_else(|| {
            json!({ "role": "assistant", "content": response.get("reply").and_then(Value::as_str).unwrap_or("") })
        });
        let content = message.get("content").and_then(Value::as_str).unwrap_or("").to_string();
        let calls: Vec<Value> = message.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        let with_tools = round + 1 < MAX_ROUNDS && poll.is_none();

        let install_blockers = install_issues(state, &started_installs).await;

        if calls.is_empty() {
            if !content.trim().is_empty() && !nudged_claim && with_tools && has_claim(&content) {
                let correction = if !install_blockers.is_empty() {
                    let details = install_blockers.iter().map(|issue| {
                        let name = issue.get("name").and_then(Value::as_str).unwrap_or("");
                        format!("«{name}» ещё устанавливается или не готов")
                    }).collect::<Vec<_>>().join("; ");
                    format!("Системная проверка: {details}. Исправь ответ: не утверждай, что всё установлено.")
                } else if !did_mutate && started_installs.is_empty() {
                    "Системная проверка: в этом запросе ты не вызывал ни одного инструмента, который что-то меняет, и ничего не подтверждено. Не утверждай, что действие выполнено.".to_string()
                } else {
                    String::new()
                };
                if !correction.is_empty() {
                    nudged_claim = true;
                    wire.push(message.clone());
                    wire.push(json!({ "role": "user", "content": correction }));
                    continue;
                }
            }
            let blockers = blocker_lines(&tool_failures, &audit_issues, &install_blockers);
            if !content.trim().is_empty() && !nudged_audit && with_tools && !blockers.is_empty() {
                nudged_audit = true;
                wire.push(message.clone());
                wire.push(json!({
                    "role": "user",
                    "content": format!(
                        "Системная проверка на основе результатов инструментов:\n{}\nИсправь ответ: не говори «готово», «установлено» или «всё ок», пока эти проблемы не устранены инструментами. Если устранить не смог — честно перечисли, что осталось сделать пользователю.",
                        blockers.join("\n")
                    ),
                }));
                continue;
            }
            if !content.trim().is_empty()
                && !nudged_intro
                && with_tools
                && !steps.is_empty()
                && is_generic_intro(&content)
            {
                nudged_intro = true;
                wire.push(message.clone());
                wire.push(json!({
                    "role": "user",
                    "content": "Системная проверка: ты уже вызвал инструменты, но ответил только представлением. Используй их результаты, выполни просьбу пользователя и не пиши вступление «Я Чалди, спрашивай».",
                }));
                continue;
            }
            let mut reply = content.trim().to_string();
            let mut step_content = content.clone();
            // Deterministic safety net. If the model still claims success while tools report
            // failures, discard its wording completely: a "готово" next to missing mods is a bug.
            if !blockers.is_empty() && has_claim(&content) {
                reply = format!(
                    "❌ **Сборка НЕ готова.** Инструменты зафиксировали проблемы:\n{}\n\nУстрани их и повтори проверку перед запуском.",
                    blockers.join("\n")
                );
                step_content = reply.clone();
            } else {
                for line in blockers {
                    reply.push_str("\n\n");
                    reply.push_str(&line);
                }
            }
            last_reply = reply.clone();
            progress(app, json!({ "kind": "text", "label": "Чалди", "detail": reply.clone() }));
            steps.push(json!({
                "content": step_content, "toolCalls": [], "toolResults": [], "finalContent": reply,
            }));
            break;
        }

        wire.push(message.clone());
        if !content.trim().is_empty() {
            progress(app, json!({ "kind": "text", "label": "Чалди", "detail": content.clone() }));
        }
        let mut tool_results: Vec<Value> = Vec::new();
        for call in &calls {
            let id = call.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let function = call.get("function");
            let name = function.and_then(|f| f.get("name")).and_then(Value::as_str)
                .or_else(|| call.get("name").and_then(Value::as_str)).unwrap_or("").to_string();
            let args_text = function.and_then(|f| f.get("arguments")).and_then(Value::as_str)
                .or_else(|| call.get("arguments").and_then(Value::as_str)).unwrap_or("{}").to_string();
            let label = tool_label(&name);
            progress(app, json!({
                "kind": "tool_start", "label": label.clone(), "toolName": name.clone(),
                "toolCallId": id.clone(), "detail": args_text.clone(),
            }));
            let result: Value = if name == "ask_user" {
                match parse_poll(&args_text) {
                    Ok(value) if poll.is_none() => {
                        poll = Some(value);
                        json!({ "status": "waiting_for_user", "note": "Опрос показан пользователю. Закончи ход; ответ придёт следующим сообщением." })
                    }
                    Ok(_) => json!({ "error": "Only one poll per turn." }),
                    Err(error) => json!({ "error": error }),
                }
            } else {
                let signature = format!("{name}:{args_text}");
                let count = seen.entry(signature).or_insert(0);
                *count += 1;
                if *count > 1 && name != "get_instance_status" {
                    json!({ "error": "Этот вызов с теми же аргументами уже выполнялся в этом запросе.", "hint": "Результат уже есть выше. Используй его или измени аргументы; не повторяй запрос." })
                } else {
                    let parsed: Value = serde_json::from_str(&args_text).unwrap_or(Value::Null);
                    let mut pending_undo: Option<undo::UndoEntry> = None;
                    let mut undo_error: Option<String> = None;
                    if MUTATING.contains(&name.as_str()) && name != "undo_last" {
                        let reference = arg_str(&parsed, &["instance", "instanceId", "instance_id", "id", "name"]);
                        if name == "launch_instance" {
                            pending_undo = Some(undo::special(
                                &format!("Запуск игры: {reference}"),
                                UndoKind::StopGame { instance: reference },
                            ));
                        } else if name == "stop_instance" {
                            pending_undo = Some(undo::special(
                                &format!("Остановка игры: {reference}"),
                                UndoKind::StartGame { instance: reference },
                            ));
                        } else {
                            match undo_roots(state, &name, &parsed) {
                                Ok(roots) => match undo::snapshot(&state.data_dir, &label, &roots) {
                                    Ok(entry) => pending_undo = entry,
                                    Err(error) => undo_error = Some(error.message),
                                },
                                Err(error) => undo_error = Some(error.message),
                            }
                        }
                    }
                    match run_tool(app, state, &name, &parsed).await {
                        Ok(mut value) => {
                            if MUTATING.contains(&name.as_str()) {
                                did_mutate = true;
                            }
                            if (name == "create_instance" || name == "install_instance")
                                && value.get("installStarted").and_then(Value::as_bool).unwrap_or(false)
                            {
                                if let Some(instance_id) = value.get("id").and_then(Value::as_str) {
                                    started_installs.insert(instance_id.to_string());
                                }
                            }
                            if name == "stop_instance" && !value.get("changed").and_then(Value::as_bool).unwrap_or(true) {
                                pending_undo = None;
                            }
                            if let Some(entry) = pending_undo {
                                let id = entry.id.clone();
                                match undo::commit(&state.data_dir, entry) {
                                    Ok(()) => { value["undoId"] = json!(id); }
                                    Err(error) => { value["undoNote"] = json!(format!("Откат недоступен: {}", error.message)); }
                                }
                            }
                            if let Some(error) = undo_error {
                                value["undoNote"] = json!(format!("Снимок для отката не создан: {error}"));
                            }
                            value
                        }
                        Err(error) => {
                            if let Some(entry) = pending_undo {
                                undo::discard(&state.data_dir, &entry);
                            }
                            let hints = [
                                ("AI_ARGS", "Fix the arguments according to the tool schema and call again."),
                                ("INSTANCE_NOT_FOUND", "Call list_instances and use an exact id or name."),
                                ("INSTANCE_AMBIGUOUS", "Use the exact id from list_instances."),
                                ("VERSION_UNKNOWN", "Call list_minecraft_versions to see real versions."),
                                ("LOADER_INCOMPATIBLE", "Call get_loader_versions(mcVersion) to see which loaders and versions exist."),
                                ("NO_COMPATIBLE_VERSION", "This project has no build for the instance version/loader. Call get_mod_versions without filters."),
                                ("LOADER_REQUIRED", "The instance is vanilla and cannot load mods. Create a new instance with a loader."),
                                ("ALREADY_INSTALLED", "Already installed; nothing to do."),
                                ("BUSY", "The game is running or an installation is in progress. Use get_instance_status."),
                                ("INSTALL_REQUIRED", "Call install_instance, then get_instance_status(waitSeconds) before launching."),
                                ("CONFIRM_REQUIRED", "Ask the user to confirm, then call again with confirm=true."),
                                ("FILE_MISSING", "Call list_instance_files to see real paths."),
                                ("CHAPTER_NOT_FOUND", "Call list_ftb_quests and use an exact chapter id."),
                                ("QUEST_NOT_FOUND", "Call list_ftb_quests and use an exact questId."),
                            ];
                            let hint = hints.iter().find(|(code, _)| *code == error.code).map(|(_, hint)| *hint).unwrap_or("");
                            json!({ "error": error.message, "code": error.code, "hint": hint })
                        }
                    }
                }
            };
            // Remember real failures so a final "всё ок" can be challenged with concrete evidence.
            let mut failure_detail = result.get("error").and_then(Value::as_str).map(str::to_string);
            if failure_detail.is_none() && result.get("ok").and_then(Value::as_bool) == Some(false) {
                failure_detail = Some(arg_str(&result, &["note", "message", "detail"]));
            }
            if let Some(detail) = failure_detail.filter(|text| !text.is_empty()) {
                tool_failures.push(json!({ "tool": name.clone(), "detail": detail }));
            }
            if ["validate_modpack", "validate_dependencies", "build_modpack"].contains(&name.as_str()) {
                if let Some(list) = result.get("missingDependencies").and_then(Value::as_array) {
                    for entry in list {
                        audit_issues.push(entry.clone());
                    }
                }
            }
            if ["install_curseforge_mod", "validate_dependencies"].contains(&name.as_str()) {
                if let Some(list) = result.get("failed").and_then(Value::as_array) {
                    for entry in list {
                        audit_issues.push(json!({
                            "modId": arg_str(entry, &["modId", "projectId", "id"]),
                            "requiredBy": arg_str(entry, &["requiredBy"]),
                            "detail": arg_str(entry, &["error", "message"]),
                        }));
                    }
                }
            }
            let same_failures = tool_failures
                .iter()
                .filter(|item| item.get("tool").and_then(Value::as_str) == Some(name.as_str()))
                .count();
            if same_failures >= 3 {
                wire.push(json!({
                    "role": "user",
                    "content": format!("Системная проверка: инструмент {name} три раза сообщил об ошибке. Прекрати повторять его и честно скажи пользователю, что не получилось и почему."),
                }));
                forced_stop = true;
            }
            let result_text = pack(&result, MAX_RESULT_CHARS);
            progress(app, json!({
                "kind": "tool_result", "label": label.clone(), "toolName": name.clone(),
                "toolCallId": id.clone(), "detail": result_text.clone(),
            }));
            tool_results.push(json!({ "toolCallId": id.clone(), "name": name.clone(), "result": result_text.clone() }));
            wire.push(json!({
                "role": "tool", "content": result_text, "tool_call_id": id, "name": name,
            }));
        }
        steps.push(json!({ "content": content, "toolCalls": calls, "toolResults": tool_results }));
        if poll.is_some() || forced_stop {
            break;
        }
    }

    if last_reply.is_empty() && poll.is_none() {
        let mut fallback = Vec::new();
        for step in &steps {
            if let Some(results) = step.get("toolResults").and_then(Value::as_array) {
                for result in results {
                    let name = result.get("name").and_then(Value::as_str).unwrap_or("");
                    let text = result.get("result").and_then(Value::as_str).unwrap_or("");
                    let summary: String = serde_json::from_str::<Value>(text)
                        .ok()
                        .and_then(|value| {
                            value.get("error").and_then(Value::as_str).map(str::to_string)
                                .or_else(|| value.get("note").and_then(Value::as_str).map(str::to_string))
                        })
                        .unwrap_or_else(|| "результат получен".to_string());
                    fallback.push(format!("{}: {}", tool_label(name), summary));
                }
            }
        }
        let mut reply = fallback.join("\n");
        let install_blockers = install_issues(state, &started_installs).await;
        let blockers = blocker_lines(&tool_failures, &audit_issues, &install_blockers);
        if !blockers.is_empty() {
            reply = format!(
                "❌ **Не получилось выполнить запрос до конца.** Инструменты сообщили:\n{}\n\nПовтори после устранения ошибок.",
                blockers.join("\n")
            );
        }
        last_reply = reply;
    }

    Ok(json!({
        "steps": steps,
        "reply": last_reply,
        "poll": poll,
        "turnId": turn_id,
        "limit": last_limit.get("limit").cloned().unwrap_or(Value::Null),
        "remaining": last_limit.get("remaining").cloned().unwrap_or(Value::Null),
        "resetInSec": last_limit.get("resetInSec").cloned().unwrap_or(Value::Null),
    }))
}

pub async fn status(state: &AppState) -> AppResult<Value> {
    // The friends token is the only identity allowed to see/spend this account's quota.
    state.friends.auth_token()?;
    state.friends.ai_status().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_are_openai_compatible() {
        let defs = tool_defs();
        let list = defs.as_array().expect("array");
        assert!(list.len() >= 20);
        assert!(list.iter().any(|def| {
            def.get("function").and_then(Value::as_object)
                .and_then(|f| f.get("name")).and_then(Value::as_str) == Some("build_modpack")
        }));
        for name in [
            "list_instance_files", "read_instance_file", "write_instance_file", "delete_instance_file",
            "list_ftb_quests", "create_ftb_quests_chapter", "add_ftb_quest", "edit_ftb_quest",
            "launch_instance", "stop_instance", "rename_instance", "duplicate_instance",
            "delete_instance", "set_instance_settings", "undo_last", "list_undo",
            "search_curseforge", "get_curseforge_mod_info", "get_curseforge_files",
            "check_curseforge_compatibility", "install_curseforge_mod", "validate_dependencies",
        ] {
            assert!(list.iter().any(|def| {
                def.get("function").and_then(Value::as_object)
                    .and_then(|f| f.get("name")).and_then(Value::as_str) == Some(name)
            }), "missing tool {name}");
            assert!(!tool_label(name).is_empty());
        }
        for def in list {
            let function = def.get("function").expect("function");
            assert!(function.get("name").and_then(Value::as_str).unwrap_or("").len() > 2);
            assert_eq!(def.get("type").and_then(Value::as_str), Some("function"));
            assert_eq!(function.get("parameters").and_then(|p| p.get("type")).and_then(Value::as_str), Some("object"));
        }
    }

    #[test]
    fn parses_poll_and_rejects_bad_options() {
        let poll = parse_poll(r#"{"question":"Какой загрузчик?","options":["Fabric","Forge","Fabric"],"multiple":true}"#).expect("poll");
        assert_eq!(poll.get("options").and_then(Value::as_array).unwrap().len(), 2);
        assert_eq!(poll.get("multiple").and_then(Value::as_bool), Some(true));
        assert!(parse_poll(r#"{"question":"?"}"#).is_err());
    }

    #[test]
    fn detects_generic_intro() {
        assert!(is_generic_intro("Я чалди, ии-помощник сообщества Chald. Спрашивай!"));
        assert!(!is_generic_intro("Установил Fabric 1.21.1 и поставил JEI."));
    }

    #[test]
    fn detects_false_success_claims() {
        assert!(has_claim("Установил Waystonesable, всё ок!"));
        assert!(has_claim("Всё готово, модпак ready."));
        assert!(!has_claim("Ставлю мод, подожди."));
    }

    #[test]
    fn blocker_lines_report_missing_dependency() {
        let missing = vec![json!({
            "modId": "waystones", "requiredByFile": "waystonesable-1.21.jar"
        })];
        let lines = blocker_lines(&[], &missing, &[]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("waystones"));
        assert!(lines[0].contains("waystonesable"));
    }

    #[test]
    fn matches_dependency_hits_only_when_confident() {
        let hits = vec![
            json!({ "id": 123, "slug": "something-else", "title": "Random mod" }),
            json!({ "id": 321, "slug": "waystones", "title": "Waystones" }),
        ];
        let hit = confident_hit(&hits, "waystones").expect("hit");
        assert_eq!(hit.get("id").and_then(Value::as_i64), Some(321));
        assert!(confident_hit(&hits, "zzzz-unknown").is_none());
    }

    #[test]
    fn jvm_presets_are_known() {
        for id in ["none", "balanced", "performance", "lowLatency", "compatibility"] {
            assert!(jvm_preset_exists(id));
        }
        assert!(!jvm_preset_exists("ultra"));
    }

    #[test]
    fn normalizes_client_tool_messages() {
        let message = json!({
            "role": "assistant", "content": "",
            "toolCalls": [{ "id": "call_1", "name": "list_instances", "arguments": "{}" }],
        });
        let normalized = normalize_client_message(&message).expect("message");
        let call = &normalized["tool_calls"][0];
        assert_eq!(call["function"]["name"], "list_instances");
        assert_eq!(call["type"], "function");
    }

    #[test]
    fn keeps_gemini_thought_signature_on_tool_calls() {
        let message = json!({
            "role": "assistant", "content": "",
            "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": { "name": "list_instances", "arguments": "{}" },
                "extra_content": { "google": { "thought_signature": "sig-123" } },
            }],
        });
        let normalized = normalize_client_message(&message).expect("message");
        let call = &normalized["tool_calls"][0];
        assert_eq!(call["function"]["name"], "list_instances");
        assert_eq!(call["extra_content"]["google"]["thought_signature"], "sig-123");
    }
}
