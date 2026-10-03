//! `cargo xtask bench`: the performance budgets of AGENTS.md for ri, and
//! optionally pi on the same machine: `--version` time, print mode's time to
//! the first request byte, interactive first paint, keystroke-to-paint latency
//! in a pseudo-terminal, and idle memory with and without JS extensions.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

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

/// The resident set size of process `pid`, in bytes.
fn rss(pid: u32) -> Option<u64> {
    if cfg!(target_os = "linux") {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        return Some(kb * 1024);
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let kb: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(kb * 1024)
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Median wall time of running `executable` with `args` to completion.
fn run_time(
    executable: &Path,
    args: &[&str],
    env: &[(&str, OsString)],
    runs: usize,
) -> anyhow::Result<Duration> {
    let mut samples = Vec::new();
    for _ in 0..runs {
        let started = Instant::now();
        let status = std::process::Command::new(executable)
            .args(args)
            .env_clear()
            .envs(env.iter().map(|(key, value)| (*key, value)))
            .stdout(std::process::Stdio::null())
            .status()?;
        samples.push(started.elapsed());
        anyhow::ensure!(status.success(), "{} failed", executable.display());
    }
    Ok(percentile(&mut samples, 50.0))
}

/// Median wall time of `program --version`.
fn version_time(
    program: &Program,
    env: &[(&str, OsString)],
    runs: usize,
) -> anyhow::Result<Duration> {
    run_time(&program.path, &["--version"], env, runs)
}

/// Median wall time of starting the system's `true`: the machine's floor for
/// any process start, which the `--version` budget includes.
fn start_floor(runs: usize) -> anyhow::Result<Duration> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let executable = std::env::split_paths(&path)
        .map(|dir| dir.join("true"))
        .find(|candidate| candidate.is_file())
        .context("no `true` on PATH")?;
    run_time(&executable, &[], &[("PATH", path)], runs)
}

/// Median time from starting print mode to the first byte of its request,
/// received by a listener standing in for the provider.
fn first_request(
    program: &Program,
    env: &[(&str, OsString)],
    cwd: &Path,
    agent: &Path,
    runs: usize,
) -> anyhow::Result<Duration> {
    use std::io::Read as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}", listener.local_addr()?);
    std::fs::write(
        agent.join("models.json"),
        json!({"providers": {"anthropic": {"baseUrl": url}}}).to_string(),
    )?;
    let mut samples = Vec::new();
    for _ in 0..runs {
        let started = Instant::now();
        let mut child = std::process::Command::new(&program.path)
            .args([
                "-p",
                "--no-session",
                "--model",
                "anthropic/claude-sonnet-4-5",
                "hi",
            ])
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(key, value)| (*key, value)))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let (mut stream, _) = listener.accept()?;
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte)?;
        samples.push(started.elapsed());
        drop(stream);
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_file(agent.join("models.json"));
    Ok(percentile(&mut samples, 50.0))
}

/// A small extension that registers a tool, a command and an event handler,
/// for the memory measurement with extensions.
fn write_extensions(dir: &Path, count: usize) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    for index in 0..count {
        let source = format!(
            r#"import {{ Type }} from "typebox";
import type {{ ExtensionAPI }} from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {{
	pi.registerTool({{
		name: "echo_{index}",
		label: "Echo {index}",
		description: "Echoes its input",
		parameters: Type.Object({{ text: Type.String() }}),
		async execute(_id, params: {{ text: string }}) {{
			return {{ content: [{{ type: "text", text: params.text }}], details: {{}} }};
		}},
	}});
	pi.registerCommand("echo{index}", {{ description: "Echo", handler: async (args, ctx) => ctx.ui.notify(args, "info") }});
	pi.on("turn_end", () => undefined);
}}
"#
        );
        std::fs::write(dir.join(format!("echo{index}.ts")), source)?;
    }
    Ok(())
}

/// The idle memory of an interactive run, sampled 2 s after first paint.
fn idle_rss(
    program: &Program,
    model: &[String],
    cwd: &Path,
    env: &[(&str, OsString)],
) -> anyhow::Result<u64> {
    let ready = |rows: &[String]| rows.iter().any(|row| row.contains("claude-sonnet-4-5"));
    let pty = Pty::spawn(&program.path, model, cwd, env, (100, 40), true)?;
    pty.wait_for(Duration::from_secs(60), ready)
        .context("no first paint")?;
    std::thread::sleep(Duration::from_secs(2));
    let bytes = pty.pid().and_then(rss).context("cannot read memory use")?;
    pty.finish()?;
    Ok(bytes)
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
    let version = version_time(program, &env, args.runs)?;
    let request = first_request(program, &env, &cwd, &agent, args.runs)?;
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
    let idle = idle_rss(program, &model, &cwd, &env)?;
    write_extensions(&agent.join("extensions"), 10)?;
    // The first start with extensions fills compilation caches.
    idle_rss(program, &model, &cwd, &env)?;
    let with_extensions = idle_rss(program, &model, &cwd, &env)?;
    let first = percentile(&mut paints, 50.0);
    let p50 = percentile(&mut keys, 50.0);
    let p99 = percentile(&mut keys, 99.0);
    println!("{}:", program.name);
    println!("  --version median                 {:>10}", ms(version));
    println!("  print mode to first request byte {:>10}", ms(request));
    println!("  first paint median               {:>10}", ms(first));
    println!(
        "  keystroke ({} lines) p50        {:>10}",
        args.lines,
        ms(p50)
    );
    println!(
        "  keystroke ({} lines) p99        {:>10}",
        args.lines,
        ms(p99)
    );
    println!("  idle memory                      {:>10}", mib(idle));
    println!(
        "  idle memory, 10 JS extensions    {:>10}",
        mib(with_extensions)
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
    println!(
        "process start floor (`true`)      {:>10}",
        ms(start_floor(args.runs)?)
    );
    for program in &programs {
        measure(program, &args, &root)?;
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(ExitCode::SUCCESS)
}
