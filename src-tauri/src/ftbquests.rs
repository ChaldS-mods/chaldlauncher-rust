//! Minimal, safe authoring of FTB Quests SNBT files.
//!
//! Quest data lives in `config/ftbquests/quests/chapters/<HEX_ID>.snbt`, and all player-facing
//! text lives in `config/ftbquests/quests/lang/<locale>.snbt`. We only touch those files.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::errors::{AppError, AppResult};

fn quests_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("config").join("ftbquests").join("quests")
}

fn chapters_dir(game_dir: &Path) -> PathBuf {
    quests_dir(game_dir).join("chapters")
}

fn lang_path(game_dir: &Path, locale: &str) -> PathBuf {
    quests_dir(game_dir).join("lang").join(format!("{locale}.snbt"))
}

fn quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "");
    format!("\"{escaped}\"")
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    let value = value.strip_prefix('"').unwrap_or(value);
    let value = value.strip_suffix('"').unwrap_or(value);
    value.replace("\\n", "\n").replace("\\\"", "\"").replace("\\\\", "\\")
}

fn new_id() -> String {
    format!("{:016X}", rand::random::<u64>())
}

fn is_hex_id(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_or_empty(path: &Path) -> AppResult<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("{\n}\n".to_string()),
        Err(e) => Err(e.into()),
    }
}

fn write_text(path: &Path, text: &str) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("chald.tmp");
    fs::write(&temp, text)?;
    fs::rename(&temp, path)?;
    Ok(())
}

fn text_arg(args: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = args.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    String::new()
}

fn int_arg(args: &Value, keys: &[&str], default: i64) -> i64 {
    for key in keys {
        if let Some(value) = args.get(*key).and_then(Value::as_i64) {
            return value;
        }
        if let Some(value) = args.get(*key).and_then(Value::as_str).and_then(|v| v.parse::<i64>().ok()) {
            return value;
        }
    }
    default
}

fn float_arg(args: &Value, key: &str, default: f64) -> f64 {
    args.get(key)
        .and_then(Value::as_f64)
        .or_else(|| args.get(key).and_then(Value::as_str).and_then(|v| v.parse().ok()))
        .unwrap_or(default)
}

fn field(text: &str, key: &str) -> Option<String> {
    let needle = format!("{key}:");
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&needle) {
            let rest = rest.trim();
            if rest.starts_with('"') {
                return Some(unquote(rest));
            }
        }
    }
    None
}

/// Finds the opening/closing bracket of a newline-separated SNBT array.
fn array_bounds(text: &str, key: &str) -> Option<(usize, usize)> {
    let at = text.find(&format!("{key}:"))?;
    let open = text[at..].find('[')? + at;
    let mut depth = 0i32;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in text[open..].char_indices() {
        if quoted {
            if escaped { escaped = false; }
            else if ch == '\\' { escaped = true; }
            else if ch == '"' { quoted = false; }
            continue;
        }
        match ch {
            '"' => quoted = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 { return Some((open, open + index)); }
            }
            _ => {}
        }
    }
    None
}

fn lang_map(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(": ") {
            if !key.is_empty() && value.starts_with('"') {
                map.insert(key.to_string(), unquote(value));
            }
        }
    }
    map
}

fn lang_set(text: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}: ");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if let Some(line) = lines.iter_mut().find(|line| line.trim_start().starts_with(&prefix)) {
        *line = format!("\t{prefix}{value}");
    } else {
        let at = lines.iter().rposition(|line| line.trim_start().starts_with('}'))
            .unwrap_or_else(|| lines.len().saturating_sub(1));
        lines.insert(at, format!("\t{prefix}{value}"));
    }
    format!("{}\n", lines.join("\n"))
}

fn string_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| quote(v)).collect();
    format!("[{}]", items.join(", "))
}

