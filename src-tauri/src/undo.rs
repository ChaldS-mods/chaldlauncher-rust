//! Reversible changes made by the AI agent.
//!
//! Before a mutating tool runs we snapshot the paths it can touch. Small files are copied;
//! large ones are hard-linked when possible, so even a whole instance snapshot stays cheap.
//! The journal lives on disk, so `undo_last` also works after a launcher restart.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::errors::{AppError, AppResult};

const MAX_SNAPSHOT_FILES: usize = 50_000;
const MAX_COPY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DIRECT_COPY_LIMIT: u64 = 8 * 1024 * 1024;
const KEEP_ENTRIES: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    /// Absolute path of the file/directory that was snapshotted.
    pub path: PathBuf,
    /// Name of the backup entry inside the undo folder.
    pub backup: String,
    pub existed: bool,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndoKind {
    Files { changes: Vec<FileChange> },
    StopGame { instance: String },
    StartGame { instance: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEntry {
    pub id: String,
    pub label: String,
    pub created_at: i64,
    pub kind: UndoKind,
}

fn root(data: &Path) -> PathBuf {
    data.join("ai-undo")
}

fn journal_path(data: &Path) -> PathBuf {
    root(data).join("journal.json")
}

fn backup_dir(data: &Path, id: &str) -> PathBuf {
    root(data).join(id)
}

fn new_id() -> String {
    format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>())
}

fn load(data: &Path) -> Vec<UndoEntry> {
    let Ok(bytes) = fs::read(journal_path(data)) else { return Vec::new() };
    serde_json::from_slice::<Vec<UndoEntry>>(&bytes).unwrap_or_default()
}

fn save(data: &Path, entries: &[UndoEntry]) -> AppResult<()> {
    fs::create_dir_all(root(data))?;
    let path = journal_path(data);
    let temp = path.with_extension("tmp");
    fs::write(&temp, serde_json::to_vec(entries)?)?;
    fs::rename(&temp, &path)?;
    Ok(())
}

fn backup_file(src: &Path, dest: &Path) -> AppResult<()> {
    let len = fs::metadata(src).map(|m| m.len()).unwrap_or(u64::MAX);
    if len <= DIRECT_COPY_LIMIT {
        fs::copy(src, dest)?;
        return Ok(());
    }
    // Hard links are instant for jars/game files. All our writers replace files atomically
    // (write temp → rename), so the link keeps the original bytes.
    if fs::hard_link(src, dest).is_ok() {
        return Ok(());
    }
    fs::copy(src, dest)?;
    Ok(())
}

struct Collector<'a> {
    backup_root: &'a Path,
    changes: &'a mut Vec<FileChange>,
    seen: &'a mut HashSet<PathBuf>,
    copied: u64,
}

impl Collector<'_> {
    fn add_path(&mut self, path: &Path) -> AppResult<()> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => {
                // Absent now: undo must delete whatever appears here later.
                self.changes.push(FileChange {
                    path: path.to_path_buf(),
                    backup: String::new(),
                    existed: false,
                    is_dir: true,
                });
                return Ok(());
            }
        };
        if !self.seen.insert(path.to_path_buf()) {
            return Ok(());
        }
        if self.changes.len() >= MAX_SNAPSHOT_FILES {
            return Err(AppError::new("UNDO_TOO_LARGE", "Слишком много файлов для отката"));
        }
        if meta.file_type().is_symlink() {
            return Ok(()); // never follow links outside the instance
        }
        if meta.is_dir() {
            self.changes.push(FileChange {
                path: path.to_path_buf(),
                backup: String::new(),
                existed: true,
                is_dir: true,
            });
            for entry in fs::read_dir(path)? {
                self.add_path(&entry?.path())?;
            }
            return Ok(());
        }
        if !meta.is_file() {
            return Ok(());
        }
        self.copied = self.copied.saturating_add(meta.len());
        if self.copied > MAX_COPY_BYTES {
            return Err(AppError::new("UNDO_TOO_LARGE", "Снимок слишком большой для отката"));
        }
        fs::create_dir_all(self.backup_root)?;
        let name = format!("{:05}", self.changes.len());
        backup_file(path, &self.backup_root.join(&name))?;
        self.changes.push(FileChange {
            path: path.to_path_buf(),
            backup: name,
            existed: true,
            is_dir: false,
        });
        Ok(())
    }
}

