//! `/login` and `/logout`: the provider selector and the login dialog.
//!
//! Ports of `oauth-selector.ts` and `login-dialog.ts` in
//! `packages/coding-agent/src/modes/interactive/components`, and the login
//! handlers of `interactive-mode.ts`, in pi `v1.0.0`.

use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use yapi_ai::auth::{AuthEvent, AuthPrompt, CredentialKind};
use yapi_ai::providers;
use yapi_ai::registry::{LoginKind, ModelRegistry};
use yapi_tui::fuzzy::fuzzy_filter;
use yapi_tui::lines::{self, StyledLine, styled};
use yapi_tui::select_list::visible_range;
use yapi_tui::text_input::{InputEvent, TextInput};
use yapi_types::collate::locale_compare;

use super::selectors::{Action, Outcome, Ui};

const MAX_VISIBLE: usize = 8;

/// How a provider's credential is configured: its kind and where it comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The configured kind.
    pub kind: LoginKind,
    /// pi's source label.
    pub source: String,
}

/// One row of the provider selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderOption {
    /// Provider id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The method this row signs in with.
    pub kind: LoginKind,
    /// Method name, searched but not shown.
    pub method: String,
    /// Whether the method stores an API key; otherwise it is configured outside yapi.
    pub can_login: bool,
    /// The current credential, if any.
    pub status: Option<Status>,
    /// Whether the OAuth sign-in is a subscription.
    pub subscription: bool,
}

/// Every provider and method `/login` offers, optionally of one kind, sorted
/// by name. OAuth rows are limited to the sign-ins yapi implements.
pub fn login_options(registry: &ModelRegistry, kind: Option<LoginKind>) -> Vec<ProviderOption> {
    // Built-in providers, then custom ones from models.json and extensions.
    let mut ids: Vec<String> = providers::PROVIDERS
        .iter()
        .map(|info| info.id.to_owned())
        .collect();
    for model in registry.models() {
        if !ids.contains(&model.provider) {
            ids.push(model.provider.clone());
        }
    }
    let llama = yapi_ai::llama::PROVIDER_ID;
    if registry.llama_enabled() && !ids.iter().any(|id| id == llama) {
        ids.push(llama.to_owned());
    }
    let mut options = Vec::new();
    for id in ids {
        let name = registry.provider_name(&id);
        let status = registry.login_status(&id).map(|source| Status {
            kind: if registry.is_using_oauth(&id) {
                LoginKind::OAuth
            } else {
                LoginKind::ApiKey
            },
            source,
        });
        let info = providers::info(&id);
        let flow = registry.oauth_flow(&id);
        let subscription = flow.as_ref().is_some_and(|flow| flow.is_subscription());
        if kind.is_none_or(|kind| kind == LoginKind::OAuth)
            && let Some(flow) = &flow
        {
            options.push(ProviderOption {
                id: id.clone(),
                name: name.clone(),
                kind: LoginKind::OAuth,
                method: flow.name().to_owned(),
                can_login: true,
                status: status.clone(),
                subscription,
            });
        }
        // Custom providers take a key; OAuth-only built-ins do not.
        let api_key = match info {
            Some(info) => info
                .api_key
                .map(|method| (method.name.to_owned(), method.login)),
            None if id == llama => Some(("llama.cpp server".to_owned(), true)),
            None => Some(("API key".to_owned(), true)),
        };
        if kind.is_none_or(|kind| kind == LoginKind::ApiKey)
            && let Some((method, can_login)) = api_key
        {
            options.push(ProviderOption {
                id,
                name,
                kind: LoginKind::ApiKey,
                method,
                can_login,
                status,
                subscription,
            });
        }
    }
    options.sort_by(|a, b| locale_compare(&a.name, &b.name));
    options
}

/// Rows for `/logout`: providers with a stored credential, sorted by name.
pub fn logout_options(registry: &ModelRegistry) -> Vec<ProviderOption> {
    let mut options: Vec<ProviderOption> = registry
        .stored_credentials()
        .into_iter()
        .map(|(id, kind)| {
            let kind = match kind {
                CredentialKind::OAuth => LoginKind::OAuth,
                CredentialKind::ApiKey => LoginKind::ApiKey,
            };
            ProviderOption {
                name: registry.provider_name(&id),
                subscription: registry
                    .oauth_flow(&id)
                    .is_some_and(|flow| flow.is_subscription()),
                id,
                kind,
                method: String::new(),
                can_login: true,
                status: Some(Status {
                    kind,
                    source: "stored credential".into(),
                }),
            }
        })
        .collect();
    options.sort_by(|a, b| locale_compare(&a.name, &b.name));
    options
}

/// The rows `/login <provider>` matches by id or name, ignoring case.
pub fn find_options(registry: &ModelRegistry, reference: &str) -> Vec<ProviderOption> {
    let reference = reference.trim().to_lowercase();
    if reference.is_empty() {
        return Vec::new();
    }
    login_options(registry, None)
        .into_iter()
        .filter(|option| {
            option.id.to_lowercase() == reference || option.name.to_lowercase() == reference
        })
        .collect()
}

