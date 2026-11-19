use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// "CHPK", version 1. The same container the old Electron installer used:
/// `[file 1, deflate][file 2, deflate]...[JSON header][u32 LE header length][MAGIC 8 bytes]`
pub const MAGIC: &[u8; 8] = b"CHPK\x01\x00\x00\x00";
const TRAILER: usize = 12;
const MAX_HEADER: usize = 16 * 1024 * 1024;
const MAX_FILES: usize = 200_000;
const MAX_TOTAL: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadMeta {
    pub version: String,
    pub product_name: String,
    pub exe: String,
    pub publisher: String,
    pub app_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadFile {
    /// relative path with `/`
    pub p: String,
    /// size before compression
    pub s: u64,
    /// size in the archive
    pub c: u64,
    /// sha256 of the content, hex
    pub h: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadHeader {
    pub meta: PayloadMeta,
    pub files: Vec<PayloadFile>,
    pub total_size: u64,
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn bad<T>(what: &str) -> Result<T, String> {
    Err(format!("Файл установщика повреждён ({what})"))
}

fn is_hex_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// A relative path inside the payload: no escapes, absolute paths or names Windows forbids.
pub fn check_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > 400 {
        return Err("Некорректный путь в архиве".to_string());
    }
    if path.starts_with('/') || path.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*' | '\\' | ':')) {
        return Err(format!("Недопустимый путь в архиве: {path}"));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("Недопустимый путь в архиве: {path}"));
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(format!("Недопустимый путь в архиве: {path}"));
        }
        let base = segment.split('.').next().unwrap_or(segment).to_ascii_lowercase();
        let reserved = matches!(base.as_str(), "con" | "prn" | "aux" | "nul")
            || (base.is_ascii()
                && base.len() == 4
                && (base.starts_with("com") || base.starts_with("lpt"))
                && base.as_bytes()[3].is_ascii_digit()
                && base.as_bytes()[3] != b'0');
        if reserved {
            return Err(format!("Недопустимый путь в архиве: {path}"));
        }
    }
    Ok(())
}

/// Every check the old `validateHeader` made, plus a guard that the sizes match the data region.
pub fn validate_header(raw: &Value, data_bytes: u64) -> Result<PayloadHeader, String> {
    let object = match raw.as_object() {
        Some(object) => object,
        None => return bad("заголовок"),
    };
    let meta_raw = match object.get("meta").and_then(Value::as_object) {
        Some(meta) => meta,
        None => return bad("meta"),
    };
    let mut field = |key: &str| -> Result<String, String> {
        match meta_raw.get(key).and_then(Value::as_str) {
            Some(value) if !value.is_empty() && value.len() <= 128 => Ok(value.to_string()),
            _ => bad(&format!("meta.{key}")),
        }
    };
    let version = field("version")?;
    let product_name = field("productName")?;
    let exe = field("exe")?;
    let publisher = field("publisher")?;
    let app_id = field("appId")?;

    if !product_name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ' ' | '-')) {
        return bad("meta.productName");
    }
    let exe_ok = exe.to_ascii_lowercase().ends_with(".exe")
        && exe.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ' ' | '-'));
    if !exe_ok {
        return bad("meta.exe");
    }

    let files_raw = match object.get("files").and_then(Value::as_array) {
        Some(files) if !files.is_empty() && files.len() <= MAX_FILES => files,
        _ => return bad("files"),
    };

    let mut seen = std::collections::HashSet::new();
    let mut files = Vec::with_capacity(files_raw.len());
    let mut sum_s: u64 = 0;
    let mut sum_c: u64 = 0;
    for raw in files_raw {
        let item = match raw.as_object() {
            Some(item) => item,
            None => return bad("file"),
        };
        let p = match item.get("p").and_then(Value::as_str) {
            Some(p) => p.to_string(),
            None => return bad("file.p"),
        };
        check_rel_path(&p)?;
        if !seen.insert(p.to_ascii_lowercase()) {
            return bad(&format!("дубликат {p}"));
        }
        let s = item.get("s").and_then(Value::as_u64);
        let c = item.get("c").and_then(Value::as_u64);
        let (s, c) = match (s, c) {
            (Some(s), Some(c)) if c >= 1 => (s, c),
            _ => return bad(&format!("размер {p}")),
        };
        let h = match item.get("h").and_then(Value::as_str) {
            Some(h) if is_hex_sha256(h) => h.to_string(),
            _ => return bad(&format!("хэш {p}")),
        };
        sum_s = sum_s.saturating_add(s);
        sum_c = sum_c.saturating_add(c);
        files.push(PayloadFile { p, s, c, h });
    }
    if sum_s > MAX_TOTAL {
        return bad("слишком большой размер");
    }
    if sum_c != data_bytes {
        return bad("размеры не сходятся");
    }
    Ok(PayloadHeader {
        meta: PayloadMeta { version, product_name, exe, publisher, app_id },
        files,
        total_size: sum_s,
    })
}