fn descriptions(args: &Value) -> Vec<String> {
    if let Some(list) = args.get("description").and_then(Value::as_array) {
        return list.iter().filter_map(Value::as_str).map(str::to_string).collect();
    }
    match args.get("description").and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => text.lines().map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

fn chapter_file(game_dir: &Path, id: &str) -> AppResult<PathBuf> {
    if !is_hex_id(id) {
        return Err(AppError::new("AI_ARGS", "Chapter id must be 16 uppercase hex characters."));
    }
    Ok(chapters_dir(game_dir).join(format!("{id}.snbt")))
}

fn resolve_chapter(game_dir: &Path, reference: &str, lang: &HashMap<String, String>) -> AppResult<String> {
    let reference = reference.trim();
    if is_hex_id(reference) && chapter_file(game_dir, reference)?.is_file() {
        return Ok(reference.to_string());
    }
    let lower = reference.to_lowercase();
    let mut matches = Vec::new();
    if let Ok(entries) = fs::read_dir(chapters_dir(game_dir)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("snbt") { continue; }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let id = field(&text, "id").unwrap_or_default();
            let title = lang.get(&format!("chapter.{id}.title")).cloned().unwrap_or_default();
            if id.to_lowercase() == lower || title.to_lowercase() == lower {
                matches.push(id);
            }
        }
    }
    match matches.len() {
        0 => Err(AppError::new("CHAPTER_NOT_FOUND", format!("Глава «{reference}» не найдена. Вызови list_ftb_quests."))),
        1 => Ok(matches.remove(0)),
        _ => Err(AppError::new("CHAPTER_AMBIGUOUS", "Несколько глав с таким названием — используй 16-символьный id.")),
    }
}

fn quest_ids(block: &str) -> Vec<String> {
    let candidates: Vec<(usize, String)> = block.lines().filter_map(|line| {
        let trimmed = line.trim_start();
        let id = trimmed.strip_prefix("id: \"")?;
        Some((line.len() - trimmed.len(), unquote(id)))
    }).collect();
    let Some(min_indent) = candidates.iter().map(|(indent, _)| *indent).min() else {
        return Vec::new();
    };
    candidates.into_iter().filter(|(indent, _)| *indent == min_indent).map(|(_, id)| id).collect()
}

pub fn list(game_dir: &Path) -> AppResult<Value> {
    let lang = lang_map(&read_or_empty(&lang_path(game_dir, "en_us"))?);
    let mut chapters = Vec::new();
    if let Ok(entries) = fs::read_dir(chapters_dir(game_dir)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("snbt") { continue; }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let id = field(&text, "id").unwrap_or_else(|| {
                path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
            });
            let title = lang.get(&format!("chapter.{id}.title")).cloned().unwrap_or_else(|| id.clone());
            let quests = array_bounds(&text, "quests")
                .map(|(open, close)| quest_ids(&text[open + 1..close]))
                .unwrap_or_default();
            let quests: Vec<Value> = quests.iter().map(|quest_id| {
                let name = lang.get(&format!("quest.{quest_id}.title")).cloned().unwrap_or_else(|| quest_id.clone());
                json!([quest_id, name])
            }).collect();
            chapters.push(Value::Array(vec![json!(id), json!(title), json!(quests)]));
        }
    }
    Ok(json!({
        "questsRoot": quests_dir(game_dir).to_string_lossy(),
        "chapterCount": chapters.len(),
        "chapters": chapters,
        "note": "Each chapter is [id, title, [[questId, questTitle], ...]]. Use ids with add_ftb_quest / edit_ftb_quest."
    }))
}

