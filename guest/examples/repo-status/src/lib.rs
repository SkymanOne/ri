//! Reports the git branch and uncommitted files: a warning when a session
//! starts in a repository with changes, a `/repo-status` command, and a
//! `repo_status` tool the model can call.
//!
//! Shows `exec`, which runs a process and waits for it. The extension needs
//! the process grant, which every extension has by default.

use yapi_extension_api::{Api, Context, Tool, ToolResult, exec, json, notify};

/// The branch and the changed files, or `None` outside a git repository.
fn status(ctx: &Context) -> Option<(String, Vec<String>)> {
    let cwd = ctx.cwd();
    // Works before the first commit too, and prints nothing on a detached HEAD.
    let branch = exec("git", &["branch", "--show-current"], cwd).ok()?;
    if branch.code != Some(0) {
        return None;
    }
    let name = match branch.stdout.trim() {
        "" => "a detached HEAD".to_owned(),
        name => name.to_owned(),
    };
    let changes = exec("git", &["status", "--porcelain"], cwd).ok()?;
    let files = changes
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.get(3..).unwrap_or(line).to_owned())
        .collect();
    Some((name, files))
}

fn summary(ctx: &Context) -> String {
    match status(ctx) {
        None => "Not a git repository".into(),
        Some((branch, files)) if files.is_empty() => format!("On {branch}, no uncommitted files"),
        Some((branch, files)) => format!(
            "On {branch}, {} uncommitted file(s):\n{}",
            files.len(),
            files.join("\n")
        ),
    }
}

fn init(api: &mut Api) {
    api.on("session_start", |_event, ctx| {
        if let Some((_, files)) = status(ctx).filter(|(_, files)| !files.is_empty()) {
            notify(
                &format!("{} uncommitted file(s) in this repository", files.len()),
                "warning",
            );
        }
        Ok(None)
    });
    api.register_command(
        "repo-status",
        "Show the git branch and uncommitted files",
        |_args, ctx| {
            notify(&summary(ctx), "info");
            Ok(())
        },
    );
    api.register_tool(Tool::new(
        "repo_status",
        "Report the current git branch and the files with uncommitted changes",
        json!({"type": "object", "properties": {}}),
        |_params, ctx| Ok(ToolResult::text(summary(ctx))),
    ));
}

yapi_extension_api::extension!(init);
