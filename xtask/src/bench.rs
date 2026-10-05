//! `cargo xtask bench`: the measures behind the performance budgets of
//! AGENTS.md for yapi, and optionally pi on the same machine: `--version`
//! time, print mode's time to the first request byte, interactive first
//! paint, keystroke-to-paint latency in a pseudo-terminal, memory when idle,
//! with a large session open, after a session of turns with tool calls and
//! with JS extensions loaded, and install size.
//!
//! Startup measurements alternate between the programs run by run, so drift
//! on the machine (thermal throttling, background work) affects both alike.
//! Each result is reported as the median with the range of its samples.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::json;
use yapi_mock::pty::Pty;
use yapi_mock::{Cassette, MockServer};

/// Measure yapi (and pi with `--pi`).
#[derive(clap::Args)]
pub struct Args {
    /// The yapi executable; build it with `cargo build --release -p yapi`.
    #[arg(long, default_value = "target/release/yapi")]
    yapi: PathBuf,
    /// Also measure this pi executable.
    #[arg(long)]
    pi: Option<PathBuf>,
    /// A directory holding only a pi installation, such as the prefix of
    /// `npm install --prefix <dir> --ignore-scripts @earendil-works/pi-coding-agent`,
    /// whose size is reported as pi's install size. Its example extensions
    /// are loaded for one memory measure.
    #[arg(long)]
    pi_install: Option<PathBuf>,
    /// Startups to time, per program and measure.
    #[arg(long, default_value_t = 20)]
    runs: usize,
    /// Idle memory samples, per program and measure.
    #[arg(long, default_value_t = 5)]
    memory_runs: usize,
    /// Keystrokes to time.
    #[arg(long, default_value_t = 200)]
    keys: usize,
    /// Approximate transcript lines in the session opened for the keystroke test.
    #[arg(long, default_value_t = 10_000)]
    lines: usize,
}

struct Program {
    name: &'static str,
    path: PathBuf,
    env: Vec<(&'static str, OsString)>,
    cwd: PathBuf,
    agent: PathBuf,
}

/// Samples of one measure.
enum Samples {
    Time(Vec<Duration>),
    Bytes(Vec<u64>),
}

impl Samples {
    /// The median, or the given percentile for times.
    fn value(&self, percent: f64) -> f64 {
        match self {
            Samples::Time(samples) => {
                let mut sorted: Vec<f64> = samples.iter().map(Duration::as_secs_f64).collect();
                percentile(&mut sorted, percent) * 1000.0
            }
            Samples::Bytes(samples) => {
                let mut sorted: Vec<f64> = samples.iter().map(|&bytes| bytes as f64).collect();
                percentile(&mut sorted, percent) / 1e6
            }
        }
    }

    fn range(&self) -> (f64, f64) {
        (self.value(0.0), self.value(100.0))
    }

    fn unit(&self) -> &'static str {
        match self {
            Samples::Time(_) => "ms",
            Samples::Bytes(_) => "MB",
        }
    }

    fn len(&self) -> usize {
        match self {
            Samples::Time(samples) => samples.len(),
            Samples::Bytes(samples) => samples.len(),
        }
    }
}

/// One row of the report: a measure, its statistic and each program's samples.
struct Row {
    measure: String,
    percent: f64,
    samples: Vec<Samples>,
}

fn percentile(sorted: &mut [f64], percent: f64) -> f64 {
    sorted.sort_by(f64::total_cmp);
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * percent / 100.0).round() as usize;
    sorted[index.min(sorted.len() - 1)]
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

/// The resident set size of process `pid` and its descendants, in bytes, as
/// `ps` reports it.
fn tree_rss(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,rss="])
        .output()
        .ok()?;
    let table: Vec<[u64; 3]> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<u64> = line
                .split_whitespace()
                .filter_map(|field| field.parse().ok())
                .collect();
            fields.try_into().ok()
        })
        .collect();
    let mut members = vec![u64::from(pid)];
    let mut kb = 0;
    let mut found = false;
    while let Some(member) = members.pop() {
        for &[process, parent, size] in &table {
            if process == member {
                kb += size;
                found = true;
            }
            if parent == member {
                members.push(process);
            }
        }
    }
    found.then_some(kb * 1024)
}

