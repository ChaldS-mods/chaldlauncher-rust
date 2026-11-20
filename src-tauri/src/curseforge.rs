use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::download::{download_file, DownloadSpec};
use crate::errors::{AppError, AppResult};
use crate::game;
use crate::instances;
use crate::net;
use crate::paths::{resolve_inside, safe_file_name};
use crate::settings::Settings;
use crate::state::AppState;

const API: &str = "https://api.curseforge.com";
const GAME_ID: u32 = 432;
const PAGE_SIZE: u32 = 20;
const API_KEY: &str = "$2a$10$bL4bIL5pUWqfcO7KQtnMReakwtfHbNKh6v1uTpKlzhwoueEJQnPnm";
const CF_INDEX_FILE: &str = ".chald-curseforge.json";

fn class_id(kind: &str) -> u32 {
    match kind {
        "modpack" => 4471,
        "resourcepack" => 12,
        "shader" => 6552,
        "datapack" => 6945,
        _ => 6,
    }
}

fn sort_field(sort: &str) -> u32 {
    match sort {
        "popularity" => 2,
        "lastUpdated" => 3,
        "name" => 4,
        "totalDownloads" => 6,
        _ => 1, // featured
    }
}

fn loader_id(loader: &str) -> Option<u32> {
    match loader {
        "forge" => Some(1),
        "fabric" => Some(4),
        "quilt" => Some(5),
        "neoforge" => Some(6),
        _ => None,
    }
}

fn headers() -> [(&'static str, &'static str); 3] {
    [
        ("x-api-key", API_KEY),
        ("Accept", "application/json"),
        ("User-Agent", "ChaldStudio/ChaldLauncher/1.1.0 (Minecraft launcher)"),
    ]
}

async fn api_get<T: for<'de> Deserialize<'de>>(url: &str, purpose: &str) -> AppResult<T> {
    net::fetch_json_with_headers(url, purpose, &headers()).await
}

#[derive(Debug, Deserialize)]
struct Pagination {
    #[serde(default)]
    index: u32,
    #[serde(default, rename = "totalCount")]
    total_count: u32,
}

