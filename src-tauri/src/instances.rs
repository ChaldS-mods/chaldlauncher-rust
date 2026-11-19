use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::{AppError, AppResult};
use crate::paths::{self, is_instance_id, is_version_id, resolve_inside, slugify};
use crate::settings::Settings;

const MAX_COVER_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct InstanceSettings {
    pub java_path: String,
    pub memory_min_mb: Option<u32>,
    pub memory_max_mb: Option<u32>,
    pub jvm_args: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fullscreen: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Loader {
    pub r#type: String,
    pub version: Option<String>,
}

fn default_schema() -> u32 {
    1
}
fn default_kind() -> String {
    "custom".to_string()
}
fn default_created() -> String {
    "1970-01-01T00:00:00.000Z".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceMeta {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub mc_version: String,
    pub loader: Loader,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub installed: bool,
    #[serde(default = "default_created")]
    pub created_at: String,
    #[serde(default)]
    pub last_played_at: Option<String>,
    #[serde(default)]
    pub cover_color: u32,
    #[serde(default)]
    pub cover_file: Option<String>,
    #[serde(default)]
    pub settings: InstanceSettings,
    #[serde(default)]
    pub imported_from: Option<String>,
    #[serde(default)]
    pub play_time_sec: Option<u64>,
}

/// `InstanceView` from `src/shared/types.ts`: the metadata plus everything the card needs.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceView {
    #[serde(flatten)]
    pub meta: InstanceMeta,
    pub state: String,
    pub state_reason: Option<String>,
    pub path: String,
    pub game_dir: String,
    pub mod_count: u32,
    pub pack_count: u32,
    pub cover_data_url: Option<String>,
    pub running: bool,
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn dir_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn root_of(data: &Path, settings: &Settings) -> PathBuf {
    paths::instances_root(data, &settings.instances_dir)
}

fn read_meta(dir: &Path) -> AppResult<InstanceMeta> {
    let bytes = fs::read(dir.join("instance.json"))
        .map_err(|_| AppError::new("INSTANCE_NOT_FOUND", "Инстанс не найден"))?;
    serde_json::from_slice(&bytes).map_err(|_| AppError::new("INSTANCE_CORRUPT", "instance.json повреждён"))
}

fn write_meta(dir: &Path, meta: &InstanceMeta) -> AppResult<()> {
    let json = serde_json::to_vec_pretty(meta)?;
    let tmp = dir.join("instance.json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, dir.join("instance.json"))?;
    Ok(())
}

fn count_dir(dir: &Path, pred: impl Fn(&str, bool) -> bool) -> u32 {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if pred(&entry.file_name().to_string_lossy(), is_dir) {
                count += 1;
            }
        }
    }
    count
}

fn cover_data_url(dir: &Path, meta: &InstanceMeta) -> Option<String> {
    let name = meta.cover_file.as_deref()?;
    let file = resolve_inside(dir, &[name]).ok()?;
    let metadata = fs::metadata(&file).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_COVER_BYTES {
        return None;
    }
    let bytes = fs::read(&file).ok()?;
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

fn compute_state(data: &Path, meta: &InstanceMeta) -> (String, Option<String>) {
    if !meta.installed || meta.version_id.is_none() {
        return ("not-installed".to_string(), Some("Установка не завершена".to_string()));
    }
    let version_id = meta.version_id.as_deref().unwrap_or_default();
    let versions = paths::versions_dir(data);
    let json = versions.join(version_id).join(format!("{version_id}.json"));
    let jar = versions
        .join(&meta.mc_version)
        .join(format!("{}.jar", meta.mc_version));
    if !json.is_file() || !jar.is_file() {
        return (
            "broken".to_string(),
            Some("Не найдены файлы версии. Запусти восстановление.".to_string()),
        );
    }
    ("ready".to_string(), None)
}

fn to_view(
    data: &Path,
    settings: &Settings,
    meta: InstanceMeta,
    running_ids: &HashSet<String>,
) -> AppResult<InstanceView> {
    let root = root_of(data, settings);
    let dir = resolve_inside(&root, &[&meta.id])?;
    let game_dir = dir.join(".minecraft");
    let (state, state_reason) = compute_state(data, &meta);
    let mod_count = count_dir(&game_dir.join("mods"), |name, is_dir| !is_dir && name.ends_with(".jar"));
    let pack_count = count_dir(&game_dir.join("resourcepacks"), |name, _| !name.ends_with(".disabled"));
    let cover_data_url = cover_data_url(&dir, &meta);
    let running = running_ids.contains(&meta.id);
    Ok(InstanceView {
        meta,
        state,
        state_reason,
        path: dir_string(&dir),
        game_dir: dir_string(&game_dir),
        mod_count,
        pack_count,
        cover_data_url,
        running,
    })
}

fn broken_view(root: &Path, id: &str, reason: &str, running_ids: &HashSet<String>) -> InstanceView {
    let dir = root.join(id);
    let game_dir = dir.join(".minecraft");
    InstanceView {
        meta: InstanceMeta {
            schema_version: 1,
            id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            mc_version: "?".to_string(),
            loader: Loader { r#type: "vanilla".to_string(), version: None },
            kind: "custom".to_string(),
            version_id: None,
            installed: false,
            created_at: default_created(),
            last_played_at: None,
            cover_color: 0,
            cover_file: None,
            settings: InstanceSettings::default(),
            imported_from: None,
            play_time_sec: None,
        },
        state: "broken".to_string(),
        state_reason: Some(reason.to_string()),
        path: dir_string(&dir),
        game_dir: dir_string(&game_dir),
        mod_count: 0,
        pack_count: 0,
        cover_data_url: None,
        running: running_ids.contains(id),
    }
}

pub fn list(
    data: &Path,
    settings: &Settings,
    running_ids: &HashSet<String>,
) -> AppResult<Vec<InstanceView>> {
    let root = root_of(data, settings);
    fs::create_dir_all(&root)?;
    let mut views = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if !is_instance_id(&id) || !entry.path().join("instance.json").is_file() {
            continue;
        }
        match read_meta(&entry.path()) {
            Ok(meta) => match to_view(data, settings, meta, running_ids) {
                Ok(view) => views.push(view),
                Err(e) => views.push(broken_view(&root, &id, &e.message, running_ids)),
            },
            Err(e) => views.push(broken_view(&root, &id, &e.message, running_ids)),
        }
    }
    views.sort_by(|a, b| {
        let key = |view: &InstanceView| {
            view.meta
                .last_played_at
                .clone()
                .unwrap_or_else(|| view.meta.created_at.clone())
        };
        key(b).cmp(&key(a))
    });
    Ok(views)
}

fn validate_instance_settings(settings: &InstanceSettings) -> AppResult<()> {
    for value in [settings.memory_min_mb, settings.memory_max_mb].into_iter().flatten() {
        if !(256..=65_536).contains(&value) {
            return Err(AppError::new("INSTANCE_INVALID", "Память: от 256 до 65536 МБ"));
        }
    }
    if let (Some(min), Some(max)) = (settings.memory_min_mb, settings.memory_max_mb) {
        if min > max {
            return Err(AppError::new("INSTANCE_INVALID", "Минимум памяти не может быть больше максимума"));
        }
    }
    if let Some(width) = settings.width {
        if !(320..=7680).contains(&width) {
            return Err(AppError::new("INSTANCE_INVALID", "Некорректная ширина окна"));
        }
    }
    if let Some(height) = settings.height {
        if !(240..=4320).contains(&height) {
            return Err(AppError::new("INSTANCE_INVALID", "Некорректная высота окна"));
        }
    }
    if settings.jvm_args.chars().count() > 2000 || settings.java_path.chars().count() > 1024 {
        return Err(AppError::new("INSTANCE_INVALID", "Слишком длинные настройки"));
    }
    Ok(())
}

fn text<'a>(value: &'a Value, key: &str) -> AppResult<&'a str> {
    value
        .as_str()
        .ok_or_else(|| AppError::new("INSTANCE_INVALID", format!("Некорректное поле {key}")))
}

