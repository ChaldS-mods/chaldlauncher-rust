use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;


use crate::download::{download_file, is_file_valid, DownloadSpec};
use crate::errors::{AppError, AppResult};
use crate::net::fetch_text;
use crate::paths::{maven_path, resolve_inside, shared_root, versions_dir};
use crate::version_json::{current_platform, rules_allow, Library, RuleContext};

type Reporter<'a> = &'a (dyn Fn(&str, Option<f64>) + Sync);

pub fn installer_url(loader: &str, mc_version: &str, loader_version: &str) -> String {
    if loader == "forge" {
        let version = format!("{mc_version}-{loader_version}");
        format!(
            "https://maven.minecraftforge.net/net/minecraftforge/forge/{version}/forge-{version}-installer.jar"
        )
    } else {
        format!(
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/{loader_version}/neoforge-{loader_version}-installer.jar"
        )
    }
}

fn read_entry(archive: &mut zip::ZipArchive<File>, name: &str) -> AppResult<Vec<u8>> {
    let clean = name.trim_start_matches('/');
    let mut entry = archive
        .by_name(clean)
        .map_err(|_| AppError::new("INSTALL_PROFILE", format!("В установщике нет файла {clean}")))?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_json<T: serde::de::DeserializeOwned>(archive: &mut zip::ZipArchive<File>, name: &str) -> AppResult<T> {
    let bytes = read_entry(archive, name)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| AppError::new("INSTALL_PROFILE", format!("Некорректный {name}: {e}")))
}

fn main_class_of(jar: &Path) -> AppResult<String> {
    let file = File::open(jar)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AppError::new("INSTALL_PROFILE", format!("Не удалось открыть {}: {e}", jar.display())))?;
    let manifest = read_entry(&mut archive, "META-INF/MANIFEST.MF")?;
    let text = String::from_utf8_lossy(&manifest);
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Main-Class:") {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    Err(AppError::new(
        "INSTALL_PROFILE",
        format!("В {} нет Main-Class", jar.file_name().unwrap_or_default().to_string_lossy()),
    ))
}

fn run_java(java: &str, args: &[String], cwd: &Path, cancel: &AtomicBool, title: &str) -> AppResult<()> {
    let mut child = Command::new(java)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AppError::new("SPAWN_FAILED", format!("Не удалось запустить Java: {e}")))?;
    let tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    if let Some(stdout) = child.stdout.take() {
        pipe_tail(stdout, tail.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        pipe_tail(stderr, tail.clone());
    }
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            return Err(AppError::new("CANCELLED", "Установка отменена"));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let lines = tail.lock().unwrap().clone();
                let context = lines.iter().rev().take(6).cloned().collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
                return Err(AppError::new(
                    "INSTALL_FAILED",
                    format!("Обработчик {title} завершился с кодом {:?}. {context}", status.code()),
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(150)),
            Err(e) => return Err(AppError::new("INSTALL_FAILED", e.to_string())),
        }
    }
}

fn pipe_tail<R: Read + Send + 'static>(stream: R, tail: Arc<Mutex<Vec<String>>>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            let mut buffer = tail.lock().unwrap();
            buffer.push(line);
            if buffer.len() > 30 {
                let excess = buffer.len() - 30;
                buffer.drain(0..excess);
            }
        }
    });
}