/// A provider in `/login` argument completion, with every method it offers;
/// pi's `LoginProviderCompletionOption`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionOption {
    /// Provider id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Sign-in methods, OAuth first.
    pub kinds: Vec<LoginKind>,
    /// Whether the OAuth sign-in is a subscription.
    pub subscription: bool,
}

impl CompletionOption {
    /// pi's `getLoginProviderSearchText`.
    pub fn search_text(&self) -> String {
        let kinds: Vec<String> = self
            .kinds
            .iter()
            .map(|kind| {
                let id = match kind {
                    LoginKind::OAuth => "oauth",
                    LoginKind::ApiKey => "api_key",
                };
                format!("{id} {}", kind_label(*kind, self.subscription))
            })
            .collect();
        format!("{} {} {}", self.id, self.name, kinds.join(" "))
    }

    /// pi's `formatLoginProviderCompletionDescription`.
    pub fn description(&self) -> String {
        let kinds: Vec<&str> = self
            .kinds
            .iter()
            .map(|kind| kind_label(*kind, self.subscription))
            .collect();
        let kinds = kinds.join("/");
        if self.name == self.id {
            kinds
        } else {
            format!("{} · {kinds}", self.name)
        }
    }
}

/// `/login` rows grouped by provider and sorted by name; pi's
/// `getLoginProviderCompletionOptions`.
pub fn completion_options(options: Vec<ProviderOption>) -> Vec<CompletionOption> {
    let mut providers: Vec<CompletionOption> = Vec::new();
    for option in options {
        if let Some(existing) = providers
            .iter_mut()
            .find(|provider| provider.id == option.id)
        {
            if !existing.kinds.contains(&option.kind) {
                existing.kinds.push(option.kind);
                existing.kinds.sort_by_key(|kind| *kind != LoginKind::OAuth);
            }
            continue;
        }
        providers.push(CompletionOption {
            id: option.id,
            name: option.name,
            kinds: vec![option.kind],
            subscription: option.subscription,
        });
    }
    providers.sort_by(|a, b| locale_compare(&a.name, &b.name));
    providers
}

/// pi's `formatAuthSelectorProviderType`.
pub fn kind_label(kind: LoginKind, subscription: bool) -> &'static str {
    match kind {
        LoginKind::ApiKey => "API key",
        LoginKind::OAuth if !subscription => "account",
        LoginKind::OAuth => "subscription",
    }
}

fn is_env_list(source: &str) -> bool {
    source.split(", ").all(|name| {
        let mut chars = name.chars();
        chars.next().is_some_and(|c| c.is_ascii_uppercase())
            && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    })
}

/// pi's `formatAuthSelectorProviderStatus`.
fn status_spans(option: &ProviderOption, ui: &Ui<'_>) -> Vec<Span<'static>> {
    let theme = ui.theme;
    let Some(status) = &option.status else {
        return vec![Span::styled(" • not configured", theme.fg("muted"))];
    };
    if status.kind != option.kind {
        return vec![
            Span::styled(" • ", theme.fg("muted")),
            Span::styled(
                format!(
                    "{} configured",
                    kind_label(status.kind, option.subscription)
                ),
                theme.fg("warning"),
            ),
        ];
    }
    let text = match status.source.as_str() {
        "" | "OAuth" | "stored credential" => " ✓ configured".to_owned(),
        source if is_env_list(source) => format!(" ✓ env: {source}"),
        source => format!(" ✓ {source}"),
    };
    vec![Span::styled(text, theme.fg("success"))]
}

/// Which list the selector shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Providers to configure.
    Login,
    /// Providers to log out of.
    Logout,
}

/// pi's `OAuthSelectorComponent`.
pub struct ProviderSelector {
    mode: Mode,
    all: Vec<ProviderOption>,
    filtered: Vec<ProviderOption>,
    selected: usize,
    input: TextInput,
    show_kinds: bool,
}

impl ProviderSelector {
    /// A selector over `options`, searching for `search` at first.
    pub fn new(mode: Mode, options: Vec<ProviderOption>, search: &str) -> ProviderSelector {
        let show_kinds = options
            .first()
            .is_some_and(|first| options.iter().any(|option| option.kind != first.kind));
        let mut input = TextInput::new("> ");
        input.focused = true;
        input.set_value(search);
        let mut selector = ProviderSelector {
            mode,
            filtered: options.clone(),
            all: options,
            selected: 0,
            input,
            show_kinds,
        };
        selector.filter();
        selector
    }

