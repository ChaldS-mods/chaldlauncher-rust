use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use base64::Engine;
use futures_util::stream::{self, StreamExt};
use futures_util::pin_mut;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::download::{download_file, is_file_valid, DownloadSpec};
use crate::errors::{AppError, AppResult};
use crate::game;
use crate::instances;
use crate::net;
use crate::paths::{resolve_inside, safe_file_name};
use crate::settings::Settings;

const API: &str = "https://api.modrinth.com/v2";
const PAGE_SIZE: usize = 20;
const MAX_ICON_BYTES: u64 = 256 * 1024;
const MAX_BODY_CHARS: usize = 60_000;
const MAX_CHANGELOG_CHARS: usize = 6_000;
const MAX_VERSION_LIST: usize = 150;
const MAX_DEP_DEPTH: usize = 6;
const INDEX_FILE: &str = ".chald-modrinth.json";

#[derive(Debug, Clone, Deserialize)]
struct MrFile {
    url: String,
    filename: String,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    hashes: MrHashes,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MrHashes {
    #[serde(default)]
    sha1: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Dependency {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    version_id: Option<String>,
    #[serde(default)]
    dependency_type: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MrVersion {
    id: String,
    project_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version_number: Option<String>,
    #[serde(default)]
    version_type: String,
    #[serde(default)]
    game_versions: Vec<String>,
    #[serde(default)]
    loaders: Vec<String>,
    #[serde(default)]
    files: Vec<MrFile>,
    #[serde(default)]
    dependencies: Vec<Dependency>,
    #[serde(default)]
    date_published: Option<String>,
    #[serde(default)]
    downloads: Option<u64>,
    #[serde(default)]
    changelog: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MrProject {
    id: String,
    #[serde(default)]
    slug: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    project_type: String,
    #[serde(default)]
    downloads: Option<u64>,
    #[serde(default)]
    followers: Option<u64>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    loaders: Vec<String>,
    #[serde(default)]
    game_versions: Vec<String>,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    license: Option<License>,
    #[serde(default)]
    updated: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct License {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    hits: Vec<SearchHit>,
    #[serde(default)]
    total_hits: u64,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    project_id: String,
    #[serde(default)]
    slug: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    follows: u64,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    display_categories: Option<Vec<String>>,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    project_type: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HitOut {
    id: String,
    slug: String,
    title: String,
    description: String,
    author: String,
    downloads: u64,
    follows: u64,
    categories: Vec<String>,
    #[serde(rename = "type")]
    kind: String,
    icon_data_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchOut {
    hits: Vec<HitOut>,
    total: u64,
    offset: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectOut {
    id: String,
    slug: String,
    title: String,
    description: String,
    body: String,
    #[serde(rename = "type")]
    kind: String,
    downloads: u64,
    follows: u64,
    categories: Vec<String>,
    loaders: Vec<String>,
    game_versions: Vec<String>,
    license: Option<String>,
    updated: Option<String>,
    icon_data_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionOut {
    id: String,
    name: String,
    version_number: String,
    #[serde(rename = "type")]
    kind: String,
    game_versions: Vec<String>,
    loaders: Vec<String>,
    published: String,
    downloads: u64,
    changelog: String,
    file_name: String,
    size: u64,
    required_deps: usize,
}

fn project_type(value: &str) -> String {
    match value {
        "mod" | "modpack" | "resourcepack" | "shader" | "datapack" => value.to_string(),
        _ => "mod".to_string(),
    }
}

pub fn loader_filter(loader: &str) -> Option<Vec<String>> {
    match loader {
        "vanilla" | "" => None,
        "quilt" => Some(vec!["quilt".to_string(), "fabric".to_string()]),
        other => Some(vec![other.to_string()]),
    }
}

fn build_facets(query_type: &str, mc_version: Option<&str>, loader: Option<&str>) -> Vec<Vec<String>> {
    let mut facets = vec![vec![format!("project_type:{query_type}")]];
    if let Some(mc) = mc_version.filter(|value| !value.is_empty()) {
        facets.push(vec![format!("versions:{mc}")]);
    }
    if query_type == "mod" {
        if let Some(loader) = loader.filter(|value| !value.is_empty()) {
            let filter = loader_filter(loader).unwrap_or_default();
            if !filter.is_empty() {
                facets.push(filter.into_iter().map(|value| format!("categories:{value}")).collect());
            }
        }
    }
    facets
}

async fn api_project(project_id: &str) -> AppResult<MrProject> {
    net::fetch_json(&format!("{API}/project/{project_id}"), "Modrinth: проект").await
}

async fn api_version(version_id: &str) -> AppResult<MrVersion> {
    net::fetch_json(&format!("{API}/version/{version_id}"), "Modrinth: версия").await
}

/// Required Modrinth project ids declared by one installed version.
pub async fn required_dependencies(version_id: &str) -> AppResult<Vec<String>> {
    let version = api_version(version_id).await?;
    Ok(version
        .dependencies
        .into_iter()
        .filter(|dep| dep.dependency_type == "required")
        .filter_map(|dep| dep.project_id)
        .collect())
}

fn versions_url(project_id: &str, mc_version: Option<&str>, loaders: Option<&[String]>) -> String {
    let mut url = url::Url::parse(&format!("{API}/project/{project_id}/version"))
        .expect("static Modrinth url");
    {
        let mut query = url.query_pairs_mut();
        if let Some(mc) = mc_version.filter(|value| !value.is_empty()) {
            query.append_pair("game_versions", &json!([mc]).to_string());
        }
        if let Some(loaders) = loaders.filter(|value| !value.is_empty()) {
            query.append_pair("loaders", &json!(loaders).to_string());
        }
    }
    url.to_string()
}

async fn api_versions(project_id: &str, mc_version: Option<&str>, loaders: Option<&[String]>) -> AppResult<Vec<MrVersion>> {
    net::fetch_json(&versions_url(project_id, mc_version, loaders), "Modrinth: версии проекта").await
}

fn pick_version(list: &[MrVersion]) -> Option<&MrVersion> {
    list.iter()
        .find(|version| version.version_type == "release")
        .or_else(|| list.iter().find(|version| version.version_type == "beta"))
        .or_else(|| list.first())
}

fn primary_file(version: &MrVersion) -> AppResult<&MrFile> {
    version
        .files
        .iter()
        .find(|file| file.primary)
        .or_else(|| version.files.first())
        .ok_or_else(|| AppError::new("MODRINTH_NO_FILE", "У этой версии нет файлов для скачивания"))
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

async fn icon_data(url: Option<&str>) -> Option<String> {
    let url = url?;
    if url.is_empty() {
        return None;
    }
    let (content_type, bytes) = net::get_bytes(url, "Иконка проекта", MAX_ICON_BYTES).await.ok()?;
    if bytes.len() < 16 {
        return None;
    }
    let mime = if ["image/png", "image/jpeg", "image/webp", "image/gif"].contains(&content_type.as_str()) {
        content_type
    } else if bytes[0] == 0x89 && bytes[1] == 0x50 {
        "image/png".to_string()
    } else if bytes[0] == 0xff && bytes[1] == 0xd8 {
        "image/jpeg".to_string()
    } else if bytes[0] == 0x47 && bytes[1] == 0x49 {
        "image/gif".to_string()
    } else if bytes[0] == 0x52 && bytes[1] == 0x49 && bytes.get(8) == Some(&0x57) {
        "image/webp".to_string()
    } else {
        return None;
    };
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

pub async fn search(input: &Value) -> AppResult<Value> {
    let query = input.get("query").and_then(Value::as_str).unwrap_or("");
    let kind = project_type(input.get("type").and_then(Value::as_str).unwrap_or("mod"));
    let mc_version = input.get("mcVersion").and_then(Value::as_str);
    let loader = input.get("loader").and_then(Value::as_str);
    let sort = match input.get("sort").and_then(Value::as_str).unwrap_or("relevance") {
        "downloads" | "follows" | "newest" | "updated" => {
            input.get("sort").and_then(Value::as_str).unwrap_or("relevance").to_string()
        }
        _ => "relevance".to_string(),
    };
    let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0).min(10_000);

    let mut url = url::Url::parse(&format!("{API}/search")).expect("static Modrinth url");
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("query", query);
        pairs.append_pair("facets", &json!(build_facets(&kind, mc_version, loader)).to_string());
        pairs.append_pair("index", &sort);
        pairs.append_pair("offset", &offset.to_string());
        pairs.append_pair("limit", &PAGE_SIZE.to_string());
    }
    let response: SearchResponse = net::fetch_json(url.as_str(), "Modrinth: поиск").await?;
    let SearchResponse { hits, total_hits, offset } = response;

    let mut icons: Vec<Option<String>> = vec![None; hits.len()];
    let urls: Vec<Option<String>> = hits.iter().map(|hit| hit.icon_url.clone()).collect();
    let futures = stream::iter(urls.into_iter().enumerate().map(|(index, url)| async move {
        (index, icon_data(url.as_deref()).await)
    }))
    .buffer_unordered(6);
    pin_mut!(futures);
    while let Some((index, icon)) = futures.next().await {
        icons[index] = icon;
    }

    let hits: Vec<HitOut> = hits
        .into_iter()
        .zip(icons)
        .map(|(hit, icon)| HitOut {
            id: hit.project_id,
            slug: hit.slug,
            title: hit.title,
            description: hit.description,
            author: hit.author,
            downloads: hit.downloads,
            follows: hit.follows,
            categories: hit.display_categories.unwrap_or(hit.categories),
            kind: project_type(&hit.project_type),
            icon_data_url: icon,
        })
        .collect();
    Ok(serde_json::to_value(SearchOut { hits, total: total_hits, offset })?)
}

pub async fn project(project_id: &str) -> AppResult<Value> {
    let project = api_project(project_id).await?;
    let icon = icon_data(project.icon_url.as_deref()).await;
    Ok(serde_json::to_value(ProjectOut {
        id: project.id,
        slug: project.slug,
        title: project.title,
        description: project.description,
        body: truncate_chars(project.body.as_deref().unwrap_or(""), MAX_BODY_CHARS),
        kind: project_type(&project.project_type),
        downloads: project.downloads.unwrap_or(0),
        follows: project.followers.unwrap_or(0),
        categories: project.categories,
        loaders: project.loaders,
        game_versions: project.game_versions,
        license: project.license.and_then(|license| license.name),
        updated: project.updated,
        icon_data_url: icon,
    })?)
}

pub async fn versions(project_id: &str, mc_version: Option<&str>, loader: Option<&str>) -> AppResult<Value> {
    let loaders = loader.and_then(loader_filter);
    let list = api_versions(project_id, mc_version, loaders.as_deref()).await?;
    let out: Vec<VersionOut> = list
        .into_iter()
        .take(MAX_VERSION_LIST)
        .map(|version| {
            let file = version.files.iter().find(|file| file.primary).or_else(|| version.files.first());
            VersionOut {
                id: version.id,
                name: version.name.unwrap_or_default(),
                version_number: version.version_number.unwrap_or_default(),
                kind: version.version_type,
                game_versions: version.game_versions,
                loaders: version.loaders,
                published: version.date_published.unwrap_or_default(),
                downloads: version.downloads.unwrap_or(0),
                changelog: truncate_chars(version.changelog.as_deref().unwrap_or(""), MAX_CHANGELOG_CHARS),
                file_name: file.map(|file| file.filename.clone()).unwrap_or_default(),
                size: file.and_then(|file| file.size).unwrap_or(0),
                required_deps: version
                    .dependencies
                    .iter()
                    .filter(|dep| dep.dependency_type == "required" && dep.project_id.is_some())
                    .count(),
            }
        })
        .collect();
    Ok(serde_json::to_value(out)?)
}

// ---------------------------------------------------------------- installed index

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledProject {
    file: String,
    #[serde(rename = "versionId", default)]
    version_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct InstalledIndex {
    #[serde(default)]
    projects: HashMap<String, InstalledProject>,
}

fn index_path(game_dir: &Path) -> PathBuf {
    game_dir.join(INDEX_FILE)
}

fn read_installed(game_dir: &Path) -> InstalledIndex {
    std::fs::read_to_string(index_path(game_dir))
        .ok()
        .and_then(|text| serde_json::from_str::<InstalledIndex>(&text).ok())
        .unwrap_or_default()
}

fn write_installed(game_dir: &Path, index: &InstalledIndex) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(index)?;
    let path = index_path(game_dir);
    let temp = path.with_extension("json.tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, &path)?;
    Ok(())
}

fn content_dir_name(kind: &str) -> &'static str {
    match kind {
        "resourcepack" => "resourcepacks",
        "shader" => "shaderpacks",
        "datapack" => "datapacks",
        _ => "mods",
    }
}

fn exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

pub fn list_installed_projects(game_dir: &Path, kind: &str) -> Vec<String> {
    let dir = game_dir.join(content_dir_name(kind));
    let index = read_installed(game_dir);
    let mut out = Vec::new();
    for (project_id, info) in index.projects {
        if info.file.contains('/') || info.file.contains('\\') {
            continue;
        }
        if exists(&dir.join(&info.file)) || exists(&dir.join(format!("{}.disabled", info.file))) {
            out.push(project_id);
        }
    }
    out
}

pub async fn installed(data: &Path, settings: &Settings, id: &str, kind: &str) -> AppResult<Vec<String>> {
    let game_dir = instances::game_dir_of(data, settings, id)?;
    Ok(list_installed_projects(&game_dir, kind))
}

/// (project id, installed file, version id) for the AI agent's mod tools.
pub fn installed_info(game_dir: &Path) -> Vec<(String, String, String)> {
    read_installed(game_dir)
        .projects
        .into_iter()
        .map(|(project_id, info)| (project_id, info.file, info.version_id))
        .collect()
}

// ---------------------------------------------------------------- installing

pub struct InstallTarget {
    pub game_dir: PathBuf,
    pub mc_version: String,
    pub loader: String,
    pub kind: String,
    pub world_name: Option<String>,
}

struct PlanItem {
    project_id: String,
    title: String,
    version: MrVersion,
    file: MrFile,
    dest: PathBuf,
    #[allow(dead_code)]
    requested: bool,
}

fn target_dir(target: &InstallTarget) -> AppResult<PathBuf> {
    if target.kind == "datapack" {
        let world = target.world_name.as_deref().unwrap_or("").trim();
        if world.is_empty() || world.contains('/') || world.contains('\\') || world.contains("..") {
            return Err(AppError::new("MR_WORLD", "Выбери мир для установки датапака"));
        }
        return Ok(target.game_dir.join("saves").join(world).join("datapacks"));
    }
    Ok(target.game_dir.join(content_dir_name(&target.kind)))
}

async fn plan_install(target: &InstallTarget, project_id: &str, version_id: Option<&str>) -> AppResult<Vec<PlanItem>> {
    if target.kind == "mod" && target.loader == "vanilla" {
        return Err(AppError::new(
            "LOADER_REQUIRED",
            "Моды не работают в инстансе Vanilla. Создай инстанс с Fabric, Quilt, Forge или NeoForge.",
        ));
    }
    let ext = if target.kind == "mod" { ".jar" } else { ".zip" };
    let dir = target_dir(target)?;
    let installed = read_installed(&target.game_dir);
    let loaders = if target.kind == "mod" { loader_filter(&target.loader) } else { None };

    let mut plan = Vec::new();
    let mut seen = HashSet::new();
    let mut queue: VecDeque<(String, Option<String>, usize)> =
        VecDeque::from([(project_id.to_string(), version_id.map(str::to_string), 0)]);

    while let Some((current_id, pinned_version, depth)) = queue.pop_front() {
        if !seen.insert(current_id.clone()) {
            continue;
        }
        let requested = depth == 0;
        if let Some(have) = installed.projects.get(&current_id) {
            if exists(&dir.join(&have.file)) {
                if requested {
                    return Err(AppError::new(
                        "ALREADY_INSTALLED",
                        "Этот проект уже установлен в инстансе. Чтобы поставить другую версию, сначала удали старый файл.",
                    ));
                }
                continue;
            }
        }

        let project = api_project(&current_id).await?;
        let version = match pinned_version.as_deref() {
            Some(version_id) => {
                let version = api_version(version_id).await?;
                if version.project_id != current_id {
                    return Err(AppError::new("MODRINTH_FORMAT", "Эта версия относится к другому проекту"));
                }
                version
            }
            None => {
                let list = api_versions(&current_id, Some(&target.mc_version), loaders.as_deref()).await?;
                pick_version(&list)
                    .cloned()
                    .ok_or_else(|| {
                        let what = if requested {
                            "Для этого проекта".to_string()
                        } else {
                            format!("Для обязательной зависимости «{}»", project.title)
                        };
                        AppError::new(
                            "NO_COMPATIBLE_VERSION",
                            format!(
                                "{what} нет версии под Minecraft {}{}",
                                target.mc_version,
                                if loaders.is_some() { format!(" и {}", target.loader) } else { String::new() }
                            ),
                        )
                    })?
            }
        };
        let file = primary_file(&version)?.clone();
        let name = safe_file_name(&file.filename)?;
        if !name.to_lowercase().ends_with(ext) {
            return Err(AppError::new("CONTENT_TYPE", format!("Ожидался файл {ext}: {name}")));
        }
        plan.push(PlanItem {
            project_id: current_id.clone(),
            title: project.title,
            version: version.clone(),
            file,
            dest: resolve_inside(&dir, &[&name])?,
            requested,
        });

        if target.kind == "mod" && depth < MAX_DEP_DEPTH {
            for dependency in &version.dependencies {
                if dependency.dependency_type == "required" {
                    if let Some(dependency_project) = &dependency.project_id {
                        queue.push_back((
                            dependency_project.clone(),
                            dependency.version_id.clone(),
                            depth + 1,
                        ));
                    }
                }
            }
        }
    }
    Ok(plan)
}

async fn run_plan(
    game_dir: &Path,
    kind: &str,
    plan: &[PlanItem],
    report: &(dyn Fn(&str, f64) + Sync),
) -> AppResult<Vec<String>> {
    let mut index = read_installed(game_dir);
    let mut names = Vec::new();
    let total = plan.len().max(1);
    for (number, item) in plan.iter().enumerate() {
        report(&format!("Скачиваю: {}", item.title), number as f64 / total as f64);
        let valid = is_file_valid(&item.dest, item.file.size, item.file.hashes.sha1.as_deref(), None);
        if !valid && exists(&item.dest) {
            return Err(AppError::new(
                "CONTENT_EXISTS",
                format!(
                    "Файл с таким именем уже есть и отличается: {}",
                    item.dest.file_name().unwrap_or_default().to_string_lossy()
                ),
            ));
        }
        if !valid {
            download_file(
                &DownloadSpec {
                    url: item.file.url.clone(),
                    dest: item.dest.clone(),
                    sha1: item.file.hashes.sha1.clone(),
                    sha256: None,
                    size: item.file.size,
                    purpose: if kind == "mod" { "Мод Modrinth".to_string() } else { "Ресурспак Modrinth".to_string() },
                },
                |_| {},
            )
            .await?;
        }
        index.projects.insert(
            item.project_id.clone(),
            InstalledProject {
                file: item.dest.file_name().unwrap_or_default().to_string_lossy().to_string(),
                version_id: item.version.id.clone(),
            },
        );
        names.push(item.dest.file_name().unwrap_or_default().to_string_lossy().to_string());
    }
    write_installed(game_dir, &index)?;
    report("Готово", 1.0);
    Ok(names)
}

pub async fn install(
    data: &Path,
    settings: &Settings,
    app: &AppHandle,
    instance_id: &str,
    project_id: &str,
    kind: &str,
    version_id: Option<&str>,
    world_name: Option<&str>,
) -> AppResult<Vec<String>> {
    let meta = instances::read_meta_at(data, settings, instance_id)?;
    let game_dir = instances::game_dir_of(data, settings, instance_id)?;
    let target = InstallTarget {
        game_dir: game_dir.clone(),
        mc_version: meta.mc_version.clone(),
        loader: meta.loader.r#type.clone(),
        kind: kind.to_string(),
        world_name: world_name.map(str::to_string),
    };
    let op_id = format!("modrinth:{instance_id}:{project_id}");
    let progress = |label: &str, fraction: Option<f64>| {
        game::emit(
            app,
            "op:progress",
            json!({
                "opId": op_id,
                "kind": "install",
                "instanceId": instance_id,
                "label": label,
                "fraction": fraction,
                "status": "running",
                "message": null,
            }),
        );
    };
    progress("Ищу подходящую версию…", None);
    let plan = match plan_install(&target, project_id, version_id).await {
        Ok(plan) => plan,
        Err(error) => {
            game::emit(
                app,
                "op:progress",
                json!({ "opId": format!("modrinth:{instance_id}:{project_id}"), "kind": "install", "instanceId": instance_id, "label": "Установка прервана", "fraction": null, "status": "cancelled", "message": null }),
            );
            return Err(error);
        }
    };
    let result = run_plan(&game_dir, kind, &plan, &|label, fraction| progress(label, Some(fraction))).await;
    match &result {
        Ok(_) => game::emit(
            app,
            "op:progress",
            json!({ "opId": format!("modrinth:{instance_id}:{project_id}"), "kind": "install", "instanceId": instance_id, "label": "Установлено", "fraction": 1.0, "status": "done", "message": null }),
        ),
        Err(error) => game::emit(
            app,
            "op:progress",
            json!({ "opId": format!("modrinth:{instance_id}:{project_id}"), "kind": "install", "instanceId": instance_id, "label": "Установка прервана", "fraction": null, "status": "cancelled", "message": error.message.clone() }),
        ),
    }
    result
}

pub fn open_page(kind: &str, slug: &str) -> AppResult<()> {
    if !slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || slug.is_empty() || slug.len() > 64 {
        return Err(AppError::new("BAD_ARGUMENT", "Некорректная ссылка на проект"));
    }
    let segment = match kind {
        "modpack" => "modpack",
        "resourcepack" => "resourcepack",
        "shader" => "shader",
        "datapack" => "datapack",
        _ => "mod",
    };
    open::that(format!("https://modrinth.com/{segment}/{slug}"))
        .map_err(|e| AppError::new("OPEN_FAILED", e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facets_and_filters() {
        assert_eq!(loader_filter("quilt"), Some(vec!["quilt".to_string(), "fabric".to_string()]));
        assert_eq!(loader_filter("fabric"), Some(vec!["fabric".to_string()]));
        assert_eq!(loader_filter("vanilla"), None);
        let facets = build_facets("mod", Some("1.20.1"), Some("fabric"));
        assert_eq!(facets.len(), 3);
        assert_eq!(facets[2], vec!["categories:fabric".to_string()]);
    }

    #[test]
    fn version_selection() {
        let list = vec![
            MrVersion { id: "a".into(), project_id: "p".into(), name: None, version_number: None, version_type: "alpha".into(), game_versions: vec![], loaders: vec![], files: vec![], dependencies: vec![], date_published: None, downloads: None, changelog: None },
            MrVersion { id: "b".into(), project_id: "p".into(), name: None, version_number: None, version_type: "release".into(), game_versions: vec![], loaders: vec![], files: vec![], dependencies: vec![], date_published: None, downloads: None, changelog: None },
        ];
        assert_eq!(pick_version(&list).map(|v| v.id.as_str()), Some("b"));
    }

    #[test]
    fn content_dirs() {
        assert_eq!(content_dir_name("mod"), "mods");
        assert_eq!(content_dir_name("resourcepack"), "resourcepacks");
        assert_eq!(content_dir_name("shader"), "shaderpacks");
        assert_eq!(content_dir_name("datapack"), "datapacks");
    }
}

// ---------------------------------------------------------------- modrinth modpacks (.mrpack)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use zip::ZipArchive;

use crate::state::AppState;

const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_OVERRIDE_ENTRY: u64 = 256 * 1024 * 1024;
const MAX_OVERRIDE_TOTAL: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
struct MrpackFileEnv {
    #[serde(default)]
    client: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MrpackFileHashes {
    sha1: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MrpackFile {
    path: String,
    hashes: MrpackFileHashes,
    #[serde(default)]
    env: Option<MrpackFileEnv>,
    downloads: Vec<String>,
    #[serde(default, rename = "fileSize")]
    file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct MrpackIndex {
    #[serde(rename = "formatVersion")]
    format_version: u32,
    game: String,
    #[allow(dead_code)]
    #[serde(default, rename = "versionId")]
    version_id: String,
    name: String,
    #[allow(dead_code)]
    #[serde(default)]
    summary: Option<String>,
    files: Vec<MrpackFile>,
    dependencies: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct PackPlanFile {
    rel: String,
    sha1: String,
    size: Option<u64>,
    urls: Vec<String>,
}

#[derive(Debug, Clone)]
struct PackPlan {
    name: String,
    mc_version: String,
    loader_type: String,
    loader_version: Option<String>,
    files: Vec<PackPlanFile>,
}

fn safe_pack_path(raw: &str) -> AppResult<String> {
    if raw.is_empty() || raw.contains('\0') || raw.contains('\\') || raw.starts_with('/') || (raw.len() >= 2 && raw.as_bytes()[1] == b':') {
        return Err(AppError::new("PACK_PATH", &format!("Недопустимый путь в сборке: {}", &raw.chars().take(80).collect::<String>())));
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(AppError::new("PACK_PATH", &format!("Недопустимый путь в сборке: {}", &raw.chars().take(80).collect::<String>())));
        }
        parts.push(part);
    }
    if parts.is_empty() || raw.ends_with('/') {
        return Err(AppError::new("PACK_PATH", &format!("Недопустимый путь в сборке: {}", &raw.chars().take(80).collect::<String>())));
    }
    Ok(parts.join("/"))
}

fn read_mrpack_index(pack_file: &Path) -> AppResult<MrpackIndex> {
    let f = std::fs::File::open(pack_file).map_err(|_| AppError::new("PACK_INVALID", "Файл сборки повреждён"))?;
    let mut zip = ZipArchive::new(f).map_err(|_| AppError::new("PACK_INVALID", "Файл сборки повреждён"))?;
    let mut entry = zip.by_name("modrinth.index.json").map_err(|_| AppError::new("PACK_INVALID", "В сборке нет modrinth.index.json"))?;
    if entry.size() > MAX_INDEX_BYTES {
        return Err(AppError::new("PACK_INVALID", "В сборке нет modrinth.index.json"));
    }
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut entry, &mut buf).map_err(|_| AppError::new("PACK_INVALID", "modrinth.index.json не читается"))?;
    let index: MrpackIndex = serde_json::from_str(&buf).map_err(|e| AppError::new("PACK_INVALID", &format!("Неверный формат сборки: {e}")))?;
    if index.format_version != 1 || index.game != "minecraft" {
        return Err(AppError::new("PACK_INVALID", "Неверный формат сборки"));
    }
    Ok(index)
}

fn plan_pack(index: &MrpackIndex) -> AppResult<PackPlan> {
    let mc_version = index.dependencies.get("minecraft").cloned().ok_or_else(|| AppError::new("PACK_INVALID", "В сборке не указана версия Minecraft"))?;
    if !mc_version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')) || mc_version.len() > 40 {
        return Err(AppError::new("PACK_INVALID", "В сборке не указана версия Minecraft"));
    }
    let loader_keys = [
        ("neoforge", "neoforge"),
        ("forge", "forge"),
        ("fabric-loader", "fabric"),
        ("quilt-loader", "quilt"),
    ];
    let mut found: Vec<(&str, &str, String)> = Vec::new();
    for (key, loader_type) in loader_keys {
        if let Some(version) = index.dependencies.get(key) {
            found.push((key, loader_type, version.chars().take(64).collect()));
        }
    }
    if found.len() > 1 {
        return Err(AppError::new("PACK_INVALID", "В сборке указано несколько загрузчиков"));
    }
    let (loader_type, loader_version) = if let Some((_, t, v)) = found.first() {
        (t.to_string(), Some(v.clone()))
    } else {
        ("vanilla".to_string(), None)
    };
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for file in &index.files {
        if file.env.as_ref().and_then(|env| env.client.as_deref()) == Some("unsupported") {
            continue;
        }
        let rel = safe_pack_path(&file.path)?;
        if !seen.insert(rel.clone()) {
            return Err(AppError::new("PACK_INVALID", &format!("Файл указан дважды: {rel}")));
        }
        if file.hashes.sha1.len() != 40 || !file.hashes.sha1.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(AppError::new("PACK_INVALID", "Некорректный хеш файла в сборке"));
        }
        if file.downloads.is_empty() {
            return Err(AppError::new("PACK_INVALID", "У файла сборки нет ссылок для скачивания"));
        }
        files.push(PackPlanFile {
            rel,
            sha1: file.hashes.sha1.to_ascii_lowercase(),
            size: file.file_size,
            urls: file.downloads.clone(),
        });
    }
    Ok(PackPlan {
        name: if index.name.trim().is_empty() { "Modrinth Pack".to_string() } else { index.name.trim().chars().take(48).collect() },
        mc_version,
        loader_type,
        loader_version,
        files,
    })
}

fn extract_pack_overrides(pack_file: &Path, game_dir: &Path, cancel: &AtomicBool) -> AppResult<u32> {
    let f = std::fs::File::open(pack_file).map_err(|_| AppError::new("PACK_INVALID", "Файл сборки повреждён"))?;
    let mut zip = ZipArchive::new(f).map_err(|_| AppError::new("PACK_INVALID", "Файл сборки повреждён"))?;
    let mut total: u64 = 0;
    let mut count: u32 = 0;
    for prefix in ["overrides/", "client-overrides/"] {
        for i in 0..zip.len() {
            if cancel.load(Ordering::SeqCst) {
                return Err(AppError::new("CANCELLED", "Установка отменена"));
            }
            let mut entry = zip.by_index(i).map_err(|_| AppError::new("PACK_INVALID", "Файл сборки повреждён"))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().replace('\\', "/");
            if !name.starts_with(prefix) {
                continue;
            }
            let rel = safe_pack_path(name.strip_prefix(prefix).unwrap_or(""))?;
            if entry.size() > MAX_OVERRIDE_ENTRY {
                return Err(AppError::new("PACK_TOO_BIG", &format!("Файл слишком большой: {rel}")));
            }
            total = total.saturating_add(entry.size());
            if total > MAX_OVERRIDE_TOTAL {
                return Err(AppError::new("PACK_TOO_BIG", "Сборка распаковывается в слишком большой объём"));
            }
            let parts: Vec<&str> = rel.split('/').collect();
            let refs: Vec<&str> = parts.iter().map(|s| *s).collect();
            let dest = resolve_inside(game_dir, &refs)?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&dest)?;
            std::io::copy(&mut entry, &mut out)?;
            count += 1;
        }
    }
    Ok(count)
}

fn emit_pack_progress(app: &AppHandle, project_id: &str, instance_id: Option<&str>, label: &str, fraction: Option<f64>, status: &str, message: Option<String>) {
    game::emit(
        app,
        "op:progress",
        json!({
            "opId": format!("modpack:{project_id}"),
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
    project_id: &str,
    version_id: Option<&str>,
    cancel: Arc<AtomicBool>,
) -> AppResult<Option<String>> {
    emit_pack_progress(app, project_id, None, "Загружаю сведения о сборке…", None, "running", None);
    let mut instance_id: Option<String> = None;
    let mut title = "Сборка".to_string();

    let result: AppResult<String> = crate::download::with_tracker(app, &format!("modpack:{project_id}"), "modpack", None, async {
        let project = api_project(project_id).await?;
        title = project.title.clone();
        let version = match version_id {
            Some(id) => {
                let v = api_version(id).await?;
                if v.project_id != project_id {
                    return Err(AppError::new("MODRINTH_FORMAT", "Эта версия относится к другому проекту"));
                }
                v
            }
            None => {
                let list = api_versions(project_id, None, None).await?;
                pick_version(&list).cloned().ok_or_else(|| AppError::new("NO_COMPATIBLE_VERSION", "У сборки нет доступных версий"))?
            }
        };
        let file = primary_file(&version)?;
        if !file.filename.to_lowercase().ends_with(".mrpack") {
            return Err(AppError::new("PACK_INVALID", "Это не файл сборки Modrinth (.mrpack)"));
        }

        let cache_dir = state.data_dir.join("cache").join("modpacks");
        std::fs::create_dir_all(&cache_dir)?;
        let pack_name = safe_file_name(&format!("{}-{}.mrpack", project.id, version.id))?;
        let pack_file = cache_dir.join(pack_name);
        emit_pack_progress(app, project_id, None, &format!("Скачиваю сборку «{title}»…"), None, "running", None);
        let spec = DownloadSpec {
            url: file.url.clone(),
            dest: pack_file.clone(),
            sha1: file.hashes.sha1.clone(),
            sha256: None,
            size: file.size,
            purpose: "Сборка Modrinth".to_string(),
        };
        download_file(&spec, |_| {}).await?;

        let index = read_mrpack_index(&pack_file)?;
        let plan = plan_pack(&index)?;
        crate::download::expect_files(plan.files.len() as u64, plan.files.iter().map(|f| f.size.unwrap_or(0)).sum());
        title = plan.name.clone();
        let hash = project.id.chars().fold(0u32, |acc, c| (acc + c as u32) % 8);
        let settings = state.settings.get()?;
        let running: std::collections::HashSet<String> = state.games.running_ids().into_iter().collect();
        let input = json!({
            "name": plan.name,
            "description": project.description.chars().take(120).collect::<String>(),
            "mcVersion": plan.mc_version,
            "loader": { "type": plan.loader_type, "version": plan.loader_version },
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

        let total = plan.files.len().max(1);
        let mut done = 0usize;
        for item in &plan.files {
            if cancel.load(Ordering::SeqCst) {
                return Err(AppError::new("CANCELLED", "Установка отменена"));
            }
            let parts: Vec<&str> = item.rel.split('/').collect();
            let refs: Vec<&str> = parts.iter().map(|s| *s).collect();
            let dest = resolve_inside(&game_dir, &refs)?;
            let mut last_err: Option<AppError> = None;
            let mut ok = false;
            for url in &item.urls {
                let spec = DownloadSpec {
                    url: url.clone(),
                    dest: dest.clone(),
                    sha1: Some(item.sha1.clone()),
                    sha256: None,
                    size: item.size,
                    purpose: "Файл сборки".to_string(),
                };
                match download_file(&spec, |_| {}).await {
                    Ok(()) => { ok = true; break; }
                    Err(e) => {
                        if cancel.load(Ordering::SeqCst) {
                            return Err(AppError::new("CANCELLED", "Установка отменена"));
                        }
                        last_err = Some(e);
                    }
                }
            }
            if !ok {
                return Err(last_err.unwrap_or_else(|| AppError::new("DOWNLOAD_FAILED", "Не удалось скачать файл сборки")));
            }
            done += 1;
            emit_pack_progress(
                app,
                project_id,
                instance_id.as_deref(),
                &format!("Файлы сборки: {done} из {total}"),
                Some(0.9 * (done as f64 / total as f64)),
                "running",
                None,
            );
        }

        emit_pack_progress(app, project_id, instance_id.as_deref(), "Копирую настройки сборки…", Some(0.95), "running", None);
        extract_pack_overrides(&pack_file, &game_dir, cancel.as_ref())?;

        emit_pack_progress(
            app,
            project_id,
            instance_id.as_deref(),
            &format!("Сборка «{title}» добавлена. Устанавливаю Minecraft…"),
            Some(1.0),
            "done",
            None,
        );
        Ok(created.meta.id)
    }).await;

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
                emit_pack_progress(app, project_id, None, "Установка сборки отменена", None, "cancelled", None);
                Ok(None)
            } else {
                emit_pack_progress(app, project_id, None, &format!("Не удалось установить «{title}»"), None, "error", Some(error.message.clone()));
                Err(error)
            }
        }
    }
}

pub fn cancel_pack(state: &AppState, project_id: &str) {
    if let Some(flag) = state.modrinth_packs.lock().unwrap().get(project_id).cloned() {
        flag.store(true, Ordering::SeqCst);
    }
}

/// Best-effort repair: re-download tracked Modrinth content files that are missing.
pub async fn repair_tracked(
    data: &Path,
    settings: &Settings,
    _app: &AppHandle,
    instance_id: &str,
) -> AppResult<usize> {
    let game_dir = instances::game_dir_of(data, settings, instance_id)?;
    let snapshot = read_installed(&game_dir);
    if snapshot.projects.is_empty() {
        return Ok(0);
    }

    let mut repaired = 0usize;
    for (project_id, info) in snapshot.projects {
        if info.file.contains('/') || info.file.contains('\\') || info.version_id.is_empty() {
            continue;
        }

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

        let kind = match api_project(&project_id).await {
            Ok(project) => match project_type(&project.project_type).as_str() {
                "resourcepack" => "resourcepack",
                "shader" => "shader",
                "datapack" => "datapack",
                "modpack" => continue,
                _ => "mod",
            },
            Err(_) => "mod",
        };

        let version = match api_version(&info.version_id).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let file = match primary_file(&version) {
            Ok(f) => f.clone(),
            Err(_) => continue,
        };
        let name = match safe_file_name(&file.filename) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let dir = game_dir.join(content_dir_name(kind));
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let dest = match resolve_inside(&dir, &[&name]) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let valid = is_file_valid(&dest, file.size, file.hashes.sha1.as_deref(), None);
        if !valid && exists(&dest) {
            continue;
        }
        if !valid {
            let spec = DownloadSpec {
                url: file.url.clone(),
                dest: dest.clone(),
                sha1: file.hashes.sha1.clone(),
                sha256: None,
                size: file.size,
                purpose: "Восстановление Modrinth".to_string(),
            };
            if download_file(&spec, |_| {}).await.is_err() {
                continue;
            }
            repaired += 1;
        }

        let mut index = read_installed(&game_dir);
        index.projects.insert(
            project_id,
            InstalledProject {
                file: name,
                version_id: info.version_id,
            },
        );
        let _ = write_installed(&game_dir, &index);
    }

    Ok(repaired)
}
