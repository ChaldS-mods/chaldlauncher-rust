//! Работа с .zip, брошенными на страницу модов. Порт `src/main/archiveImport.ts`.
//!
//! Архив извне считается враждебным: каждое имя проверяется, ничего не
//! перезаписывается, а при ошибке удаляется уже записанное.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rand::RngCore;
use serde_json::{json, Value};
use zip::ZipArchive;

use crate::errors::{AppError, AppResult};
use crate::paths::{resolve_inside, safe_file_name};

const MAX_ZIP_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

const GAME_FOLDERS: &[&str] = &[
    "mods", "resourcepacks", "shaderpacks", "config", "saves", "datapacks",
    "defaultconfigs", "kubejs", "scripts", "screenshots",
];
const KNOWN_WRAPPERS: &[&str] = &[".minecraft", "minecraft", "overrides"];
const ROOT_FILES: &[&str] = &["options.txt", "optionsof.txt", "optionsshaders.txt", "servers.dat"];
const JUNK_NAMES: &[&str] = &[".ds_store", "thumbs.db", "desktop.ini"];
const COPY_EXCLUDE: &[&str] = &[
    "logs", "crash-reports", ".fabric", ".mixin.out", "webcache", "webcache2", "cache", ".cache",
    "natives", "bin", "versions", "libraries", "assets", "runtime", "jarmods", "patches",
    "usercache.json", "usernamecache.json", "realms_persistence.json", "treatmenttags.json",
    "launcher_profiles.json", "launcher_accounts.json", "launcher_accounts_microsoft_store.json",
    "launcher_msa_credentials.bin", "launcher_log.txt", "launcher_settings.json",
    "launcher_ui_state.json", "launcher_gamer_pic.png", "launcher_entitlements.json",
    "launcher_cef_log.txt", "instance.cfg", "mmc-pack.json", "profile.json",
    "minecraftinstance.json", "manifest.json", "modlist.html", "profileimage.png",
    ".chald-modrinth.json", ".chald-curseforge.json", "app.db", "app.db-wal", "app.db-shm",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Category {
    Mod,
    Other,
    Ignored,
}

#[derive(Clone)]
struct Item {
    index: usize,
    dest: Vec<String>,
    category: Category,
    size: u64,
}

pub struct ArchivePlan {
    kind: &'static str,
    items: Vec<Item>,
    mods: u64,
    mod_bytes: u64,
    groups: Vec<(String, u64, u64)>,
    ignored: u64,
}

fn is_jar(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".jar") || lower.ends_with(".jar.disabled")
}

/// Safe relative path with forward slashes, or `None` for anything that escapes.
fn clean_entry_name(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    let normalized = raw.replace('\\', "/");
    if normalized.starts_with('/') {
        return None;
    }
    if let Some(drive) = normalized.get(..2) {
        if drive.ends_with(':') && drive.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            return None;
        }
    }
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        parts.push(part.to_string());
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn canonical_folder(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let alias = match lower.as_str() {
        "mod" | "mods" => Some("mods"),
        "resourcepack" | "resourcepacks" | "resource_packs" | "resource-packs" | "texturepacks" => {
            Some("resourcepacks")
        }
        "shaders" | "shader" | "shaderpack" => Some("shaderpacks"),
        _ => None,
    };
    alias
        .map(str::to_string)
        .unwrap_or_else(|| if GAME_FOLDERS.contains(&lower.as_str()) { lower } else { name.to_string() })
}

fn looks_like_instance_content(rels: &[Vec<String>]) -> bool {
    if rels.iter().any(|r| {
        if r.len() == 1 {
            is_jar(&r[0])
        } else {
            GAME_FOLDERS.contains(&canonical_folder(&r[0]).as_str())
        }
    }) {
        return true;
    }
    let Some(first) = rels.first().and_then(|r| r.first()) else {
        return false;
    };
    KNOWN_WRAPPERS.contains(&first.to_ascii_lowercase().as_str())
        && rels.iter().all(|r| r.len() >= 2 && r[0] == *first)
}

