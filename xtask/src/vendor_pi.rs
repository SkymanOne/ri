//! `cargo xtask vendor-pi`: bundle the npm packages pi extensions import and
//! ri provides, `typebox` and `@earendil-works/pi-tui`, into
//! `guest/ri-js/js/vendor`. The bundles share chunks, so each package is
//! included once. Also copies pi's HTML export template, unchanged, into
//! `crates/ri/assets/export-html`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

/// The pinned esbuild release.
const ESBUILD: &str = "esbuild@0.25.10";
const VENDOR: &str = "guest/ri-js/js/vendor";
/// Where the fixture generator's `npm ci` installs pi and its dependencies.
const NODE_MODULES: &str =
    "tests/fixtures/pi/generator/node_modules/@earendil-works/pi-coding-agent/node_modules";
/// Module name in the runtime, and what it re-exports.
const ENTRIES: [(&str, &str); 4] = [
    ("typebox", "typebox"),
    ("typebox-value", "typebox/value"),
    ("typebox-compile", "typebox/compile"),
    ("pi-tui", "@earendil-works/pi-tui"),
];
/// pi's HTML export template in the installed package, and where ri keeps it.
const EXPORT_TEMPLATE: &str = "tests/fixtures/pi/generator/node_modules/@earendil-works/pi-coding-agent/dist/core/export-html";
const EXPORT_ASSETS: &str = "crates/ri/assets/export-html";
const EXPORT_FILES: [&str; 5] = [
    "template.html",
    "template.css",
    "template.js",
    "vendor/marked.min.js",
    "vendor/highlight.min.js",
];

/// Node built-ins the bundles import by their bare names.
const BARE_BUILTINS: [&str; 7] = ["events", "fs", "path", "os", "child_process", "util", "url"];

/// Regenerate the vendored bundles, then rebuild the runtime with
/// `cargo xtask js-runtime`. Install the sources first with `npm ci` in
/// `tests/fixtures/pi/generator`; needs Node and network access for `npx`.
#[derive(clap::Args)]
pub struct Args {}

pub fn run(_args: Args) -> anyhow::Result<()> {
    let modules = Path::new(NODE_MODULES).canonicalize().with_context(|| {
        format!("{NODE_MODULES} is missing; run npm ci in tests/fixtures/pi/generator")
    })?;
    // A fixed path: esbuild's output names its inputs, so this keeps it stable.
    let scratch = Path::new("target/vendor-pi").to_path_buf();
    let _ = fs::remove_dir_all(&scratch);
    let entries_dir = scratch.join("entries");
    fs::create_dir_all(&entries_dir)?;
    let mut entry_paths: Vec<PathBuf> = Vec::new();
    for (name, module) in ENTRIES {
        let path = entries_dir.join(format!("{name}.mjs"));
        fs::write(&path, format!("export * from '{module}';\n"))?;
        entry_paths.push(path);
    }
    let out = scratch.join("out");
    let mut command = Command::new("npx");
    command
        .args(["--yes", ESBUILD])
        .args(&entry_paths)
        .args([
            "--bundle",
            "--splitting",
            "--format=esm",
            "--platform=neutral",
            "--main-fields=module,main",
            "--conditions=import,default",
            "--external:node:*",
            "--legal-comments=inline",
            "--target=es2022",
            "--out-extension:.js=.mjs",
            "--chunk-names=chunk-[hash]",
            "--log-level=warning",
        ])
        .arg(format!("--outdir={}", out.display()))
        .env("NODE_PATH", &modules);
    for builtin in BARE_BUILTINS {
        command.arg(format!("--external:{builtin}"));
    }
    let status = command.status().context("running esbuild through npx")?;
    if !status.success() {
        bail!("esbuild failed");
    }
    // Replace the bundles; the license notes stay.
    for entry in fs::read_dir(VENDOR)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "mjs") {
            fs::remove_file(path)?;
        }
    }
    let mut written = 0;
    for entry in fs::read_dir(&out)? {
        let path = entry?.path();
        if let Some(name) = path.file_name() {
            fs::copy(&path, Path::new(VENDOR).join(name))?;
            written += 1;
        }
    }
    let _ = fs::remove_dir_all(&scratch);
    for file in EXPORT_FILES {
        let target = Path::new(EXPORT_ASSETS).join(file);
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::copy(Path::new(EXPORT_TEMPLATE).join(file), &target)
            .with_context(|| format!("copying the export template's {file}"))?;
    }
    eprintln!(
        "wrote {written} modules to {VENDOR} and the export template to {EXPORT_ASSETS}; now run cargo xtask js-runtime"
    );
    Ok(())
}
