//! `yapi new`: creates a Cargo project for a native extension package from
//! the template in `templates/extension`, which `cargo generate` also reads.

use std::path::{Path, PathBuf};

use crate::{err, out};

/// The template's files, by path in the project, with cargo-generate's
/// `{{project-name}}` and `{{crate_name}}` placeholders.
macro_rules! template {
    ($($path:literal),* $(,)?) => {
        [$(($path, include_str!(concat!("../templates/extension/", $path)))),*]
    };
}
const FILES: [(&str, &str); 7] = template![
    "Cargo.toml",
    ".cargo/config.toml",
    ".gitignore",
    "README.md",
    "extensions/.gitkeep",
    "package.json",
    "src/lib.rs",
];

/// Whether `name` can name a Cargo package: ASCII letters, digits, `-` and
/// `_`, starting with a letter or `_`.
fn valid_name(name: &str) -> bool {
    name.starts_with(|first: char| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Writes the template into `dir` for the package `name`.
fn create(dir: &Path, name: &str) -> std::io::Result<()> {
    let crate_name = name.replace('-', "_");
    for (path, contents) in FILES {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = contents
            .replace("{{project-name}}", name)
            .replace("{{crate_name}}", &crate_name);
        std::fs::write(path, text)?;
    }
    Ok(())
}

/// Create a Cargo project for a native extension package in PATH, which must
/// not exist or be empty.
#[derive(clap::Parser)]
struct NewArgs {
    /// Where to create the project.
    path: PathBuf,
    /// The package name; the directory's name by default.
    #[arg(long, allow_hyphen_values = true)]
    name: Option<String>,
}

/// Runs `yapi new` with `args` (after `new`); the exit code.
pub fn run(args: &[String]) -> u8 {
    let NewArgs { path, name } = match crate::parse_command("yapi new", args) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let dir = match std::path::absolute(&path) {
        Ok(dir) => dir,
        Err(error) => {
            err(&format!("Error: {}: {error}", path.display()));
            return 1;
        }
    };
    let Some(name) = name.or_else(|| Some(dir.file_name()?.to_str()?.to_owned())) else {
        err(&format!(
            "Error: cannot name a package after {}. Pass --name.",
            path.display()
        ));
        return 1;
    };
    if !valid_name(&name) {
        err(&format!(
            "Error: \"{name}\" is not a valid package name. Use ASCII letters, digits, - and _, starting with a letter."
        ));
        return 1;
    }
    let empty = std::fs::read_dir(&dir).map(|mut entries| entries.next().is_none());
    if dir.exists() && !matches!(empty, Ok(true)) {
        err(&format!("Error: {} already exists", path.display()));
        return 1;
    }
    if let Err(error) = create(&dir, &name) {
        err(&format!("Error: writing {}: {error}", dir.display()));
        return 1;
    }
    let crate_name = name.replace('-', "_");
    out(&format!(
        "Created native extension package \"{name}\" in {}\n\nNext:",
        path.display()
    ));
    if path != Path::new(".") {
        out(&format!("  cd {}", path.display()));
    }
    out("  rustup target add wasm32-wasip2");
    out("  cargo build --release");
    out(&format!(
        "  cp target/wasm32-wasip2/release/{crate_name}.wasm extensions/"
    ));
    out("  yapi -e .");
    0
}