#[derive(Debug, Deserialize)]
struct Author {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct Logo {
    #[allow(dead_code)]
    #[serde(default, rename = "thumbnailUrl")]
    thumbnail_url: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Links {
    #[serde(default, rename = "websiteUrl")]
    website_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModItem {
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    summary: String,
    #[serde(default, rename = "downloadCount")]
    download_count: u64,
    #[serde(default)]
    authors: Vec<Author>,
    #[allow(dead_code)]
    #[serde(default)]
    logo: Option<Logo>,
    #[serde(default, rename = "classId")]
    class_id: Option<u32>,
    #[serde(default, rename = "dateModified")]
    date_modified: Option<String>,
    #[serde(default)]
    links: Option<Links>,
    #[serde(default, rename = "latestFilesIndexes")]
    latest_files_indexes: Option<Vec<FileIndex>>,
}

#[derive(Debug, Deserialize, Default)]
struct FileIndex {
    #[serde(default, rename = "gameVersion")]
    game_version: Option<String>,
    #[serde(default, rename = "modLoader")]
    mod_loader: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<ModItem>,
    pagination: Option<Pagination>,
}

#[derive(Debug, Deserialize)]
struct ModResponse {
    data: ModItem,
}

#[derive(Debug, Deserialize)]
struct DescriptionResponse {
    #[serde(default)]
    data: String,
}

#[derive(Debug, Deserialize, Clone)]
struct HashInfo {
    #[serde(default)]
    value: String,
    #[serde(default)]
    algo: i32,
}

/// CurseForge file relation: 3 = RequiredDependency, 4 = OptionalDependency.
#[derive(Debug, Deserialize, Clone)]
struct CfFileDependency {
    #[serde(default, rename = "modId")]
    mod_id: i64,
    #[serde(default, rename = "relationType")]
    relation_type: i32,
    #[serde(default, rename = "isOptional")]
    is_optional: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct FileItem {
    id: i64,
    #[serde(default, rename = "displayName")]
    display_name: String,
    #[serde(default, rename = "fileName")]
    file_name: String,
    #[serde(default, rename = "fileDate")]
    file_date: String,
    #[serde(default, rename = "fileLength")]
    file_length: u64,
    #[serde(default, rename = "downloadCount")]
    download_count: u64,
    #[serde(default, rename = "downloadUrl")]
    download_url: Option<String>,
    #[serde(default, rename = "gameVersions")]
    game_versions: Vec<String>,
    #[serde(default, rename = "releaseType")]
    release_type: i32,
    #[serde(default)]
    hashes: Vec<HashInfo>,
    #[serde(default, rename = "isAvailable")]
    is_available: Option<bool>,
    #[serde(default)]
    dependencies: Vec<CfFileDependency>,
}

#[derive(Debug, Deserialize)]
struct FilesResponse {
    #[serde(default)]
    data: Vec<FileItem>,
}

#[derive(Debug, Deserialize)]
struct FileResponse {
    data: FileItem,
}

fn class_from_id(class_id: Option<u32>, fallback: &str) -> String {
    match class_id {
        Some(12) => "resourcepack",
        Some(4471) => "modpack",
        Some(6552) => "shader",
        Some(6945) => "datapack",
        Some(6) => "mod",
        _ => fallback,
    }
    .to_string()
}

fn release_type_name(value: i32) -> &'static str {
    match value {
        1 => "release",
        2 => "beta",
        _ => "alpha",
    }
}

fn file_to_info(file: &FileItem) -> Value {
    let sha1 = file
        .hashes
        .iter()
        .find(|h| h.algo == 1)
        .map(|h| h.value.to_lowercase());
    json!({
        "id": file.id,
        "fileName": file.file_name,
        "displayName": if file.display_name.is_empty() { file.file_name.clone() } else { file.display_name.clone() },
        "size": file.file_length,
        "downloads": file.download_count,
        "published": file.file_date,
        "releaseType": release_type_name(file.release_type),
        "gameVersions": file.game_versions,
        "downloadUrl": file.download_url,
        "sha1": sha1,
        "dependencies": file.dependencies.iter().map(|dep| json!({
            "modId": dep.mod_id,
            "relationType": dep.relation_type,
            "required": dep.relation_type == 3 && !dep.is_optional,
        })).collect::<Vec<_>>(),
        "requiredDependencies": required_dependency_ids(file),
    })
}

/// CurseForge relationType 3 means RequiredDependency. Optional flags still win.
fn required_dependency_ids(file: &FileItem) -> Vec<i64> {
    let mut out = Vec::new();
    for dep in &file.dependencies {
        if dep.relation_type != 3 || dep.is_optional || dep.mod_id <= 0 {
            continue;
        }
        if !out.contains(&dep.mod_id) {
            out.push(dep.mod_id);
        }
    }
    out
}

pub async fn search(input: &Value) -> AppResult<Value> {
    let query = input.get("query").and_then(Value::as_str).unwrap_or("").trim();
    let class_type = input.get("classType").and_then(Value::as_str).unwrap_or("mod");
    let mc_version = input.get("mcVersion").and_then(Value::as_str).filter(|s| !s.is_empty());
    let loader = input.get("loader").and_then(Value::as_str).filter(|s| !s.is_empty());
    let sort = input.get("sort").and_then(Value::as_str).unwrap_or("featured");
    let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32;

    let mut url = reqwest::Url::parse(&format!("{API}/v1/mods/search")).expect("static url");
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("gameId", &GAME_ID.to_string());
        qp.append_pair("classId", &class_id(class_type).to_string());
        qp.append_pair("pageSize", &PAGE_SIZE.to_string());
        qp.append_pair("index", &offset.to_string());
        qp.append_pair("sortField", &sort_field(sort).to_string());
        qp.append_pair("sortOrder", "desc");
        if !query.is_empty() {
            qp.append_pair("searchFilter", query);
        }
        if let Some(mc) = mc_version {
            qp.append_pair("gameVersion", mc);
        }
        if class_type == "mod" {
            if let Some(loader) = loader {
                if let Some(id) = loader_id(loader) {
                    qp.append_pair("modLoaderType", &id.to_string());
                }
            }
        }
    }
    let response: SearchResponse = api_get(url.as_str(), "CurseForge: поиск").await?;
    let hits: Vec<Value> = response
        .data
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "slug": if m.slug.is_empty() { m.id.to_string() } else { m.slug.clone() },
                "title": m.name,
                "description": m.summary,
                "author": m.authors.first().map(|a| a.name.clone()).unwrap_or_default(),
                "downloads": m.download_count,
                "type": class_from_id(m.class_id, class_type),
                "iconDataUrl": Value::Null,
                "websiteUrl": m.links.as_ref().and_then(|l| l.website_url.clone()),
            })
        })
        .collect();
    let total = response.pagination.as_ref().map(|p| p.total_count).unwrap_or(hits.len() as u32);
    let index = response.pagination.as_ref().map(|p| p.index).unwrap_or(offset);
    Ok(json!({ "hits": hits, "total": total, "offset": index }))
}

pub async fn project(mod_id: i64) -> AppResult<Value> {
    let mod_resp: ModResponse = api_get(&format!("{API}/v1/mods/{mod_id}"), "CurseForge: проект").await?;
    let desc: String = match api_get::<DescriptionResponse>(&format!("{API}/v1/mods/{mod_id}/description"), "CurseForge: описание").await {
        Ok(d) => d.data,
        Err(_) => String::new(),
    };
    let m = mod_resp.data;
    let indexes = m.latest_files_indexes.unwrap_or_default();
    let mut game_versions: Vec<String> = Vec::new();
    let mut loaders: Vec<String> = Vec::new();
    for idx in &indexes {
        if let Some(gv) = &idx.game_version {
            if !game_versions.iter().any(|v| v == gv) {
                game_versions.push(gv.clone());
            }
        }
        let loader_name = match idx.mod_loader.unwrap_or(-1) {
            1 => Some("Forge"),
            4 => Some("Fabric"),
            5 => Some("Quilt"),
            6 => Some("NeoForge"),
            _ => None,
        };
        if let Some(name) = loader_name {
            if !loaders.iter().any(|v| v == name) {
                loaders.push(name.to_string());
            }
        }
    }
    game_versions.truncate(20);
    Ok(json!({
        "id": m.id,
        "slug": if m.slug.is_empty() { m.id.to_string() } else { m.slug },
        "title": m.name,
        "description": m.summary,
        "bodyHtml": desc,
        "author": m.authors.first().map(|a| a.name.clone()).unwrap_or_default(),
        "downloads": m.download_count,
        "type": class_from_id(m.class_id, "mod"),
        "iconDataUrl": Value::Null,
        "websiteUrl": m.links.as_ref().and_then(|l| l.website_url.clone()),
        "updated": m.date_modified,
        "gameVersions": game_versions,
        "loaders": loaders,
    }))
}