    fn filter(&mut self) {
        let query = self.input.value().to_owned();
        self.filtered = if query.is_empty() {
            self.all.clone()
        } else {
            fuzzy_filter(self.all.clone(), &query, |option| {
                format!(
                    "{} {} {} {}",
                    option.name,
                    option.id,
                    match option.kind {
                        LoginKind::OAuth => "oauth",
                        LoginKind::ApiKey => "api_key",
                    },
                    option.method
                )
            })
        };
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// The rows at `width`, and the cursor.
    pub fn render(
        &mut self,
        width: usize,
        ui: &Ui<'_>,
    ) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let theme = ui.theme;
        let mut out = vec![ui.border(width)];
        out.extend(lines::spacer(1));
        let title = match self.mode {
            Mode::Login => "Select provider to configure:",
            Mode::Logout => "Select provider to logout:",
        };
        out.push(lines::truncated_text(
            &styled(title, theme.fg("accent").add_modifier(Modifier::BOLD)),
            width,
            1,
        ));
        out.extend(lines::spacer(1));
        let input = self.input.render(width);
        let cursor = self.input.cursor_column().map(|col| (out.len(), col));
        out.push(input);
        out.extend(lines::spacer(1));
        let count = self.filtered.len();
        let (start, end) = visible_range(self.selected, count, MAX_VISIBLE);
        for (index, option) in self.filtered[start..end].iter().enumerate() {
            let mut spans = if start + index == self.selected {
                vec![
                    Span::styled("→ ", theme.fg("accent")),
                    Span::styled(option.name.clone(), theme.fg("accent")),
                ]
            } else {
                vec![
                    Span::raw("  "),
                    Span::styled(option.name.clone(), theme.fg("text")),
                ]
            };
            if self.show_kinds {
                spans.push(Span::styled(
                    format!(" [{}]", kind_label(option.kind, option.subscription)),
                    theme.fg("muted"),
                ));
            }
            spans.extend(status_spans(option, ui));
            out.push(lines::truncated_text(&Line::from(spans), width, 1));
        }
        if start > 0 || end < count {
            out.push(lines::truncated_text(
                &styled(
                    format!("  ({}/{count})", self.selected + 1),
                    theme.fg("muted"),
                ),
                width,
                1,
            ));
        }
        if count == 0 {
            let message = match (self.all.is_empty(), self.mode) {
                (true, Mode::Login) => "No providers available",
                (true, Mode::Logout) => "No providers logged in. Use /login first.",
                (false, _) => "No matching providers",
            };
            out.push(lines::truncated_text(
                &styled(format!("  {message}"), theme.fg("muted")),
                width,
                1,
            ));
        }
        out.extend(lines::spacer(1));
        out.push(ui.border(width));
        (out, cursor)
    }

    /// Handles a key.
    pub fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        let kb = ui.keys;
        if kb.matches(data, "tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
        } else if kb.matches(data, "tui.select.down") {
            self.selected = (self.selected + 1).min(self.filtered.len().saturating_sub(1));
        } else if kb.matches(data, "tui.select.confirm") {
            if let Some(option) = self.filtered.get(self.selected) {
                return Outcome::Done(Action::Provider(Box::new(option.clone())));
            }
        } else if kb.matches(data, "tui.select.cancel") {
            return Outcome::Cancel;
        } else {
            if let InputEvent::Submit(_) = self.input.handle_input(data, kb)
                && let Some(option) = self.filtered.get(self.selected)
            {
                return Outcome::Done(Action::Provider(Box::new(option.clone())));
            }
            self.filter();
        }
        Outcome::None
    }
}

/// A row of the login dialog's content.
enum Row {
    Spacer,
    Text(StyledLine),
    Input,
}

/// A question the dialog is waiting on.
pub struct Pending {
    /// Where the answer goes; dropping it cancels the sign-in step.
    pub reply: oneshot::Sender<String>,
    /// Fires when the sign-in withdraws the question.
    pub withdrawn: CancellationToken,
}

/// pi's `LoginDialogComponent`, which replaces the editor during a sign-in.
pub struct LoginDialog {
    title: String,
    rows: Vec<Row>,
    input: TextInput,
    pending: Option<Pending>,
    /// Cancels the whole sign-in.
    pub cancel: CancellationToken,
}

impl LoginDialog {
    /// A dialog titled `Login to <name>`, or `title` when given.
    pub fn new(name: &str, title: Option<&str>) -> LoginDialog {
        let mut input = TextInput::new("> ");
        input.focused = true;
        LoginDialog {
            title: title.map_or_else(|| format!("Login to {name}"), str::to_owned),
            rows: Vec::new(),
            input,
            pending: None,
            cancel: CancellationToken::new(),
        }
    }

    fn text(&mut self, line: StyledLine) {
        self.rows.push(Row::Text(line));
    }

    fn cancel_hint(ui: &Ui<'_>, description: &str) -> StyledLine {
        let mut spans = vec![Span::raw("(")];
        spans.extend(ui.key_hint("tui.select.cancel", description));
        spans.push(Span::raw(")"));
        Line::from(spans)
    }

