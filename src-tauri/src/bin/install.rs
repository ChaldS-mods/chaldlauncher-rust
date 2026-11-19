use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mc = args.first().cloned().unwrap_or_else(|| "1.20.1".to_string());
    let loader = args.get(1).cloned().unwrap_or_else(|| "vanilla".to_string());
    let loader_version = args.get(2).filter(|value| !value.is_empty()).cloned();
    let data = args.get(3).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp/chald-rust-test"));

    let last_bucket = AtomicI32::new(-1);
    let report = |label: &str, fraction: Option<f64>| {
        let bucket = fraction.map(|value| (value * 100.0) as i32 / 5).unwrap_or(-1);
        if last_bucket.swap(bucket, Ordering::Relaxed) != bucket {
            match fraction {
                Some(value) => println!("[{:>3.0}%] {label}", value * 100.0),
                None => println!("[  ? ] {label}"),
            }
        }
    };

    println!("installing {mc} / {loader} {loader_version:?} into {}", data.display());
    let result = tauri::async_runtime::block_on(chaldlauncher_lib::install_test(
        &mc,
        &loader,
        loader_version.as_deref(),
        &data,
        &report,
    ));
    match result {
        Ok(version_id) => println!("OK version_id={version_id}"),
        Err(error) => {
            eprintln!("ERROR {}: {}", error.code, error.message);
            std::process::exit(1);
        }
    }
}
