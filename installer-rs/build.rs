use std::fs;
use std::io::Write;
use std::path::Path;

fn ensure_placeholder_payload() {
    let path = Path::new("payload/payload.chpk");
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Minimal valid CHPK (v1) with one placeholder file so `include_bytes!` compiles in dev.
    let data = b"placeholder";
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    let _ = encoder.write_all(data);
    let comp = encoder.finish().unwrap_or_default();
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    hasher.update(data);
    let hash = format!("{:x}", hasher.finalize());
    let header = serde_json::json!({
        "meta": {
            "version": "0.0.0",
            "productName": "ChaldLauncher",
            "exe": "ChaldLauncher.exe",
            "publisher": "Chald Studio",
            "appId": "studio.chald.launcher"
        },
        "files": [{ "p": "ChaldLauncher.exe", "s": data.len(), "c": comp.len(), "h": hash }],
        "totalSize": data.len()
    });
    let header_bytes = serde_json::to_vec(&header).unwrap_or_default();
    let mut blob = Vec::with_capacity(comp.len() + header_bytes.len() + 12);
    blob.extend_from_slice(&comp);
    blob.extend_from_slice(&header_bytes);
    blob.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
    blob.extend_from_slice(b"CHPK\x01\x00\x00\x00");
    let _ = fs::write(path, blob);
}

fn main() {
    println!("cargo:rerun-if-changed=payload/payload.chpk");
    ensure_placeholder_payload();
    tauri_build::build()
}
