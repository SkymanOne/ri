//! `cargo xtask js-runtime`: build the `yapi-js` runtime component and record
//! the hash of its inputs next to it, so CI can reject a stale artifact
//! without a wasm toolchain.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, bail};
use sha2::{Digest as _, Sha256};
use yapi_types::time::hex;

/// Files and directories the JS runtime is built from.
const RUNTIME_INPUTS: [&str; 4] = [
    "guest/Cargo.toml",
    "guest/Cargo.lock",
    "guest/yapi-js",
    "wit",
];
/// Files and directories the examples are built from, with the crates the
/// SDK's `widgets` feature takes from yapi's workspace.
const EXAMPLE_INPUTS: [&str; 10] = [
    "guest/Cargo.toml",
    "guest/Cargo.lock",
    "guest/yapi-extension-api",
    "guest/examples",
    "wit",
    "crates/yapi-tui/Cargo.toml",
    "crates/yapi-tui/src",
    "crates/yapi-tui/themes",
    "crates/yapi-types/Cargo.toml",
    "crates/yapi-types/src",
];
/// The JS runtime yapi embeds.
const ARTIFACT: &str = "crates/yapi-ext/yapi-js.wasm";
/// The Rust SDK's example extensions, one crate each; test fixtures.
const EXAMPLES: &str = "guest/examples";
/// Where each example's build lands, as `<crate>.wasm`.
const FIXTURES: &str = "crates/yapi-ext/tests/fixtures";
const RECORD: &str = "crates/yapi-ext/yapi-js.wasm.inputs";
const TARGET: &str = "wasm32-wasip2";

/// Build `crates/yapi-ext/yapi-js.wasm` and the example fixtures from `guest/`.
///
/// Needs the `wasm32-wasip2` Rust target (`rustup target add wasm32-wasip2`),
/// and, when the runtime's inputs changed, a WASI SDK for QuickJS's C
/// sources, found through `--wasi-sdk` or `WASI_SDK_PATH`.
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
    let runtime_inputs = inputs_hash(&RUNTIME_INPUTS)?;
    let inputs = format!("{runtime_inputs} {}", inputs_hash(&EXAMPLE_INPUTS)?);
    if args.check {
        let record = fs::read_to_string(RECORD).with_context(|| format!("reading {RECORD}"))?;
        let artifacts = artifacts_hash()?;
        if record != format_record(&inputs, &artifacts) {
            eprintln!(
                "{ARTIFACT} or an example in {FIXTURES} is stale or was changed by hand: run `cargo xtask js-runtime` and commit the result"
            );
            return Ok(ExitCode::FAILURE);
        }
        eprintln!("{ARTIFACT} and the examples in {FIXTURES} match their inputs");
        return Ok(ExitCode::SUCCESS);
    }

    fs::create_dir_all("target/guest")?;
    let target_dir = Path::new("target/guest").canonicalize()?;
    // Each package builds on its own, so no package's features reach
    // another's dependencies, such as serde_json's from the SDK's widgets.
    let build = |package: &str, sdk: Option<&Path>| -> anyhow::Result<()> {
        let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        command
            .args([
                "build",
                "--release",
                "--locked",
                "--target",
                TARGET,
                "-p",
                package,
            ])
            .current_dir("guest")
            .env("CARGO_TARGET_DIR", &target_dir);
        match sdk {
            Some(sdk) => {
                command
                    .env("WASI_SDK_PATH", sdk)
                    .env("CC_wasm32_wasip2", sdk.join("bin/clang"))
                    .env("AR_wasm32_wasip2", sdk.join("bin/llvm-ar"))
                    .env(
                        "CFLAGS_wasm32_wasip2",
                        format!("--sysroot={}", sdk.join("share/wasi-sysroot").display()),
                    );
            }
            // Panic messages name yapi-tui's files by this path, the same
            // in every checkout.
            None => {
                let root = std::env::current_dir()?;
                command.env(
                    "RUSTFLAGS",
                    format!("--remap-path-prefix={}=/yapi", root.display()),
                );
            }
        }
        if !command
            .status()
            .context("running cargo build for the guest")?
            .success()
        {
            bail!("building {package} failed");
        }
        Ok(())
    };
    // The runtime builds again only when its inputs or the artifact changed.
    let record = fs::read_to_string(RECORD).unwrap_or_default();
    let recorded = |prefix: &str| {
        record
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .and_then(|rest| rest.split(' ').next())
            .map(str::to_owned)
    };
    let runtime_current = recorded("inputs ").as_deref() == Some(runtime_inputs.as_str())
        && recorded("artifacts ") == file_hash(Path::new(ARTIFACT)).ok();
    if !runtime_current {
        let Some(sdk) = args.wasi_sdk else {
            bail!(
                "set WASI_SDK_PATH or pass --wasi-sdk to a WASI SDK (https://github.com/WebAssembly/wasi-sdk)"
            );
        };
        build("yapi-js", Some(&sdk))?;
        fs::copy(
            target_dir.join(TARGET).join("release/yapi_js.wasm"),
            ARTIFACT,
        )?;
    }
    let examples = examples()?;
    fs::create_dir_all(FIXTURES)?;
    for example in &examples {
        build(example, None)?;
        let built = target_dir
            .join(TARGET)
            .join("release")
            .join(format!("{}.wasm", example.replace('-', "_")));
        let fixture = Path::new(FIXTURES).join(format!("{example}.wasm"));
        fs::copy(&built, &fixture)?;
        eprintln!(
            "wrote {} ({} bytes)",
            fixture.display(),
            fs::metadata(&fixture)?.len()
        );
    }
    fs::write(RECORD, format_record(&inputs, &artifacts_hash()?))?;
    if !runtime_current {
        eprintln!("wrote {ARTIFACT} ({} bytes)", fs::metadata(ARTIFACT)?.len());
    }
    eprintln!("wrote {RECORD}");
    Ok(ExitCode::SUCCESS)
}

/// The example crates, named after their directories, in name order.
fn examples() -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(EXAMPLES).with_context(|| format!("reading {EXAMPLES}"))? {
        let entry = entry?;
        if entry.path().join("Cargo.toml").is_file() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn format_record(inputs: &str, artifacts: &str) -> String {
    format!("inputs {inputs}\nartifacts {artifacts}\n")
}

/// The runtime and every example's fixture, hashed in order.
fn artifacts_hash() -> anyhow::Result<String> {
    let mut hashes = vec![file_hash(Path::new(ARTIFACT))?];
    for example in examples()? {
        hashes.push(file_hash(
            &Path::new(FIXTURES).join(format!("{example}.wasm")),
        )?);
    }
    Ok(hashes.join(" "))
}

fn file_hash(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex(&Sha256::digest(bytes)))
}

/// Hashes every file's path and contents under `inputs`, in path order.
/// Build output directories are skipped.
fn inputs_hash(inputs: &[&str]) -> anyhow::Result<String> {
    let mut files = Vec::new();
    for input in inputs {
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
