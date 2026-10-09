//! Runs scripts that ship with the extension. `/script <name> [args]` runs
//! `scripts/<name>` with `sh`, from the root of the extension's package, or
//! from the folder of its `.wasm` file when it was not installed from a
//! package, and shows what the script printed.
//!
//! Shows `extension_path`, and `envAdd`, which gives the script
//! `EXTENSION_DIR` on top of the environment it inherits, so it still finds
//! programs on `PATH` without the extension reading the environment.

use std::path::Path;

use yapi_extension_api::{Api, extension_path, json, notify, request};

fn init(api: &mut Api) {
    api.register_command(
        "script",
        "Run one of the extension's scripts (usage: /script name [args])",
        |args, ctx| async move {
            let path = extension_path();
            let dir = path.package_root.unwrap_or_else(|| {
                let parent = Path::new(&path.file).parent().unwrap_or(Path::new("."));
                parent.to_string_lossy().into_owned()
            });
            let mut words = args.split_whitespace();
            let name = words.next().ok_or("Usage: /script name [args]")?;
            let mut command = vec![format!("{dir}/scripts/{name}")];
            command.extend(words.map(str::to_owned));
            let output = request(
                "exec.sync",
                &json!({
                    "command": "sh",
                    "args": command,
                    "cwd": ctx.cwd(),
                    "envAdd": {"EXTENSION_DIR": dir},
                }),
            )?;
            let shown = output["error"].as_str().or(output["stdout"].as_str());
            notify(shown.unwrap_or_default().trim_end(), "info");
            Ok(())
        },
    );
}

yapi_extension_api::extension!(init);