#[derive(Debug, serde::Deserialize)]
struct InstallProfile {
    #[serde(default)]
    json: Option<String>,
    #[serde(default)]
    data: Option<HashMap<String, ProfileData>>,
    #[serde(default)]
    processors: Vec<Processor>,
    #[serde(default)]
    libraries: Vec<Library>,
    #[serde(default)]
    #[allow(dead_code)]
    version: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ProfileData {
    client: String,
    #[serde(default)]
    #[allow(dead_code)]
    server: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Processor {
    #[serde(default)]
    sides: Vec<String>,
    jar: String,
    #[serde(default)]
    classpath: Vec<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    outputs: HashMap<String, String>,
}

fn data_value(raw: &str, libraries: &Path, tmp: &Path) -> String {
    if raw.len() >= 2 && raw.starts_with('[') && raw.ends_with(']') {
        return libraries
            .join(maven_path(&raw[1..raw.len() - 1]).unwrap_or_default())
            .to_string_lossy()
            .to_string();
    }
    if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') {
        return raw[1..raw.len() - 1].to_string();
    }
    if let Some(entry) = raw.strip_prefix('/') {
        return tmp.join(entry).to_string_lossy().to_string();
    }
    raw.to_string()
}

fn resolve_arg(
    raw: &str,
    vars: &HashMap<String, String>,
    data: &HashMap<String, String>,
    libraries: &Path,
) -> AppResult<String> {
    if raw.len() >= 2 && raw.starts_with('[') && raw.ends_with(']') {
        return Ok(libraries
            .join(maven_path(&raw[1..raw.len() - 1])?)
            .to_string_lossy()
            .to_string());
    }
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(rest);
            rest = "";
            break;
        };
        let key = &after[..end];
        let value = vars.get(key).or_else(|| data.get(key));
        let Some(value) = value else {
            return Err(AppError::new(
                "INSTALL_PROFILE",
                format!("Установщик использует неизвестный параметр {{{key}}}"),
            ));
        };
        out.push_str(value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Installs Forge / NeoForge into the shared directories: unpack the installer, download the profile
/// libraries, run its processors with Java and write the resulting version json.
pub async fn install_forge_like(
    loader: &str,
    mc_version: &str,
    loader_version: &str,
    data_dir: &Path,
    java_path: &str,
    report: Reporter<'_>,
    cancel: &AtomicBool,
) -> AppResult<String> {
    let title = if loader == "forge" { "Forge" } else { "NeoForge" };
    let root = shared_root(data_dir);
    let installer_path = root
        .join("installers")
        .join(format!("{loader}-{mc_version}-{loader_version}-installer.jar"));

    report(&format!("Загрузка установщика {title}…"), None);
    let url = installer_url(loader, mc_version, loader_version);
    let mut sha1 = None;
    if let Ok(text) = fetch_text(&format!("{url}.sha1"), "Контрольная сумма установщика").await {
        let token = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        if token.len() == 40 && token.chars().all(|c| c.is_ascii_hexdigit()) {
            sha1 = Some(token);
        }
    }
    if !is_file_valid(&installer_path, None, sha1.as_deref(), None) {
        download_file(
            &DownloadSpec {
                url,
                dest: installer_path.clone(),
                sha1,
                sha256: None,
                size: None,
                purpose: format!("Установщик {title}"),
            },
            |_| {},
        )
        .await?;
    }

    let file = File::open(&installer_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AppError::new("INSTALL_PROFILE", format!("Установщик {title} повреждён: {e}")))?;
    let profile: InstallProfile = read_json(&mut archive, "install_profile.json")?;
    let Some(json_name) = profile.json.clone() else {
        return Err(AppError::new(
            "LOADER_UNSUPPORTED",
            format!("Этот установщик {title} старого формата и не поддерживается"),
        ));
    };
    let version_json: crate::version_json::VersionJson = read_json(&mut archive, &json_name)?;
    if version_json.inherits_from.as_deref() != Some(mc_version) {
        return Err(AppError::new(
            "LOADER_INCOMPATIBLE",
            format!("Профиль {title} не соответствует Minecraft {mc_version}"),
        ));
    }

    let tmp = std::env::temp_dir().join(format!("chald-loader-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;

    report("Распаковка установщика…", None);
    let libraries = crate::paths::libraries_dir(data_dir);
    for index in 0..archive.len() {
        let (name, bytes, is_dir, size) = {
            let mut entry = archive.by_index(index).map_err(|e| AppError::new("INSTALL_PROFILE", e.to_string()))?;
            let name = entry.name().to_string();
            let is_dir = entry.is_dir();
            let size = entry.size();
            let mut bytes = Vec::new();
            if !is_dir {
                entry.read_to_end(&mut bytes)?;
            }
            (name, bytes, is_dir, size)
        };
        if is_dir || !name.starts_with("maven/") {
            continue;
        }
        let rel = &name["maven/".len()..];
        let dest = resolve_inside(&libraries, &[rel])?;
        if is_file_valid(&dest, Some(size), None, None) {
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, bytes)?;
    }

    // Libraries the processors need (some come from the installer's own maven folder, some from the network).
    let platform = current_platform();
    let ctx = RuleContext { platform, features: HashMap::new() };
    let mut jobs = Vec::new();
    for library in profile.libraries.iter().filter(|library| rules_allow(library.rules.as_ref(), &ctx)) {
        let artifact = library.downloads.as_ref().and_then(|downloads| downloads.artifact.as_ref());
        let rel = artifact
            .and_then(|artifact| artifact.path.clone())
            .unwrap_or(maven_path(&library.name)?);
        let url = match artifact {
            Some(artifact) if !artifact.url.is_empty() => artifact.url.clone(),
            _ => match &library.url {
                Some(base) if !base.is_empty() => format!("{}{rel}", if base.ends_with('/') { base.clone() } else { format!("{base}/") }),
                _ => String::new(),
            },
        };
        if url.is_empty() {
            continue;
        }
        jobs.push(DownloadSpec {
            url,
            dest: libraries.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR)),
            sha1: artifact.and_then(|a| a.sha1.clone()).or_else(|| library.sha1.clone()),
            sha256: None,
            size: artifact.and_then(|a| a.size).or(library.size),
            purpose: format!("Библиотека {title}"),
        });
    }
    let total_jobs = jobs.len().max(1);
    for (index, job) in jobs.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return Err(AppError::new("CANCELLED", "Установка отменена"));
        }
        download_file(job, |_| {}).await?;
        report(
            &format!("Библиотеки {title} {}/{}…", index + 1, total_jobs),
            None,
        );
    }

