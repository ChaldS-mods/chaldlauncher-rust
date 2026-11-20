//! Local dependency audit: reads mod manifests inside `.minecraft/mods`.
//!
//! CurseForge/Modrinth metadata is not always complete, so a mod can be installed "successfully"
//! and still crash because a required mod is missing. This module reads the jars themselves and
//! compares what they declare with what is actually present.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::Path;

use serde_json::{json, Value};

use crate::errors::AppResult;

/// Loader/library ids that are not separate mod jars.
const IGNORED: &[&str] = &[
    "minecraft",
    "fabric",
    "fabricloader",
    "fabric_loader",
    "quilt",
    "quilt_loader",
    "forge",
    "neoforge",
    "java",
];

fn ignored(id: &str) -> bool {
    let id = id.trim().to_lowercase();
    IGNORED.contains(&id.as_str())
}

fn add(deps: &mut Vec<String>, id: &str) {
    let id = id.trim();
    if id.is_empty() || ignored(id) { return; }
    if !deps.iter().any(|existing| existing.eq_ignore_ascii_case(id)) {
        deps.push(id.to_string());
    }
}

fn parse_toml(text: &str) -> (Option<String>, Vec<String>) {
    let mut own_id: Option<String> = None;
    let mut deps = Vec::new();
    let mut section = String::new();
    let mut pending_dep: Option<String> = None;
    let mut mandatory = true;

    let flush = |deps: &mut Vec<String>, pending: &mut Option<String>, mandatory: bool| {
        if mandatory {
            if let Some(id) = pending.take() { add(deps, &id); }
        } else {
            pending.take();
        }
    };

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("[[") && line.ends_with("]]") {
            flush(&mut deps, &mut pending_dep, mandatory);
            section = line[2..line.len() - 2].trim().to_string();
            mandatory = true;
            continue;
        }
        if line.starts_with('[') {
            flush(&mut deps, &mut pending_dep, mandatory);
            section.clear();
            mandatory = true;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'').trim();
        if key == "modId" || key == "modid" {
            if section.starts_with("dependencies.") {
                pending_dep = Some(value.to_string());
            } else if section == "mods" && own_id.is_none() {
                own_id = Some(value.to_string());
            }
        } else if key == "mandatory" && section.starts_with("dependencies.") {
            mandatory = value.eq_ignore_ascii_case("true");
        }
    }
    flush(&mut deps, &mut pending_dep, mandatory);
    (own_id, deps)
}

fn parse_json_manifest(text: &str, file: &str) -> (Option<String>, Vec<String>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return (None, Vec::new()) };
    let mut deps = Vec::new();
    // fabric.mod.json
    let own_id = value.get("id").and_then(Value::as_str).map(str::to_string);
    if let Some(depends) = value.get("depends") {
        match depends {
            Value::Object(map) => {
                for key in map.keys() { add(&mut deps, key); }
            }
            Value::Array(list) => {
                for item in list {
                    if let Some(id) = item.get("id").and_then(Value::as_str) { add(&mut deps, id); }
                }
            }
            _ => {}
        }
    }
    // quilt.mod.json nests the same data under quilt_loader.
    if let Some(loader) = value.get("quilt_loader") {
        let own = loader.get("id").and_then(Value::as_str).map(str::to_string);
        if own_id.is_none() { /* caller keeps the first id */ }
        if let Some(depends) = loader.get("depends").and_then(Value::as_array) {
            for item in depends {
                if let Some(id) = item.get("id").and_then(Value::as_str) { add(&mut deps, id); }
            }
        }
        let _ = own;
    }
    // mcmod.info is an array of objects with requiredMods.
    for item in value.as_array().into_iter().flatten() {
        if let Some(list) = item.get("requiredMods").and_then(Value::as_array) {
            for id in list.iter().filter_map(Value::as_str) { add(&mut deps, id); }
        }
    }
    let _ = file;
    (own_id, deps)
}

fn read_jar(path: &Path) -> (Option<String>, Vec<String>) {
    let Ok(file) = fs::File::open(path) else { return (None, Vec::new()) };
    let Ok(mut archive) = zip::ZipArchive::new(file) else { return (None, Vec::new()) };
    for name in [
        "fabric.mod.json",
        "quilt.mod.json",
        "neoforge.mods.toml",
        "META-INF/mods.toml",
        "mcmod.info",
    ] {
        let Ok(mut entry) = archive.by_name(name) else { continue };
        let mut text = String::new();
        if entry.read_to_string(&mut text).is_err() { continue; }
        if name.ends_with(".toml") {
            return parse_toml(&text);
        }
        return parse_json_manifest(&text, name);
    }
    (None, Vec::new())
}

/// Files that declare a dependency are reported in `declared`; missing ids are in `missing`.
pub fn scan(game_dir: &Path) -> AppResult<Value> {
    let mods_dir = game_dir.join("mods");
    let mut installed: HashSet<String> = HashSet::new();
    let mut jars: Vec<(String, Option<String>, Vec<String>)> = Vec::new();
    let entries = match fs::read_dir(&mods_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(json!({ "installedModIds": [], "declared": [], "missing": [] })),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".jar") { continue; }
        let (own_id, deps) = read_jar(&path);
        if let Some(id) = &own_id { installed.insert(id.to_lowercase()); }
        jars.push((name, own_id, deps));
    }
    let mut declared = Vec::new();
    let mut missing = Vec::new();
    for (file, own_id, deps) in &jars {
        if deps.is_empty() { continue; }
        declared.push(json!({ "file": file, "modId": own_id, "requires": deps }));
        for dep in deps {
            if installed.contains(&dep.to_lowercase()) { continue; }
            missing.push(json!({
                "modId": dep,
                "requiredByFile": file,
                "source": "manifest",
                "hint": format!("Install the mod with id «{dep}» (search Modrinth and CurseForge), then validate again."),
            }));
        }
    }
    let mut installed_ids: Vec<String> = installed.into_iter().collect();
    installed_ids.sort();
    Ok(json!({
        "installedModIds": installed_ids,
        "declared": declared,
        "missing": missing,
        "note": "Manifest-based audit. A missing id may be embedded in another jar or optional on some loaders; check before claiming the modpack is broken.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toml_dependencies() {
        let text = r#"
[[mods]]
modId = "waystonesable"
version = "1.0"

[[dependencies.waystonesable]]
modId = "waystones"
mandatory = true
versionRange = "[1.0,)"

[[dependencies.waystonesable]]
modId = "jei"
mandatory = false
"#;
        let (own, deps) = parse_toml(text);
        assert_eq!(own.as_deref(), Some("waystonesable"));
        assert_eq!(deps, vec!["waystones"]);
    }

    #[test]
    fn parses_fabric_depends_and_ignores_loader() {
        let text = r#"{"id":"waystonesable","depends":{"minecraft":"1.20.1","fabricloader":">=0.15","waystones":"*"}}"#;
        let (own, deps) = parse_json_manifest(text, "fabric.mod.json");
        assert_eq!(own.as_deref(), Some("waystonesable"));
        assert_eq!(deps, vec!["waystones"]);
    }
}