/// Removes wrapper folders: `sborka/mods/x.jar` -> `mods/x.jar`. At most two levels.
fn strip_wrappers(rels: &[Vec<String>]) -> Vec<Vec<String>> {
    let mut current = rels.to_vec();
    for _ in 0..2 {
        if current.is_empty() || current.iter().any(|r| r.len() < 2) {
            break;
        }
        let Some(first) = current.first().and_then(|r| r.first()).cloned() else {
            break;
        };
        if !current.iter().all(|r| r[0] == first) {
            break;
        }
        let inner: Vec<Vec<String>> = current.iter().map(|r| r[1..].to_vec()).collect();
        let known = KNOWN_WRAPPERS.contains(&first.to_ascii_lowercase().as_str());
        let canonical = canonical_folder(&first);
        if GAME_FOLDERS.contains(&canonical.as_str()) && canonical == first.to_ascii_lowercase() {
            break;
        }
        if !known && !looks_like_instance_content(&inner) {
            break;
        }
        current = inner;
    }
    current
}

fn manifest_type(archive: &mut ZipArchive<File>, want: &str) -> Option<String> {
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).ok()?;
        if entry.name() != want || entry.size() > 1024 * 1024 {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        return value.get("manifestType").and_then(Value::as_str).map(str::to_string);
    }
    None
}

pub fn plan_archive(file: &Path) -> AppResult<ArchivePlan> {
    let meta = std::fs::metadata(file)?;
    if !meta.is_file() {
        return Err(AppError::new("CONTENT_TYPE", format!("Не файл: {}", file.display())));
    }
    if meta.len() > MAX_ZIP_BYTES {
        return Err(AppError::new("ARCHIVE_TOO_BIG", "Архив больше 2 ГБ: распакуй его и добавь файлы по частям."));
    }

    let opened = File::open(file)?;
    let mut archive = ZipArchive::new(opened)
        .map_err(|_| AppError::new("ARCHIVE_INVALID", "Не удалось открыть архив. Нужен обычный .zip."))?;
    if archive.len() > MAX_ENTRIES {
        return Err(AppError::new("ARCHIVE_TOO_BIG", "В архиве слишком много файлов."));
    }

    let mut raw: Vec<(usize, Vec<String>, u64)> = Vec::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|e| AppError::new("ARCHIVE_INVALID", e.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let Some(name) = clean_entry_name(entry.name()) else { continue };
        let parts: Vec<String> = name.split('/').map(str::to_string).collect();
        let last_lower = parts.last().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
        if parts[0].eq_ignore_ascii_case("__macosx") || JUNK_NAMES.contains(&last_lower.as_str()) {
            continue;
        }
        raw.push((index, parts, entry.size()));
    }

    let rels: Vec<Vec<String>> = raw.iter().map(|(_, parts, _)| parts.clone()).collect();
    let stripped = strip_wrappers(&rels);
    let root_at = |want: &str| -> Option<usize> {
        stripped.iter().position(|r| r.len() == 1 && r[0].eq_ignore_ascii_case(want))
    };
    let modpack = root_at("modrinth.index.json").is_some()
        || root_at("manifest.json").is_some_and(|n| {
            let name = archive
                .by_index(raw[n].0)
                .map(|entry| entry.name().to_string())
                .unwrap_or_default();
            manifest_type(&mut archive, &name).as_deref() == Some("minecraftModpack")
        });
    if modpack {
        return Ok(ArchivePlan {
            kind: "modpack",
            items: Vec::new(),
            mods: 0,
            mod_bytes: 0,
            groups: Vec::new(),
            ignored: 0,
        });
    }

    let mut items = Vec::new();
    let mut groups: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut mods = 0u64;
    let mut mod_bytes = 0u64;
    let mut ignored = 0u64;
    let mut total = 0u64;

    for (n, (index, _, size)) in raw.iter().enumerate() {
        let segs = &stripped[n];
        total += *size;
        let mut add = |category: Category, dest: Vec<String>| {
            if category == Category::Mod {
                mods += 1;
                mod_bytes += *size;
            } else if category == Category::Ignored {
                ignored += 1;
            } else {
                let dir = if dest.len() == 1 { String::new() } else { dest[0].clone() };
                let entry = groups.entry(dir).or_insert((0, 0));
                entry.0 += 1;
                entry.1 += *size;
            }
            items.push(Item { index: *index, dest, category, size: *size });
        };

        let Some(last) = segs.last() else { continue };
        if segs.len() == 1 {
            if is_jar(last) {
                add(Category::Mod, vec!["mods".into(), last.clone()]);
            } else if ROOT_FILES.contains(&last.to_ascii_lowercase().as_str()) {
                add(Category::Other, vec![last.clone()]);
            } else {
                add(Category::Ignored, segs.clone());
            }
            continue;
        }
        let top = &segs[0];
        let folder = canonical_folder(top);
        if top.starts_with('.') || COPY_EXCLUDE.contains(&top.to_ascii_lowercase().as_str()) {
            add(Category::Ignored, segs.clone());
        } else if folder == "mods" {
            if is_jar(last) {
                let mut dest = vec!["mods".to_string()];
                dest.extend_from_slice(&segs[1..]);
                add(Category::Mod, dest);
            } else {
                add(Category::Ignored, segs.clone());
            }
        } else {
            let mut dest = vec![folder];
            dest.extend_from_slice(&segs[1..]);
            add(Category::Other, dest);
        }
    }
    if total > MAX_TOTAL_BYTES {
        return Err(AppError::new("ARCHIVE_TOO_BIG", "Архив распаковывается в слишком большой объём."));
    }

    Ok(ArchivePlan {
        kind: if mods == 0 && groups.is_empty() {
            "empty"
        } else if groups.is_empty() {
            "mods"
        } else {
            "mixed"
        },
        items,
        mods,
        mod_bytes,
        groups: groups
            .into_iter()
            .map(|(dir, (files, bytes))| (dir, files, bytes))
            .collect(),
        ignored,
    })
}

