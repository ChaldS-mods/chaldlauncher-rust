use std::path::{Component, Path, PathBuf};

use crate::errors::{AppError, AppResult};

/// Same location Electron used (`app.getPath('userData')`): `<config>/ChaldLauncher`.
/// `CHALD_PROFILE` gives the launcher its own data folder, like `CHALD_PROFILE=test2 electron .`.
pub fn data_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    match std::env::var("CHALD_PROFILE").ok().filter(|p| !p.trim().is_empty()) {
        Some(profile) => {
            let clean: String = profile
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .take(24)
                .collect();
            if clean.is_empty() {
                base.join("ChaldLauncher")
            } else {
                base.join(format!("ChaldLauncher-{clean}"))
            }
        }
        None => base.join("ChaldLauncher"),
    }
}

pub fn shared_root(data: &Path) -> PathBuf {
    data.join("shared")
}

pub fn versions_dir(data: &Path) -> PathBuf {
    shared_root(data).join("versions")
}

pub fn libraries_dir(data: &Path) -> PathBuf {
    shared_root(data).join("libraries")
}

pub fn assets_dir(data: &Path) -> PathBuf {
    shared_root(data).join("assets")
}

pub fn runtime_dir(data: &Path) -> PathBuf {
    shared_root(data).join("runtime")
}

pub fn updates_dir(data: &Path) -> PathBuf {
    data.join("updates")
}

pub fn instances_root(data: &Path, instances_dir: &str) -> PathBuf {
    if instances_dir.trim().is_empty() {
        data.join("instances")
    } else {
        PathBuf::from(instances_dir)
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolves `parts` under `base` and refuses anything that escapes it (no filesystem access, no symlink resolution).
pub fn resolve_inside(base: &Path, parts: &[&str]) -> AppResult<PathBuf> {
    let root = normalize(base);
    let mut target = root.clone();
    for part in parts {
        if part.contains('\0') {
            return Err(AppError::new("PATH_INVALID", "Путь содержит недопустимые символы"));
        }
        target = normalize(&target.join(part));
    }
    if target == root || target.starts_with(&root) {
        Ok(target)
    } else {
        Err(AppError::new("PATH_TRAVERSAL", "Путь выходит за пределы разрешённой папки"))
    }
}

/// A single path segment: no separators, no dot-segments, no control characters.
pub fn safe_file_name(name: &str) -> AppResult<String> {
    if name.is_empty()
        || name.chars().count() > 255
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(|c| c.is_control())
    {
        return Err(AppError::new("FILENAME_INVALID", "Некорректное имя файла"));
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(name.to_string()),
        _ => Err(AppError::new("FILENAME_INVALID", "Некорректное имя файла")),
    }
}

pub fn is_instance_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 64 {
        return false;
    }
    let mut chars = id.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !out.is_empty() && !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    let cut: String = trimmed.chars().take(48).collect();
    if cut.is_empty() {
        "instance".to_string()
    } else {
        cut
    }
}

/// `MC version` / loader version: only characters that are safe in a file name and URL path.
pub fn is_version_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 40
        && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// Minecraft profile name: 3–16 characters, Latin letters, digits and `_` (like the TS `offlineNameSchema`).
pub fn is_offline_name(value: &str) -> bool {
    let len = value.chars().count();
    (3..=16).contains(&len) && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_inside_keeps_paths_in_the_root() {
        let base = Path::new("/tmp/chald-test");
        assert!(resolve_inside(base, &["instance", ".minecraft", "mods"]).is_ok());
        assert!(resolve_inside(base, &["..", "escape"]).is_err());
        assert!(resolve_inside(base, &["a/../../b"]).is_err());
        assert!(resolve_inside(base, &["/etc/passwd"]).is_err());
    }

    #[test]
    fn file_names_are_single_segments() {
        assert!(safe_file_name("mod.jar").is_ok());
        assert!(safe_file_name("a/b").is_err());
        assert!(safe_file_name("..").is_err());
        assert!(safe_file_name("a\nb").is_err());
    }

    #[test]
    fn instance_ids_and_versions_are_validated() {
        assert!(is_instance_id("my-pack_2"));
        assert!(!is_instance_id("My-Pack"));
        assert!(!is_instance_id("../x"));
        assert!(is_version_id("1.21.1"));
        assert!(is_version_id("0.16.9"));
        assert!(!is_version_id("1.21/../../x"));
        assert!(is_offline_name("Steve_1"));
        assert!(!is_offline_name("ab"));
    }

    #[test]
    fn slugify_and_offline_uuid() {
        assert_eq!(slugify("My Cool Pack"), "my-cool-pack");
        assert_eq!(slugify("Моя сборка"), "instance");
        let uuid = crate::offline::uuid("Steve");
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid, crate::offline::uuid("Steve"));
        assert_eq!(&uuid[14..15], "3"); // v3
    }
}

/// Library path from a Maven coordinate: `group:artifact:version[:classifier][@ext]`.
pub fn maven_path(coord: &str) -> AppResult<String> {
    let (main, ext) = match coord.split_once('@') {
        Some((main, ext)) => (main, ext),
        None => (coord, "jar"),
    };
    let parts: Vec<&str> = main.split(':').collect();
    if parts.len() < 3 || parts[0].is_empty() || parts[1].is_empty() || parts[2].is_empty() {
        return Err(AppError::new("MAVEN_INVALID", format!("Некорректная координата: {coord}")));
    }
    let classifier = parts.get(3).copied().unwrap_or("");
    let file = if classifier.is_empty() {
        format!("{}-{}.{}", parts[1], parts[2], ext)
    } else {
        format!("{}-{}-{}.{}", parts[1], parts[2], classifier, ext)
    };
    Ok(format!("{}/{}/{}/{}", parts[0].replace('.', "/"), parts[1], parts[2], file))
}
