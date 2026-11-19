use std::path::PathBuf;

use chald_installer::payload::{pack_dir, PayloadMeta};

/// Packs a built launcher folder into `payload.chpk` (the format the installer embeds).
///
/// `chald-pack <folder> <out.chpk> [--version V] [--product NAME] [--exe FILE] [--publisher P] [--app-id ID]`
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut meta = PayloadMeta {
        version: env!("CARGO_PKG_VERSION").to_string(),
        product_name: "ChaldLauncher".to_string(),
        exe: "ChaldLauncher.exe".to_string(),
        publisher: "Chald Studio".to_string(),
        app_id: "studio.chald.launcher".to_string(),
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let mut take = |name: &str| -> Option<String> {
            if arg == name {
                let value = args.get(index + 1).cloned();
                index += 1;
                value
            } else {
                arg.strip_prefix(&format!("{name}=")).map(str::to_string)
            }
        };
        if let Some(value) = take("--version") {
            meta.version = value;
        } else if let Some(value) = take("--product") {
            meta.product_name = value;
        } else if let Some(value) = take("--exe") {
            meta.exe = value;
        } else if let Some(value) = take("--publisher") {
            meta.publisher = value;
        } else if let Some(value) = take("--app-id") {
            meta.app_id = value;
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }
    if positional.len() != 2 {
        eprintln!("usage: chald-pack <folder> <out.chpk> [--version V] [--product NAME] [--exe FILE] [--publisher P] [--app-id ID]");
        std::process::exit(2);
    }
    let source = PathBuf::from(&positional[0]);
    let out = PathBuf::from(&positional[1]);
    match pack_dir(&source, &out, &meta, |rel, i, n| {
        if i % 50 == 0 || i + 1 == n {
            eprintln!("[{}/{}] {rel}", i + 1, n);
        }
    }) {
        Ok(header) => println!(
            "packed {} files, {} bytes -> {} ({} bytes on disk)",
            header.files.len(),
            header.total_size,
            out.display(),
            std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0)
        ),
        Err(error) => {
            eprintln!("ошибка: {error}");
            std::process::exit(1);
        }
    }
}