/// Wall time of running `executable` with `args` to completion.
fn run_once(
    executable: &Path,
    args: &[&str],
    env: &[(&str, OsString)],
) -> anyhow::Result<Duration> {
    let started = Instant::now();
    let status = std::process::Command::new(executable)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(key, value)| (*key, value)))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()?;
    let elapsed = started.elapsed();
    anyhow::ensure!(status.success(), "{} failed", executable.display());
    Ok(elapsed)
}

/// The first line `executable --version` prints.
fn version_of(executable: &Path) -> String {
    std::process::Command::new(executable)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// An executable on `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The machine the numbers come from.
fn machine() -> String {
    let cpu = if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|info| {
                info.lines()
                    .find(|line| line.starts_with("model name"))
                    .and_then(|line| line.split_once(':'))
                    .map(|(_, name)| name.trim().to_owned())
            })
    } else {
        std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let threads = std::thread::available_parallelism().map_or(0, usize::from);
    format!(
        "{} {}, {}, {threads} hardware threads",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cpu.unwrap_or_else(|| "unknown CPU".to_owned()),
    )
}

/// The size of a file, or of every file under a directory, in bytes.
fn disk_size(path: &Path) -> std::io::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        total += disk_size(&entry?.path())?;
    }
    Ok(total)
}

/// Time from starting print mode to the first byte of its request, received
/// by a listener standing in for the provider.
fn first_request(program: &Program, listener: &std::net::TcpListener) -> anyhow::Result<Duration> {
    use std::io::Read as _;
    let started = Instant::now();
    let mut child = std::process::Command::new(&program.path)
        .args([
            "-p",
            "--no-session",
            "--model",
            "anthropic/claude-sonnet-4-5",
            "hi",
        ])
        .current_dir(&program.cwd)
        .env_clear()
        .envs(program.env.iter().map(|(key, value)| (*key, value)))
        // Print mode reads piped stdin into the prompt.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let (mut stream, _) = listener.accept()?;
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte)?;
    let elapsed = started.elapsed();
    drop(stream);
    let _ = child.kill();
    let _ = child.wait();
    Ok(elapsed)
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

const MODEL: [&str; 3] = ["--model", "anthropic/claude-sonnet-4-5", "--no-session"];

fn model_args() -> Vec<String> {
    MODEL.iter().map(|arg| (*arg).to_owned()).collect()
}

fn ready(rows: &[String]) -> bool {
    rows.iter().any(|row| row.contains("claude-sonnet-4-5"))
}

/// Time from start to the interactive first paint.
fn first_paint(program: &Program) -> anyhow::Result<Duration> {
    let pty = Pty::spawn(
        &program.path,
        &model_args(),
        &program.cwd,
        &program.env,
        (100, 40),
        true,
    )?;
    let paint = pty
        .wait_for(Duration::from_secs(20), ready)
        .context("no first paint")?;
    pty.finish()?;
    Ok(paint)
}

/// The memory of an interactive run started with `args`: the resident set
/// size of the program and its child processes, 2 s after `drive` returns.
/// `drive` starts at the first paint.
fn memory(
    program: &Program,
    args: &[String],
    drive: impl FnOnce(&mut Pty) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    let mut pty = Pty::spawn(
        &program.path,
        args,
        &program.cwd,
        &program.env,
        (100, 40),
        true,
    )?;
    let started = pty.wait_for(Duration::from_secs(60), ready).is_some();
    let driven = if started {
        drive(&mut pty)
    } else {
        Err(anyhow::anyhow!("no first paint"))
    };
    if let Err(err) = driven {
        let screen = pty.rows().join("\n");
        pty.finish()?;
        return Err(err.context(format!("{}, screen:\n{screen}", program.name)));
    }
    std::thread::sleep(Duration::from_secs(2));
    let bytes = pty
        .pid()
        .and_then(tree_rss)
        .context("cannot read memory use")?;
    pty.finish()?;
    Ok(bytes)
}