    /// Shows an event from the sign-in.
    pub fn notify(&mut self, event: AuthEvent, ui: &Ui<'_>) {
        let theme = ui.theme;
        let click = if cfg!(target_os = "macos") {
            "Cmd+click to open"
        } else {
            "Ctrl+click to open"
        };
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                self.rows.clear();
                self.rows.push(Row::Spacer);
                self.text(styled(url.clone(), theme.fg("accent")));
                self.text(styled(click, theme.fg("dim")));
                if let Some(instructions) = instructions {
                    self.rows.push(Row::Spacer);
                    self.text(styled(instructions, theme.fg("warning")));
                }
                open_browser(&url);
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => {
                self.rows.clear();
                self.rows.push(Row::Spacer);
                self.text(styled(verification_uri, theme.fg("accent")));
                self.text(styled(click, theme.fg("dim")));
                self.rows.push(Row::Spacer);
                self.text(styled(
                    format!("Enter code: {user_code}"),
                    theme.fg("warning"),
                ));
                self.rows.push(Row::Spacer);
                self.text(styled("Waiting for authentication...", theme.fg("dim")));
                self.text(Self::cancel_hint(ui, "to cancel"));
            }
            AuthEvent::Info { message, links } => {
                self.show_info(&message, ui);
                for link in links {
                    let text = if link.label.is_empty() {
                        link.url
                    } else {
                        format!("{}: {}", link.label, link.url)
                    };
                    self.text(styled(text, theme.fg("accent")));
                }
            }
            AuthEvent::Progress { message } => self.text(styled(message, theme.fg("dim"))),
        }
    }

    /// Replaces the content with detail lines, as pi's `showDetails`.
    pub fn show_details(&mut self, lines: Vec<StyledLine>) {
        self.rows.clear();
        self.rows.push(Row::Spacer);
        for line in lines {
            self.text(line);
        }
    }

    /// Shows provider information, as pi's `showInfo`.
    pub fn show_info(&mut self, message: &str, ui: &Ui<'_>) {
        self.rows.push(Row::Spacer);
        self.text(styled(message, ui.theme.fg("text")));
    }

    /// Adds the close hint of an information-only dialog.
    pub fn show_close_hint(&mut self, ui: &Ui<'_>) {
        self.rows.push(Row::Spacer);
        self.text(Self::cancel_hint(ui, "to close"));
    }

    /// Asks a text, secret or pasted-code question.
    pub fn prompt(&mut self, prompt: &AuthPrompt, pending: Pending, ui: &Ui<'_>) {
        let theme = ui.theme;
        self.input.set_value("");
        self.rows.push(Row::Spacer);
        match prompt {
            AuthPrompt::ManualCode { message, .. } => {
                self.text(styled(message.clone(), theme.fg("dim")));
                self.rows.push(Row::Input);
                self.text(Self::cancel_hint(ui, "to cancel"));
            }
            AuthPrompt::Text {
                message,
                placeholder,
            } => {
                self.text(styled(message.clone(), theme.fg("text")));
                if let Some(placeholder) = placeholder {
                    self.text(styled(format!("e.g., {placeholder}"), theme.fg("dim")));
                }
                self.rows.push(Row::Input);
                self.text(Self::submit_hint(ui));
            }
            AuthPrompt::Secret { message } => {
                self.text(styled(message.clone(), theme.fg("text")));
                self.rows.push(Row::Input);
                self.text(Self::submit_hint(ui));
            }
            AuthPrompt::Select { .. } => {}
        }
        self.pending = Some(pending);
    }

    fn submit_hint(ui: &Ui<'_>) -> StyledLine {
        let mut spans = vec![Span::raw("(")];
        spans.extend(ui.key_hint("tui.select.cancel", "to cancel,"));
        spans.push(Span::raw(" "));
        spans.extend(ui.key_hint("tui.select.confirm", "to submit"));
        spans.push(Span::raw(")"));
        Line::from(spans)
    }

    /// The rows at `width`, and the cursor.
    pub fn render(
        &mut self,
        width: usize,
        ui: &Ui<'_>,
    ) -> (Vec<StyledLine>, Option<(usize, usize)>) {
        let theme = ui.theme;
        let mut out = vec![ui.border(width)];
        out.extend(lines::text(
            &[styled(
                self.title.clone(),
                theme.fg("accent").add_modifier(Modifier::BOLD),
            )],
            width,
            1,
            0,
            None,
        ));
        let mut cursor = None;
        for row in &self.rows {
            match row {
                Row::Spacer => out.extend(lines::spacer(1)),
                Row::Text(line) => {
                    out.extend(lines::text(std::slice::from_ref(line), width, 1, 0, None))
                }
                Row::Input => {
                    let line = self.input.render(width);
                    cursor = self.input.cursor_column().map(|col| (out.len(), col));
                    out.push(line);
                }
            }
        }
        out.push(ui.border(width));
        (out, cursor)
    }

    /// Handles a key: escape cancels the sign-in; enter answers the open question.
    pub fn handle_input(&mut self, data: &str, ui: &Ui<'_>) -> Outcome {
        if ui.keys.matches(data, "tui.select.cancel") {
            self.cancel.cancel();
            self.pending = None;
            return Outcome::Done(Action::LoginCancelled);
        }
        if let InputEvent::Submit(value) = self.input.handle_input(data, ui.keys)
            && let Some(pending) = self.pending.take()
        {
            // pi replaces the input with the submitted text.
            if let Some(index) = self.rows.iter().position(|row| matches!(row, Row::Input)) {
                self.rows[index] = Row::Text(lines::raw(format!("> {value}")));
            }
            if !pending.withdrawn.is_cancelled() {
                let _ = pending.reply.send(value);
            }
        }
        Outcome::None
    }
}

