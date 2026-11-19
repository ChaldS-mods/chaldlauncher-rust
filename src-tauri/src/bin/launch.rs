use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data = args.first().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp/chald-rust-test"));
    let mc = args.get(1).cloned().unwrap_or_else(|| "1.20.1".to_string());
    let version_id = args.get(2).cloned().unwrap_or_else(|| mc.clone());
    let mut child = match chaldlauncher_lib::launch_test(&data, &mc, &version_id) {
        Ok(child) => child,
        Err(error) => {
            eprintln!("ERROR {}: {}", error.code, error.message);
            std::process::exit(1);
        }
    };
    println!("pid {}", child.id());
    std::thread::sleep(Duration::from_secs(30));
    match child.try_wait() {
        Ok(Some(status)) => println!("exited: {:?}", status.code()),
        Ok(None) => {
            println!("still running after 30s -> stopping");
            let _ = child.kill();
        }
        Err(error) => eprintln!("wait error: {error}"),
    }
}