pub fn create_chapter(game_dir: &Path, title: &str, icon: Option<&str>) -> AppResult<Value> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::new("AI_ARGS", "Chapter title is required."));
    }
    fs::create_dir_all(chapters_dir(game_dir))?;
    let count = fs::read_dir(chapters_dir(game_dir))?.filter(|entry| {
        entry.as_ref().map(|e| e.path().extension().and_then(|x| x.to_str()) == Some("snbt")).unwrap_or(false)
    }).count();
    let id = new_id();
    let path = chapter_file(game_dir, &id)?;
    let mut snbt = format!(
        "{{\n\tdefault_hide_dependency_lines: false\n\tdefault_quest_shape: \"\"\n\tfilename: {}\n",
        quote(&id)
    );
    if let Some(icon) = icon.filter(|v| !v.trim().is_empty()) {
        snbt.push_str(&format!("\ticon: {{ id: {} }}\n", quote(icon.trim())));
    }
    snbt.push_str(&format!(
        "\tid: {}\n\timages: [ ]\n\torder_index: {}\n\tquest_links: [ ]\n\tquests: [\n\t]\n}}\n",
        quote(&id), count
    ));
    write_text(&path, &snbt)?;

    let lang_file = lang_path(game_dir, "en_us");
    let lang = read_or_empty(&lang_file)?;
    let lang = lang_set(&lang, &format!("chapter.{id}.title"), &quote(title));
    write_text(&lang_file, &lang)?;

    Ok(json!({
        "ok": true,
        "chapterId": id,
        "title": title,
        "file": path.to_string_lossy(),
        "langFile": lang_file.to_string_lossy(),
        "next": "Call add_ftb_quest(instance, chapter, title, ...)."
    }))
}

fn task_snbt(args: &Value, id: &str) -> AppResult<String> {
    let kind = text_arg(args, &["taskType", "type"]);
    let kind = if kind.is_empty() { "checkmark".to_string() } else { kind.to_lowercase() };
    let head = format!("\t\t\t\t{{\n\t\t\t\t\tid: {}\n", quote(id));
    let body = match kind.as_str() {
        "checkmark" => "\t\t\t\t\ttype: \"checkmark\"\n".to_string(),
        "item" => {
            let item = text_arg(args, &["item", "itemId"]);
            if item.is_empty() { return Err(AppError::new("AI_ARGS", "item is required for item tasks.")); }
            format!(
                "\t\t\t\t\ttype: \"item\"\n\t\t\t\t\titem: {{ count: 1, id: {} }}\n\t\t\t\t\tcount: {}L\n\t\t\t\t\tconsume_items: false\n",
                quote(&item), int_arg(args, &["count", "taskCount"], 1).max(1)
            )
        }
        "kill" => {
            let entity = text_arg(args, &["entity", "entityId"]);
            if entity.is_empty() { return Err(AppError::new("AI_ARGS", "entity is required for kill tasks.")); }
            format!("\t\t\t\t\ttype: \"kill\"\n\t\t\t\t\tentity: {}\n\t\t\t\t\tvalue: {}L\n",
                quote(&entity), int_arg(args, &["count", "value"], 1).max(1))
        }
        "advancement" => {
            let value = text_arg(args, &["advancement"]);
            if value.is_empty() { return Err(AppError::new("AI_ARGS", "advancement is required.")); }
            format!("\t\t\t\t\ttype: \"advancement\"\n\t\t\t\t\tadvancement: {}\n", quote(&value))
        }
        "dimension" => {
            let value = text_arg(args, &["dimension"]);
            if value.is_empty() { return Err(AppError::new("AI_ARGS", "dimension is required.")); }
            format!("\t\t\t\t\ttype: \"dimension\"\n\t\t\t\t\tdimension: {}\n", quote(&value))
        }
        "stage" => {
            let value = text_arg(args, &["stage"]);
            if value.is_empty() { return Err(AppError::new("AI_ARGS", "stage is required.")); }
            format!("\t\t\t\t\ttype: \"stage\"\n\t\t\t\t\tstage: {}\n", quote(&value))
        }
        other => return Err(AppError::new("AI_ARGS", format!("Unknown taskType «{other}». Use checkmark, item, kill, advancement, dimension or stage."))),
    };
    Ok(format!("{head}{body}\t\t\t\t}}"))
}

