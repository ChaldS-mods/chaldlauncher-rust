use std::collections::HashMap;
use std::path::MAIN_SEPARATOR_STR;

use serde_json::Value;

use crate::errors::{AppError, AppResult};
use crate::version_json::{resolve_libraries, rules_allow, ArgItem, Platform, RuleContext, VersionJson};

#[derive(Debug, Clone)]
pub struct LaunchAuth {
    pub name: String,
    pub uuid: String,
    pub access_token: String,
    /// `msa` or `legacy`
    pub user_type: String,
    #[allow(dead_code)]
    pub offline_fallback: bool,
}

pub struct LaunchInput<'a> {
    pub version: &'a VersionJson,
    pub platform: Platform,
    pub java_path: String,
    pub game_dir: String,
    pub assets_dir: String,
    pub libraries_dir: String,
    pub natives_dir: String,
    pub client_jar: String,
    pub auth: LaunchAuth,
    pub memory_min_mb: u32,
    pub memory_max_mb: u32,
    pub extra_jvm_args: Vec<String>,
    pub resolution: Option<(u32, u32)>,
    pub fullscreen: bool,
    pub launcher_name: String,
    pub launcher_version: String,
    /// Forge/NeoForge need the vanilla version id in `${version_name}`.
    pub version_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LaunchCommand {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub secrets: Vec<String>,
}

/// Splits an argument string honoring simple "double" and 'single' quotes. No shell is involved.
pub fn split_args(input: &str) -> AppResult<Vec<String>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for ch in input.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    has = true;
                } else if ch.is_whitespace() {
                    if has || !current.is_empty() {
                        out.push(current.clone());
                    }
                    current.clear();
                    has = false;
                } else {
                    current.push(ch);
                    has = true;
                }
            }
        }
    }
    if quote.is_some() {
        return Err(AppError::new("ARGS_INVALID", "Незакрытая кавычка в аргументах JVM"));
    }
    if has || !current.is_empty() {
        out.push(current);
    }
    Ok(out)
}