fn parse_loader(value: &Value) -> AppResult<Loader> {
    let object = value
        .as_object()
        .ok_or_else(|| AppError::new("INSTANCE_INVALID", "Некорректный загрузчик"))?;
    let kind = object
        .get("type")
        .map(|v| text(v, "loader.type"))
        .transpose()?
        .unwrap_or("vanilla");
    if !["vanilla", "fabric", "quilt", "forge", "neoforge"].contains(&kind) {
        return Err(AppError::new("INSTANCE_INVALID", "Неизвестный загрузчик"));
    }
    let version = match object.get("version") {
        Some(Value::String(v)) if !v.is_empty() => Some(v.clone()),
        Some(Value::Null) | None => None,
        _ => return Err(AppError::new("INSTANCE_INVALID", "Некорректная версия загрузчика")),
    };
    if kind != "vanilla" && version.is_none() {
        return Err(AppError::new("INSTANCE_INVALID", "Выбери версию загрузчика"));
    }
    Ok(Loader { r#type: kind.to_string(), version })
}

fn unique_id(root: &Path, wanted: &str) -> String {
    let base = if wanted.is_empty() { "instance" } else { wanted };
    let mut id = base.to_string();
    let mut n = 2;
    while root.join(&id).exists() {
        let trimmed: String = base.chars().take(60).collect();
        id = format!("{trimmed}-{n}");
        n += 1;
    }
    id
}

pub fn create(
    data: &Path,
    settings: &Settings,
    running_ids: &HashSet<String>,
    input: &Value,
) -> AppResult<InstanceView> {
    let object = input
        .as_object()
        .ok_or_else(|| AppError::new("INSTANCE_INVALID", "Некорректные данные"))?;
    let name = text(object.get("name").unwrap_or(&Value::Null), "name")?.trim().to_string();
    if name.is_empty() || name.chars().count() > 48 {
        return Err(AppError::new("INSTANCE_INVALID", "Название: от 1 до 48 символов"));
    }
    let description = object
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if description.chars().count() > 120 {
        return Err(AppError::new("INSTANCE_INVALID", "Описание не длиннее 120 символов"));
    }
    let mc_version = text(object.get("mcVersion").unwrap_or(&Value::Null), "mcVersion")?.to_string();
    if !is_version_id(&mc_version) {
        return Err(AppError::new("INSTANCE_INVALID", "Некорректная версия Minecraft"));
    }
    let loader = parse_loader(object.get("loader").unwrap_or(&Value::Null))?;
    let kind = object
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("custom")
        .to_string();
    if !["official", "modded", "custom"].contains(&kind.as_str()) {
        return Err(AppError::new("INSTANCE_INVALID", "Неизвестный тип инстанса"));
    }
    let cover_color = object.get("coverColor").and_then(|v| v.as_u64()).unwrap_or(0);
    if cover_color > 7 {
        return Err(AppError::new("INSTANCE_INVALID", "Некорректный цвет обложки"));
    }
    let instance_settings: InstanceSettings = match object.get("settings") {
        Some(Value::Null) | None => InstanceSettings::default(),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| AppError::new("INSTANCE_INVALID", format!("Некорректные настройки инстанса: {e}")))?,
    };
    validate_instance_settings(&instance_settings)?;

    let root = root_of(data, settings);
    fs::create_dir_all(&root)?;

    let id = match object.get("id").and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()) {
        Some(wanted) => {
            if !is_instance_id(wanted) {
                return Err(AppError::new("INSTANCE_INVALID", "Некорректный ID инстанса"));
            }
            if root.join(wanted).exists() {
                return Err(AppError::new("INSTANCE_EXISTS", format!("Папка «{wanted}» уже существует")));
            }
            wanted.to_string()
        }
        None => unique_id(&root, &slugify(&name)),
    };

    let meta = InstanceMeta {
        schema_version: 1,
        id: id.clone(),
        name,
        description,
        mc_version,
        loader,
        kind,
        version_id: None,
        installed: false,
        created_at: now_iso(),
        last_played_at: None,
        cover_color: cover_color as u32,
        cover_file: None,
        settings: instance_settings,
        imported_from: None,
        play_time_sec: None,
    };

    let dir = resolve_inside(&root, &[&id])?;
    let game_dir = dir.join(".minecraft");
    for sub in ["mods", "config", "saves", "resourcepacks"] {
        fs::create_dir_all(game_dir.join(sub))?;
    }
    write_meta(&dir, &meta)?;
    to_view(data, settings, meta, running_ids)
}

