//! `yapi new`: creates a Cargo project for a native extension package from
//! the template in `templates/extension`, which `cargo generate` also reads.

use std::io::Write as _;
use std::path::{Path, PathBuf};

const USAGE: &str = "Usage:\n  yapi new <path> [--name <name>]\n\nCreate a Cargo project for a native extension package in <path>, which must not\nexist or be empty. The package is named after the directory unless --name says\notherwise.\n";

/// The template's files, by path in the project, with cargo-generate's
/// `{{project-name}}` and `{{crate_name}}` placeholders.
const FILES: [(&str, &str); 7] = [
    (
        "Cargo.toml",
        include_str!("../templates/extension/Cargo.toml"),
    ),
    (
        ".cargo/config.toml",
        include_str!("../templates/extension/.cargo/config.toml"),
    ),
    (
        ".gitignore",
        include_str!("../templates/extension/.gitignore"),
    ),
    (
        "README.md",
        include_str!("../templates/extension/README.md"),
    ),
    (
        "extensions/.gitkeep",
        include_str!("../templates/extension/extensions/.gitkeep"),
    ),
    (
        "package.json",
        include_str!("../templates/extension/package.json"),
    ),
    (
        "src/lib.rs",
        include_str!("../templates/extension/src/lib.rs"),
    ),
];

fn out(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

fn err(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

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

/// Runs `yapi new` with `args` (after `new`); the exit code.
pub fn run(args: &[String]) -> u8 {
    let mut path = None;
    let mut name = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                let _ = write!(std::io::stdout(), "{USAGE}");
                return 0;
            }
            "--name" => match rest.next() {
                Some(value) => name = Some(value.clone()),
                None => {
                    err("Error: --name requires a value");
                    return 1;
                }
            },
            _ if path.is_none() && !arg.starts_with('-') => path = Some(PathBuf::from(arg)),
            _ => {
                err("Usage: yapi new <path> [--name <name>]");
                return 1;
            }
        }
    }
    let Some(path) = path else {
        err("Usage: yapi new <path> [--name <name>]");
        return 1;
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