pub async fn files(mod_id: i64, mc_version: Option<&str>, loader: Option<&str>) -> AppResult<Value> {
    let mut url = reqwest::Url::parse(&format!("{API}/v1/mods/{mod_id}/files")).expect("static url");
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("pageSize", "50");
        qp.append_pair("index", "0");
        if let Some(mc) = mc_version.filter(|s| !s.is_empty()) {
            qp.append_pair("gameVersion", mc);
        }
        if let Some(loader) = loader.filter(|s| !s.is_empty()) {
            if let Some(id) = loader_id(loader) {
                qp.append_pair("modLoaderType", &id.to_string());
            }
        }
    }
    let response: FilesResponse = api_get(url.as_str(), "CurseForge: файлы").await?;
    let files: Vec<Value> = response
        .data
        .into_iter()
        .filter(|f| f.is_available.unwrap_or(true))
        .map(|f| file_to_info(&f))
        .collect();
    Ok(Value::Array(files))
}

async fn get_file_info(mod_id: i64, file_id: i64) -> AppResult<FileItem> {
    let response: FileResponse = api_get(&format!("{API}/v1/mods/{mod_id}/files/{file_id}"), "CurseForge: файл").await?;
    Ok(response.data)
}

pub(crate) fn resolve_download_url(file: &FileItem) -> String {
    if let Some(url) = &file.download_url {
        if !url.is_empty() {
            return url.clone();
        }
    }
    let a = file.id / 1000;
    let b = file.id % 1000;
    format!(
        "https://edge.forgecdn.net/files/{a}/{b}/{}",
        urlencoding::encode(&file.file_name)
    )
}