pub fn update(
    data: &Path,
    settings: &Settings,
    running_ids: &HashSet<String>,
    input: &Value,
) -> AppResult<InstanceView> {
    let object = input
        .as_object()
        .ok_or_else(|| AppError::new("INSTANCE_INVALID", "Некорректные данные"))?;
    let id = text(object.get("id").unwrap_or(&Value::Null), "id")?;
    if !is_instance_id(id) {
        return Err(AppError::new("INSTANCE_INVALID", "Некорректный ID инстанса"));
    }
    let patch = object
        .get("patch")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AppError::new("INSTANCE_INVALID", "Некорректные данные"))?;
    let root = root_of(data, settings);
    let dir = resolve_inside(&root, &[id])?;
    let mut meta = read_meta(&dir)?;
    if let Some(value) = patch.get("name") {
        let name = text(value, "name")?.trim().to_string();
        if name.is_empty() || name.chars().count() > 48 {
            return Err(AppError::new("INSTANCE_INVALID", "Название: от 1 до 48 символов"));
        }
        meta.name = name;
    }
    if let Some(value) = patch.get("description") {
        let description = text(value, "description")?.trim().to_string();
        if description.chars().count() > 120 {
            return Err(AppError::new("INSTANCE_INVALID", "Описание не длиннее 120 символов"));
        }
        meta.description = description;
    }
    if let Some(value) = patch.get("kind") {
        let kind = text(value, "kind")?;
        if !["official", "modded", "custom"].contains(&kind) {
            return Err(AppError::new("INSTANCE_INVALID", "Неизвестный тип инстанса"));
        }
        meta.kind = kind.to_string();
    }
    if let Some(value) = patch.get("coverColor") {
        let color = value
            .as_u64()
            .ok_or_else(|| AppError::new("INSTANCE_INVALID", "Некорректный цвет обложки"))?;
        if color > 7 {
            return Err(AppError::new("INSTANCE_INVALID", "Некорректный цвет обложки"));
        }
        meta.cover_color = color as u32;
    }
    if let Some(value) = patch.get("settings") {
        let instance_settings: InstanceSettings = serde_json::from_value(value.clone())
            .map_err(|e| AppError::new("INSTANCE_INVALID", format!("Некорректные настройки инстанса: {e}")))?;
        validate_instance_settings(&instance_settings)?;
        meta.settings = instance_settings;
    }
    write_meta(&dir, &meta)?;
    to_view(data, settings, meta, running_ids)
}