fn flatten(items: &[ArgItem], ctx: &RuleContext) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        match item {
            ArgItem::Plain(value) => out.push(value.clone()),
            ArgItem::Conditional { rules, value } => {
                if !rules_allow(Some(rules), ctx) {
                    continue;
                }
                match value {
                    Value::String(s) => out.push(s.clone()),
                    Value::Array(list) => {
                        for value in list {
                            if let Some(s) = value.as_str() {
                                out.push(s.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

fn substitute(arg: &str, vars: &HashMap<String, String>) -> AppResult<String> {
    let mut out = String::new();
    let mut rest = arg;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(AppError::new("LAUNCH_PLACEHOLDER", format!("Некорректный параметр запуска: {arg}")));
        };
        let key = &after[..end];
        let value = vars
            .get(key)
            .ok_or_else(|| AppError::new("LAUNCH_PLACEHOLDER", format!("Неизвестный параметр запуска: ${{{key}}}")))?;
        out.push_str(value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn classpath(input: &LaunchInput<'_>, libs: &[crate::version_json::ResolvedLibrary]) -> Vec<String> {
    let mut entries: Vec<String> = libs
        .iter()
        .filter(|lib| !lib.native)
        .map(|lib| {
            std::path::Path::new(&input.libraries_dir)
                .join(lib.path.replace('/', MAIN_SEPARATOR_STR))
                .to_string_lossy()
                .to_string()
        })
        .collect();
    entries.push(input.client_jar.clone());
    entries.sort();
    entries.dedup();
    entries
}

pub fn build_launch_command(input: &LaunchInput<'_>) -> AppResult<LaunchCommand> {
    let mut features = HashMap::new();
    features.insert("has_custom_resolution".to_string(), input.resolution.is_some());
    let ctx = RuleContext { platform: input.platform.clone(), features };
    let libs = resolve_libraries(input.version, &ctx)?;
    let classpath = classpath(input, &libs).join(if cfg!(windows) { ";" } else { ":" });

    let version = input.version;
    let assets_index = version
        .assets
        .clone()
        .or_else(|| version.asset_index.as_ref().map(|index| index.id.clone()))
        .unwrap_or_default();
    let version_name = input
        .version_name
        .clone()
        .unwrap_or_else(|| version.id.clone());

    let mut vars = HashMap::new();
    vars.insert("auth_player_name".to_string(), input.auth.name.clone());
    vars.insert("auth_uuid".to_string(), input.auth.uuid.clone());
    vars.insert("auth_access_token".to_string(), input.auth.access_token.clone());
    vars.insert(
        "auth_session".to_string(),
        format!("token:{}:{}", input.auth.access_token, input.auth.uuid),
    );
    vars.insert("auth_xuid".to_string(), String::new());
    vars.insert("clientid".to_string(), String::new());
    vars.insert("user_type".to_string(), input.auth.user_type.clone());
    vars.insert("user_properties".to_string(), "{}".to_string());
    vars.insert("version_name".to_string(), version_name);
    vars.insert("version_type".to_string(), version.version_type.clone().unwrap_or_else(|| "release".to_string()));
    vars.insert("game_directory".to_string(), input.game_dir.clone());
    vars.insert("assets_root".to_string(), input.assets_dir.clone());
    vars.insert("game_assets".to_string(), input.assets_dir.clone());
    vars.insert("assets_index_name".to_string(), assets_index);
    vars.insert("natives_directory".to_string(), input.natives_dir.clone());
    vars.insert("library_directory".to_string(), input.libraries_dir.clone());
    vars.insert("classpath_separator".to_string(), if cfg!(windows) { ";".to_string() } else { ":".to_string() });
    vars.insert("launcher_name".to_string(), input.launcher_name.clone());
    vars.insert("launcher_version".to_string(), input.launcher_version.clone());
    vars.insert("classpath".to_string(), classpath);
    vars.insert(
        "resolution_width".to_string(),
        input.resolution.map(|(w, _)| w.to_string()).unwrap_or_default(),
    );
    vars.insert(
        "resolution_height".to_string(),
        input.resolution.map(|(_, h)| h.to_string()).unwrap_or_default(),
    );

    let (jvm_raw, game_raw) = if let Some(arguments) = &version.arguments {
        (
            flatten(arguments.jvm.as_deref().unwrap_or(&[]), &ctx),
            flatten(arguments.game.as_deref().unwrap_or(&[]), &ctx),
        )
    } else if let Some(legacy) = &version.minecraft_arguments {
        let mut game = split_args(legacy)?;
        if input.resolution.is_some() {
            game.push("--width".to_string());
            game.push("${resolution_width}".to_string());
            game.push("--height".to_string());
            game.push("${resolution_height}".to_string());
        }
        (
            vec![
                "-Djava.library.path=${natives_directory}".to_string(),
                "-Dminecraft.launcher.brand=${launcher_name}".to_string(),
                "-Dminecraft.launcher.version=${launcher_version}".to_string(),
                "-cp".to_string(),
                "${classpath}".to_string(),
            ],
            game,
        )
    } else {
        return Err(AppError::new("LAUNCH_NO_ARGS", "В описании версии нет аргументов запуска"));
    };

    let mut jvm = jvm_raw;
    if !jvm.iter().any(|arg| arg == "-cp" || arg == "-classpath") {
        jvm.push("-cp".to_string());
        jvm.push("${classpath}".to_string());
    }
    let mut game = game_raw;
    if input.fullscreen {
        game.push("--fullscreen".to_string());
    }

    let mut args = vec![
        format!("-Xms{}M", input.memory_min_mb),
        format!("-Xmx{}M", input.memory_max_mb),
    ];
    for arg in jvm {
        args.push(substitute(&arg, &vars)?);
    }
    args.extend(input.extra_jvm_args.iter().cloned());
    args.push(version.main_class.clone());
    for arg in game {
        args.push(substitute(&arg, &vars)?);
    }

    let secrets = if input.auth.access_token.len() >= 8 {
        vec![input.auth.access_token.clone()]
    } else {
        Vec::new()
    };
    Ok(LaunchCommand {
        command: input.java_path.clone(),
        args,
        cwd: input.game_dir.clone(),
        secrets,
    })
}

pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret, "***");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: serde_json::Value) -> VersionJson {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn split_args_handles_quotes() {
        assert_eq!(split_args("-Dfoo=\"a b\" -Xmx2G").unwrap(), vec!["-Dfoo=a b", "-Xmx2G"]);
        assert_eq!(split_args("'one two' three").unwrap(), vec!["one two", "three"]);
        assert!(split_args("\"unterminated").is_err());
    }

    #[test]
    fn builds_a_legacy_command() {
        let version = version(serde_json::json!({
            "id": "1.12.2", "mainClass": "net.minecraft.client.main.Main",
            "minecraftArguments": "--username ${auth_player_name} --gameDir ${game_directory} --version ${version_name}",
            "libraries": []
        }));
        let input = LaunchInput {
            version: &version,
            platform: crate::version_json::current_platform(),
            java_path: "java".into(),
            game_dir: "/games/pack".into(),
            assets_dir: "/shared/assets".into(),
            libraries_dir: "/shared/libraries".into(),
            natives_dir: "/games/pack/.natives".into(),
            client_jar: "/shared/versions/1.12.2/1.12.2.jar".into(),
            auth: LaunchAuth { name: "Steve".into(), uuid: "u".into(), access_token: "0".into(), user_type: "legacy".into(), offline_fallback: false },
            memory_min_mb: 1024,
            memory_max_mb: 4096,
            extra_jvm_args: vec!["-XX:+UseG1GC".into()],
            resolution: Some((1280, 720)),
            fullscreen: false,
            launcher_name: "ChaldLauncher".into(),
            launcher_version: "1.1.0".into(),
            version_name: None,
        };
        let command = build_launch_command(&input).unwrap();
        assert!(command.args.contains(&"-Xmx4096M".to_string()));
        assert!(command.args.contains(&"--username".to_string()));
        assert!(command.args.contains(&"Steve".to_string()));
        assert!(command.args.contains(&"1.12.2".to_string()));
        assert!(command.args.iter().any(|a| a == "--width"));
        assert!(command.secrets.is_empty());
    }

    #[test]
    fn redacts_the_token() {
        assert_eq!(redact("token abc123 end", &["abc123".into()]), "token *** end");
    }
}