/// The memory of an interactive run 2 s after first paint.
fn idle_rss(program: &Program) -> anyhow::Result<u64> {
    memory(program, &model_args(), |_| Ok(()))
}

/// Arguments that open a fresh copy of a session of about `lines` transcript
/// lines.
fn large_session(program: &Program, root: &Path, lines: usize) -> anyhow::Result<Vec<String>> {
    let session = root.join(format!("{}-session.jsonl", program.name));
    write_session(&session, &program.cwd, lines)?;
    let mut args = model_args();
    args.truncate(2);
    args.extend(["--session".to_owned(), session.display().to_string()]);
    Ok(args)
}

/// Turns in the active-session measure.
const TURNS: usize = 20;

/// The provider's side of the active-session measure. Each turn calls a tool,
/// reading a 1,000-line file or running a command that prints 3,000 lines,
/// then answers in Markdown, ending with "Finished task N.".
fn conversation() -> anyhow::Result<Cassette> {
    let reply = |events: Vec<serde_json::Value>| {
        let body: String = events
            .iter()
            .map(|event| {
                let kind = event["type"].as_str().unwrap_or_default();
                format!("event: {kind}\ndata: {event}\n\n")
            })
            .collect();
        json!({"request": {"method": "POST", "path": "/v1/messages"},
            "response": {"headers": {"content-type": "text/event-stream"}, "chunks": [body]}})
    };
    let start = |id: String| {
        json!({"type": "message_start", "message": {"id": id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 1000, "output_tokens": 1}}})
    };
    let end = |reason: &str| {
        [
            json!({"type": "message_delta", "delta": {"stop_reason": reason, "stop_sequence": null},
                "usage": {"output_tokens": 100}}),
            json!({"type": "message_stop"}),
        ]
    };
    let mut interactions = Vec::new();
    for turn in 0..TURNS {
        let (tool, input) = if turn % 2 == 0 {
            ("read", json!({"path": "sample.txt"}))
        } else {
            ("bash", json!({"command": "seq 1 3000"}))
        };
        let mut events = vec![
            start(format!("msg_{turn}_tool")),
            json!({"type": "content_block_start", "index": 0, "content_block":
                {"type": "tool_use", "id": format!("toolu_{turn}"), "name": tool, "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta":
                {"type": "input_json_delta", "partial_json": input.to_string()}}),
            json!({"type": "content_block_stop", "index": 0}),
        ];
        events.extend(end("tool_use"));
        interactions.push(reply(events));
        let text = format!(
            "Step {turn} is done. Here is what the output shows.\n\n## Findings\n\n\
             - The output has the expected structure.\n\
             - Every line follows the same pattern.\n\
             - Nothing needs to change.\n\n\
             ```rust\nfn step_{turn}() -> usize {{\n    {turn}\n}}\n```\n\n\
             Finished task {turn}."
        );
        let mut events = vec![
            start(format!("msg_{turn}_text")),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            json!({"type": "content_block_stop", "index": 0}),
        ];
        events.extend(end("end_turn"));
        interactions.push(reply(events));
    }
    Ok(serde_json::from_value(
        json!({ "interactions": interactions }),
    )?)
}

