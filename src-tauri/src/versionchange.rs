//! Смена версии Minecraft / загрузчика у существующего инстанса. Порт `src/main/versionChange.ts`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::errors::{AppError, AppResult};
use crate::instances::{InstanceMeta, Loader};
use crate::paths;

/// Where an instance is switched: Minecraft version + loader (`vanilla` has no loader version).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionTarget {
    pub mc_version: String,
    pub loader_type: String,
    pub loader_version: Option<String>,
}

impl VersionTarget {
    pub fn from_meta(meta: &InstanceMeta) -> Self {
        Self {
            mc_version: meta.mc_version.clone(),
            loader_type: meta.loader.r#type.clone(),
            loader_version: meta.loader.version.clone(),
        }
    }

    pub fn matches(&self, other: &Self) -> bool {
        self.mc_version == other.mc_version
            && self.loader_type == other.loader_type
            && self.loader_version == other.loader_version
    }

    pub fn loader(&self) -> Loader {
        Loader {
            r#type: self.loader_type.clone(),
            version: self.loader_version.clone(),
        }
    }
}

/// «Official» and «modded» follow the loader; a kind the user set to «custom» stays as it is.
pub fn kind_after_change(kind: &str, loader_type: &str) -> String {
    if kind == "custom" {
        kind.to_string()
    } else if loader_type == "vanilla" {
        "official".to_string()
    } else {
        "modded".to_string()
    }
}

fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    // UTC calendar from unix seconds (civil-from-days, Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{:02}-{:02}-{:02}-{:02}-{:02}",
        month,
        day,
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

fn copy_tree(src: &Path, dest: &Path) -> AppResult<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let target = paths::resolve_inside(dest, &[name.to_string_lossy().as_ref()])?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn remove_tree(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

/// Copies `saves/` to `<instance>/backups/saves-<label>-<time>` and returns that folder.
/// The backup lives outside `.minecraft` so the game never sees it.
pub fn backup_saves(game_dir: &Path, label: &str) -> AppResult<Option<PathBuf>> {
    let src = game_dir.join("saves");
    let Ok(entries) = fs::read_dir(&src) else { return Ok(None) };
    if entries.count() == 0 {
        return Ok(None);
    }
    let safe_label: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._+-".contains(c) { c } else { '_' })
        .collect();
    let dest = game_dir
        .parent()
        .ok_or_else(|| AppError::new("PATH", "Не удалось определить папку инстанса"))?
        .join("backups")
        .join(format!("saves-{safe_label}-{}", stamp()));
    match copy_tree(&src, &dest) {
        Ok(()) => Ok(Some(dest)),
        Err(error) => {
            remove_tree(&dest);
            Err(error)
        }
    }
}

const MOD_SUFFIX: &str = ".disabled";

/// Renames `mods/*.jar` to `*.jar.disabled`, so mods built for the old version do not break the new one.
/// Nothing is deleted: a taken name gets a number. Returns how many mods were switched off.
pub fn disable_mods(game_dir: &Path) -> AppResult<u32> {
    let dir = game_dir.join("mods");
    let Ok(entries) = fs::read_dir(&dir) else { return Ok(0) };
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !entry.file_type()?.is_file() || !name.ends_with(".jar") {
            continue;
        }
        let from = dir.join(name.as_ref());
        let mut to = dir.join(format!("{name}{MOD_SUFFIX}"));
        for index in 1..10_000 {
            if !to.exists() {
                break;
            }
            let stem = &name[..name.len() - 4];
            to = dir.join(format!("{stem}-{index}.jar{MOD_SUFFIX}"));
        }
        fs::rename(from, to)?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_follows_loader_but_custom_stays() {
        assert_eq!(kind_after_change("official", "fabric"), "modded");
        assert_eq!(kind_after_change("modded", "vanilla"), "official");
        assert_eq!(kind_after_change("custom", "forge"), "custom");
    }

    #[test]
    fn compares_targets() {
        let a = VersionTarget {
            mc_version: "1.20.1".into(),
            loader_type: "forge".into(),
            loader_version: Some("47.4.29".into()),
        };
        let mut b = a.clone();
        assert!(a.matches(&b));
        b.loader_version = None;
        assert!(!a.matches(&b));
    }

    #[test]
    fn backs_up_saves_and_disables_mods() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("instance").join(".minecraft");
        fs::create_dir_all(game.join("saves/world/region")).unwrap();
        fs::write(game.join("saves/world/region/r.0.0.mca"), b"world").unwrap();
        fs::create_dir_all(game.join("mods")).unwrap();
        fs::write(game.join("mods/a.jar"), b"a").unwrap();
        fs::write(game.join("mods/b.jar"), b"b").unwrap();
        fs::write(game.join("mods/old.jar.disabled"), b"old").unwrap();

        let backup = backup_saves(&game, "1.20.1").unwrap().unwrap();
        assert!(backup.join("world/region/r.0.0.mca").is_file());
        assert!(backup.starts_with(game.parent().unwrap().join("backups")));

        assert_eq!(disable_mods(&game).unwrap(), 2);
        assert!(game.join("mods/a.jar.disabled").is_file());
        assert!(game.join("mods/b.jar.disabled").is_file());
        assert!(game.join("mods/old.jar.disabled").is_file());
        assert!(!game.join("mods/a.jar").exists());
        assert_eq!(disable_mods(&game).unwrap(), 0);
    }
}
