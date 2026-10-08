//! Replaces the footer with the session's token counts and cost, the model
//! and the git branch. `/footer` turns it on and off.
//!
//! A port of pi's `custom-footer.ts` example. The footer is a component that
//! reads the session each time yapi renders it.

use std::cell::Cell;

use yapi_extension_api::{Api, Component, json, notify, request, theme};

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

/// A token count as the footer shows it, such as `950` or `1.2k`.
fn count(tokens: f64) -> String {
    if tokens < 1000.0 {
        format!("{tokens}")
    } else {
        format!("{:.1}k", tokens / 1000.0)
    }
}

struct Footer {
    model: String,
}

impl Component for Footer {
    fn render(&mut self, width: usize) -> Vec<String> {
        let (mut input, mut output, mut cost) = (0.0, 0.0, 0.0);
        let branch = request("session.read", &json!({"method": "getBranch", "args": []}))
            .unwrap_or_default();
        for entry in branch.as_array().into_iter().flatten() {
            let message = &entry["message"];
            if entry["type"] == "message" && message["role"] == "assistant" {
                let usage = &message["usage"];
                input += usage["input"].as_f64().unwrap_or_default();
                output += usage["output"].as_f64().unwrap_or_default();
                cost += usage["cost"]["total"].as_f64().unwrap_or_default();
            }
        }
        let left = format!("↑{} ↓{} ${cost:.3}", count(input), count(output));
        let footer = request("ui.footerData", &json!({})).unwrap_or_default();
        let right = match footer["gitBranch"].as_str().filter(|name| !name.is_empty()) {
            Some(name) => format!("{} ({name})", self.model),
            None => self.model.clone(),
        };
        // Both sides are plain text, one column a character. yapi cuts
        // the line to the width.
        let used = left.chars().count() + right.chars().count();
        let pad = " ".repeat(width.saturating_sub(used).max(1));
        let th = theme();
        vec![format!(
            "{}{pad}{}",
            th.fg("dim", &left),
            th.fg("dim", &right)
        )]
    }
}

fn init(api: &mut Api) {
    ENABLED.set(false);
    api.register_command("footer", "Toggle custom footer", |_args, ctx| async move {
        let enabled = !ENABLED.get();
        ENABLED.set(enabled);
        if enabled {
            let model = ctx.data()["model"]["id"]
                .as_str()
                .filter(|id| !id.is_empty());
            let model = model.unwrap_or("no-model").to_owned();
            ctx.set_footer(Some(Box::new(Footer { model })));
            notify("Custom footer enabled", "info");
        } else {
            ctx.set_footer(None);
            notify("Default footer restored", "info");
        }
        Ok(())
    });
}

yapi_extension_api::extension!(init);
