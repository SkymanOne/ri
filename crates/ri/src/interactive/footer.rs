//! The footer: working directory, branch, session name, token totals,
//! context use and model.
//!
//! Port of `components/footer.ts` in pi `v1.0.0`.

use std::path::Path;

use ratatui_core::text::{Line, Span};
use ri_core::agent_session::{ContextUsage, UsageTotals};
use ri_tui::lines::{self, StyledLine};
use ri_tui::text::visible_width;
use ri_tui::theme::Theme;

/// pi's compact token counts: `999`, `1.2k`, `45k`, `1.2M`, `12M`.
pub fn format_tokens(count: u64) -> String {
    let count = count as f64;
    if count < 1000.0 {
        format!("{count}")
    } else if count < 10_000.0 {
        format!("{:.1}k", count / 1000.0)
    } else if count < 1_000_000.0 {
        format!("{}k", (count / 1000.0).round())
    } else if count < 10_000_000.0 {
        format!("{:.1}M", count / 1_000_000.0)
    } else {
        format!("{}M", (count / 1_000_000.0).round())
    }
}

/// The working directory with the home directory as `~`.
pub fn format_cwd(cwd: &Path, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return cwd.display().to_string();
    };
    match cwd.strip_prefix(home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Ok(relative) => format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display()),
        Err(_) => cwd.display().to_string(),
    }
}

/// The current git branch of `cwd`, read from `.git/HEAD`; a short commit for
/// a detached head.
pub fn git_branch(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        let git = current.join(".git");
        let git_dir = if git.is_dir() {
            Some(git)
        } else if git.is_file() {
            let text = std::fs::read_to_string(&git).ok()?;
            text.trim()
                .strip_prefix("gitdir:")
                .map(|path| current.join(path.trim()))
        } else {
            None
        };
        if let Some(git_dir) = git_dir {
            let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
            let head = head.trim();
            return Some(match head.strip_prefix("ref: refs/heads/") {
                Some(branch) => branch.to_owned(),
                None => "detached".to_owned(),
            });
        }
        dir = current.parent();
    }
    None
}

/// What the footer shows.
pub struct FooterData<'a> {
    /// Working directory as shown.
    pub cwd: &'a str,
    /// Git branch.
    pub branch: Option<&'a str>,
    /// Session name.
    pub session_name: Option<&'a str>,
    /// Session totals.
    pub totals: UsageTotals,
    /// Context use, when a model is selected.
    pub context: Option<ContextUsage>,
    /// The model's context window, used when the context is unknown.
    pub context_window: u64,
    /// Auto-compaction is on.
    pub auto_compact: bool,
    /// The model id.
    pub model: Option<&'a str>,
    /// The model's provider.
    pub provider: Option<&'a str>,
    /// The model reasons, so the thinking level shows.
    pub reasoning: bool,
    /// The thinking level.
    pub thinking: &'a str,
    /// More than one provider has models, so the provider shows when it fits.
    pub several_providers: bool,
    /// The provider is a subscription.
    pub subscription: bool,
}