    // `data` values: [maven], 'literal', /file-in-installer
    let mut data_values: HashMap<String, String> = HashMap::new();
    for (key, value) in profile.data.as_ref().into_iter().flatten() {
        data_values.insert(key.clone(), data_value(&value.client, &libraries, &tmp));
        if let Some(entry) = value.client.strip_prefix('/') {
            let bytes = read_entry(&mut archive, entry)?;
            let dest = resolve_inside(&tmp, &[entry])?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, bytes)?;
        }
    }
    let mc_jar = versions_dir(data_dir)
        .join(mc_version)
        .join(format!("{mc_version}.jar"));
    let mut vars = HashMap::new();
    vars.insert("SIDE".to_string(), "client".to_string());
    vars.insert("ROOT".to_string(), root.to_string_lossy().to_string());
    vars.insert("INSTALLER".to_string(), installer_path.to_string_lossy().to_string());
    vars.insert("LIBRARY_DIR".to_string(), libraries.to_string_lossy().to_string());
    vars.insert("MINECRAFT_JAR".to_string(), mc_jar.to_string_lossy().to_string());
    vars.insert("MINECRAFT_VERSION".to_string(), mc_version.to_string());

    let processors: Vec<&Processor> = profile
        .processors
        .iter()
        .filter(|processor| processor.sides.is_empty() || processor.sides.iter().any(|side| side == "client"))
        .collect();
    for (index, processor) in processors.iter().enumerate() {
        report(
            &format!("Обработка файлов {title} {}/{}…", index + 1, processors.len()),
            None,
        );
        let outputs: Vec<(PathBuf, String)> = processor
            .outputs
            .iter()
            .map(|(file, sha1)| {
                Ok((
                    PathBuf::from(resolve_arg(file, &vars, &data_values, &libraries)?),
                    resolve_arg(&data_value(sha1, &libraries, &tmp), &vars, &data_values, &libraries)?,
                ))
            })
            .collect::<AppResult<Vec<_>>>()?;
        if !outputs.is_empty()
            && outputs
                .iter()
                .all(|(file, sha1)| is_file_valid(file, None, Some(sha1), None))
        {
            continue;
        }
        let classpath: Vec<PathBuf> = std::iter::once(&processor.jar)
            .chain(processor.classpath.iter())
            .map(|coord| Ok(libraries.join(maven_path(coord)?.replace('/', std::path::MAIN_SEPARATOR_STR))))
            .collect::<AppResult<Vec<_>>>()?;
        let main = main_class_of(&classpath[0])?;
        let classpath_text = classpath
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(if cfg!(windows) { ";" } else { ":" });
        let mut java_args = vec!["-cp".to_string(), classpath_text, main];
        for raw in &processor.args {
            java_args.push(resolve_arg(raw, &vars, &data_values, &libraries)?);
        }
        for (file, _) in &outputs {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        run_java(java_path, &java_args, &root, cancel, title)?;
        for (file, sha1) in &outputs {
            if !is_file_valid(file, None, Some(sha1), None) {
                return Err(AppError::new(
                    "INSTALL_FAILED",
                    format!("Обработчик {title} создал неверный файл: {}", file.display()),
                ));
            }
        }
    }

    let out_dir = versions_dir(data_dir).join(&version_json.id);
    std::fs::create_dir_all(&out_dir)?;
    std::fs::write(
        out_dir.join(format!("{}.json", version_json.id)),
        serde_json::to_vec_pretty(&version_json)?,
    )?;
    let _ = std::fs::remove_dir_all(&tmp);
    report(&format!("{title} установлен"), None);
    Ok(version_json.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_urls() {
        assert!(installer_url("forge", "1.20.1", "47.2.0").ends_with("forge-1.20.1-47.2.0-installer.jar"));
        assert!(installer_url("neoforge", "1.21.1", "21.1.200").contains("neoforge-21.1.200-installer.jar"));
    }

    #[test]
    fn data_values_and_args() {
        let libraries = Path::new("/data/libraries");
        let tmp = Path::new("/tmp/unpack");
        assert_eq!(data_value("[net.minecraft:client:1.20.1]", libraries, tmp), "/data/libraries/net/minecraft/client/1.20.1/client-1.20.1.jar");
        assert_eq!(data_value("'literal'", libraries, tmp), "literal");
        assert_eq!(data_value("/data/client.lzma", libraries, tmp), "/tmp/unpack/data/client.lzma");
        let mut vars = HashMap::new();
        vars.insert("SIDE".to_string(), "client".to_string());
        let mut data = HashMap::new();
        data.insert("MAPPINGS".to_string(), "/tmp/mappings.tsrg".to_string());
        assert_eq!(resolve_arg("{SIDE}", &vars, &data, libraries).unwrap(), "client");
        assert_eq!(resolve_arg("{MAPPINGS}", &vars, &data, libraries).unwrap(), "/tmp/mappings.tsrg");
        assert!(resolve_arg("{NOPE}", &vars, &data, libraries).is_err());
    }
}