pub fn analysis_json(token: &str, name: &str, plan: &ArchivePlan) -> Value {
    json!({
        "token": token,
        "name": name,
        "mods": plan.mods,
        "modBytes": plan.mod_bytes,
        "groups": plan.groups.iter().map(|(dir, files, bytes)| json!({
            "dir": dir, "files": files, "bytes": bytes,
        })).collect::<Vec<_>>(),
        "ignored": plan.ignored,
    })
}

/// Writes the planned files; existing files are skipped, never overwritten.
pub fn apply_archive(file: &Path, game_dir: &Path, mode: &str) -> AppResult<Value> {
    let plan = plan_archive(file)?;
    if matches!(plan.kind, "modpack" | "empty") {
        return Err(AppError::new("ARCHIVE_CHANGED", "Содержимое архива изменилось. Добавь его заново."));
    }
    let opened = File::open(file)?;
    let mut archive = ZipArchive::new(opened)
        .map_err(|e| AppError::new("ARCHIVE_INVALID", e.to_string()))?;

    let mut result = json!({ "mods": 0u64, "other": 0u64, "skipped": 0u64 });
    let mut created: Vec<PathBuf> = Vec::new();
    let mut total = 0u64;

    let outcome = (|| -> AppResult<()> {
        for item in &plan.items {
            if item.category == Category::Ignored || (item.category == Category::Other && mode != "all") {
                continue;
            }
            if item.size > MAX_ENTRY_BYTES {
                return Err(AppError::new("ARCHIVE_TOO_BIG", "Файл в архиве слишком большой."));
            }
            total += item.size;
            if total > MAX_TOTAL_BYTES {
                return Err(AppError::new("ARCHIVE_TOO_BIG", "Архив распаковывается в слишком большой объём."));
            }
            let names: Vec<String> = item
                .dest
                .iter()
                .enumerate()
                .map(|(index, part)| {
                    if index + 1 == item.dest.len() {
                        safe_file_name(part)
                    } else {
                        Ok(part.clone())
                    }
                })
                .collect::<AppResult<_>>()?;
            let parts: Vec<&str> = names.iter().map(String::as_str).collect();
            let dest = resolve_inside(game_dir, &parts)?;
            if dest.exists() {
                result["skipped"] = (result["skipped"].as_u64().unwrap_or(0) + 1).into();
                continue;
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut entry = archive
                .by_index(item.index)
                .map_err(|error| AppError::new("ARCHIVE_CORRUPT", error.to_string()))?;
            let mut bytes = Vec::with_capacity(item.size as usize);
            entry.read_to_end(&mut bytes)?;
            if bytes.len() as u64 != item.size {
                return Err(AppError::new("ARCHIVE_CORRUPT", "Архив повреждён."));
            }
            std::fs::write(&dest, &bytes)?;
            created.push(dest);
            if item.category == Category::Mod {
                result["mods"] = (result["mods"].as_u64().unwrap_or(0) + 1).into();
            } else {
                result["other"] = (result["other"].as_u64().unwrap_or(0) + 1).into();
            }
        }
        Ok(())
    })();

    if outcome.is_err() {
        for path in created {
            let _ = std::fs::remove_file(path);
        }
    }
    outcome?;
    Ok(result)
}

/// Loose .jar files: duplicates are skipped, not an error.
pub fn import_jars(game_dir: &Path, sources: &[String]) -> AppResult<(u64, u64)> {
    let dir = game_dir.join("mods");
    std::fs::create_dir_all(&dir)?;
    let mut imported = 0u64;
    let mut skipped = 0u64;
    for source in sources {
        let path = Path::new(source);
        if !path.is_absolute() {
            return Err(AppError::new("PATH_INVALID", format!("Не удалось определить путь: {source}")));
        }
        let name = safe_file_name(&path.file_name().unwrap_or_default().to_string_lossy())?;
        if !is_jar(&name) {
            return Err(AppError::new("CONTENT_TYPE", format!("Ожидался .jar: {name}")));
        }
        if !path.is_file() {
            return Err(AppError::new("CONTENT_TYPE", format!("Не файл: {name}")));
        }
        let dest = resolve_inside(&dir, &[&name])?;
        if dest.exists() {
            skipped += 1;
            continue;
        }
        std::fs::copy(path, dest)?;
        imported += 1;
    }
    Ok((imported, skipped))
}

fn new_token() -> String {
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `content:add`: sorts picked files, plans archives, remembers ones needing a decision.
pub fn add_content(
    game_dir: &Path,
    paths: Option<&[String]>,
    pending: &mut HashMap<String, (String, PathBuf)>,
    instance_id: &str,
) -> AppResult<Value> {
    let mut out = json!({
        "imported": 0u64,
        "skipped": 0u64,
        "pending": Vec::<Value>::new(),
        "errors": Vec::<String>::new(),
    });
    let Some(files) = paths else { return Ok(out) };
    let mut jars = Vec::new();
    for file in files {
        let name = Path::new(file)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = Path::new(file)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !Path::new(file).is_absolute() {
            push_error(&mut out, format!("«{name}»: не удалось определить путь к файлу"));
        } else if ext == "jar" {
            jars.push(file.clone());
        } else if ext == "zip" {
            match plan_archive(Path::new(file)) {
                Ok(plan) if plan.kind == "modpack" => push_error(
                    &mut out,
                    format!("«{name}»: это сборка. Установи её на странице Modrinth или CurseForge."),
                ),
                Ok(plan) if plan.kind == "empty" => push_error(
                    &mut out,
                    format!("«{name}»: в архиве нет ни модов, ни других нужных файлов"),
                ),
                Ok(plan) if plan.kind == "mods" => match apply_archive(Path::new(file), game_dir, "mods") {
                    Ok(applied) => {
                        add_u64(&mut out, "imported", applied["mods"].as_u64().unwrap_or(0));
                        add_u64(&mut out, "skipped", applied["skipped"].as_u64().unwrap_or(0));
                    }
                    Err(err) => push_error(&mut out, format!("«{name}»: {err}")),
                },
                Ok(plan) => {
                    let token = new_token();
                    pending.insert(token.clone(), (instance_id.to_string(), PathBuf::from(file)));
                    if pending.len() > 50 {
                        if let Some(oldest) = pending.keys().next().cloned() {
                            pending.remove(&oldest);
                        }
                    }
                    out["pending"].as_array_mut().unwrap().push(analysis_json(&token, &name, &plan));
                }
                Err(err) => push_error(&mut out, format!("«{name}»: {err}")),
            }
        } else {
            push_error(&mut out, format!("«{name}»: поддерживаются файлы .jar и архивы .zip"));
        }
    }
    if !jars.is_empty() {
        match import_jars(game_dir, &jars) {
            Ok((imported, skipped)) => {
                add_u64(&mut out, "imported", imported);
                add_u64(&mut out, "skipped", skipped);
            }
            Err(err) => push_error(&mut out, err.to_string()),
        }
    }
    Ok(out)
}

fn push_error(out: &mut Value, message: String) {
    out["errors"].as_array_mut().unwrap().push(Value::String(message));
}

fn add_u64(out: &mut Value, key: &str, value: u64) {
    out[key] = json!(out[key].as_u64().unwrap_or(0) + value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rejects_traversal_and_absolute_names() {
        assert_eq!(clean_entry_name("mods/a.jar"), Some("mods/a.jar".into()));
        assert_eq!(clean_entry_name("mods\\a.jar"), Some("mods/a.jar".into()));
        assert_eq!(clean_entry_name("../a.jar"), None);
        assert_eq!(clean_entry_name("/etc/passwd"), None);
        assert_eq!(clean_entry_name("C:/x.jar"), None);
        assert_eq!(clean_entry_name("a/../../b"), None);
    }

    #[test]
    fn strips_one_wrapper_folder() {
        let rels = vec![
            vec!["sborka".into(), "mods".into(), "a.jar".into()],
            vec!["sborka".into(), "config".into(), "x.cfg".into()],
        ];
        let stripped = strip_wrappers(&rels);
        assert_eq!(stripped[0], vec!["mods", "a.jar"]);
        assert_eq!(stripped[1], vec!["config", "x.cfg"]);
    }

    #[test]
    fn plans_and_applies_a_wrapped_archive() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.zip");
        {
            let file = File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("MyPack/mods/a.jar", options).unwrap();
            zip.write_all(b"JAR").unwrap();
            zip.start_file("MyPack/config/x.cfg", options).unwrap();
            zip.write_all(b"CFG").unwrap();
            zip.start_file("MyPack/readme.txt", options).unwrap();
            zip.write_all(b"README").unwrap();
            zip.start_file("MyPack/../../escape.jar", options).unwrap();
            zip.write_all(b"EVIL").unwrap();
            zip.finish().unwrap();
        }

        let plan = plan_archive(&zip_path).unwrap();
        assert_eq!(plan.kind, "mixed");
        assert_eq!(plan.mods, 1);
        assert_eq!(plan.groups.len(), 1);
        // readme is ignored; the traversal entry is dropped before planning
        assert_eq!(plan.ignored, 1);

        let game = dir.path().join("game");
        let mods_only = apply_archive(&zip_path, &game, "mods").unwrap();
        assert_eq!(mods_only["mods"], 1);
        assert_eq!(mods_only["other"], 0);
        assert!(game.join("mods/a.jar").is_file());
        assert!(!game.join("config/x.cfg").exists());

        let everything = apply_archive(&zip_path, &game, "all").unwrap();
        assert_eq!(everything["other"], 1);
        assert_eq!(everything["skipped"], 1); // a.jar already there
        assert!(game.join("config/x.cfg").is_file());
        assert!(!game.join("escape.jar").exists());
        assert!(!game.join("readme.txt").exists());
    }

    #[test]
    fn import_jars_skips_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.jar");
        std::fs::write(&source, b"x").unwrap();
        let game = dir.path().join("game");
        assert_eq!(import_jars(&game, &[source.to_string_lossy().to_string()]).unwrap(), (1, 0));
        assert_eq!(import_jars(&game, &[source.to_string_lossy().to_string()]).unwrap(), (0, 1));
    }
}