/// The footer rows at `width`.
pub fn render(data: &FooterData<'_>, theme: &Theme, width: usize) -> Vec<StyledLine> {
    let dim = theme.fg("dim");
    let mut pwd = data.cwd.to_owned();
    if let Some(branch) = data.branch {
        pwd = format!("{pwd} ({branch})");
    }
    if let Some(name) = data.session_name {
        pwd = format!("{pwd} • {name}");
    }
    let pwd_line = {
        let line = lines::styled(pwd, dim);
        let mut cut = lines::truncate(&line, width, "");
        if lines::width(&line) > width {
            cut = lines::truncate(&line, width.saturating_sub(3), "");
            cut.spans.push(Span::styled("...", dim));
        }
        cut
    };

    let totals = &data.totals;
    let mut parts: Vec<String> = Vec::new();
    if totals.input > 0 {
        parts.push(format!("↑{}", format_tokens(totals.input)));
    }
    if totals.output > 0 {
        parts.push(format!("↓{}", format_tokens(totals.output)));
    }
    if totals.cache_read > 0 {
        parts.push(format!("R{}", format_tokens(totals.cache_read)));
    }
    if totals.cache_write > 0 {
        parts.push(format!("W{}", format_tokens(totals.cache_write)));
    }
    if (totals.cache_read > 0 || totals.cache_write > 0)
        && let Some(rate) = totals.cache_hit_rate
    {
        parts.push(format!("CH{rate:.1}%"));
    }
    if totals.cost != 0.0 || data.subscription {
        parts.push(format!(
            "${:.3}{}",
            totals.cost,
            if data.subscription { " (sub)" } else { "" }
        ));
    }
    let window = data
        .context
        .map_or(data.context_window, |context| context.context_window);
    let percent = data.context.and_then(|context| context.percent());
    let auto = if data.auto_compact { " (auto)" } else { "" };
    let context_text = match percent {
        Some(percent) => format!("{percent:.1}%/{}{auto}", format_tokens(window)),
        None if data.context.is_some() => format!("?/{}{auto}", format_tokens(window)),
        None => format!("0.0%/{}{auto}", format_tokens(window)),
    };
    let percent_value = percent.unwrap_or(0.0);
    let context_style = if percent_value > 90.0 {
        Some(theme.fg("error"))
    } else if percent_value > 70.0 {
        Some(theme.fg("warning"))
    } else {
        None
    };

    let mut left_text = parts.join(" ");
    if !left_text.is_empty() {
        left_text.push(' ');
    }
    let mut left: Vec<Span<'static>> = vec![Span::styled(left_text, dim)];
    left.push(Span::styled(context_text, context_style.unwrap_or(dim)));
    let mut left_line = Line::from(left);
    if lines::width(&left_line) > width {
        left_line = lines::truncate(&left_line, width, "...");
    }
    let left_width = lines::width(&left_line);

    // pi's agent holds a placeholder model named `unknown` until one is chosen.
    let model = data.model.unwrap_or("unknown");
    let mut right = model.to_owned();
    if data.reasoning {
        right = if data.thinking == "off" {
            format!("{model} • thinking off")
        } else {
            format!("{model} • {}", data.thinking)
        };
    }
    if data.several_providers
        && let Some(provider) = data.provider
    {
        let with_provider = format!("({provider}) {right}");
        if left_width + 2 + visible_width(&with_provider) <= width {
            right = with_provider;
        }
    }
    let right_width = visible_width(&right);
    let mut stats = left_line;
    if left_width + 2 + right_width <= width {
        stats.spans.push(Span::styled(
            " ".repeat(width - left_width - right_width),
            dim,
        ));
        stats.spans.push(Span::styled(right, dim));
    } else if width > left_width + 2 {
        let available = width - left_width - 2;
        let cut = ri_tui::text::truncate_to_width(&right, available, "", false);
        let cut_width = visible_width(&cut);
        stats.spans.push(Span::styled(
            " ".repeat(width.saturating_sub(left_width + cut_width)),
            dim,
        ));
        stats.spans.push(Span::styled(cut, dim));
    }
    vec![pwd_line, stats]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_token_counts() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1234), "1.2k");
        assert_eq!(format_tokens(45_400), "45k");
        assert_eq!(format_tokens(1_234_567), "1.2M");
        assert_eq!(format_tokens(200_000), "200k");
    }

    #[test]
    fn shortens_home() {
        assert_eq!(
            format_cwd(Path::new("/home/u/proj"), Some(Path::new("/home/u"))),
            "~/proj"
        );
        assert_eq!(
            format_cwd(Path::new("/home/u"), Some(Path::new("/home/u"))),
            "~"
        );
        assert_eq!(
            format_cwd(Path::new("/srv"), Some(Path::new("/home/u"))),
            "/srv"
        );
    }
}