/// Snapshot everything under `roots`. Returns `None` when there is nothing to track.
pub fn snapshot(data: &Path, label: &str, roots: &[PathBuf]) -> AppResult<Option<UndoEntry>> {
    if roots.is_empty() {
        return Ok(None);
    }
    // Make roots absolute so the journal is independent of the current directory.
    let absolute: Vec<PathBuf> = roots.iter().map(|p| {
        if p.is_absolute() { p.clone() } else { data.join(p) }
    }).collect();
    let id = new_id();
    let dir = backup_dir(data, &id);
    let mut changes = Vec::new();
    let mut seen = HashSet::new();
    {
        let mut collector = Collector {
            backup_root: &dir,
            changes: &mut changes,
            seen: &mut seen,
            copied: 0,
        };
        for path in &absolute {
            collector.add_path(path)?;
        }
    }
    if changes.is_empty() {
        return Ok(None);
    }
    Ok(Some(UndoEntry {
        id,
        label: label.to_string(),
        created_at: Utc::now().timestamp(),
        kind: UndoKind::Files { changes },
    }))
}

pub fn special(label: &str, kind: UndoKind) -> UndoEntry {
    UndoEntry {
        id: new_id(),
        label: label.to_string(),
        created_at: Utc::now().timestamp(),
        kind,
    }
}

pub fn commit(data: &Path, entry: UndoEntry) -> AppResult<()> {
    let mut entries = load(data);
    entries.push(entry);
    while entries.len() > KEEP_ENTRIES {
        let old = entries.remove(0);
        let _ = fs::remove_dir_all(backup_dir(data, &old.id));
    }
    save(data, &entries)
}

pub fn discard(data: &Path, entry: &UndoEntry) {
    let _ = fs::remove_dir_all(backup_dir(data, &entry.id));
}

pub fn list(data: &Path) -> Value {
    let entries = load(data);
    Value::Array(entries.iter().rev().map(|entry| json!({
        "id": entry.id,
        "label": entry.label,
        "at": entry.created_at,
        "kind": match &entry.kind {
            UndoKind::Files { .. } => "files",
            UndoKind::StopGame { .. } => "stop_game",
            UndoKind::StartGame { .. } => "start_game",
        },
    })).collect())
}

pub fn last(data: &Path) -> AppResult<Option<UndoEntry>> {
    Ok(load(data).pop())
}

fn apply_files(data: &Path, id: &str, changes: &[FileChange]) -> AppResult<()> {
    let dir = backup_dir(data, id);
    let mut sorted: Vec<&FileChange> = changes.iter().collect();
    sorted.sort_by_key(|change| std::cmp::Reverse(change.path.components().count()));

    for change in &sorted {
        if fs::symlink_metadata(&change.path).is_ok() {
            let result = if change.path.is_dir() && !change.path.is_symlink() {
                fs::remove_dir_all(&change.path)
            } else {
                fs::remove_file(&change.path)
            };
            result?;
        }
    }
    for change in changes.iter().filter(|c| c.existed && c.is_dir) {
        fs::create_dir_all(&change.path)?;
    }
    for change in changes.iter().filter(|c| c.existed && !c.is_dir) {
        if let Some(parent) = change.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let source = dir.join(&change.backup);
        if !source.is_file() {
            return Err(AppError::new("UNDO_MISSING", format!("Нет копии файла {}", change.path.display())));
        }
        let temp = change.path.with_extension(format!("undo-{}", change.backup));
        fs::copy(&source, &temp)?;
        fs::rename(&temp, &change.path)?;
    }
    Ok(())
}

/// Restore the newest entry. Game start/stop needs a callback because it lives in AppState.
pub fn undo<F, G>(data: &Path, start_game: F, stop_game: G) -> AppResult<Option<String>>
where
    F: FnOnce(&str) -> AppResult<()>,
    G: FnOnce(&str),
{
    let Some(entry) = last(data)? else { return Ok(None) };
    match &entry.kind {
        UndoKind::Files { changes } => apply_files(data, &entry.id, changes)?,
        UndoKind::StopGame { instance } => stop_game(instance),
        UndoKind::StartGame { instance } => start_game(instance)?,
    }
    let mut entries = load(data);
    entries.pop();
    save(data, &entries)?;
    discard(data, &entry);
    Ok(Some(entry.label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrip() {
        let base = std::env::temp_dir().join(format!("chald-undo-test-{}", new_id()));
        let data = base.join("data");
        let file = data.join("instance").join("a.txt");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "original").unwrap();

        let entry = snapshot(&data, "write", &[file.parent().unwrap().to_path_buf()])
            .unwrap().unwrap();
        commit(&data, entry).unwrap();
        fs::write(&file, "changed").unwrap();
        fs::write(file.parent().unwrap().join("new.txt"), "new").unwrap();

        undo(&data, |_| Ok(()), |_| {}).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "original");
        assert!(!file.parent().unwrap().join("new.txt").exists());
        assert!(last(&data).unwrap().is_none());
        fs::remove_dir_all(&base).unwrap();
    }
}
