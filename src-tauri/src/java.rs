use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::errors::{AppError, AppResult};

#[derive(Debug, Clone, Serialize)]
pub struct JavaInstall {
    pub path: String,
    pub major: u32,
    /// `system` or `launcher`
    pub source: String,
}

pub fn parse_major(output: &str) -> Option<u32> {
    let index = output.find("version \"")?;
    let rest = &output[index + 9..];
    let number: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = number.split('.');
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

pub fn java_major(java_path: &str) -> Option<u32> {
    let output = Command::new(java_path).arg("-version").output().ok()?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    parse_major(&text)
}

fn java_exe() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.push(entry.path());
            }
        }
    }
    out
}

fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

fn which(name: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let candidate = dir.join(name);
        if is_file(&candidate) {
            return Some(candidate);
        }
        if cfg!(windows) {
            let candidate = dir.join(format!("{name}.exe"));
            if is_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_java_binary(root: &Path, depth: u32) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let direct = root.join("bin").join(java_exe());
    if is_file(&direct) {
        return Some(direct);
    }
    for dir in subdirs(root) {
        if let Some(found) = find_java_binary(&dir, depth - 1) {
            return Some(found);
        }
    }
    None
}

fn platform_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(windows)]
    {
        for base in ["ProgramFiles", "ProgramFiles(x86)"].into_iter().filter_map(std::env::var_os) {
            let base = PathBuf::from(base);
            for vendor in ["Java", "Eclipse Adoptium", "Microsoft", "Zulu", "BellSoft", "Amazon Corretto"] {
                roots.extend(subdirs(&base.join(vendor)));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        for dir in subdirs(Path::new("/Library/Java/JavaVirtualMachines")) {
            roots.push(dir.join("Contents").join("Home"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        roots.extend(subdirs(Path::new("/usr/lib/jvm")));
    }
    roots
}

pub fn detect(runtime_dir: &Path) -> Vec<JavaInstall> {
    let mut candidates: Vec<(String, String)> = vec![("java".to_string(), "system".to_string())];
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        candidates.push((
            Path::new(&home).join("bin").join(java_exe()).to_string_lossy().to_string(),
            "system".to_string(),
        ));
    }
    for root in platform_roots() {
        candidates.push((
            root.join("bin").join(java_exe()).to_string_lossy().to_string(),
            "system".to_string(),
        ));
    }
    for dir in subdirs(runtime_dir) {
        if let Some(binary) = find_java_binary(&dir, 5) {
            candidates.push((binary.to_string_lossy().to_string(), "launcher".to_string()));
        }
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (path, source) in candidates {
        let resolved = if Path::new(&path).is_absolute() {
            std::fs::canonicalize(&path).ok()
        } else {
            which(&path)
        };
        let Some(real) = resolved else { continue };
        if !seen.insert(real.clone()) {
            continue;
        }
        let text = real.to_string_lossy().to_string();
        if let Some(major) = java_major(&text) {
            out.push(JavaInstall { path: text, major, source });
        }
    }
    out.sort_by_key(|java| java.major);
    out
}

/// Exact match first, then the lowest newer major.
pub fn pick(required: u32, installs: &[JavaInstall]) -> Option<JavaInstall> {
    if let Some(exact) = installs.iter().find(|java| java.major == required) {
        return Some(exact.clone());
    }
    installs
        .iter()
        .filter(|java| java.major > required)
        .min_by_key(|java| java.major)
        .cloned()
}

pub fn ensure_java(configured: &str, required: u32, runtime_dir: &Path) -> AppResult<String> {
    if !configured.trim().is_empty() {
        let major = java_major(configured).ok_or_else(|| {
            AppError::new("JAVA_INVALID", format!("Не удалось запустить Java по пути: {configured}"))
        })?;
        if major < required {
            return Err(AppError::new("JAVA_OLD", format!("Нужна Java {required}+, указана Java {major}")));
        }
        return Ok(configured.to_string());
    }
    if let Some(found) = pick(required, &detect(runtime_dir)) {
        return Ok(found.path);
    }
    Err(AppError::new(
        "JAVA_NOT_FOUND",
        format!("Не найдена Java {required}+. Установи её или укажи путь в настройках."),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_and_legacy_versions() {
        assert_eq!(parse_major("openjdk version \"17.0.9\" 2023-10-17"), Some(17));
        assert_eq!(parse_major("java version \"1.8.0_392\""), Some(8));
        assert_eq!(parse_major("java version \"21\""), Some(21));
        assert_eq!(parse_major("no version here"), None);
    }
}