/// Types [`TURNS`] prompts, each once the previous turn has finished. pi
/// keeps a prompt submitted before its startup completes in the editor and
/// adds a notice to the transcript; Enter then submits it again.
fn work(pty: &mut Pty) -> anyhow::Result<()> {
    let early = |rows: &[String]| {
        rows.iter()
            .filter(|row| row.contains("Startup is still in progress"))
            .count()
    };
    pty.settle();
    for turn in 0..TURNS {
        let notices = early(&pty.rows());
        pty.write(&format!("Task {turn}\r"))?;
        let marker = format!("Finished task {turn}.");
        let finished = |rows: &[String]| {
            rows.iter().any(|row| row.contains(&marker))
                && !rows.iter().any(|row| row.contains("Working"))
        };
        let started = Instant::now();
        loop {
            let left = Duration::from_secs(60).saturating_sub(started.elapsed());
            pty.wait_for(left, |rows| finished(rows) || early(rows) > notices)
                .with_context(|| format!("turn {turn} did not finish"))?;
            if finished(&pty.rows()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            pty.write("\r")?;
        }
    }
    Ok(())
}

/// The memory of an interactive session after [`TURNS`] turns with tool
/// calls, against a mock provider that checks each program made the same
/// requests.
fn active_rss(program: &Program, runtime: &tokio::runtime::Runtime) -> anyhow::Result<u64> {
    let server = runtime.block_on(MockServer::start(
        std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        conversation()?,
    ))?;
    let models = program.agent.join("models.json");
    std::fs::write(
        &models,
        json!({"providers": {"anthropic": {"baseUrl": server.url()}}}).to_string(),
    )?;
    let mut args = model_args();
    args.truncate(2);
    let bytes = memory(program, &args, work);
    std::fs::remove_file(&models)?;
    let bytes = bytes?;
    server
        .finish()
        .with_context(|| format!("{} made other requests", program.name))?;
    Ok(bytes)
}

/// pi's example extensions that the memory measure leaves out: those that
/// replace the footer, header or editor, where the benchmark looks for the
/// first paint; those that start commands, timers or file watchers when a
/// session starts; one that commits to the working directory's repository on
/// exit; and those that override a built-in tool `built-in-tool-renderer.ts`
/// also overrides, which pi refuses to load together.
const SKIPPED_EXAMPLES: [&str; 13] = [
    "border-status-editor.ts",
    "custom-footer.ts",
    "custom-header.ts",
    "modal-editor.ts",
    "rainbow-editor.ts",
    "github-issue-autocomplete.ts",
    "mac-system-theme.ts",
    "file-trigger.ts",
    "auto-commit-on-exit.ts",
    "bash-spawn-hook.ts",
    "minimal-mode.ts",
    "ssh.ts",
    "tool-override.ts",
];

/// Copies pi's single-file example extensions from a pi installation into
/// `dir`, except [`SKIPPED_EXAMPLES`]; the number copied.
fn copy_examples(pi_install: &Path, dir: &Path) -> anyhow::Result<usize> {
    let examples =
        pi_install.join("node_modules/@earendil-works/pi-coding-agent/examples/extensions");
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    let mut count = 0;
    for entry in std::fs::read_dir(&examples)
        .with_context(|| format!("pi's examples in {}", examples.display()))?
    {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(".ts") && !SKIPPED_EXAMPLES.contains(&name) {
            std::fs::copy(&path, dir.join(name))?;
            count += 1;
        }
    }
    Ok(count)
}

/// Keystroke-to-paint latencies in a session of about `lines` lines.
fn keystrokes(program: &Program, root: &Path, args: &Args) -> anyhow::Result<Vec<Duration>> {
    let session_args = large_session(program, root, args.lines)?;
    let mut pty = Pty::spawn(
        &program.path,
        &session_args,
        &program.cwd,
        &program.env,
        (100, 40),
        true,
    )?;
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
    Ok(keys)
}

/// Runs `measure` `runs` times for each program, alternating between them.
fn alternate<T>(
    programs: &[Program],
    runs: usize,
    mut measure: impl FnMut(usize, &Program) -> anyhow::Result<T>,
) -> anyhow::Result<Vec<Vec<T>>> {
    let mut samples: Vec<Vec<T>> = programs.iter().map(|_| Vec::new()).collect();
    for _ in 0..runs {
        for (index, program) in programs.iter().enumerate() {
            samples[index].push(measure(index, program)?);
        }
    }
    Ok(samples)
}

fn format_value(value: f64, unit: &str) -> String {
    format!("{value:.1} {unit}")
}

fn print_report(programs: &[Program], rows: &[Row]) {
    let mut header = "| Measure |".to_owned();
    let mut rule = "|---|".to_owned();
    for program in programs {
        header.push_str(&format!(" {} |", program.name));
        rule.push_str("---|");
    }
    if programs.len() == 2 {
        header.push_str(&format!(" {} / {} |", programs[1].name, programs[0].name));
        rule.push_str("---|");
    }
    println!("{header}\n{rule}");
    for row in rows {
        let mut line = format!("| {} |", row.measure);
        for samples in &row.samples {
            let (low, high) = samples.range();
            let value = samples.value(row.percent);
            if samples.len() > 1 {
                line.push_str(&format!(
                    " {} ({:.1} to {:.1}) |",
                    format_value(value, samples.unit()),
                    low,
                    high
                ));
            } else {
                line.push_str(&format!(" {} |", format_value(value, samples.unit())));
            }
        }
        if let [first, second] = row.samples.as_slice() {
            let base = first.value(row.percent);
            if base > 0.0 {
                line.push_str(&format!(" {:.1}x |", second.value(row.percent) / base));
            }
        }
        println!("{line}");
    }
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let root = std::env::temp_dir().join(format!("yapi-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd)?;
    let mut targets = vec![(
        "yapi",
        std::fs::canonicalize(&args.yapi).context("yapi executable; build with --release")?,
        "YAPI_CODING_AGENT_DIR",
    )];
    if let Some(pi) = &args.pi {
        targets.push((
            "pi",
            std::fs::canonicalize(pi).context("pi executable")?,
            "PI_CODING_AGENT_DIR",
        ));
    }
    let mut programs = Vec::new();
    for (name, path, dir_var) in targets {
        let agent = root.join(format!("{name}-agent"));
        std::fs::create_dir_all(&agent)?;
        let env = vec![
            ("PATH", std::env::var_os("PATH").unwrap_or_default()),
            (dir_var, agent.clone().into_os_string()),
            ("HOME", root.clone().into_os_string()),
            ("PI_OFFLINE", "1".into()),
            ("PI_SKIP_VERSION_CHECK", "1".into()),
            ("ANTHROPIC_API_KEY", "mock".into()),
        ];
        programs.push(Program {
            name,
            path,
            env,
            cwd: cwd.clone(),
            agent,
        });
    }

    println!("- Machine: {}", machine());
    for program in &programs {
        println!(
            "- {}: {} ({})",
            program.name,
            version_of(&program.path),
            program.path.display()
        );
    }
    let node = on_path("node");
    if args.pi.is_some()
        && let Some(node) = &node
    {
        println!("- Node.js: {} ({})", version_of(node), node.display());
    }
    println!(
        "- Samples: {} startups and {} memory samples per program, alternating, and {} keystrokes in a {}-line session",
        args.runs, args.memory_runs, args.keys, args.lines
    );
    println!(
        "- Values: medians with the range of the samples in parentheses, except the keystroke percentiles"
    );
    println!(
        "- Memory: resident set size of the program and its child processes, 2 s after first paint or the last turn"
    );

    let mut rows = Vec::new();
    let floor_env = [("PATH", std::env::var_os("PATH").unwrap_or_default())];
    let truth = on_path("true").context("no `true` on PATH")?;
    let floor = (0..args.runs)
        .map(|_| run_once(&truth, &[], &floor_env))
        .collect::<anyhow::Result<Vec<_>>>()?;
    println!(
        "- Process start floor (`true`): {}",
        format_value(Samples::Time(floor).value(50.0), "ms")
    );
    if args.pi.is_some()
        && let Some(node) = &node
    {
        let runs = (0..args.runs)
            .map(|_| run_once(node, &["-e", ""], &floor_env))
            .collect::<anyhow::Result<Vec<_>>>()?;
        println!(
            "- Node.js start floor (`node -e \"\"`): {}",
            format_value(Samples::Time(runs).value(50.0), "ms")
        );
    }
    println!();

    let version = alternate(&programs, args.runs, |_, program| {
        run_once(&program.path, &["--version"], &program.env)
    })?;
    rows.push(Row {
        measure: "`--version`".to_owned(),
        percent: 50.0,
        samples: version.into_iter().map(Samples::Time).collect(),
    });

    let listeners = programs
        .iter()
        .map(|program| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let url = format!("http://{}", listener.local_addr()?);
            std::fs::write(
                program.agent.join("models.json"),
                json!({"providers": {"anthropic": {"baseUrl": url}}}).to_string(),
            )?;
            Ok(listener)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let request = alternate(&programs, args.runs, |index, program| {
        first_request(program, &listeners[index])
    })?;
    for program in &programs {
        let _ = std::fs::remove_file(program.agent.join("models.json"));
    }
    rows.push(Row {
        measure: "Print mode, start to first request byte".to_owned(),
        percent: 50.0,
        samples: request.into_iter().map(Samples::Time).collect(),
    });

    let paint = alternate(&programs, args.runs, |_, program| first_paint(program))?;
    rows.push(Row {
        measure: "Interactive first paint".to_owned(),
        percent: 50.0,
        samples: paint.into_iter().map(Samples::Time).collect(),
    });

    let mut keys = Vec::new();
    for program in &programs {
        keys.push(keystrokes(program, &root, &args)?);
    }
    for percent in [50.0, 99.0] {
        rows.push(Row {
            measure: format!(
                "Keystroke to paint, p{percent:.0}, {}-line session",
                args.lines
            ),
            percent,
            samples: keys.iter().cloned().map(Samples::Time).collect(),
        });
    }

    let idle = alternate(&programs, args.memory_runs, |_, program| idle_rss(program))?;
    rows.push(Row {
        measure: "Memory, idle after first paint".to_owned(),
        percent: 50.0,
        samples: idle.into_iter().map(Samples::Bytes).collect(),
    });

    let opened = alternate(&programs, args.memory_runs, |_, program| {
        memory(program, &large_session(program, &root, args.lines)?, |_| {
            Ok(())
        })
    })?;
    rows.push(Row {
        measure: format!("Memory, {}-line session open", args.lines),
        percent: 50.0,
        samples: opened.into_iter().map(Samples::Bytes).collect(),
    });

    let sample: String = (1..=1000)
        .map(|line| {
            format!("{line:>4}: a line of the file the session reads on every other turn\n")
        })
        .collect();
    std::fs::write(cwd.join("sample.txt"), sample)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let active = alternate(&programs, args.memory_runs, |_, program| {
        active_rss(program, &runtime)
    })?;
    rows.push(Row {
        measure: format!("Memory after {TURNS} turns with tool calls"),
        percent: 50.0,
        samples: active.into_iter().map(Samples::Bytes).collect(),
    });

    for program in &programs {
        write_extensions(&program.agent.join("extensions"), 10)?;
        // The first start with extensions fills compilation caches.
        idle_rss(program)?;
    }
    let extended = alternate(&programs, args.memory_runs, |_, program| idle_rss(program))?;
    rows.push(Row {
        measure: "Memory with 10 small JS extensions".to_owned(),
        percent: 50.0,
        samples: extended.into_iter().map(Samples::Bytes).collect(),
    });

    if let Some(install) = &args.pi_install {
        let mut count = 0;
        for program in &programs {
            count = copy_examples(install, &program.agent.join("extensions"))?;
            idle_rss(program)?;
        }
        let examples = alternate(&programs, args.memory_runs, |_, program| idle_rss(program))?;
        rows.push(Row {
            measure: format!("Memory with {count} of pi's example extensions"),
            percent: 50.0,
            samples: examples.into_iter().map(Samples::Bytes).collect(),
        });
    }

    print_report(&programs, &rows);

    println!();
    let binary = disk_size(&programs[0].path)?;
    println!(
        "- Install size, yapi: {} (one executable)",
        format_value(binary as f64 / 1e6, "MB")
    );
    if let Some(install) = &args.pi_install {
        let packages = disk_size(install).context("pi install directory")?;
        let runtime = node.as_deref().map(disk_size).transpose()?.unwrap_or(0);
        println!(
            "- Install size, pi: {} ({} of packages in {}, plus the {} Node.js executable)",
            format_value((packages + runtime) as f64 / 1e6, "MB"),
            format_value(packages as f64 / 1e6, "MB"),
            install.display(),
            format_value(runtime as f64 / 1e6, "MB"),
        );
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(ExitCode::SUCCESS)
}
