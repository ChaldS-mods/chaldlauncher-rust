use std::path::PathBuf;

/// Command line of the installer: `--silent` (`/S`), `--dir <path>`, `--no-desktop`, `--no-start-menu`, `--launch`.
#[derive(Debug, Clone)]
pub struct Cli {
    pub silent: bool,
    pub dir: Option<PathBuf>,
    pub desktop: bool,
    pub start_menu: bool,
    pub launch: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self { silent: false, dir: None, desktop: true, start_menu: true, launch: false }
    }
}

pub fn parse(args: impl Iterator<Item = String>) -> Cli {
    let mut cli = Cli::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--silent" | "/S" | "/s" => cli.silent = true,
            "--no-desktop" => cli.desktop = false,
            "--no-start-menu" => cli.start_menu = false,
            "--launch" => cli.launch = true,
            "--dir" => {
                if let Some(value) = args.next() {
                    cli.dir = Some(PathBuf::from(value));
                }
            }
            other => {
                if let Some(value) = other.strip_prefix("--dir=") {
                    cli.dir = Some(PathBuf::from(value));
                }
            }
        }
    }
    cli
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_silent_contract() {
        let cli = parse(
            ["--silent", "--dir", "C:\\Games\\Chald", "--no-desktop", "--launch"]
                .iter()
                .map(|s| s.to_string()),
        );
        assert!(cli.silent);
        assert_eq!(cli.dir.unwrap().to_string_lossy(), "C:\\Games\\Chald");
        assert!(!cli.desktop);
        assert!(cli.start_menu);
        assert!(cli.launch);

        let cli = parse(["/S", "--dir=D:\\X"].iter().map(|s| s.to_string()));
        assert!(cli.silent);
        assert_eq!(cli.dir.unwrap().to_string_lossy(), "D:\\X");
    }
}
