#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// The launcher files, packed by `chald-pack` into the old `.chpk` container.
static PAYLOAD: &[u8] = include_bytes!("../payload/payload.chpk");

fn main() {
    let cli = chald_installer::cli::parse(std::env::args().skip(1));
    if cli.silent {
        std::process::exit(chald_installer::silent::run(PAYLOAD, &cli));
    }
    chald_installer::run_gui(PAYLOAD.to_vec(), cli);
}
