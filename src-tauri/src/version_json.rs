use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult};
use crate::paths::{is_version_id, maven_path, resolve_inside, versions_dir};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct VersionJson {
    pub id: String,
    pub inherits_from: Option<String>,
    #[serde(rename = "type")]
    pub version_type: Option<String>,
    pub main_class: String,
    pub minecraft_arguments: Option<String>,
    pub arguments: Option<Arguments>,
    pub libraries: Vec<Library>,
    pub asset_index: Option<AssetIndex>,
    pub assets: Option<String>,
    pub downloads: Option<Downloads>,
    pub java_version: Option<JavaVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Arguments {
    pub game: Option<Vec<ArgItem>>,
    pub jvm: Option<Vec<ArgItem>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgItem {
    Plain(String),
    Conditional {
        #[serde(default)]
        rules: Vec<Rule>,
        value: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Rule {
    pub action: String,
    pub os: Option<OsRule>,
    pub features: Option<HashMap<String, bool>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OsRule {
    pub name: Option<String>,
    pub arch: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Artifact {
    pub path: Option<String>,
    pub url: String,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Library {
    pub name: String,
    pub downloads: Option<LibDownloads>,
    pub rules: Option<Vec<Rule>>,
    pub natives: Option<HashMap<String, String>>,
    pub extract: Option<Extract>,
    pub url: Option<String>,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LibDownloads {
    pub artifact: Option<Artifact>,
    pub classifiers: Option<HashMap<String, Artifact>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Extract {
    pub exclude: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AssetIndex {
    pub id: String,
    pub url: String,
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub total_size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Downloads {
    pub client: Option<Artifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct JavaVersion {
    pub major_version: Option<u32>,
    pub component: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Platform {
    pub name: String,
    pub arch: String,
    #[allow(dead_code)]
    pub version: String,
}

pub fn current_platform() -> Platform {
    let name = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    };
    let arch = match std::env::consts::ARCH {
        "x86" => "x86",
        "aarch64" => "arm64",
        _ => "x64",
    };
    Platform { name: name.to_string(), arch: arch.to_string(), version: String::new() }
}

pub struct RuleContext {
    pub platform: Platform,
    pub features: HashMap<String, bool>,
}

fn rule_matches(rule: &Rule, ctx: &RuleContext) -> bool {
    if let Some(os) = &rule.os {
        if let Some(name) = &os.name {
            if name != &ctx.platform.name {
                return false;
            }
        }
        if let Some(arch) = &os.arch {
            if arch != &ctx.platform.arch {
                return false;
            }
        }
        // os.version is a regex against os.release(); it is only used by ancient versions, ignore it.
    }
    if let Some(features) = &rule.features {
        for (key, value) in features {
            if ctx.features.get(key).copied().unwrap_or(false) != *value {
                return false;
            }
        }
    }
    true
}

/// No rules = allowed. Otherwise the last matching rule wins; no match = disallowed.
pub fn rules_allow(rules: Option<&Vec<Rule>>, ctx: &RuleContext) -> bool {
    let Some(rules) = rules else { return true };
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule, ctx) {
            allowed = rule.action == "allow";
        }
    }
    allowed
}

fn lib_key(name: &str) -> String {
    let main = name.split('@').next().unwrap_or(name);
    let parts: Vec<&str> = main.split(':').collect();
    let group = parts.first().copied().unwrap_or("");
    let artifact = parts.get(1).copied().unwrap_or("");
    let classifier = parts.get(3).copied().unwrap_or("");
    if classifier.is_empty() {
        format!("{group}:{artifact}")
    } else {
        format!("{group}:{artifact}:{classifier}")
    }
}

/// Merge a child profile (Fabric/Forge) over its vanilla parent. Child entries win.
pub fn merge_versions(parent: VersionJson, child: VersionJson) -> VersionJson {
    let mut seen = HashSet::new();
    let mut libraries = Vec::new();
    for library in child.libraries.iter().chain(parent.libraries.iter()) {
        if seen.insert(lib_key(&library.name)) {
            libraries.push(library.clone());
        }
    }
    let arguments = if parent.arguments.is_some() || child.arguments.is_some() {
        let parent_args = parent.arguments.clone().unwrap_or_default();
        let child_args = child.arguments.clone().unwrap_or_default();
        let mut game = parent_args.game.unwrap_or_default();
        game.extend(child_args.game.unwrap_or_default());
        let mut jvm = parent_args.jvm.unwrap_or_default();
        jvm.extend(child_args.jvm.unwrap_or_default());
        Some(Arguments { game: Some(game), jvm: Some(jvm) })
    } else {
        None
    };
    VersionJson {
        id: child.id,
        inherits_from: child.inherits_from.clone().or_else(|| parent.inherits_from.clone()),
        version_type: child.version_type.clone().or_else(|| parent.version_type.clone()),
        main_class: if child.main_class.is_empty() { parent.main_class } else { child.main_class },
        minecraft_arguments: child.minecraft_arguments.clone().or_else(|| parent.minecraft_arguments.clone()),
        arguments,
        libraries,
        asset_index: child.asset_index.clone().or_else(|| parent.asset_index.clone()),
        assets: child.assets.clone().or_else(|| parent.assets.clone()),
        downloads: child.downloads.clone().or_else(|| parent.downloads.clone()),
        java_version: child.java_version.clone().or_else(|| parent.java_version.clone()),
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedLibrary {
    pub name: String,
    /// Maven path with `/` separators, relative to `shared/libraries`
    pub path: String,
    pub url: String,
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub native: bool,
    pub extract_exclude: Vec<String>,
}

fn with_trailing_slash(value: &str) -> String {
    if value.ends_with('/') {
        value.to_string()
    } else {
        format!("{value}/")
    }
}

pub fn resolve_libraries(json: &VersionJson, ctx: &RuleContext) -> AppResult<Vec<ResolvedLibrary>> {
    let mut out = Vec::new();
    for library in &json.libraries {
        if !rules_allow(library.rules.as_ref(), ctx) {
            continue;
        }
        let exclude = library.extract.as_ref().and_then(|e| e.exclude.clone()).unwrap_or_default();

        if let Some(artifact) = library.downloads.as_ref().and_then(|d| d.artifact.as_ref()) {
            let path = artifact.path.clone().unwrap_or(maven_path(&library.name)?);
            out.push(ResolvedLibrary {
                name: library.name.clone(),
                path,
                url: artifact.url.clone(),
                sha1: artifact.sha1.clone(),
                size: artifact.size,
                native: false,
                extract_exclude: exclude.clone(),
            });
        } else if library.downloads.is_none() && library.natives.is_none() {
            // Maven-style entry (Fabric/Quilt): `url` is the repository base.
            let path = maven_path(&library.name)?;
            let base = with_trailing_slash(library.url.as_deref().unwrap_or("https://libraries.minecraft.net/"));
            out.push(ResolvedLibrary {
                name: library.name.clone(),
                url: format!("{base}{path}"),
                path,
                sha1: library.sha1.clone(),
                size: library.size,
                native: false,
                extract_exclude: exclude.clone(),
            });
        }

        if let Some(natives) = &library.natives {
            if let Some(template) = natives.get(&ctx.platform.name) {
                let classifier = template.replace("${arch}", if ctx.platform.arch == "x86" { "32" } else { "64" });
                if let Some(artifact) = library
                    .downloads
                    .as_ref()
                    .and_then(|d| d.classifiers.as_ref())
                    .and_then(|c| c.get(&classifier))
                {
                    let path = artifact
                        .path
                        .clone()
                        .unwrap_or(maven_path(&format!("{}:{classifier}", library.name))?);
                    out.push(ResolvedLibrary {
                        name: format!("{}:{classifier}", library.name),
                        path,
                        url: artifact.url.clone(),
                        sha1: artifact.sha1.clone(),
                        size: artifact.size,
                        native: true,
                        extract_exclude: exclude.clone(),
                    });
                }
            }
        }
    }
    Ok(out)
}

fn load_inner(versions: &Path, id: &str) -> AppResult<VersionJson> {
    if !is_version_id(id) {
        return Err(AppError::new("VERSION_INVALID", "Некорректный ID версии"));
    }
    let file = versions.join(id).join(format!("{id}.json"));
    let text = fs::read_to_string(&file)
        .map_err(|_| AppError::new("VERSION_MISSING", format!("Нет файла версии {id}")))?;
    let mut json: VersionJson = serde_json::from_str(&text)
        .map_err(|e| AppError::new("VERSION_CORRUPT", format!("Описание версии {id} повреждено: {e}")))?;
    if json.id.is_empty() {
        json.id = id.to_string();
    }
    match json.inherits_from.clone() {
        Some(parent_id) => {
            let parent = load_inner(versions, &parent_id)?;
            Ok(merge_versions(parent, json))
        }
        None => Ok(json),
    }
}

pub fn load_resolved_version(data: &Path, id: &str) -> AppResult<VersionJson> {
    load_inner(&versions_dir(data), id)
}

pub fn client_jar(data: &Path, mc_version: &str) -> PathBuf {
    versions_dir(data).join(mc_version).join(format!("{mc_version}.jar"))
}

/// Unpacks legacy native jars (pre-1.19) into the instance's `.natives` folder.
pub fn extract_natives(libs: &[ResolvedLibrary], libraries_dir: &Path, natives_dir: &Path) -> AppResult<()> {
    fs::create_dir_all(natives_dir)?;
    for library in libs.iter().filter(|lib| lib.native) {
        let jar = libraries_dir.join(library.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let file = fs::File::open(&jar)
            .map_err(|e| AppError::new("NATIVE_MISSING", format!("Нет нативной библиотеки {}: {e}", library.name)))?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| AppError::new("NATIVE_CORRUPT", format!("Нативная библиотека {} повреждена: {e}", library.name)))?;
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|e| AppError::new("NATIVE_CORRUPT", e.to_string()))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            if name.starts_with("META-INF/") || library.extract_exclude.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let target = resolve_inside(natives_dir, &[&name])?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = fs::File::create(&target)?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: serde_json::Value) -> VersionJson {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn merge_prefers_child_and_dedupes_libraries() {
        let parent = version(serde_json::json!({
            "id": "1.21.1", "mainClass": "net.minecraft.client.main.Main",
            "libraries": [{"name": "a:b:1"}, {"name": "c:d:2"}],
            "arguments": {"game": ["--gameDir", "${game_directory}"]}
        }));
        let child = version(serde_json::json!({
            "id": "fabric-loader-0.16.9-1.21.1", "inheritsFrom": "1.21.1", "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
            "libraries": [{"name": "a:b:1"}, {"name": "e:f:3"}],
            "arguments": {"jvm": ["-DFabricMcEmu=net.minecraft.client.main.Main"]}
        }));
        let merged = merge_versions(parent, child);
        assert_eq!(merged.id, "fabric-loader-0.16.9-1.21.1");
        assert_eq!(merged.main_class, "net.fabricmc.loader.impl.launch.knot.KnotClient");
        assert_eq!(merged.libraries.len(), 3);
        assert_eq!(merged.arguments.unwrap().game.unwrap().len(), 2);
    }

    #[test]
    fn rules_respect_the_platform() {
        let ctx = RuleContext { platform: current_platform(), features: HashMap::new() };
        let allow_windows = vec![Rule { action: "allow".into(), os: Some(OsRule { name: Some("windows".into()), ..Default::default() }), features: None }];
        let allow_linux = vec![Rule { action: "allow".into(), os: Some(OsRule { name: Some("linux".into()), ..Default::default() }), features: None }];
        let allowed = if cfg!(windows) { allow_windows.clone() } else { allow_linux.clone() };
        let denied = if cfg!(windows) { allow_linux } else { allow_windows };
        assert!(rules_allow(Some(&allowed), &ctx));
        assert!(!rules_allow(Some(&denied), &ctx));
    }

    #[test]
    fn maven_style_entries_resolve() {
        let json = version(serde_json::json!({
            "id": "x", "mainClass": "M",
            "libraries": [
                {"name": "net.fabricmc:fabric-loader:0.16.9", "url": "https://maven.fabricmc.net/"},
                {"name": "org.lwjgl:lwjgl:3.3.3", "downloads": {"artifact": {"path": "org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar", "url": "https://libraries.minecraft.net/org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar"}}}
            ]
        }));
        let ctx = RuleContext { platform: current_platform(), features: HashMap::new() };
        let libs = resolve_libraries(&json, &ctx).unwrap();
        assert_eq!(libs.len(), 2);
        assert_eq!(libs[0].url, "https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.16.9/fabric-loader-0.16.9.jar");
        assert_eq!(libs[1].path, "org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3.jar");
    }
}
