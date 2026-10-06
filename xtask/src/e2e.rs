//! `cargo xtask e2e`: run the end-to-end scenarios against pi and yapi.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use std::path::Path;

use yapi_mock::scenario::{
    Program, first_difference, fixtures_dir, load_scenarios, normalize, run,
};

use crate::write_json;

/// Run `tests/fixtures/scenarios` against yapi, pi, or both.
///
/// Without flags, compares yapi with the goldens recorded from pi. `--record-pi`
/// rewrites the goldens from pi; `--differential` runs both and compares them
/// directly. pi runs from the fixture generator's install (needs Node).
#[derive(clap::Args)]
pub struct Args {
    /// Rewrite the goldens from pi.
    #[arg(long, conflicts_with = "differential")]
    record_pi: bool,
    /// Compare live pi and yapi runs instead of using the goldens.
    #[arg(long)]
    differential: bool,
    /// Only scenarios whose name contains this text.
    #[arg(long)]
    only: Option<String>,
    /// The pi executable.
    #[arg(
        long,
        default_value = "tests/fixtures/pi/generator/node_modules/.bin/pi"
    )]
    pi: PathBuf,
    /// The yapi executable; built with `cargo build -p yapi` when absent.
    #[arg(long, default_value = "target/debug/yapi")]
    yapi: PathBuf,
}

pub fn run_command(args: Args) -> anyhow::Result<ExitCode> {
    tokio::runtime::Runtime::new()?.block_on(e2e(args))
}

async fn e2e(args: Args) -> anyhow::Result<ExitCode> {
    let pi = Program::Pi(std::fs::canonicalize(&args.pi).context("pi executable")?);
    let yapi = Program::Yapi(
        std::fs::canonicalize(&args.yapi).context("yapi executable; run cargo build -p yapi")?,
    );
    let goldens = fixtures_dir().join("scenarios");
    let mut failures = 0;
    for scenario in load_scenarios()? {
        if args
            .only
            .as_ref()
            .is_some_and(|only| !scenario.name.contains(only.as_str()))
        {
            continue;
        }
        // A golden recorded for another operating system cannot match here.
        if !args.record_pi
            && !args.differential
            && let Some(reason) = scenario.skip_reason()
        {
            eprintln!("skipped {}: {reason}", scenario.name);
            continue;
        }
        let golden_path = goldens.join(format!("{}.json", scenario.name));
        if args.record_pi {
            write_json(&golden_path, &normalize(&run(&scenario, &pi).await?))?;
            eprintln!("recorded {}", scenario.name);
            continue;
        }
        let expected = if args.differential {
            normalize(&run(&scenario, &pi).await?)
        } else {
            serde_json::from_str(&std::fs::read_to_string(&golden_path).with_context(|| {
                format!(
                    "missing golden {}; run cargo xtask e2e --record-pi",
                    golden_path.display()
                )
            })?)?
        };
        let actual = normalize(&run(&scenario, &yapi).await?);
        match first_difference(&expected, &actual) {
            None => eprintln!("ok      {}", scenario.name),
            Some(diff) => {
                failures += 1;
                let dir = Path::new("target/e2e");
                std::fs::create_dir_all(dir)?;
                let actual_path = dir.join(format!("{}.actual.json", scenario.name));
                write_json(&actual_path, &actual)?;
                eprintln!(
                    "DIFFER  {}: {diff} (yapi output in {})",
                    scenario.name,
                    actual_path.display()
                );
            }
        }
    }
    Ok(if failures == 0 {
        ExitCode::SUCCESS
    } else {
        eprintln!("{failures} scenario(s) differ");
        ExitCode::FAILURE
    })
}