pub fn open_page(slug: &str) -> AppResult<()> {
    if slug.is_empty() || slug.len() > 128 || slug.chars().any(|c| c.is_control()) {
        return Err(AppError::new("BAD_ARGUMENT", "Некорректная ссылка на проект"));
    }
    let segment = "mc-mods"; // renderer doesn't pass kind for curseforge:openPage in ipc currently
    open::that(format!("https://www.curseforge.com/minecraft/{segment}/{}", urlencoding::encode(slug)))
        .map_err(|e| AppError::new("OPEN_FAILED", e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------- installed index

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CfInstalledIndex {
    #[serde(default)]
    projects: HashMap<String, CfInstalledEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CfInstalledEntry {
    file: String,
    #[serde(rename = "fileId")]
    file_id: i64,
}

fn cf_index_path(game_dir: &Path) -> PathBuf {
    game_dir.join(CF_INDEX_FILE)
}

fn read_cf_installed(game_dir: &Path) -> CfInstalledIndex {
    std::fs::read_to_string(cf_index_path(game_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_cf_installed(game_dir: &Path, index: &CfInstalledIndex) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(index)?;
    let path = cf_index_path(game_dir);
    let temp = path.with_extension("json.tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, &path)?;
    Ok(())
}

fn cf_dir_name(kind: &str) -> &'static str {
    match kind {
        "resourcepack" => "resourcepacks",
        "shader" => "shaderpacks",
        "datapack" => "datapacks",
        _ => "mods",
    }
}

pub fn list_installed(game_dir: &Path, kind: &str) -> Vec<i64> {
    let dir = game_dir.join(cf_dir_name(kind));
    let index = read_cf_installed(game_dir);
    let mut out = Vec::new();
    for (project_id, info) in index.projects {
        if info.file.contains('/') || info.file.contains('\\') {
            continue;
        }
        let exists = dir.join(&info.file).exists() || dir.join(format!("{}.disabled", info.file)).exists();
        if exists {
            if let Ok(id) = project_id.parse::<i64>() {
                out.push(id);
            }
        }
    }
    out
}

pub async fn installed(data: &Path, settings: &Settings, id: &str, kind: &str) -> AppResult<Vec<i64>> {
    let game_dir = instances::game_dir_of(data, settings, id)?;
    Ok(list_installed(&game_dir, kind))
}

/// `(projectId, installed file name, fileId)` for the dependency audit, same shape as Modrinth.
pub fn installed_info(game_dir: &Path) -> Vec<(i64, String, i64)> {
    let index = read_cf_installed(game_dir);
    let mut out: Vec<(i64, String, i64)> = index
        .projects
        .into_iter()
        .filter_map(|(project_id, entry)| {
            project_id.parse::<i64>().ok().map(|id| (id, entry.file, entry.file_id))
        })
        .collect();
    out.sort_by_key(|item| item.0);
    out
}

/// Required CurseForge dependency ids declared by a concrete file.
pub async fn required_dependencies(mod_id: i64, file_id: i64) -> AppResult<Vec<i64>> {
    if file_id <= 0 {
        return Ok(Vec::new());
    }
    let file = get_file_info(mod_id, file_id).await?;
    Ok(required_dependency_ids(&file))
}

/// Best-effort repair: re-download tracked CurseForge content files that are missing.
pub async fn repair_tracked(
    data: &Path,
    settings: &Settings,
    _app: &AppHandle,
    instance_id: &str,
) -> AppResult<usize> {
    let game_dir = instances::game_dir_of(data, settings, instance_id)?;
    let snapshot = read_cf_installed(&game_dir);
    if snapshot.projects.is_empty() {
        return Ok(0);
    }

    let mut repaired = 0usize;
    for (project_id_str, info) in snapshot.projects {
        if info.file.contains('/') || info.file.contains('\\') {
            continue;
        }
        let mod_id: i64 = match project_id_str.parse() {
            Ok(id) => id,
            Err(_) => continue,
        };

        let mut exists_anywhere = false;
        for dir_name in ["mods", "resourcepacks", "shaderpacks", "datapacks"] {
            let dir = game_dir.join(dir_name);
            if dir.join(&info.file).exists() || dir.join(format!("{}.disabled", info.file)).exists() {
                exists_anywhere = true;
                break;
            }
        }
        if exists_anywhere {
            continue;
        }

        let kind = match api_get::<ModResponse>(&format!("{API}/v1/mods/{mod_id}"), "CurseForge: проект").await {
            Ok(project) => match class_from_id(project.data.class_id, "mod").as_str() {
                "resourcepack" => "resourcepack",
                "shader" => "shader",
                "datapack" => "datapack",
                "modpack" => continue,
                _ => "mod",
            },
            Err(_) => "mod",
        };

        let file = match get_file_info(mod_id, info.file_id).await {
            Ok(f) => f,
            Err(_) => continue,
        };
        let name = match safe_file_name(&file.file_name) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let dir = game_dir.join(cf_dir_name(kind));
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let dest = match resolve_inside(&dir, &[&name]) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let sha1 = file.hashes.iter().find(|h| h.algo == 1).map(|h| h.value.to_lowercase());
        let valid = crate::download::is_file_valid(&dest, if file.file_length > 0 { Some(file.file_length) } else { None }, sha1.as_deref(), None);
        if !valid && dest.exists() {
            continue;
        }
        if !valid {
            let spec = DownloadSpec {
                url: resolve_download_url(&file),
                dest: dest.clone(),
                sha1,
                sha256: None,
                size: if file.file_length > 0 { Some(file.file_length) } else { None },
                purpose: "Восстановление CurseForge".to_string(),
            };
            if download_file(&spec, |_| {}).await.is_err() {
                continue;
            }
            repaired += 1;
        }

        let mut index = read_cf_installed(&game_dir);
        index.projects.insert(
            project_id_str,
            CfInstalledEntry {
                file: name,
                file_id: file.id,
            },
        );
        let _ = write_cf_installed(&game_dir, &index);
    }

    Ok(repaired)
}

// ---------------------------------------------------------------- install single file

pub async fn install_file(
    data: &Path,
    settings: &Settings,
    app: &AppHandle,
    instance_id: &str,
    mod_id: i64,
    kind: &str,
    file_id: Option<i64>,
    world_name: Option<&str>,
) -> AppResult<String> {
    let meta = instances::read_meta_at(data, settings, instance_id)?;
    let game_dir = instances::game_dir_of(data, settings, instance_id)?;
    if kind == "mod" && meta.loader.r#type == "vanilla" {
        return Err(AppError::new(
            "LOADER_REQUIRED",
            "Моды не работают в инстансе Vanilla. Создай инстанс с Fabric, Quilt, Forge или NeoForge.",
        ));
    }
    let op_id = format!("curseforge:{instance_id}:{mod_id}");
    let emit = |label: &str, fraction: Option<f64>, status: &str| {
        game::emit(
            app,
            "op:progress",
            json!({
                "opId": op_id,
                "kind": "install",
                "instanceId": instance_id,
                "label": label,
                "fraction": fraction,
                "status": status,
                "message": Value::Null,
            }),
        );
    };
    emit("Ищу файл на CurseForge…", None, "running");
    let result: AppResult<String> = async {
        let file = if let Some(fid) = file_id {
            let files_resp: FilesResponse = api_get(&format!("{API}/v1/mods/{mod_id}/files?pageSize=50&index=0"), "CurseForge: файлы").await?;
            files_resp.data.into_iter().find(|f| f.id == fid).unwrap_or(get_file_info(mod_id, fid).await?)
        } else {
            let loader = if meta.loader.r#type == "vanilla" { None } else { Some(meta.loader.r#type.as_str()) };
            let response: FilesResponse = api_get(
                {
                    let mut url = reqwest::Url::parse(&format!("{API}/v1/mods/{mod_id}/files")).expect("static url");
                    {
                        let mut qp = url.query_pairs_mut();
                        qp.append_pair("pageSize", "50");
                        qp.append_pair("index", "0");
                        qp.append_pair("gameVersion", &meta.mc_version);
                        if let Some(loader) = loader {
                            if let Some(id) = loader_id(loader) {
                                qp.append_pair("modLoaderType", &id.to_string());
                            }
                        }
                    }
                    &url.to_string()
                },
                "CurseForge: файлы",
            )
            .await?;
            let mut iter = response.data.into_iter().filter(|f| f.is_available.unwrap_or(true));
            let first = iter.next();
            let picked = std::iter::once(first.clone())
                .flatten()
                .chain(iter)
                .find(|f| f.release_type == 1)
                .or(first)
                .ok_or_else(|| AppError::new("CF_NO_FILE", "Подходящий файл CurseForge не найден для этой версии / загрузчика"))?;
            picked
        };

        let dir = if kind == "datapack" {
            let world = world_name.unwrap_or("").trim();
            if world.is_empty() || world.contains('/') || world.contains('\\') || world.contains("..") {
                return Err(AppError::new("CF_WORLD", "Выбери мир для установки датапака"));
            }
            game_dir.join("saves").join(world).join("datapacks")
        } else {
            game_dir.join(cf_dir_name(kind))
        };
        std::fs::create_dir_all(&dir)?;
        let dest_name = safe_file_name(&file.file_name)?;
        let dest = dir.join(&dest_name);
        let url = resolve_download_url(&file);
        let sha1 = file.hashes.iter().find(|h| h.algo == 1).map(|h| h.value.to_lowercase());
        let spec = DownloadSpec {
            url,
            dest: dest.clone(),
            sha1,
            sha256: None,
            size: if file.file_length > 0 { Some(file.file_length) } else { None },
            purpose: match kind {
                "mod" => "Мод CurseForge",
                "shader" => "Шейдер CurseForge",
                "datapack" => "Датапак CurseForge",
                _ => "Ресурспак CurseForge",
            }
            .to_string(),
        };
        download_file(&spec, |_| {}).await?;

        let mut index = read_cf_installed(&game_dir);
        index.projects.insert(mod_id.to_string(), CfInstalledEntry { file: dest_name.clone(), file_id: file.id });
        write_cf_installed(&game_dir, &index)?;
        Ok(dest_name)
    }
    .await;
    match result {
        Ok(name) => {
            emit("Установлено", Some(1.0), "done");
            Ok(name)
        }
        Err(error) => {
            emit("Установка прервана", None, "cancelled");
            Err(error)
        }
    }
}

/// Install a CurseForge mod together with every required dependency declared by its files.
/// BFS with a visited set and depth cap; optional/embedded relations are ignored.
pub async fn install_with_dependencies(
    data: &Path,
    settings: &Settings,
    app: &AppHandle,
    instance_id: &str,
    mod_id: i64,
    kind: &str,
    file_id: Option<i64>,
) -> AppResult<Value> {
    const MAX_DEP_DEPTH: usize = 6;
    const MAX_QUEUE: usize = 80;

    let root_file = install_file(data, settings, app, instance_id, mod_id, kind, file_id, None).await?;
    let game_dir = instances::game_dir_of(data, settings, instance_id)?;
    let mut installed: Vec<Value> = vec![json!({ "modId": mod_id, "file": root_file })];
    let mut already_installed: Vec<i64> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    let mut visited: HashSet<i64> = HashSet::new();
    visited.insert(mod_id);

    let mut queue: Vec<(i64, i64, usize)> = Vec::new(); // (dep id, parent id, depth)
    let root_file_id = installed_info(&game_dir)
        .into_iter()
        .find(|(id, _, _)| *id == mod_id)
        .map(|(_, _, fid)| fid)
        .unwrap_or(0);
    match required_dependencies(mod_id, root_file_id).await {
        Ok(deps) => queue.extend(deps.into_iter().map(|dep| (dep, mod_id, 0))),
        Err(error) => failed.push(json!({
            "modId": mod_id, "code": error.code, "error": error.message,
            "hint": "Не удалось прочитать зависимости основного файла; обязательно вызови validate_dependencies.",
        })),
    }

    while let Some((dep_id, parent_id, depth)) = queue.pop() {
        if installed.len() + already_installed.len() + failed.len() > MAX_QUEUE {
            failed.push(json!({
                "modId": dep_id, "requiredBy": parent_id, "code": "DEP_LIMIT",
                "error": "Цепочка зависимостей слишком большая; установка остановлена.",
            }));
            break;
        }
        if !visited.insert(dep_id) {
            continue;
        }
        let index = read_cf_installed(&game_dir);
        let dep_present = index.projects.contains_key(&dep_id.to_string());
        let dep_file_id = if dep_present {
            index.projects.get(&dep_id.to_string()).map(|entry| entry.file_id).unwrap_or(0)
        } else {
            match install_file(data, settings, app, instance_id, dep_id, kind, None, None).await {
                Ok(name) => {
                    installed.push(json!({ "modId": dep_id, "requiredBy": parent_id, "file": name }));
                    installed_info(&game_dir)
                        .into_iter()
                        .find(|(id, _, _)| *id == dep_id)
                        .map(|(_, _, fid)| fid)
                        .unwrap_or(0)
                }
                Err(error) => {
                    failed.push(json!({
                        "modId": dep_id, "requiredBy": parent_id, "code": error.code, "error": error.message,
                        "hint": "Не удалось скачать обязательный мод. Попробуй другую версию/загрузчик или установи его с Modrinth, затем вызови validate_dependencies.",
                    }));
                    continue;
                }
            }
        };
        if dep_present {
            already_installed.push(dep_id);
        }
        if depth + 1 >= MAX_DEP_DEPTH {
            continue;
        }
        match required_dependencies(dep_id, dep_file_id).await {
            Ok(deps) => queue.extend(deps.into_iter().map(|next| (next, dep_id, depth + 1))),
            Err(error) => failed.push(json!({
                "modId": dep_id, "requiredBy": parent_id, "code": error.code, "error": error.message,
                "hint": "Не удалось прочитать зависимости этого мода; вызови validate_dependencies.",
            })),
        }
    }

    Ok(json!({
        "ok": failed.is_empty(),
        "instance": instance_id,
        "modId": mod_id,
        "kind": kind,
        "installed": installed,
        "alreadyInstalled": already_installed,
        "failed": failed,
        "note": "Обязательные зависимости CurseForge установлены рекурсивно из метаданных файлов. Перед ответом вызови validate_dependencies для аудита манифестов JAR.",
    }))
}

// ---------------------------------------------------------------- modpack install

#[derive(Debug, Deserialize)]
struct CfManifest {
    #[serde(default)]
    name: String,
    #[allow(dead_code)]
    #[serde(default)]
    version: String,
    #[serde(default, rename = "manifestType")]
    manifest_type: Option<String>,
    #[serde(default)]
    overrides: Option<String>,
    minecraft: CfManifestMinecraft,
    #[serde(default)]
    files: Vec<CfManifestFile>,
}

#[derive(Debug, Deserialize)]
struct CfManifestMinecraft {
    version: String,
    #[serde(default, rename = "modLoaders")]
    mod_loaders: Vec<CfManifestLoader>,
}

#[derive(Debug, Deserialize)]
struct CfManifestLoader {
    id: String,
    #[serde(default)]
    primary: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CfManifestFile {
    #[serde(rename = "projectID")]
    project_id: i64,
    #[serde(rename = "fileID")]
    file_id: i64,
    #[serde(default)]
    required: Option<bool>,
}

fn parse_cf_manifest(raw: &str) -> AppResult<CfManifest> {
    let manifest: CfManifest = serde_json::from_str(raw)
        .map_err(|_| AppError::new("PACK_INVALID", "Некорректный manifest.json CurseForge"))?;
    if let Some(kind) = &manifest.manifest_type {
        if kind != "minecraftModpack" {
            return Err(AppError::new("PACK_INVALID", "Это не сборка Minecraft (minecraftModpack)"));
        }
    }
    Ok(manifest)
}

fn parse_loader_id(id: &str) -> (String, Option<String>) {
    let lower = id.to_lowercase();
    for prefix in ["neoforge", "forge", "fabric", "quilt"] {
        if let Some(rest) = lower.strip_prefix(&format!("{prefix}-")) {
            return (prefix.to_string(), Some(rest.to_string()));
        }
    }
    ("vanilla".to_string(), None)
}

fn emit_cf_pack(app: &AppHandle, mod_id: i64, file_id: Option<i64>, instance_id: Option<&str>, label: &str, fraction: Option<f64>, status: &str, message: Option<String>) {
    let key = format!("cf:{mod_id}:{}", file_id.unwrap_or(0));
    game::emit(
        app,
        "op:progress",
        json!({
            "opId": format!("modpack:{key}"),
            "kind": "modpack",
            "instanceId": instance_id,
            "label": label,
            "fraction": fraction,
            "status": status,
            "message": message,
        }),
    );
}

pub async fn install_pack(
    app: &AppHandle,
    state: &AppState,
    mod_id: i64,
    file_id: Option<i64>,
    cancel: Arc<AtomicBool>,
) -> AppResult<Option<String>> {
    let mut instance_id: Option<String> = None;
    let mut title = "Сборка CurseForge".to_string();
    emit_cf_pack(app, mod_id, file_id, None, "Загружаю сведения о сборке CurseForge…", None, "running", None);

    let result: AppResult<String> = crate::download::with_tracker(app, &format!("modpack:cf:{mod_id}:{}", file_id.unwrap_or(0)), "modpack", None, async {
        let project: ModResponse = api_get(&format!("{API}/v1/mods/{mod_id}"), "CurseForge: проект").await?;
        if !project.data.name.is_empty() {
            title = project.data.name.clone();
        }
        if class_from_id(project.data.class_id, "mod") != "modpack" {
            return Err(AppError::new("PACK_INVALID", "Это не сборка CurseForge (modpack)"));
        }
        let pack_meta = if let Some(fid) = file_id {
            get_file_info(mod_id, fid).await?
        } else {
            let files_resp: FilesResponse = api_get(&format!("{API}/v1/mods/{mod_id}/files?pageSize=50&index=0"), "CurseForge: файлы сборки").await?;
            files_resp.data.into_iter().next().ok_or_else(|| AppError::new("CF_NO_FILE", "У сборки нет файлов для скачивания"))?
        };
        let lower = pack_meta.file_name.to_lowercase();
        if !(lower.ends_with(".zip") || lower.ends_with(".cfpack")) {
            return Err(AppError::new("PACK_INVALID", "Файл сборки CurseForge должен быть .zip"));
        }

        let cache_dir = state.data_dir.join("cache").join("modpacks");
        std::fs::create_dir_all(&cache_dir)?;
        let pack_file = cache_dir.join(safe_file_name(&format!("cf-{mod_id}-{}.zip", pack_meta.id))?);
        emit_cf_pack(app, mod_id, file_id, None, &format!("Скачиваю сборку «{title}»…"), None, "running", None);
        let sha1 = pack_meta.hashes.iter().find(|h| h.algo == 1).map(|h| h.value.to_lowercase());
        download_file(
            &DownloadSpec {
                url: resolve_download_url(&pack_meta),
                dest: pack_file.clone(),
                sha1,
                sha256: None,
                size: if pack_meta.file_length > 0 { Some(pack_meta.file_length) } else { None },
                purpose: "Сборка CurseForge".to_string(),
            },
            |_| {},
        )
        .await?;

        let (manifest, overrides_prefix) = {
            let f = std::fs::File::open(&pack_file).map_err(|_| AppError::new("PACK_INVALID", "Не удалось открыть архив сборки CurseForge"))?;
            let mut zip = zip::ZipArchive::new(f).map_err(|_| AppError::new("PACK_INVALID", "Не удалось открыть архив сборки CurseForge"))?;
            let mut manifest_name: Option<String> = None;
            for i in 0..zip.len() {
                let entry = zip.by_index(i).map_err(|_| AppError::new("PACK_INVALID", "Архив сборки повреждён"))?;
                if entry.is_dir() { continue; }
                let name = entry.name().replace('\\', "/");
                if name.split('/').last().map(|s| s.eq_ignore_ascii_case("manifest.json")).unwrap_or(false) {
                    manifest_name = Some(name);
                    break;
                }
            }
            let manifest_name = manifest_name.ok_or_else(|| AppError::new("PACK_INVALID", "В архиве нет manifest.json"))?;
            let mut entry = zip.by_name(&manifest_name).map_err(|_| AppError::new("PACK_INVALID", "В архиве нет manifest.json"))?;
            let mut raw = String::new();
            std::io::Read::read_to_string(&mut entry, &mut raw).map_err(|_| AppError::new("PACK_INVALID", "manifest.json сборки CurseForge повреждён"))?;
            let manifest = parse_cf_manifest(&raw)?;
            let prefix = manifest.overrides.clone().unwrap_or_else(|| "overrides".to_string()).trim_matches('/').to_string();
            (manifest, if prefix.is_empty() { "overrides".to_string() } else { prefix })
        };
        if !manifest.name.trim().is_empty() {
            title = manifest.name.trim().chars().take(48).collect();
        }
        let primary = manifest.minecraft.mod_loaders.iter().find(|l| l.primary.unwrap_or(false)).or_else(|| manifest.minecraft.mod_loaders.first());
        let (loader_type, loader_version) = primary.map(|l| parse_loader_id(&l.id)).unwrap_or_else(|| ("vanilla".to_string(), None));
        let hash = mod_id.to_string().chars().fold(0u32, |acc, c| (acc + c as u32) % 8);
        let settings = state.settings.get()?;
        let running: std::collections::HashSet<String> = state.games.running_ids().into_iter().collect();
        let input = json!({
            "name": title,
            "description": project.data.summary.chars().take(120).collect::<String>(),
            "mcVersion": manifest.minecraft.version,
            "loader": { "type": loader_type, "version": loader_version },
            "kind": "custom",
            "coverColor": hash,
            "settings": {
                "javaPath": "", "memoryMinMb": null, "memoryMaxMb": null,
                "jvmArgs": "", "width": null, "height": null, "fullscreen": null
            }
        });
        let created = instances::create(&state.data_dir, &settings, &running, &input)?;
        instance_id = Some(created.meta.id.clone());
        let game_dir = instances::game_dir_of(&state.data_dir, &settings, &created.meta.id)?;
        std::fs::create_dir_all(&game_dir).map_err(|e| {
            AppError::new("IO", format!("Не удалось подготовить папку игры: {e}"))
        })?;
        std::fs::create_dir_all(game_dir.join("mods"))?;

        let files: Vec<_> = manifest.files.into_iter().filter(|f| f.required.unwrap_or(true)).collect();
        crate::download::expect_files(files.len() as u64, 0);
        let total = files.len().max(1);
        let mut cf_index = CfInstalledIndex::default();
        for (i, f) in files.iter().enumerate() {
            if cancel.load(Ordering::SeqCst) {
                return Err(AppError::new("CANCELLED", "Установка отменена"));
            }
            emit_cf_pack(app, mod_id, file_id, instance_id.as_deref(), &format!("Скачиваю моды ({}/{})…", i + 1, total), Some(i as f64 / total as f64 * 0.85), "running", None);
            let info = get_file_info(f.project_id, f.file_id).await.map_err(|e| {
                AppError::new(
                    "CF_PACK_FILE",
                    &format!(
                        "Не удалось получить файл CurseForge project={} file={}: {}",
                        f.project_id, f.file_id, e.message
                    ),
                )
            })?;
            let dest_name = safe_file_name(&info.file_name)?;
            let dest = game_dir.join("mods").join(&dest_name);
            let sha1 = info.hashes.iter().find(|h| h.algo == 1).map(|h| h.value.to_lowercase());
            let spec = DownloadSpec {
                url: resolve_download_url(&info),
                dest,
                sha1,
                sha256: None,
                size: if info.file_length > 0 { Some(info.file_length) } else { None },
                purpose: "Мод сборки CurseForge".to_string(),
            };
            download_file(&spec, |_| {}).await.map_err(|e| {
                AppError::new(
                    "CF_PACK_FILE",
                    &format!(
                        "Не удалось скачать файл CurseForge project={} file={}: {}",
                        f.project_id, f.file_id, e.message
                    ),
                )
            })?;
            cf_index.projects.insert(f.project_id.to_string(), CfInstalledEntry { file: dest_name, file_id: info.id });
        }
        write_cf_installed(&game_dir, &cf_index)?;

        emit_cf_pack(app, mod_id, file_id, instance_id.as_deref(), "Копирую overrides…", Some(0.9), "running", None);
        {
            let f = std::fs::File::open(&pack_file).map_err(|_| AppError::new("PACK_INVALID", "Архив сборки повреждён"))?;
            let mut zip = zip::ZipArchive::new(f).map_err(|_| AppError::new("PACK_INVALID", "Архив сборки повреждён"))?;
            let prefix = format!("{}/", overrides_prefix);
            for i in 0..zip.len() {
                if cancel.load(Ordering::SeqCst) {
                    return Err(AppError::new("CANCELLED", "Установка отменена"));
                }
                let mut entry = zip.by_index(i).map_err(|_| AppError::new("PACK_INVALID", "Архив сборки повреждён"))?;
                if entry.is_dir() { continue; }
                let name = entry.name().replace('\\', "/");
                let rel = if name.to_lowercase().starts_with(&prefix.to_lowercase()) {
                    name[prefix.len()..].to_string()
                } else {
                    // try to find prefix anywhere (some zips have a root folder)
                    let lower = name.to_lowercase();
                    let needle = format!("/{prefix}");
                    if let Some(idx) = lower.find(&needle) {
                        name[idx + needle.len()..].to_string()
                    } else {
                        continue;
                    }
                };
                if rel.is_empty() || rel.contains("..") { continue; }
                let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
                if parts.is_empty() { continue; }
                let mut refs: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
                if let Some(last) = refs.last_mut() {
                    *last = safe_file_name(last).unwrap_or_else(|_| last.clone());
                }
                let ref_strs: Vec<&str> = refs.iter().map(|s| s.as_str()).collect();
                let dest = resolve_inside(&game_dir, &ref_strs)?;
                if let Some(parent) = dest.parent() { std::fs::create_dir_all(parent)?; }
                let mut out = std::fs::File::create(&dest)?;
                std::io::copy(&mut entry, &mut out)?;
            }
        }

        emit_cf_pack(app, mod_id, file_id, instance_id.as_deref(), &format!("Сборка «{title}» добавлена. Устанавливаю Minecraft…"), Some(1.0), "done", None);
        Ok(created.meta.id)
    })
    .await;

    match result {
        Ok(id) => Ok(Some(id)),
        Err(error) => {
            if let Some(id) = instance_id.take() {
                if let Ok(settings) = state.settings.get() {
                    let _ = instances::remove(&state.data_dir, &settings, &json!({ "id": id, "confirm": true }));
                }
            }
            let cancelled = cancel.load(Ordering::SeqCst) || error.code == "CANCELLED";
            if cancelled {
                emit_cf_pack(app, mod_id, file_id, None, "Установка сборки отменена", None, "cancelled", None);
                Ok(None)
            } else {
                emit_cf_pack(app, mod_id, file_id, None, &format!("Не удалось установить «{title}»"), None, "error", Some(error.message.clone()));
                Err(error)
            }
        }
    }
}

pub fn pack_key(mod_id: i64, file_id: Option<i64>) -> String {
    format!("cf:{mod_id}:{}", file_id.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_required_file_dependencies() {
        let file: FileItem = serde_json::from_value(json!({
            "id": 42,
            "dependencies": [
                { "modId": 10, "relationType": 3, "isOptional": false },
                { "modId": 11, "relationType": 3, "isOptional": true },
                { "modId": 12, "relationType": 4, "isOptional": false },
                { "modId": 13, "relationType": 6, "isOptional": false }
            ]
        }))
        .expect("file");
        assert_eq!(required_dependency_ids(&file), vec![10]);
        let info = file_to_info(&file);
        assert_eq!(info["requiredDependencies"], json!([10]));
        assert_eq!(info["dependencies"][0]["required"], json!(true));
        assert_eq!(info["dependencies"][1]["required"], json!(false));
    }
}