/// What reopens when a sign-in is cancelled: pi's `onBack`.
#[derive(Clone, Debug)]
pub(super) enum Back {
    /// Nothing; the editor returns.
    Editor,
    /// The authentication method menu, for all providers or these.
    Menu(Option<Vec<ProviderOption>>),
    /// The provider selector, of one kind or all, with its search.
    Providers(Option<LoginKind>, String),
}

/// A sign-in in progress.
pub(super) struct LoginRun {
    id: u64,
    option: ProviderOption,
    back: Back,
    /// The dialog while a select prompt takes its place.
    parked: Option<Box<LoginDialog>>,
    /// The open select prompt and its option ids.
    select: Option<(oneshot::Sender<String>, Vec<String>)>,
    had_model: bool,
}

/// pi's `defaultModelPerProvider` entry for `provider`.
fn default_model(provider: &str) -> Option<&'static str> {
    yapi_core::model_resolver::DEFAULT_MODEL_PER_PROVIDER
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, id)| *id)
}

const ACCOUNT: &str = "Sign in with an account";
/// pi's `RADIUS_PROVIDER_ID`.
const RADIUS: &str = "radius";
/// pi's `RADIUS_LOGIN_INTRO`, under the title of Radius's method picker.
const RADIUS_LOGIN_INTRO: &str =
    "Radius is a service crafted for Pi by the builders of Pi, Earendil Works";
const API_KEY: &str = "Sign in with an API key";

impl super::App {
    /// `/login`, with an optional provider id or name.
    pub(super) fn login_command(&mut self, reference: &str) {
        if reference.is_empty() {
            self.open_login_menu(None);
            return;
        }
        let options = find_options(&self.session.registry(), reference);
        if options.len() == 1 {
            self.start_login(options[0].clone(), Back::Editor);
            return;
        }
        if options.len() > 1 && options.iter().all(|option| option.id == options[0].id) {
            self.open_login_menu(Some(options));
            return;
        }
        self.open_login_providers(None, reference);
    }

    /// pi's `showLoginAuthTypeSelector`.
    pub(super) fn open_login_menu(&mut self, options: Option<Vec<ProviderOption>>) {
        let account = options
            .as_ref()
            .and_then(|options| {
                options
                    .iter()
                    .find(|option| option.kind == LoginKind::OAuth)
            })
            .and_then(|option| self.session.registry().oauth_flow(&option.id))
            .and_then(|flow| flow.login_label().map(str::to_owned))
            .unwrap_or_else(|| ACCOUNT.to_owned());
        let kinds: Vec<LoginKind> = match &options {
            Some(options) => [LoginKind::OAuth, LoginKind::ApiKey]
                .into_iter()
                .filter(|kind| options.iter().any(|option| option.kind == *kind))
                .collect(),
            None => vec![LoginKind::OAuth, LoginKind::ApiKey],
        };
        if kinds.is_empty() {
            self.status("No login methods available.");
            return;
        }
        if let Some(options) = &options
            && kinds.len() == 1
        {
            self.start_login(options[0].clone(), Back::Editor);
            return;
        }
        // The top-level menu offers Radius directly, as its last option.
        let radius = options
            .is_none()
            .then(|| {
                login_options(&self.session.registry(), Some(LoginKind::OAuth))
                    .into_iter()
                    .find(|option| option.id == RADIUS)
            })
            .flatten();
        let radius_text = radius
            .as_ref()
            .map(|option| format!("Sign in with {}", option.name));
        let mut labels: Vec<&str> = kinds
            .iter()
            .map(|kind| match kind {
                LoginKind::OAuth => account.as_str(),
                LoginKind::ApiKey => API_KEY,
            })
            .collect();
        labels.extend(radius_text.as_deref());
        let title = match options.as_ref().and_then(|options| options.first()) {
            Some(option) => format!("Select authentication method for {}:", option.name),
            None => "Select authentication method:".to_owned(),
        };
        let mut dialog = super::selectors::ChoiceDialog::new(&title, &labels);
        if let Some(option) = &radius {
            let ui = self.ui();
            dialog.suffix = Some((kinds.len(), status_spans(option, &ui)));
            dialog.shimmer = Some((kinds.len(), std::time::Instant::now()));
        }
        self.dialog = Some(super::Dialog::LoginMenu(options, kinds.clone(), radius));
        self.selector = Some(super::selectors::Selector::Choice(dialog));
    }

    /// A choice in the method menu.
    pub(super) fn login_menu_chosen(
        &mut self,
        options: Option<Vec<ProviderOption>>,
        kind: LoginKind,
    ) {
        match &options {
            Some(list) => {
                if let Some(option) = list.iter().find(|option| option.kind == kind) {
                    self.start_login(option.clone(), Back::Menu(options.clone()));
                }
            }
            None => self.open_login_providers(Some(kind), ""),
        }
    }

    /// pi's `showLoginProviderSelector`.
    pub(super) fn open_login_providers(&mut self, kind: Option<LoginKind>, search: &str) {
        let options = login_options(&self.session.registry(), kind);
        if options.is_empty() {
            self.status(match kind {
                Some(LoginKind::OAuth) => "No account providers available.",
                Some(LoginKind::ApiKey) => "No API key providers available.",
                None => "No login providers available.",
            });
            return;
        }
        self.dialog = Some(super::Dialog::LoginProviders(kind, search.to_owned()));
        self.selector = Some(super::selectors::Selector::Providers(Box::new(
            ProviderSelector::new(Mode::Login, options, search),
        )));
    }