pub fn read_header_bytes(bytes: &[u8]) -> Result<PayloadHeader, String> {
    if bytes.len() < TRAILER + 2 {
        return bad("слишком короткий");
    }
    let tail = &bytes[bytes.len() - TRAILER..];
    if tail[4..12] != MAGIC[..] {
        return bad("неверная подпись");
    }
    let header_len = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]) as usize;
    if header_len == 0 || header_len > MAX_HEADER || header_len > bytes.len() - TRAILER {
        return bad("заголовок");
    }
    let data_len = bytes.len() - TRAILER - header_len;
    let json: Value = serde_json::from_slice(&bytes[data_len..data_len + header_len])
        .map_err(|_| "Файл установщика повреждён (заголовок)".to_string())?;
    validate_header(&json, data_len as u64)
}

pub fn read_header_file(file: &Path) -> Result<PayloadHeader, String> {
    let bytes = fs::read(file).map_err(|e| format!("Не удалось прочитать файл установщика: {e}"))?;
    read_header_bytes(&bytes)
}

fn resolve_inside(base: &Path, rel: &str) -> Result<PathBuf, String> {
    check_rel_path(rel)?;
    let mut out = base.to_path_buf();
    for segment in rel.split('/') {
        out.push(segment);
    }
    Ok(out)
}

/// Unpacks the payload into `dest`. `on_progress(done, total, file)` is throttled to ~25 events per second.
pub fn extract_bytes(
    bytes: &[u8],
    header: &PayloadHeader,
    dest: &Path,
    cancel: &std::sync::atomic::AtomicBool,
    mut on_progress: impl FnMut(u64, u64, &str),
) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let mut offset: usize = 0;
    let mut done: u64 = 0;
    let total = header.total_size.max(1);
    let mut last_emit = Instant::now();

    for file in &header.files {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Установка отменена".to_string());
        }
        let target = resolve_inside(dest, &file.p)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let start = offset;
        let end = start
            .checked_add(file.c as usize)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| format!("Файл повреждён: {}", file.p))?;
        offset = end;

        let mut decoder = DeflateDecoder::new(&bytes[start..end]);
        let mut out = File::create(&target).map_err(|e| format!("Не удалось записать {}: {e}", file.p))?;
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("Установка отменена".to_string());
            }
            let read = decoder.read(&mut buffer).map_err(|_| format!("Файл повреждён: {}", file.p))?;
            if read == 0 {
                break;
            }
            written += read as u64;
            if written > file.s {
                return Err(format!("Файл повреждён: {}", file.p));
            }
            hasher.update(&buffer[..read]);
            out.write_all(&buffer[..read]).map_err(|e| format!("Не удалось записать {}: {e}", file.p))?;
            done += read as u64;
            if last_emit.elapsed().as_millis() >= 40 || done >= total {
                last_emit = Instant::now();
                on_progress(done.min(total), total, &file.p);
            }
        }
        out.flush().map_err(|e| format!("Не удалось записать {}: {e}", file.p))?;
        if written != file.s || hex(&hasher.finalize()) != file.h {
            return Err(format!("Файл повреждён: {}", file.p));
        }
    }
    on_progress(total, total, "");
    Ok(())
}

fn list_files(root: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    fn walk(root: &Path, rel: &str, out: &mut Vec<String>) -> Result<(), String> {
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .map_err(|e| format!("Не удалось прочитать {}: {e}", dir.display()))?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let next = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let ty = entry.file_type().map_err(|e| e.to_string())?;
            if ty.is_symlink() {
                return Err(format!("Символические ссылки не поддерживаются: {next}"));
            }
            if ty.is_dir() {
                walk(root, &next, out)?;
            } else if ty.is_file() {
                check_rel_path(&next)?;
                out.push(next);
            }
        }
        Ok(())
    }
    walk(root, "", &mut out)?;
    Ok(out)
}