fn reward_snbt(reward: &Value) -> AppResult<String> {
    let kind = text_arg(reward, &["type"]).to_lowercase();
    let id = new_id();
    let head = format!("\t\t\t\t{{\n\t\t\t\t\tid: {}\n", quote(&id));
    let body = match kind.as_str() {
        "xp" => format!("\t\t\t\t\ttype: \"xp\"\n\t\t\t\t\txp: {}\n", int_arg(reward, &["xp", "value", "count"], 100)),
        "xp_levels" => format!("\t\t\t\t\ttype: \"xp_levels\"\n\t\t\t\t\txp_levels: {}\n", int_arg(reward, &["xpLevels", "value", "count"], 5)),
        "item" => {
            let item = text_arg(reward, &["item", "itemId"]);
            if item.is_empty() { return Err(AppError::new("AI_ARGS", "Reward item is required.")); }
            format!("\t\t\t\t\ttype: \"item\"\n\t\t\t\t\titem: {{ count: 1, id: {} }}\n\t\t\t\t\tcount: {}\n",
                quote(&item), int_arg(reward, &["count"], 1).max(1))
        }
        "command" => {
            let command = text_arg(reward, &["command", "text"]);
            if command.is_empty() { return Err(AppError::new("AI_ARGS", "Reward command is required.")); }
            format!("\t\t\t\t\ttype: \"command\"\n\t\t\t\t\tcommand: {}\n\t\t\t\t\tpermission_level: 2\n", quote(&command))
        }
        "toast" => {
            let text = text_arg(reward, &["description", "text", "message"]);
            if text.is_empty() { return Err(AppError::new("AI_ARGS", "Reward description is required.")); }
            format!("\t\t\t\t\ttype: \"toast\"\n\t\t\t\t\tdescription: {}\n", quote(&text))
        }
        other => return Err(AppError::new("AI_ARGS", format!("Unknown reward type «{other}». Use xp, xp_levels, item, command or toast."))),
    };
    Ok(format!("{head}{body}\t\t\t\t}}"))
}

