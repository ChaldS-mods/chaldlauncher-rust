//! `.chaldlauncher` пакеты: инспекция, импорт и экспорт. Порт `src/main/chaldpack.ts`.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;
use zip::{write::FileOptions, ZipArchive, ZipWriter};

use crate::download::{download_file, DownloadSpec};
use crate::errors::{AppError, AppResult};
use crate::game;
use crate::instances;
use crate::paths;
use crate::state::AppState;

#[allow(dead_code)]
pub const PACK_EXT: &str = ".chaldlauncher";
pub const MANIFEST: &str = "chaldlauncher.json";
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PACK_OVERRIDES_BYTES: u64 = 2048 * 1024 * 1024;
const MAX_PACK_FILES: usize = 20_000;
const MAX_OVERRIDE_ENTRY: u64 = 256 * 1024 * 1024;
const MAX_OVERRIDE_TOTAL: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackLoader {
    #[serde(rename = "type")]
    pub r#type: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackFile {
    pub path: String,
    pub sha1: String,
    #[serde(default)]
    pub size: Option<u64>,
    pub url: String,
    pub project_id: String,
    pub version_id: String,
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String {
    "modrinth".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChaldPack {
    pub format: String,
    pub format_version: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: PackLoader,
    pub kind: String,
    #[serde(default, rename = "coverColor")]
    pub cover_color: u32,
    pub files: Vec<PackFile>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackSummary {
    pub name: String,
    pub description: String,
    #[serde(rename = "mcVersion")]
    pub mc_version: String,
    pub loader: PackLoader,
    pub downloads: usize,
    pub override_files: usize,
    pub override_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub downloads: usize,
    pub identified: usize,
    pub bundled_files: usize,
    pub bundled_bytes: u64,
    pub notices: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PendingChaldPack {
    pub file: PathBuf,
    pub manifest: ChaldPack,
}

fn clean_text(text: &str, max: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

fn valid_pack_path(raw: &str) -> AppResult<String> {
    if raw.is_empty() || raw.len() > 300 || raw.contains('\\') || raw.contains('\0') {
        return Err(AppError::new("PACK_INVALID", "Некорректный путь в архиве"));
    }
    let normalized = raw.replace('\\', "/");
    if normalized.starts_with('/') || normalized.contains("..") || normalized.contains("//") {
        return Err(AppError::new("PACK_INVALID", "Некорректный путь в архиве"));
    }
    for part in normalized.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.chars().any(|c| c.is_control()) {
            return Err(AppError::new("PACK_INVALID", "Некорректный путь в архиве"));
        }
    }
    Ok(normalized)
}

fn validate_manifest(manifest: &ChaldPack) -> AppResult<()> {
    if manifest.format != "chaldlauncher-instance" || manifest.format_version != 1 {
        return Err(AppError::new("PACK_INVALID", "Неверный формат файла"));
    }
    if manifest.name.trim().is_empty() || manifest.name.chars().count() > 48 {
        return Err(AppError::new("PACK_INVALID", "Некорректное имя в файле"));
    }
    if !paths::is_version_id(&manifest.mc_version) {
        return Err(AppError::new("PACK_INVALID", "Некорректная версия Minecraft"));
    }
    if !["vanilla", "fabric", "quilt", "forge", "neoforge"].contains(&manifest.loader.r#type.as_str()) {
        return Err(AppError::new("PACK_INVALID", "Неизвестный загрузчик"));
    }
    if manifest.loader.r#type != "vanilla" && manifest.loader.version.as_deref().unwrap_or("").is_empty() {
        return Err(AppError::new("PACK_INVALID", "Не указана версия загрузчика"));
    }
    if !["official", "modded", "custom"].contains(&manifest.kind.as_str()) {
        return Err(AppError::new("PACK_INVALID", "Неизвестный тип инстанса"));
    }
    if manifest.cover_color > 7 || manifest.files.len() > 5000 {
        return Err(AppError::new("PACK_INVALID", "Файл повреждён или имеет неверный формат"));
    }
    for file in &manifest.files {
        let rel = valid_pack_path(&file.path)?;
        if !rel.starts_with("mods/") && !rel.starts_with("resourcepacks/") {
            return Err(AppError::new("PACK_INVALID", "Файлы лежат только в mods/ и resourcepacks/"));
        }
        if file.sha1.len() != 40 || !file.sha1.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(AppError::new("PACK_INVALID", "Некорректный хеш файла"));
        }
        let Ok(url) = reqwest::Url::parse(&file.url) else {
            return Err(AppError::new("PACK_INVALID", "Некорректная ссылка на файл"));
        };
        let host_ok = matches!(
            url.host_str(),
            Some("cdn.modrinth.com")
                | Some("edge.forgecdn.net")
                | Some("media.forgecdn.net")
                | Some("mediafilez.forgecdn.net")
        );
        if url.scheme() != "https" || !host_ok {
            return Err(AppError::new(
                "PACK_INVALID",
                "Ссылка на файл должна вести на CDN Modrinth или CurseForge",
            ));
        }
    }
    Ok(())
}

pub fn inspect_pack(file: &Path) -> AppResult<(ChaldPack, PackSummary)> {
    let f = fs::File::open(file)
        .map_err(|_| AppError::new("PACK_INVALID", "Не удалось открыть файл"))?;
    let mut zip = ZipArchive::new(f)
        .map_err(|_| AppError::new("PACK_INVALID", "Файл повреждён или это не архив .chaldlauncher"))?;
    let mut manifest: ChaldPack = {
        let mut entry = zip
            .by_name(MANIFEST)
            .map_err(|_| AppError::new("PACK_INVALID", &format!("В файле нет {MANIFEST}")))?;
        if entry.size() > MAX_MANIFEST_BYTES {
            return Err(AppError::new("PACK_INVALID", &format!("В файле нет {MANIFEST}")));
        }
        let mut buf = String::new();
        entry
            .read_to_string(&mut buf)
            .map_err(|_| AppError::new("PACK_INVALID", &format!("{MANIFEST} не читается")))?;
        serde_json::from_str(&buf)
            .map_err(|e| AppError::new("PACK_INVALID", &format!("Неверный формат файла: {e}")))?
    };
    manifest.name = clean_text(&manifest.name, 48);
    manifest.description = clean_text(&manifest.description, 120);
    validate_manifest(&manifest)?;

    let mut dl = HashSet::new();
    for item in &manifest.files {
        let rel = valid_pack_path(&item.path)?;
        if !dl.insert(rel.clone()) {
            return Err(AppError::new("PACK_INVALID", &format!("Файл указан дважды: {rel}")));
        }
    }
    let mut count = 0usize;
    let mut total = 0u64;
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|_| AppError::new("PACK_INVALID", "Файл повреждён"))?;
        if entry.is_dir() || entry.name() == MANIFEST {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        if !name.starts_with("overrides/") {
            return Err(AppError::new(
                "PACK_INVALID",
                &format!("Лишний файл в архиве: {}", &name.chars().take(80).collect::<String>()),
            ));
        }
        let rel = valid_pack_path(name.strip_prefix("overrides/").unwrap_or(""))?;
        if dl.contains(&rel) {
            return Err(AppError::new(
                "PACK_INVALID",
                &format!("Файл есть и в списке загрузки, и в архиве: {rel}"),
            ));
        }
        total = total.saturating_add(entry.size());
        count += 1;
        if count > MAX_PACK_FILES {
            return Err(AppError::new("PACK_TOO_BIG", "В архиве слишком много файлов"));
        }
        if total > MAX_PACK_OVERRIDES_BYTES {
            return Err(AppError::new(
                "PACK_TOO_BIG",
                "Архив распаковывается в слишком большой объём",
            ));
        }
    }

    let summary = PackSummary {
        name: manifest.name.clone(),
        description: manifest.description.clone(),
        mc_version: manifest.mc_version.clone(),
        loader: manifest.loader.clone(),
        downloads: manifest.files.len(),
        override_files: count,
        override_bytes: total,
    };
    Ok((manifest, summary))
}

fn emit_pack(app: &AppHandle, op_id: &str, instance_id: Option<&str>, label: &str, fraction: Option<f64>, status: &str, message: Option<String>) {
    game::emit(
        app,
        "op:progress",
        json!({
            "opId": op_id,
            "kind": "modpack",
            "instanceId": instance_id,
            "label": label,
            "fraction": fraction,
            "status": status,
            "message": message,
        }),
    );
}

fn write_index_files(game_dir: &Path, manifest: &ChaldPack) -> AppResult<()> {
    let mut mr_projects = serde_json::Map::new();
    let mut cf_projects = serde_json::Map::new();
    for file in &manifest.files {
        let base = Path::new(&file.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&file.path)
            .trim_end_matches(".disabled")
            .to_string();
        if file.source == "curseforge" {
            if let Ok(file_id) = file.version_id.parse::<i64>() {
                cf_projects.insert(file.project_id.clone(), json!({ "file": base, "fileId": file_id }));
            }
        } else {
            mr_projects.insert(file.project_id.clone(), json!({ "file": base, "versionId": file.version_id }));
        }
    }
    if !mr_projects.is_empty() {
        let path = game_dir.join(".chald-modrinth.json");
        fs::write(&path, serde_json::to_vec_pretty(&json!({ "projects": mr_projects }))?)?;
    }
    if !cf_projects.is_empty() {
        let path = game_dir.join(".chald-curseforge.json");
        fs::write(&path, serde_json::to_vec_pretty(&json!({ "projects": cf_projects }))?)?;
    }
    Ok(())
}

fn extract_overrides(pack_file: &Path, game_dir: &Path, cancel: &AtomicBool) -> AppResult<()> {
    let f = fs::File::open(pack_file)?;
    let mut zip = ZipArchive::new(f).map_err(|_| AppError::new("PACK_INVALID", "Файл сборки повреждён"))?;
    let mut total = 0u64;
    for prefix in ["overrides/", "client-overrides/"] {
        for i in 0..zip.len() {
            if cancel.load(Ordering::SeqCst) {
                return Err(AppError::new("CANCELLED", "Импорт отменён"));
            }
            let mut entry = zip.by_index(i).map_err(|_| AppError::new("PACK_INVALID", "Файл повреждён"))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().replace('\\', "/");
            if !name.starts_with(prefix) {
                continue;
            }
            let rel = valid_pack_path(name.strip_prefix(prefix).unwrap_or(""))?;
            if entry.size() > MAX_OVERRIDE_ENTRY {
                return Err(AppError::new("PACK_TOO_BIG", &format!("Файл слишком большой: {rel}")));
            }
            total = total.saturating_add(entry.size());
            if total > MAX_OVERRIDE_TOTAL {
                return Err(AppError::new("PACK_TOO_BIG", "Сборка распаковывается в слишком большой объём"));
            }
            let parts: Vec<&str> = rel.split('/').collect();
            let refs: Vec<&str> = parts.iter().map(|s| *s).collect();
            let dest = paths::resolve_inside(game_dir, &refs)?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = fs::File::create(&dest)?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}

pub async fn run_import(
    app: &AppHandle,
    state: &AppState,
    token: &str,
    pending: PendingChaldPack,
    cancel: Arc<AtomicBool>,
) -> AppResult<Option<String>> {
    let op_id = format!("modpack:chald:{token}");
    emit_pack(app, &op_id, None, &format!("Импорт «{}»…", pending.manifest.name), None, "running", None);
    let mut instance_id: Option<String> = None;

    let result: AppResult<String> = crate::download::with_tracker(app, &op_id, "modpack", None, async {
        let (manifest, _) = inspect_pack(&pending.file)?;
        let settings = state.settings.get()?;
        let running: std::collections::HashSet<String> = state.games.running_ids().into_iter().collect();
        let input = json!({
            "name": manifest.name,
            "description": manifest.description,
            "mcVersion": manifest.mc_version,
            "loader": { "type": manifest.loader.r#type, "version": manifest.loader.version },
            "kind": manifest.kind,
            "coverColor": manifest.cover_color,
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

        crate::download::expect_files(manifest.files.len() as u64, manifest.files.iter().map(|f| f.size.unwrap_or(0)).sum());
        let total = manifest.files.len().max(1);
        let mut done = 0usize;
        for file in &manifest.files {
            if cancel.load(Ordering::SeqCst) {
                return Err(AppError::new("CANCELLED", "Импорт отменён"));
            }
            let rel = valid_pack_path(&file.path)?;
            let parts: Vec<&str> = rel.split('/').collect();
            let refs: Vec<&str> = parts.iter().map(|s| *s).collect();
            let dest = paths::resolve_inside(&game_dir, &refs)?;
            let spec = DownloadSpec {
                url: file.url.clone(),
                dest,
                sha1: Some(file.sha1.to_ascii_lowercase()),
                sha256: None,
                size: file.size,
                purpose: "Файл сборки".to_string(),
            };
            download_file(&spec, |_| {}).await?;
            done += 1;
            emit_pack(
                app,
                &op_id,
                instance_id.as_deref(),
                &format!("Файлы сборки: {done} из {total}"),
                Some(0.85 * (done as f64 / total as f64)),
                "running",
                None,
            );
        }

        emit_pack(app, &op_id, instance_id.as_deref(), "Копирую файлы из архива…", Some(0.9), "running", None);
        extract_overrides(&pending.file, &game_dir, cancel.as_ref())?;
        write_index_files(&game_dir, &manifest)?;

        emit_pack(
            app,
            &op_id,
            instance_id.as_deref(),
            &format!("Инстанс «{}» импортирован. Устанавливаю Minecraft…", manifest.name),
            Some(1.0),
            "done",
            None,
        );
        Ok(created.meta.id)
    })
    .await;

    match result {
        Ok(id) => Ok(Some(id)),
        Err(error) => {
            if let Some(id) = instance_id.take() {
                let settings = state.settings.get().ok();
                if let Some(settings) = settings {
                    let _ = instances::remove(&state.data_dir, &settings, &json!({ "id": id, "confirm": true }));
                }
            }
            if error.code == "CANCELLED" || cancel.load(Ordering::SeqCst) {
                emit_pack(app, &op_id, None, "Импорт отменён", None, "cancelled", None);
                Ok(None)
            } else {
                emit_pack(app, &op_id, None, &format!("Не удалось импортировать «{}»", pending.manifest.name), None, "error", Some(error.message.clone()));
                Err(error)
            }
        }
    }
}

fn should_skip_export(name: &str, is_dir: bool, top: bool) -> bool {
    if top {
        if is_dir {
            return matches!(name, "logs" | "crash-reports" | "screenshots" | ".fabric" | ".mixin.out" | "webcache" | "webcache2" | "downloads" | "debug" | ".cache");
        }
        if matches!(name, ".chald-modrinth.json" | ".chald-curseforge.json") {
            return true;
        }
    }
    name.ends_with(".part") || name.starts_with("hs_err_pid")
}

fn walk_files(root: &Path, rel: &Path, out: &mut Vec<(String, PathBuf, u64)>, top: bool, include_worlds: bool) -> AppResult<()> {
    let dir = if rel.as_os_str().is_empty() { root.to_path_buf() } else { root.join(rel) };
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else { continue };
        if should_skip_export(name, file_type.is_dir(), top) {
            continue;
        }
        let child_rel = if rel.as_os_str().is_empty() { PathBuf::from(name) } else { rel.join(name) };
        if file_type.is_dir() {
            if top && name == "saves" && !include_worlds {
                continue;
            }
            walk_files(root, &child_rel, out, false, include_worlds)?;
        } else if file_type.is_file() {
            let abs = root.join(&child_rel);
            let size = fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
            let rel_str = child_rel.to_string_lossy().replace('\\', "/");
            out.push((rel_str, abs, size));
        }
    }
    Ok(())
}

pub async fn export_instance(
    app: &AppHandle,
    state: &AppState,
    id: &str,
    include_worlds: bool,
    out: &Path,
) -> AppResult<(ExportResult, String)> {
    let op_id = format!("export:{id}");
    let emit = |label: &str, fraction: Option<f64>, status: &str| {
        game::emit(app, "op:progress", json!({
            "opId": op_id,
            "kind": "export",
            "instanceId": Value::Null,
            "label": label,
            "fraction": fraction,
            "status": status,
            "message": Value::Null,
        }));
    };
    emit("Подготовка…", Some(0.0), "running");
    let settings = state.settings.get()?;
    if state.installs.lock().unwrap().contains_key(id) {
        return Err(AppError::new("BUSY", "Идёт установка инстанса: подожди её окончания"));
    }
    if include_worlds && state.games.running_ids().iter().any(|running| running == id) {
        return Err(AppError::new("BUSY", "Останови игру, чтобы экспортировать миры"));
    }
    let meta = instances::read_meta_at(&state.data_dir, &settings, id)?;
    let game_dir = instances::game_dir_of(&state.data_dir, &settings, id)?;

    emit("Собираю файлы…", Some(0.2), "running");
    let mut files = Vec::new();
    walk_files(&game_dir, Path::new(""), &mut files, true, include_worlds)?;
    let bundled_bytes: u64 = files.iter().map(|(_, _, size)| *size).sum();
    if files.len() > MAX_PACK_FILES {
        return Err(AppError::new(
            "PACK_TOO_BIG",
            &format!("Слишком много файлов для архива ({}). Не включай миры или удали лишнее.", files.len()),
        ));
    }
    if bundled_bytes > MAX_PACK_OVERRIDES_BYTES {
        let mb = bundled_bytes / 1024 / 1024;
        let hint = if include_worlds { " Попробуй экспорт без миров." } else { "" };
        return Err(AppError::new(
            "PACK_TOO_BIG",
            &format!("Файлы весят {mb} МБ (предел 2048 МБ).{hint}"),
        ));
    }

    let manifest = json!({
        "format": "chaldlauncher-instance",
        "formatVersion": 1,
        "name": meta.name,
        "description": meta.description,
        "mcVersion": meta.mc_version,
        "loader": { "type": meta.loader.r#type, "version": meta.loader.version },
        "kind": meta.kind,
        "coverColor": meta.cover_color,
        "files": [],
    });

    let part = out.with_extension("chaldlauncher.part");
    let result = (|| -> AppResult<()> {
        let file = fs::File::create(&part)?;
        let mut zip = ZipWriter::new(file);
        let options = FileOptions::<()>::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(MANIFEST, options)
            .map_err(|e| AppError::new("EXPORT_FAILED", e.to_string()))?;
        zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())
            .map_err(|e| AppError::new("EXPORT_FAILED", e.to_string()))?;
        let total = files.len().max(1);
        for (i, (rel, abs, _)) in files.iter().enumerate() {
            let entry_name = format!("overrides/{rel}");
            zip.start_file(entry_name, options)
                .map_err(|e| AppError::new("EXPORT_FAILED", e.to_string()))?;
            let mut src = fs::File::open(abs)?;
            std::io::copy(&mut src, &mut zip)
                .map_err(|e| AppError::new("EXPORT_FAILED", e.to_string()))?;
            if i % 25 == 0 {
                let frac = 0.55 + (i as f64 / total as f64) * 0.4;
                emit(&format!("Упаковываю файлы: {i} из {total}"), Some(frac.min(0.97)), "running");
            }
        }
        emit("Записываю файл…", Some(0.97), "running");
        zip.finish().map_err(|e| AppError::new("EXPORT_FAILED", e.to_string()))?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            fs::rename(&part, out).map_err(|e| {
                let _ = fs::remove_file(&part);
                AppError::new("EXPORT_FAILED", e.to_string())
            })?;
            emit("Экспорт готов", Some(1.0), "done");
            Ok((
                ExportResult {
                    downloads: 0,
                    identified: 0,
                    bundled_files: files.len(),
                    bundled_bytes,
                    notices: Vec::new(),
                },
                out.to_string_lossy().to_string(),
            ))
        }
        Err(error) => {
            let _ = fs::remove_file(&part);
            emit("Экспорт не удался", None, "cancelled");
            Err(error)
        }
    }
}