fn copy_dir_all(from: &Path, to: &Path) -> AppResult<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

pub fn duplicate(
    data: &Path,
    settings: &Settings,
    running_ids: &HashSet<String>,
    input: &Value,
) -> AppResult<InstanceView> {
    let object = input
        .as_object()
        .ok_or_else(|| AppError::new("INSTANCE_INVALID", "Некорректные данные"))?;
    let id = text(object.get("id").unwrap_or(&Value::Null), "id")?;
    let name = text(object.get("name").unwrap_or(&Value::Null), "name")?.trim().to_string();
    if name.is_empty() || name.chars().count() > 48 {
        return Err(AppError::new("INSTANCE_INVALID", "Название: от 1 до 48 символов"));
    }
    let root = root_of(data, settings);
    let from = resolve_inside(&root, &[id])?;
    let source = read_meta(&from)?;
    let new_id = unique_id(&root, &slugify(&name));
    let to = resolve_inside(&root, &[&new_id])?;
    copy_dir_all(&from, &to)?;
    let mut meta = source;
    meta.id = new_id;
    meta.name = name;
    meta.created_at = now_iso();
    meta.last_played_at = None;
    meta.play_time_sec = Some(0);
    write_meta(&to, &meta)?;
    to_view(data, settings, meta, running_ids)
}