    /// `/logout`: pi's `showOAuthSelector("logout")`.
    pub(super) fn logout_command(&mut self) {
        let options = logout_options(&self.session.registry());
        if options.is_empty() {
            self.status("No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.");
            return;
        }
        self.dialog = Some(super::Dialog::Logout);
        self.selector = Some(super::selectors::Selector::Providers(Box::new(
            ProviderSelector::new(Mode::Logout, options, ""),
        )));
    }

    /// A provider chosen in the selector.
    pub(super) fn provider_chosen(&mut self, option: ProviderOption) {
        match self.dialog.take() {
            Some(super::Dialog::Logout) => {
                let registry = self.session.registry();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = registry.logout(&option.id).await;
                    let _ = tx.send(super::Event::LogoutDone(Box::new(option), result));
                });
            }
            Some(super::Dialog::LoginProviders(kind, search)) => {
                self.start_login(option, Back::Providers(kind, search));
            }
            _ => self.start_login(option, Back::Editor),
        }
    }

    /// The provider selector was cancelled.
    pub(super) fn providers_cancelled(&mut self, kind: Option<LoginKind>) {
        if kind.is_some() {
            self.open_login_menu(None);
        }
    }

    /// pi's `startProviderLogin`: the login dialog, or the setup notice for
    /// providers configured outside yapi.
    pub(super) fn start_login(&mut self, option: ProviderOption, back: Back) {
        let ui = super::selectors::Ui {
            theme: &self.theme,
            keys: &self.keys,
        };
        if !option.can_login {
            let mut dialog =
                LoginDialog::new(&option.name, Some(&format!("{} setup", option.name)));
            dialog.show_info(
                &format!("{} is configured outside yapi.", option.method),
                &ui,
            );
            dialog.show_close_hint(&ui);
            self.selector = Some(super::selectors::Selector::Login(Box::new(dialog)));
            self.login = Some(LoginRun {
                id: 0,
                option,
                back,
                parked: None,
                select: None,
                had_model: true,
            });
            return;
        }
        self.next_login += 1;
        let id = self.next_login;
        let mut dialog = LoginDialog::new(&option.name, None);
        if option.id == "amazon-bedrock" && option.kind == LoginKind::ApiKey {
            let theme = &self.theme;
            dialog.show_details(vec![
                styled(
                    "You can also use an AWS profile, IAM keys, or role-based credentials.",
                    theme.fg("text"),
                ),
                styled("See:", theme.fg("muted")),
                styled(
                    format!("  {}", yapi_core::auth_guidance::PROVIDER_DOCS),
                    theme.fg("accent"),
                ),
            ]);
        }
        let (interaction, mut requests) = yapi_ai::auth::Interaction::new(dialog.cancel.clone());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                if tx.send(super::Event::Auth(id, request)).is_err() {
                    break;
                }
            }
        });
        let registry = self.session.registry();
        let tx = self.tx.clone();
        let provider = option.id.clone();
        let kind = option.kind;
        let options = yapi_ai::auth::LoginOptions {
            device_id: (option.id == "openai" && kind == LoginKind::OAuth)
                .then(|| self.device_id()),
        };
        tokio::spawn(async move {
            let result = registry
                .login(&provider, kind, &interaction, &options)
                .await
                .map(drop);
            let _ = tx.send(super::Event::LoginDone(id, result));
        });
        self.selector = Some(super::selectors::Selector::Login(Box::new(dialog)));
        self.login = Some(LoginRun {
            id,
            option,
            back,
            parked: None,
            select: None,
            had_model: self.session.model().is_some(),
        });
    }

    /// pi's `getOrCreateDeviceId`.
    fn device_id(&mut self) -> String {
        if let Some(id) = self.session.settings().device_id {
            return id;
        }
        let id = yapi_core::time::uuid_v4();
        let _ = self
            .session
            .set_global_setting("deviceId", Some(serde_json::Value::String(id.clone())));
        id
    }

    /// A request from the running sign-in.
    pub(super) fn on_auth_request(&mut self, id: u64, request: yapi_ai::auth::AuthRequest) {
        let Some(run) = self.login.as_mut().filter(|run| run.id == id) else {
            return;
        };
        let ui = super::selectors::Ui {
            theme: &self.theme,
            keys: &self.keys,
        };
        match request {
            yapi_ai::auth::AuthRequest::Notify(event) => {
                let dialog = match (&mut self.selector, run.parked.as_mut()) {
                    (_, Some(parked)) => Some(parked.as_mut()),
                    (Some(super::selectors::Selector::Login(dialog)), None) => {
                        Some(dialog.as_mut())
                    }
                    _ => None,
                };
                if let Some(dialog) = dialog {
                    dialog.notify(event, &ui);
                }
            }
            yapi_ai::auth::AuthRequest::Prompt {
                prompt,
                reply,
                cancel,
            } => {
                if let AuthPrompt::Select { message, options } = &prompt {
                    // pi shows a selector in the dialog's place, then restores it.
                    if let Some(super::selectors::Selector::Login(dialog)) = self.selector.take() {
                        run.parked = Some(dialog);
                    }
                    let labels: Vec<&str> =
                        options.iter().map(|option| option.label.as_str()).collect();
                    run.select = Some((
                        reply,
                        options.iter().map(|option| option.id.clone()).collect(),
                    ));
                    let mut choice = super::selectors::ChoiceDialog::new(message, &labels);
                    if run.option.id == RADIUS {
                        choice.description = Some(RADIUS_LOGIN_INTRO.into());
                    }
                    self.dialog = Some(super::Dialog::LoginSelect);
                    self.selector = Some(super::selectors::Selector::Choice(choice));
                    return;
                }
                if let Some(super::selectors::Selector::Login(dialog)) = &mut self.selector {
                    dialog.prompt(
                        &prompt,
                        Pending {
                            reply,
                            withdrawn: cancel,
                        },
                        &ui,
                    );
                }
            }
        }
    }

    /// The select prompt was answered (`Some`) or cancelled.
    pub(super) fn login_select_done(&mut self, index: Option<usize>) {
        let Some(run) = self.login.as_mut() else {
            return;
        };
        if let Some((reply, ids)) = run.select.take()
            && let Some(id) = index.and_then(|index| ids.get(index))
        {
            let _ = reply.send(id.clone());
        }
        if let Some(dialog) = run.parked.take() {
            self.selector = Some(super::selectors::Selector::Login(dialog));
        }
    }

    /// Escape in the login dialog: the sign-in is cancelled; pi then reopens
    /// where it started.
    pub(super) fn login_cancelled(&mut self) {
        if let Some(run) = self.login.take() {
            self.selector = None;
            self.go_back(run.back);
        }
    }

    fn go_back(&mut self, back: Back) {
        match back {
            Back::Editor => {}
            Back::Menu(options) => self.open_login_menu(options),
            Back::Providers(kind, search) => self.open_login_providers(kind, &search),
        }
    }

    /// The sign-in finished.
    pub(super) fn on_login_done(&mut self, id: u64, result: Result<(), yapi_ai::auth::AuthError>) {
        let Some(run) = self.login.take_if(|run| run.id == id) else {
            return;
        };
        if matches!(
            self.selector,
            Some(super::selectors::Selector::Login(_) | super::selectors::Selector::Choice(_))
        ) {
            self.selector = None;
            self.dialog = None;
        }
        let name = run.option.name.clone();
        match result {
            Ok(()) => self.complete_login(&run),
            Err(yapi_ai::auth::AuthError::Cancelled) => self.go_back(run.back),
            Err(error) => self.error(match run.option.kind {
                LoginKind::OAuth => format!("Failed to login to {name}: {error}"),
                LoginKind::ApiKey => format!("Failed to save API key for {name}: {error}"),
            }),
        }
    }

    /// pi's `completeProviderAuthentication`: selects the provider's default
    /// model when the session had none, then refreshes its catalog. When the
    /// default model is not listed yet, selecting it waits for the refresh.
    fn complete_login(&mut self, run: &LoginRun) {
        let option = &run.option;
        let label = match option.kind {
            LoginKind::OAuth => format!("Logged in to {}", option.name),
            LoginKind::ApiKey => format!("Saved API key for {}", option.name),
        };
        let default = default_model(&option.id);
        let defer = !run.had_model
            && default.is_some_and(|default| {
                !self
                    .session
                    .available_models()
                    .iter()
                    .any(|model| model.provider == option.id && model.id == default)
            });
        if defer {
            let path = self.auth_path();
            self.status(format!(
                "{label}. Credentials saved to {path}. Refreshing model catalog…"
            ));
        } else if run.had_model {
            let path = self.auth_path();
            self.status(format!("{label}. Credentials saved to {path}"));
            self.warn_anthropic_subscription(None);
        } else {
            self.finish_login(option, &label);
        }
        self.refresh_catalogs(
            Some(vec![option.id.clone()]),
            super::catalogs::Refresh::Login {
                option: Box::new(option.clone()),
                label,
                defer,
            },
        );
    }

    fn auth_path(&self) -> String {
        self.session
            .registry()
            .auth_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    }

    /// Selects the signed-in provider's default model, as pi's
    /// `finishAuthentication`; a Radius catalog without its default gives
    /// its first model.
    pub(super) fn finish_login(&mut self, option: &ProviderOption, label: &str) {
        let path = self.auth_path();
        let default = default_model(&option.id);
        let models: Vec<yapi_types::model::Model> = self
            .session
            .available_models()
            .into_iter()
            .filter(|model| model.provider == option.id)
            .collect();
        let selection = match default {
            // pi's `llamaCppPostLoginGuidance`.
            None if option.id == yapi_ai::llama::PROVIDER_ID => Err(if models.is_empty() {
                format!(
                    "{label}. No llama.cpp models are loaded. Use /llama to load a model, then /model to select it."
                )
            } else {
                format!(
                    "{label}. Use /model to select a loaded llama.cpp model, or /llama to manage models."
                )
            }),
            None => Err(format!(
                "{label}, but no default model is configured for provider \"{}\". Use /model to select a model.",
                option.id
            )),
            Some(_) if models.is_empty() => Err(format!(
                "{label}, but no models are available for that provider. Use /model to select a model."
            )),
            Some(default) => {
                let found = models
                    .iter()
                    .find(|model| model.id == default)
                    .or_else(|| models.first().filter(|_| option.id == "radius"))
                    .cloned();
                match found {
                    None => Err(format!(
                        "{label}, but its default model \"{default}\" is not available. Use /model to select a model."
                    )),
                    Some(model) => self
                        .session
                        .set_model(model.clone())
                        .map(|()| {
                            let _ = self.session.set_global_setting(
                                "defaultProvider",
                                Some(serde_json::Value::String(model.provider.clone())),
                            );
                            let _ = self.session.set_global_setting(
                                "defaultModel",
                                Some(serde_json::Value::String(model.id.clone())),
                            );
                            model
                        })
                        .map_err(|error| {
                            format!("{label}, but selecting its default model failed: {error}. Use /model to select a model.")
                        }),
                }
            }
        };
        match selection {
            Ok(model) => {
                self.status(format!(
                    "{label}. Selected {}. Credentials saved to {path}",
                    model.id
                ));
                self.warn_anthropic_subscription(Some(&model));
            }
            Err(error) => {
                self.status(format!("{label}. Credentials saved to {path}"));
                self.error(error);
            }
        }
    }

    /// `/logout` finished.
    pub(super) fn on_logout_done(
        &mut self,
        option: &ProviderOption,
        result: Result<(), yapi_ai::auth::AuthError>,
    ) {
        match result {
            Ok(()) => self.status(match option.kind {
                LoginKind::OAuth => format!("Logged out of {}", option.name),
                LoginKind::ApiKey => format!(
                    "Removed stored API key for {}. Environment variables and models.json config are unchanged.",
                    option.name
                ),
            }),
            Err(error) => self.error(format!("Logout failed: {error}")),
        }
    }

    /// pi's `maybeWarnAboutAnthropicSubscriptionAuth`: once per run, when an
    /// Anthropic model uses a subscription token.
    pub(super) fn warn_anthropic_subscription(&mut self, model: Option<&yapi_types::model::Model>) {
        let current = self.session.model();
        let Some(model) = model.or(current.as_ref()) else {
            return;
        };
        if self.anthropic_warning_shown || model.provider != "anthropic" {
            return;
        }
        let disabled = self
            .session
            .settings()
            .warnings
            .and_then(|warnings| warnings.anthropic_extra_usage)
            == Some(false);
        if disabled {
            return;
        }
        let registry = self.session.registry();
        let subscription = registry.is_using_oauth("anthropic")
            || registry
                .known_api_key("anthropic")
                .is_some_and(|key| key.starts_with("sk-ant-oat"));
        if subscription {
            self.anthropic_warning_shown = true;
            self.warning("Anthropic subscription auth is active. Third-party harness usage draws from extra usage and is billed per token, not your Claude plan limits. Manage extra usage at https://claude.ai/settings/usage. Disable this warning in /settings.");
        }
    }
}