pub fn add_quest(game_dir: &Path, args: &Value) -> AppResult<Value> {
    let title = text_arg(args, &["title", "name"]);
    if title.is_empty() {
        return Err(AppError::new("AI_ARGS", "Quest title is required."));
    }
    let locale = {
        let value = text_arg(args, &["locale", "lang"]);
        if value.is_empty() { "en_us".to_string() } else { value }
    };
    let lang_file = lang_path(game_dir, &locale);
    let lang_text = read_or_empty(&lang_file)?;
    let lang = lang_map(&lang_text);
    let chapter_ref = text_arg(args, &["chapter", "chapterId"]);
    if chapter_ref.is_empty() {
        return Err(AppError::new("AI_ARGS", "chapter is required (id or title from list_ftb_quests)."));
    }
    let chapter_id = resolve_chapter(game_dir, &chapter_ref, &lang)?;
    let path = chapter_file(game_dir, &chapter_id)?;
    let text = fs::read_to_string(&path)
        .map_err(|_| AppError::new("CHAPTER_NOT_FOUND", "Файл главы не найден. Вызови create_ftb_quests_chapter."))?;
    let (open, close) = array_bounds(&text, "quests")
        .ok_or_else(|| AppError::new("QUEST_FORMAT", "В главе нет массива quests."))?;
    let existing = quest_ids(&text[open + 1..close]);

    let quest_id = new_id();
    let task_id = new_id();
    let mut reward_ids = Vec::new();
    let mut rewards_snbt = Vec::new();
    if let Some(rewards) = args.get("rewards").and_then(Value::as_array) {
        for reward in rewards.iter().take(8) {
            let rendered = reward_snbt(reward)?;
            if let Some(id) = field(&rendered, "id") { reward_ids.push(id); }
            rewards_snbt.push(rendered);
        }
    }

    let deps: Vec<String> = args.get("dependencies").and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let dependencies = string_array(&deps);
    let x = float_arg(args, "x", (existing.len() % 5) as f64 * 2.0 - 4.0);
    let y = float_arg(args, "y", (existing.len() / 5) as f64 * 2.0);
    let tasks = task_snbt(args, &task_id)?;
    let rewards = if rewards_snbt.is_empty() {
        "\t\t\t\trewards: [ ]".to_string()
    } else {
        format!("\t\t\t\trewards: [\n{}\n\t\t\t\t]", rewards_snbt.join("\n"))
    };
    let quest = format!(
        "\t\t{{\n\t\t\tid: {}\n\t\t\tdependencies: {}\n\t\t\tx: {}d\n\t\t\ty: {}d\n\t\t\ttasks: [\n{}\n\t\t\t]\n{}\n\t\t}}\n",
        quote(&quest_id), dependencies, x, y, tasks, rewards
    );
    let mut updated = String::with_capacity(text.len() + quest.len());
    updated.push_str(&text[..close]);
    updated.push_str(&quest);
    updated.push_str(&text[close..]);
    write_text(&path, &updated)?;

    let mut lang = lang_text;
    lang = lang_set(&lang, &format!("quest.{quest_id}.title"), &quote(&title));
    let descriptions = descriptions(args);
    if !descriptions.is_empty() {
        lang = lang_set(&lang, &format!("quest.{quest_id}.quest_desc"), &string_array(&descriptions));
    }
    if let Some(subtitle) = args.get("subtitle").and_then(Value::as_str).filter(|v| !v.trim().is_empty()) {
        lang = lang_set(&lang, &format!("quest.{quest_id}.quest_subtitle"), &quote(subtitle));
    }
    write_text(&lang_file, &lang)?;

    Ok(json!({
        "ok": true,
        "chapterId": chapter_id,
        "questId": quest_id,
        "taskId": task_id,
        "rewardIds": reward_ids,
        "file": path.to_string_lossy(),
        "langFile": lang_file.to_string_lossy(),
        "note": "Quest written. The game reads it on /ftbquests reload or after a restart."
    }))
}

fn quest_block_bounds(text: &str, quest_id: &str) -> Option<(usize, usize)> {
    let marker = format!("id: {}", quote(quest_id));
    let at = text.find(&marker)?;
    // Walk back to the nearest line that is just an opening brace: quests, tasks and rewards
    // are all newline-separated SNBT objects, and their indentation may differ.
    let mut cursor = at;
    let start = loop {
        let line_start = text[..cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let line_end = text[line_start..].find('\n').map(|p| line_start + p).unwrap_or(text.len());
        if text[line_start..line_end].trim() == "{" { break line_start; }
        if line_start == 0 { return None; }
        cursor = line_start - 1;
    };
    let mut depth = 0i32;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in text[start..].char_indices() {
        if quoted {
            if escaped { escaped = false; }
            else if ch == '\\' { escaped = true; }
            else if ch == '"' { quoted = false; }
            continue;
        }
        match ch {
            '"' => quoted = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + index + ch.len_utf8();
                    return Some((start, end));
                }
            }
            _ => {}
        }
    }
    None
}

fn set_field(block: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}:");
    let mut lines: Vec<String> = block.lines().map(str::to_string).collect();
    if let Some(line) = lines.iter_mut().find(|line| line.trim_start().starts_with(&prefix)) {
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        *line = format!("{indent}{key}: {value}");
    }
    format!("{}\n", lines.join("\n"))
}

fn set_array(block: &str, key: &str, inner: &str) -> Option<String> {
    let (open, close) = array_bounds(block, key)?;
    let line_start = block[..open].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let indent: String = block[line_start..open].chars().take_while(|c| c.is_whitespace()).collect();
    let mut out = String::with_capacity(block.len() + inner.len());
    out.push_str(&block[..open]);
    out.push_str("[\n");
    out.push_str(inner);
    out.push('\n');
    out.push_str(&indent);
    out.push(']');
    out.push_str(&block[close + 1..]);
    Some(out)
}