pub fn remove(data: &Path, settings: &Settings, input: &Value) -> AppResult<()> {
    let object = input
        .as_object()
        .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректные данные"))?;
    let id = text(object.get("id").unwrap_or(&Value::Null), "id")?;
    if object.get("confirm").and_then(|v| v.as_bool()) != Some(true) {
        return Err(AppError::new("CONFIRM_REQUIRED", "Удаление нужно подтвердить"));
    }
    let root = root_of(data, settings);
    let dir = resolve_inside(&root, &[id])?;
    if !dir.join("instance.json").is_file() {
        return Err(AppError::new("INSTANCE_NOT_FOUND", "Инстанс не найден"));
    }
    fs::remove_dir_all(&dir)?;
    Ok(())
}

pub fn worlds(data: &Path, settings: &Settings, input: &Value) -> AppResult<Vec<String>> {
    let object = input
        .as_object()
        .ok_or_else(|| AppError::new("INVALID_ARGS", "Некорректные данные"))?;
    let id = text(object.get("id").unwrap_or(&Value::Null), "id")?;
    let root = root_of(data, settings);
    let dir = resolve_inside(&root, &[id])?;
    let saves = dir.join(".minecraft").join("saves");
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(saves) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with('.') {
                    names.push(name);
                }
            }
        }
    }
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    Ok(names)
}

/// Needed by future install/launch code; also used to refresh the card after a game run.
pub fn add_play_time(data: &Path, settings: &Settings, id: &str, seconds: u64) -> AppResult<()> {
    if seconds == 0 {
        return Ok(());
    }
    let root = root_of(data, settings);
    let dir = resolve_inside(&root, &[id])?;
    let mut meta = read_meta(&dir)?;
    meta.play_time_sec = Some(meta.play_time_sec.unwrap_or(0).saturating_add(seconds.min(86_400)));
    write_meta(&dir, &meta)
}


/// Metadata of one instance for the launcher (game launch / install).
pub fn read_meta_at(data: &Path, settings: &Settings, id: &str) -> AppResult<InstanceMeta> {
    let root = root_of(data, settings);
    if !is_instance_id(id) {
        return Err(AppError::new("INSTANCE_ID", "Некорректный ID инстанса"));
    }
    let dir = resolve_inside(&root, &[id])?;
    read_meta(&dir)
}

/// The `.minecraft` folder of an instance.
pub fn game_dir_of(data: &Path, settings: &Settings, id: &str) -> AppResult<PathBuf> {
    Ok(resolve_inside(&root_of(data, settings), &[id])?.join(".minecraft"))
}

/// Folder that holds `instance.json`, `.minecraft`, `backups` and `.natives`.
pub fn instance_dir(data: &Path, settings: &Settings, id: &str) -> AppResult<PathBuf> {
    resolve_inside(&root_of(data, settings), &[id])
}

/// Marks an instance as installed after `installer` has finished.
pub fn patch_installed(data: &Path, settings: &Settings, id: &str, version_id: &str) -> AppResult<()> {
    let dir = resolve_inside(&root_of(data, settings), &[id])?;
    let mut meta = read_meta(&dir)?;
    meta.installed = true;
    meta.version_id = Some(version_id.to_string());
    write_meta(&dir, &meta)
}

/// Switches an existing instance to a new Minecraft version/loader after the new files are installed.
pub fn apply_version(
    data: &Path,
    settings: &Settings,
    id: &str,
    mc_version: &str,
    loader: Loader,
    kind: &str,
    version_id: &str,
) -> AppResult<()> {
    let dir = resolve_inside(&root_of(data, settings), &[id])?;
    let mut meta = read_meta(&dir)?;
    meta.mc_version = mc_version.to_string();
    meta.loader = loader;
    meta.kind = kind.to_string();
    meta.installed = true;
    meta.version_id = Some(version_id.to_string());
    write_meta(&dir, &meta)
}

/// Sets import metadata after copying files from another launcher.
pub fn patch_import_meta(
    data: &Path,
    settings: &Settings,
    id: &str,
    imported_from: &str,
    last_played_at: Option<&str>,
) -> AppResult<()> {
    let dir = resolve_inside(&root_of(data, settings), &[id])?;
    let mut meta = read_meta(&dir)?;
    meta.imported_from = Some(imported_from.to_string());
    if let Some(value) = last_played_at {
        meta.last_played_at = Some(value.to_string());
    }
    write_meta(&dir, &meta)
}