/// Opens `target` in the platform browser, best effort and without a shell.
fn open_browser(target: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_environment_sources() {
        assert!(is_env_list("OPENAI_API_KEY"));
        assert!(is_env_list("AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY"));
        assert!(!is_env_list("configured API key"));
        assert!(!is_env_list("--api-key"));
    }

    #[test]
    fn lists_providers_like_pi() {
        let registry = ModelRegistry::builtin();
        let options = login_options(&registry, Some(LoginKind::OAuth));
        let names: Vec<&str> = options.iter().map(|option| option.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Anthropic",
                "GitHub Copilot",
                "Kimi For Coding",
                "Meta",
                "OpenAI",
                "OpenAI Codex (legacy)",
                "OpenRouter",
                "Radius",
                "xAI"
            ]
        );
        let anthropic = find_options(&registry, "ANTHROPIC");
        assert_eq!(anthropic.len(), 2);
        assert!(
            find_options(&registry, "OpenAI Codex (legacy)")
                .iter()
                .all(|option| option.kind == LoginKind::OAuth)
        );
    }

    #[test]
    fn completes_providers_like_pi() {
        let registry = ModelRegistry::builtin();
        let providers = completion_options(login_options(&registry, None));
        let first = &providers[0];
        assert_eq!(
            (first.id.as_str(), first.description()),
            ("amazon-bedrock", "Amazon Bedrock · API key".to_owned())
        );
        let anthropic = providers
            .iter()
            .find(|provider| provider.id == "anthropic")
            .unwrap();
        assert_eq!(anthropic.description(), "Anthropic · subscription/API key");
        assert_eq!(
            anthropic.search_text(),
            "anthropic Anthropic oauth subscription api_key API key"
        );
        let matches: Vec<String> = fuzzy_filter(providers, "anth", CompletionOption::search_text)
            .into_iter()
            .map(|provider| provider.id)
            .collect();
        assert_eq!(matches, ["anthropic", "openai", "openai-codex"]);
    }
}