/// Edit an existing quest: text, position, dependencies, task and rewards.
pub fn edit_quest(game_dir: &Path, args: &Value) -> AppResult<Value> {
    let quest_id = text_arg(args, &["questId", "id", "quest"]);
    if quest_id.is_empty() {
        return Err(AppError::new("AI_ARGS", "questId is required (from list_ftb_quests)."));
    }
    let mut found: Option<(PathBuf, String, usize, usize)> = None;
    if let Ok(entries) = fs::read_dir(chapters_dir(game_dir)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("snbt") { continue; }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            if let Some((start, end)) = quest_block_bounds(&text, &quest_id) {
                found = Some((path, text, start, end));
                break;
            }
        }
    }
    let Some((path, text, start, end)) = found else {
        return Err(AppError::new("QUEST_NOT_FOUND", format!("Квест {quest_id} не найден. Вызови list_ftb_quests.")));
    };
    let mut block = text[start..end].to_string();

    let locale = {
        let value = text_arg(args, &["locale", "lang"]);
        if value.is_empty() { "en_us".to_string() } else { value }
    };
    let lang_file = lang_path(game_dir, &locale);
    let lang_text = read_or_empty(&lang_file)?;
    let mut lang = lang_text.clone();
    let title = text_arg(args, &["title", "name"]);
    if !title.is_empty() {
        lang = lang_set(&lang, &format!("quest.{quest_id}.title"), &quote(&title));
    }
    let subtitle = text_arg(args, &["subtitle"]);
    if !subtitle.is_empty() {
        lang = lang_set(&lang, &format!("quest.{quest_id}.quest_subtitle"), &quote(&subtitle));
    }
    let descriptions = descriptions(args);
    if !descriptions.is_empty() {
        lang = lang_set(&lang, &format!("quest.{quest_id}.quest_desc"), &string_array(&descriptions));
    }
    if lang != lang_text {
        write_text(&lang_file, &lang)?;
    }

    if let Some(value) = args.get("dependencies").and_then(Value::as_array) {
        let deps: Vec<String> = value.iter().filter_map(Value::as_str).map(str::to_string).collect();
        block = set_field(&block, "dependencies", &string_array(&deps));
    }
    for key in ["x", "y"] {
        if let Some(value) = args.get(key).and_then(Value::as_f64)
            .or_else(|| args.get(key).and_then(Value::as_str).and_then(|v| v.parse::<f64>().ok()))
        {
            block = set_field(&block, key, &format!("{value}d"));
        }
    }
    let task_keys = ["taskType", "type", "item", "itemId", "count", "taskCount", "entity", "entityId", "advancement", "dimension", "stage"];
    if task_keys.iter().any(|key| args.get(*key).is_some()) {
        let existing_id = array_bounds(&block, "tasks")
            .and_then(|(open, close)| field(&block[open + 1..close], "id"))
            .unwrap_or_else(new_id);
        let task = task_snbt(args, &existing_id)?;
        let inner = task.lines().map(|line| format!("\t\t{line}")).collect::<Vec<_>>().join("\n");
        block = set_array(&block, "tasks", &inner)
            .ok_or_else(|| AppError::new("QUEST_FORMAT", "У квеста нет массива tasks."))?;
    }
    if let Some(rewards) = args.get("rewards").and_then(Value::as_array) {
        let mut rendered = Vec::new();
        for reward in rewards.iter().take(8) {
            rendered.push(reward_snbt(reward)?);
        }
        let inner = if rendered.is_empty() {
            String::new()
        } else {
            rendered.iter().map(|reward| reward.lines().map(|line| format!("\t\t{line}")).collect::<Vec<_>>().join("\n")).collect::<Vec<_>>().join("\n")
        };
        block = set_array(&block, "rewards", &inner)
            .ok_or_else(|| AppError::new("QUEST_FORMAT", "У квеста нет массива rewards."))?;
    }
    let mut updated = String::with_capacity(text.len() + 256);
    updated.push_str(&text[..start]);
    updated.push_str(block.trim_end_matches('\n'));
    updated.push_str(&text[end..]);
    write_text(&path, &updated)?;

    Ok(json!({
        "ok": true,
        "questId": quest_id,
        "file": path.to_string_lossy(),
        "changed": json!({
            "title": !title.is_empty(),
            "subtitle": !subtitle.is_empty(),
            "description": !descriptions.is_empty(),
            "task": task_keys.iter().any(|key| args.get(*key).is_some()),
            "rewards": args.get("rewards").is_some(),
        }),
        "note": "Quest edited. Reload FTB Quests in game or restart it to see changes."
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_bounds_and_lang() {
        let text = "{\n\tquests: [\n\t\t{\n\t\t\tid: \"AB\"\n\t\t}\n\t]\n}\n";
        let (open, close) = array_bounds(text, "quests").unwrap();
        assert_eq!(&text[open..=close], "[\n\t\t{\n\t\t\tid: \"AB\"\n\t\t}\n\t]");
        assert_eq!(quest_ids(&text[open + 1..close]), vec!["AB"]);
        let lang = lang_set("{\n}\n", "quest.AB.title", "\"Hello\"");
        assert_eq!(lang, "{\n\tquest.AB.title: \"Hello\"\n}\n");
    }

    #[test]
    fn edit_quest_roundtrip() {
        let base = std::env::temp_dir().join(format!("chald-ftb-edit-{}", new_id()));
        let chapter = create_chapter(&base, "Глава", None).unwrap();
        let chapter_id = chapter.get("chapterId").unwrap().as_str().unwrap().to_string();
        let added = add_quest(&base, &json!({
            "chapter": chapter_id,
            "title": "Старый квест",
            "taskType": "item",
            "item": "minecraft:coal",
            "count": 4,
            "rewards": [{ "type": "xp", "xp": 10 }]
        })).unwrap();
        let id = added.get("questId").unwrap().as_str().unwrap().to_string();
        edit_quest(&base, &json!({
            "questId": id,
            "title": "Новый квест",
            "description": ["первая строка", "вторая строка"],
            "taskType": "checkmark",
            "rewards": [{ "type": "xp", "xp": 99 }]
        })).unwrap();
        let lang = fs::read_to_string(lang_path(&base, "en_us")).unwrap();
        assert!(lang.contains("quest.") && lang.contains("Новый квест"));
        assert!(lang.contains("вторая строка"));
        let chapter_text = fs::read_to_string(chapters_dir(&base).join(format!("{chapter_id}.snbt"))).unwrap();
        assert!(chapter_text.contains("type: \"checkmark\""));
        assert!(chapter_text.contains("xp: 99"));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn chapter_and_quest_roundtrip() {
        let base = std::env::temp_dir().join(format!("chald-ftb-test-{}", new_id()));
        let chapter = create_chapter(&base, "Начало", Some("minecraft:book")).unwrap();
        let id = chapter.get("chapterId").unwrap().as_str().unwrap().to_string();
        add_quest(&base, &json!({
            "chapter": id,
            "title": "Добычь уголь",
            "taskType": "item",
            "item": "minecraft:coal",
            "count": 8,
            "rewards": [{ "type": "xp", "xp": 50 }]
        })).unwrap();
        let listed = list(&base).unwrap();
        let chapters = listed.get("chapters").unwrap().as_array().unwrap();
        assert_eq!(chapters.len(), 1);
        let quests = chapters[0].get(2).unwrap().as_array().unwrap();
        assert_eq!(quests.len(), 1);
        fs::remove_dir_all(&base).unwrap();
    }
}
