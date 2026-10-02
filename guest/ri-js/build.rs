//! Generates the table of embedded ES modules: `js/pi/<name>.mjs` becomes
//! `ri:pi/<name>` and `js/vendor/<name>.mjs` becomes `ri:vendor/<name>`.

use std::fmt::Write as _;
use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("js");
    let mut table = String::from("const MODULES: &[(&str, &str)] = &[\n");
    for dir in ["pi", "vendor"] {
        let dir_path = root.join(dir);
        println!("cargo:rerun-if-changed={}", dir_path.display());
        let mut files: Vec<_> = std::fs::read_dir(&dir_path)
            .expect("module directory")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "mjs"))
            .collect();
        files.sort();
        for path in files {
            let stem = path.file_stem().expect("file name").to_string_lossy();
            println!("cargo:rerun-if-changed={}", path.display());
            writeln!(
                table,
                "    (\"ri:{dir}/{stem}\", include_str!({:?})),",
                path.display().to_string()
            )
            .expect("write to string");
        }
    }
    table.push_str("];\n");
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("modules.rs");
    std::fs::write(out, table).expect("write module table");
}
