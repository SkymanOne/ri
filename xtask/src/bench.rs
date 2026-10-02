//! `cargo xtask bench`: interactive first paint and keystroke-to-paint latency,
//! measured in a pseudo-terminal, for ri and optionally pi on the same machine.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use ri_mock::pty::Pty;
use serde_json::json;

/// Measure ri (and pi with `--pi`).
#[derive(clap::Args)]
pub struct Args {
    /// The ri executable; build it with `cargo build --release -p ri`.
    #[arg(long, default_value = "target/release/ri")]
    ri: PathBuf,
    /// Also measure this pi executable.
    #[arg(long)]
    pi: Option<PathBuf>,
    /// Startups to time.
    #[arg(long, default_value_t = 10)]
    runs: usize,
    /// Keystrokes to time.
    #[arg(long, default_value_t = 100)]
    keys: usize,
    /// Approximate transcript lines in the session opened for the keystroke test.
    #[arg(long, default_value_t = 10_000)]
    lines: usize,
}

struct Program {
    name: &'static str,
    path: PathBuf,
    dir_var: &'static str,
}

/// A session file whose transcript renders about `lines` lines.
fn write_session(path: &Path, cwd: &Path, lines: usize) -> anyhow::Result<()> {
    let mut out = String::new();
    let header = json!({"type": "session", "version": 3, "id": "01a0fdd3-0000-7000-8000-000000000000",
        "timestamp": "2026-01-01T00:00:00.000Z", "cwd": cwd.display().to_string()});
    out.push_str(&format!("{header}\n"));
    let body: String = (0..10)
        .map(|line| format!("Line {line} of a reply with enough words to fill part of a row."))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut parent = serde_json::Value::Null;
    for index in 0..lines / 24 + 1 {
        let user = json!({"type": "message", "id": format!("u{index:07}"), "parentId": parent,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {"role": "user", "content": [{"type": "text", "text": format!("Question {index}")}], "timestamp": 0}});
        let assistant = json!({"type": "message", "id": format!("a{index:07}"), "parentId": format!("u{index:07}"),
            "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": body}],
                "api": "anthropic-messages", "provider": "anthropic", "model": "claude-sonnet-4-5",
                "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
                    "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
                "stopReason": "stop", "timestamp": 0}});
        out.push_str(&format!("{user}\n{assistant}\n"));
        parent = json!(format!("a{index:07}"));
    }
    std::fs::write(path, out)?;
    Ok(())
}

fn percentile(samples: &mut [Duration], percent: f64) -> Duration {
    samples.sort();
    let index = ((samples.len() as f64 - 1.0) * percent / 100.0).round() as usize;
    samples.get(index).copied().unwrap_or_default()
}

fn ms(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

fn measure(program: &Program, args: &Args, root: &Path) -> anyhow::Result<()> {
    let cwd = root.join("project");
    let agent = root.join(format!("{}-agent", program.name));
    std::fs::create_dir_all(&cwd)?;
    std::fs::create_dir_all(&agent)?;
    let env: Vec<(&str, OsString)> = vec![
        ("PATH", std::env::var_os("PATH").unwrap_or_default()),
        (program.dir_var, agent.clone().into_os_string()),
        ("HOME", root.to_path_buf().into_os_string()),
        ("PI_OFFLINE", "1".into()),
        ("PI_SKIP_VERSION_CHECK", "1".into()),
        ("ANTHROPIC_API_KEY", "mock".into()),
    ];
    let model = vec![
        "--model".to_owned(),
        "anthropic/claude-sonnet-4-5".to_owned(),
        "--no-session".to_owned(),
    ];
    let ready = |rows: &[String]| rows.iter().any(|row| row.contains("claude-sonnet-4-5"));
    let mut paints = Vec::new();
    for _ in 0..args.runs {
        let pty = Pty::spawn(&program.path, &model, &cwd, &env, (100, 40), true)?;
        let paint = pty
            .wait_for(Duration::from_secs(20), ready)
            .context("no first paint")?;
        paints.push(paint);
        pty.finish()?;
    }
    let session = root.join(format!("{}-session.jsonl", program.name));
    write_session(&session, &cwd, args.lines)?;
    let mut session_args = model.clone();
    session_args.truncate(2);
    session_args.extend(["--session".to_owned(), session.display().to_string()]);
    let mut pty = Pty::spawn(&program.path, &session_args, &cwd, &env, (100, 40), true)?;
    pty.wait_for(Duration::from_secs(60), ready)
        .context("large session did not open")?;
    pty.settle();
    let count = |rows: &[String]| {
        rows.iter()
            .map(|row| row.matches('x').count())
            .sum::<usize>()
    };
    let base = count(&pty.rows());
    let mut keys = Vec::new();
    for typed in 1..=args.keys {
        pty.write("x")?;
        let latency = pty
            .wait_for(Duration::from_secs(5), |rows| count(rows) >= base + typed)
            .context("keystroke not painted")?;
        keys.push(latency);
        std::thread::sleep(Duration::from_millis(20));
    }
    pty.finish()?;
    let first = percentile(&mut paints, 50.0);
    let p50 = percentile(&mut keys, 50.0);
    let p99 = percentile(&mut keys, 99.0);
    println!(
        "{:<3} first paint median {:>9}   keystroke ({} lines) p50 {:>8}  p99 {:>8}",
        program.name,
        ms(first),
        args.lines,
        ms(p50),
        ms(p99)
    );
    Ok(())
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let root = std::env::temp_dir().join(format!("ri-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut programs = vec![Program {
        name: "ri",
        path: std::fs::canonicalize(&args.ri).context("ri executable; build with --release")?,
        dir_var: "RI_CODING_AGENT_DIR",
    }];
    if let Some(pi) = &args.pi {
        programs.push(Program {
            name: "pi",
            path: std::fs::canonicalize(pi).context("pi executable")?,
            dir_var: "PI_CODING_AGENT_DIR",
        });
    }
    for program in &programs {
        measure(program, &args, &root)?;
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(ExitCode::SUCCESS)
}
