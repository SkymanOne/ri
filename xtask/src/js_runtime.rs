//! `cargo xtask js-runtime`: build the `ri-js` runtime component and record
//! the hash of its inputs next to it, so CI can reject a stale artifact
//! without a wasm toolchain.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, bail};
use sha2::{Digest as _, Sha256};

/// Files and directories the component is built from.
const INPUTS: [&str; 4] = ["guest/Cargo.toml", "guest/Cargo.lock", "guest/ri-js", "wit"];
const ARTIFACT: &str = "crates/ri-ext/ri-js.wasm";
const RECORD: &str = "crates/ri-ext/ri-js.wasm.inputs";
const TARGET: &str = "wasm32-wasip2";

/// Build `crates/ri-ext/ri-js.wasm` from `guest/`.
///
/// Needs the `wasm32-wasip2` Rust target (`rustup target add wasm32-wasip2`)
/// and a WASI SDK for QuickJS's C sources, found through `--wasi-sdk` or
/// `WASI_SDK_PATH`.
#[derive(clap::Args)]
pub struct Args {
    /// Only check that the committed artifact was built from the current
    /// inputs.
    #[arg(long)]
    check: bool,
    /// The WASI SDK directory.
    #[arg(long, env = "WASI_SDK_PATH")]
    wasi_sdk: Option<PathBuf>,
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let inputs = inputs_hash()?;
    if args.check {
        let record = fs::read_to_string(RECORD).with_context(|| format!("reading {RECORD}"))?;
        let artifact = file_hash(Path::new(ARTIFACT))?;
        if record != format_record(&inputs, &artifact) {
            eprintln!(
                "{ARTIFACT} is stale or was changed by hand: run `cargo xtask js-runtime` and commit the result"
            );
            return Ok(ExitCode::FAILURE);
        }
        eprintln!("{ARTIFACT} matches its inputs");
        return Ok(ExitCode::SUCCESS);
    }

    let Some(sdk) = args.wasi_sdk else {
        bail!(
            "set WASI_SDK_PATH or pass --wasi-sdk to a WASI SDK (https://github.com/WebAssembly/wasi-sdk)"
        );
    };
    let target_dir = Path::new("target/guest").canonicalize().or_else(|_| {
        fs::create_dir_all("target/guest")?;
        Path::new("target/guest").canonicalize()
    })?;
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release", "--locked", "--target", TARGET])
        .current_dir("guest")
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("WASI_SDK_PATH", &sdk)
        .env("CC_wasm32_wasip2", sdk.join("bin/clang"))
        .env("AR_wasm32_wasip2", sdk.join("bin/llvm-ar"))
        .env(
            "CFLAGS_wasm32_wasip2",
            format!("--sysroot={}", sdk.join("share/wasi-sysroot").display()),
        )
        .status()
        .context("running cargo build for the guest")?;
    if !status.success() {
        bail!("building the guest failed");
    }
    fs::copy(target_dir.join(TARGET).join("release/ri_js.wasm"), ARTIFACT)?;
    let artifact = file_hash(Path::new(ARTIFACT))?;
    fs::write(RECORD, format_record(&inputs, &artifact))?;
    eprintln!(
        "wrote {ARTIFACT} ({} bytes) and {RECORD}",
        fs::metadata(ARTIFACT)?.len()
    );
    Ok(ExitCode::SUCCESS)
}

fn format_record(inputs: &str, artifact: &str) -> String {
    format!("inputs {inputs}\nartifact {artifact}\n")
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn file_hash(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex(&Sha256::digest(bytes)))
}

/// Hashes every input file's path and contents, in path order. Build output
/// directories are skipped.
fn inputs_hash() -> anyhow::Result<String> {
    let mut files = Vec::new();
    for input in INPUTS {
        collect(Path::new(input), &mut files)?;
    }
    files.sort();
    let mut digest = Sha256::new();
    for file in files {
        let path = file.to_string_lossy().replace('\\', "/");
        digest.update(path.as_bytes());
        digest.update([0]);
        // Line endings may differ between checkouts.
        let text = fs::read(&file)?;
        let text: Vec<u8> = text.into_iter().filter(|byte| *byte != b'\r').collect();
        digest.update(&text);
        digest.update([0]);
    }
    Ok(hex(&digest.finalize()))
}

fn collect(path: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        if entry.file_name() == "target" {
            continue;
        }
        collect(&entry.path(), files)?;
    }
    Ok(())
}