struct CountingWriter<W: Write> {
    inner: W,
    count: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.count += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Packs a directory into `.chpk` (one pass, the header goes last).
pub fn pack_dir(src: &Path, out: &Path, meta: &PayloadMeta, mut on_file: impl FnMut(&str, usize, usize)) -> Result<PayloadHeader, String> {
    let files = list_files(src)?;
    if files.is_empty() {
        return Err(format!("Папка пуста: {}", src.display()));
    }
    if !files.iter().any(|f| f.eq_ignore_ascii_case(&meta.exe)) {
        return Err(format!("В папке нет {}", meta.exe));
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut sink = File::create(out).map_err(|e| format!("Не удалось создать {}: {e}", out.display()))?;
    let mut entries = Vec::with_capacity(files.len());
    let mut total = 0u64;
    for (index, rel) in files.iter().enumerate() {
        on_file(rel, index, files.len());
        let path = src.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let mut source = File::open(&path).map_err(|e| format!("Не удалось прочитать {}: {e}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let counter = CountingWriter { inner: &mut sink, count: 0 };
        let mut encoder = DeflateEncoder::new(counter, Compression::new(6));
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer).map_err(|e| e.to_string())?;
            if read == 0 {
                break;
            }
            size += read as u64;
            hasher.update(&buffer[..read]);
            encoder.write_all(&buffer[..read]).map_err(|e| e.to_string())?;
        }
        let counter = encoder.finish().map_err(|e| e.to_string())?;
        entries.push(PayloadFile { p: rel.clone(), s: size, c: counter.count, h: hex(&hasher.finalize()) });
        total += size;
    }
    let header = PayloadHeader { meta: meta.clone(), files: entries, total_size: total };
    let json = serde_json::to_vec(&header).map_err(|e| e.to_string())?;
    sink.write_all(&json).map_err(|e| e.to_string())?;
    sink.write_all(&(json.len() as u32).to_le_bytes()).map_err(|e| e.to_string())?;
    sink.write_all(MAGIC).map_err(|e| e.to_string())?;
    sink.flush().map_err(|e| e.to_string())?;
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> PayloadMeta {
        PayloadMeta {
            version: "1.1.0".into(),
            product_name: "ChaldLauncher".into(),
            exe: "ChaldLauncher.exe".into(),
            publisher: "Chald Studio".into(),
            app_id: "studio.chald.launcher".into(),
        }
    }

    #[test]
    fn pack_and_extract_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("data")).unwrap();
        fs::write(src.join("ChaldLauncher.exe"), b"launcher binary").unwrap();
        fs::write(src.join("data/config.json"), b"{\"a\":1}").unwrap();
        let out = dir.path().join("payload.chpk");
        let header = pack_dir(&src, &out, &meta(), |_, _, _| {}).unwrap();
        assert_eq!(header.files.len(), 2);
        assert_eq!(header.total_size, 15 + 7);

        let bytes = fs::read(&out).unwrap();
        let read = read_header_bytes(&bytes).unwrap();
        assert_eq!(read.meta.product_name, "ChaldLauncher");
        let dest = dir.path().join("out");
        let cancel = std::sync::atomic::AtomicBool::new(false);
        extract_bytes(&bytes, &read, &dest, &cancel, |_, _, _| {}).unwrap();
        assert_eq!(fs::read(dest.join("ChaldLauncher.exe")).unwrap(), b"launcher binary");
        assert_eq!(fs::read(dest.join("data/config.json")).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn bad_magic_and_traversal_are_rejected() {
        let mut bytes = b"x".repeat(32);
        bytes.extend_from_slice(&(2u32).to_le_bytes());
        bytes.extend_from_slice(b"CHPK\x01\x00\x00\x00");
        assert!(read_header_bytes(&bytes).is_err());

        let header = serde_json::json!({
            "meta": {"version":"1","productName":"P","exe":"P.exe","publisher":"X","appId":"a"},
            "files": [{"p":"../evil","s":1,"c":1,"h":"0".repeat(64)}],
            "totalSize": 1
        });
        assert!(validate_header(&header, 1).is_err());
        let header = serde_json::json!({
            "meta": {"version":"1","productName":"P","exe":"P.exe","publisher":"X","appId":"a"},
            "files": [{"p":"a.txt","s":1,"c":1,"h":"0".repeat(64)},{"p":"A.TXT","s":1,"c":1,"h":"0".repeat(64)}],
            "totalSize": 2
        });
        assert!(validate_header(&header, 2).is_err());
    }

    #[test]
    fn cancel_stops_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("ChaldLauncher.exe"), vec![1u8; 1024]).unwrap();
        let out = dir.path().join("payload.chpk");
        pack_dir(&src, &out, &meta(), |_, _, _| {}).unwrap();
        let bytes = fs::read(&out).unwrap();
        let header = read_header_bytes(&bytes).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let err = extract_bytes(&bytes, &header, &dir.path().join("out"), &cancel, |_, _, _| {}).unwrap_err();
        assert!(err.contains("отменена"));
    }
}