/// Copies an external image as this instance's cover.
pub fn set_cover_from_file(data: &Path, settings: &Settings, id: &str, source: &Path) -> AppResult<()> {
    let ext = source
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    if !["png", "jpg", "jpeg", "webp"].contains(&ext.as_str()) {
        return Err(AppError::new("COVER_INVALID", "Поддерживаются PNG, JPG и WebP"));
    }
    let size = fs::metadata(source)
        .map_err(|_| AppError::new("COVER_INVALID", "Не удалось прочитать обложку"))?
        .len();
    if size > MAX_COVER_BYTES {
        return Err(AppError::new("COVER_INVALID", "Обложка не больше 1 МБ"));
    }
    let dir = resolve_inside(&root_of(data, settings), &[id])?;
    let mut meta = read_meta(&dir)?;
    let name = format!("cover.{ext}");
    fs::copy(source, dir.join(&name))
        .map_err(|_| AppError::new("COVER_INVALID", "Не удалось скопировать обложку"))?;
    meta.cover_file = Some(name);
    write_meta(&dir, &meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(name: &str, id: Option<&str>) -> Value {
        let mut value = json!({
            "name": name,
            "description": "",
            "mcVersion": "1.21.1",
            "loader": { "type": "fabric", "version": "0.16.9" },
            "kind": "modded",
            "coverColor": 1,
            "settings": {
                "javaPath": "", "memoryMinMb": null, "memoryMaxMb": null,
                "jvmArgs": "", "width": null, "height": null, "fullscreen": null
            }
        });
        if let Some(id) = id {
            value["id"] = json!(id);
        }
        value
    }

    #[test]
    fn create_list_update_duplicate_remove() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::default();
        let created = create(dir.path(), &settings, &input("My Pack", None)).unwrap();
        assert_eq!(created.meta.name, "My Pack");
        assert_eq!(created.state, "not-installed");
        assert!(created.game_dir.ends_with(".minecraft"));
        assert!(Path::new(&created.game_dir).join("mods").is_dir());

        let listed = list(dir.path(), &settings).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].mod_count, 0);

        let updated = update(
            dir.path(),
            &settings,
            &json!({ "id": created.meta.id, "patch": { "name": "Renamed" } }),
        )
        .unwrap();
        assert_eq!(updated.meta.name, "Renamed");

        let duplicated = duplicate(
            dir.path(),
            &settings,
            &json!({ "id": created.meta.id, "name": "Copy" }),
        )
        .unwrap();
        assert_ne!(duplicated.meta.id, created.meta.id);
        assert_eq!(list(dir.path(), &settings).unwrap().len(), 2);

        remove(
            dir.path(),
            &settings,
            &json!({ "id": created.meta.id, "confirm": true }),
        )
        .unwrap();
        assert_eq!(list(dir.path(), &settings).unwrap().len(), 1);
    }

    #[test]
    fn hostile_input_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::default();
        assert!(create(dir.path(), &settings, &input("x", Some("../evil"))).is_err());
        assert!(create(
            dir.path(),
            &settings,
            &json!({ "name": "", "mcVersion": "1.21.1", "loader": { "type": "vanilla", "version": null }, "kind": "custom", "settings": {} })
        )
        .is_err());
        assert!(create(
            dir.path(),
            &settings,
            &json!({ "name": "x", "mcVersion": "1.21.1", "loader": { "type": "fabric", "version": null }, "kind": "custom", "settings": {} })
        )
        .is_err());
    }

    #[test]
    fn worlds_lists_saves() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::default();
        let created = create(dir.path(), &settings, &input("Pack", None)).unwrap();
        let saves = Path::new(&created.game_dir).join("saves");
        fs::create_dir_all(saves.join("My World")).unwrap();
        fs::create_dir_all(saves.join(".hidden")).unwrap();
        let worlds = worlds(dir.path(), &settings, &json!({ "id": created.meta.id })).unwrap();
        assert_eq!(worlds, vec!["My World".to_string()]);
    }
}
