use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::errors::{AppError, AppResult};
use crate::paths::{resolve_inside, safe_file_name};

const DISABLED: &str = ".disabled";

fn content_dir(game_dir: &Path, kind: &str) -> PathBuf {
    game_dir.join(if kind == "resourcepacks" { "resourcepacks" } else { "mods" })
}

fn strip_disabled(name: &str) -> &str {
    name.strip_suffix(DISABLED).unwrap_or(name)
}

fn is_allowed(kind: &str, base: &str, is_dir: bool) -> bool {
    if kind == "mods" {
        !is_dir && base.ends_with(".jar")
    } else {
        is_dir || base.ends_with(".zip")
    }
}

fn target(game_dir: &Path, kind: &str, file: &str) -> AppResult<PathBuf> {
    let name = safe_file_name(file)?;
    resolve_inside(&content_dir(game_dir, kind), &[&name])
}

/// Files in `mods/` or `resourcepacks/` with the little metadata the tab shows.
pub fn list(game_dir: &Path, kind: &str) -> AppResult<Vec<Value>> {
    let dir = content_dir(game_dir, kind);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let enabled = !name.ends_with(DISABLED);
        let base = strip_disabled(&name).to_string();
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_allowed(kind, &base, is_dir) {
            continue;
        }
        let size = std::fs::metadata(entry.path()).map(|meta| meta.len()).unwrap_or(0);
        out.push(json!({
            "file": name,
            "name": base,
            "version": Value::Null,
            "size": if is_dir { 0 } else { size },
            "enabled": enabled,
            "isDir": is_dir,
            "iconDataUrl": Value::Null,
            "warning": Value::Null,
        }));
    }
    out.sort_by(|a, b| {
        let left = a.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let right = b.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase();
        left.cmp(&right)
    });
    Ok(out)
}

pub fn toggle(game_dir: &Path, kind: &str, file: &str) -> AppResult<()> {
    let from = target(game_dir, kind, file)?;
    let to = if file.ends_with(DISABLED) {
        PathBuf::from(from.to_string_lossy().trim_end_matches(DISABLED))
    } else {
        PathBuf::from(format!("{}{DISABLED}", from.to_string_lossy()))
    };
    if to.exists() {
        return Err(AppError::new("CONTENT_EXISTS", "Файл с таким именем уже существует"));
    }
    std::fs::rename(&from, &to)?;
    Ok(())
}

pub fn remove(game_dir: &Path, kind: &str, file: &str, confirm: bool) -> AppResult<()> {
    if !confirm {
        return Err(AppError::new("CONFIRM_REQUIRED", "Удаление нужно подтвердить"));
    }
    let path = target(game_dir, kind, file)?;
    if path.is_dir() {
        std::fs::remove_dir_all(&path)?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Copies files the user picked. Never overwrites an existing file.
pub fn import(game_dir: &Path, kind: &str, sources: &[String]) -> AppResult<u64> {
    let dir = content_dir(game_dir, kind);
    std::fs::create_dir_all(&dir)?;
    let ext = if kind == "mods" { ".jar" } else { ".zip" };
    let mut imported = 0u64;
    for source in sources {
        let path = Path::new(source);
        let name = safe_file_name(&path.file_name().unwrap_or_default().to_string_lossy())?;
        if !name.to_lowercase().ends_with(ext) {
            return Err(AppError::new("CONTENT_TYPE", format!("Ожидался файл {ext}: {name}")));
        }
        if !path.is_file() {
            return Err(AppError::new("CONTENT_TYPE", format!("Не файл: {name}")));
        }
        let dest = resolve_inside(&dir, &[&name])?;
        if dest.exists() {
            return Err(AppError::new("CONTENT_EXISTS", format!("Файл уже есть: {name}")));
        }
        std::fs::copy(path, &dest)?;
        imported += 1;
    }
    Ok(imported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_and_toggles_files() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let mods = game.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("sodium.jar"), b"jar").unwrap();
        std::fs::write(mods.join("old.jar.disabled"), b"jar").unwrap();
        std::fs::write(mods.join("readme.txt"), b"x").unwrap();

        let list = list(game, "mods").unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|entry| entry["enabled"] == true));
        assert!(list.iter().any(|entry| entry["enabled"] == false));

        toggle(game, "mods", "sodium.jar").unwrap();
        assert!(mods.join("sodium.jar.disabled").is_file());
        toggle(game, "mods", "sodium.jar.disabled").unwrap();
        assert!(mods.join("sodium.jar").is_file());
        assert!(toggle(game, "mods", "../escape").is_err());
    }

    #[test]
    fn import_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.jar");
        std::fs::write(&source, b"x").unwrap();
        let game = dir.path().join("game");
        assert_eq!(import(&game, "mods", &[source.to_string_lossy().to_string()]).unwrap(), 1);
        assert!(import(&game, "mods", &[source.to_string_lossy().to_string()]).is_err());
    }
}
