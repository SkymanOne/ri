//! The `codemode` tool: JavaScript the model writes that calls other tools,
//! run in a fresh `yapi-js` instance without grants. Port of pi's built-in
//! codemode extension (`extensions/codemode` in pi-coding-agent `v1.0.0`).
//!
//! The tool is registered inactive. While it is active, its description
//! lists the tools scripts may call and the declared tools say how scripts
//! call them.

mod declarations;
mod models;
mod run;

pub(crate) use models::{model_type, models_of_type};
pub(crate) use run::Runner;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use yapi_agent::{Tool, UpdateSink};
use yapi_core::agent_session::WeakSession;
use yapi_core::extensions::{Context, Extension, Loadout, Tools, builtin_source, codemode};
use yapi_core::tools::{Exposure, RegisteredTool};
use yapi_types::event::ToolResult;
use yapi_types::message::ToolDeclaration;
use yapi_types::rpc::SourceInfo;
use yapi_types::sync::lock;

use declarations::Declaration;

pub use yapi_core::extensions::codemode::NAME;

/// What scripts read about `models`: pi's codemode reference, adapted.
const REFERENCE: &str = include_str!("codemode.md");

/// Where the reference is written, once an extension has a place for it.
static DOCS: OnceLock<PathBuf> = OnceLock::new();

/// The reference's path, when scripts reach `models`.
pub(crate) fn docs() -> Option<String> {
    DOCS.get().map(|path| path.display().to_string())
}

/// Writes the reference to `path` unless it already holds it.
fn write_reference(path: &Path) {
    if std::fs::read_to_string(path).is_ok_and(|text| text == REFERENCE) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Without the file the model reads the API from the result errors.
    let _ = std::fs::write(path, REFERENCE);
}

/// pi's `CODEMODE_SOURCE_GRAMMAR`, declared for grammar-constrained sampling.
const GRAMMAR: &str = r"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
";

/// What a script sees of `tool`; pi's `toCodemodeDeclaration`.
fn declaration(tool: &RegisteredTool) -> Declaration {
    let own = tool.tool.declaration();
    Declaration {
        name: own.name.clone(),
        description: own.description.clone(),
        input: own.parameters.clone(),
        output: tool
            .tool
            .output_schema()
            .cloned()
            .unwrap_or_else(|| json!({"type": "string"})),
        namespace: tool.namespace.clone(),
    }
}

struct CodemodeTool {
    declaration: ToolDeclaration,
    runner: Arc<run::Runner>,
    session: Arc<Mutex<WeakSession>>,
}

impl Tool for CodemodeTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn execute(
        &self,
        call_id: String,
        args: Value,
        cancel: CancellationToken,
        updates: UpdateSink,
    ) -> BoxFuture<'_, Result<ToolResult, String>> {
        let session = lock(&self.session).upgrade();
        Box::pin(self.runner.execute(session, call_id, args, cancel, updates))
    }
}

/// The tool's definition, as the API facade's `createCodemodeExtension`
/// registers it for pi packages that decorate codemode.
pub(crate) fn definition() -> Value {
    json!({
        "name": NAME,
        "label": NAME,
        "description": declarations::description(&[], &HashSet::new(), None, docs().as_deref()),
        "promptSnippet": codemode::SNIPPET,
        "promptGuidelines": [codemode::GUIDELINE],
        "parameters": codemode::parameters(),
        "constrainedSampling": constrained_sampling(),
    })
}

fn constrained_sampling() -> Value {
    json!({"type": "grammar", "variants": {"openai_lark": GRAMMAR}})
}

/// Whether `tool` is codemode: yapi's, or the facade's that a package
/// registered in its place.
fn is_codemode(tool: &RegisteredTool) -> bool {
    let declaration = tool.tool.declaration();
    declaration.name == NAME
        && tool.exposure == Exposure::ModelOnly
        && declaration.parameters == codemode::parameters()
}

/// pi's built-in codemode extension. Each session gets its own.
pub struct CodemodeExtension {
    runner: Arc<run::Runner>,
    session: Arc<Mutex<WeakSession>>,
    /// The reference scripts read about `models`.
    docs: Option<PathBuf>,
}

impl CodemodeExtension {
    /// The extension; compiled runtimes are cached in `cache_dir`. With
    /// `docs`, the path of its reference, scripts reach `models`, pi's
    /// classifiers and image models.
    pub fn new(cache_dir: Option<PathBuf>, docs: Option<PathBuf>) -> CodemodeExtension {
        if let Some(path) = &docs {
            DOCS.get_or_init(|| path.clone());
        }
        let shown = docs.as_ref().map(|path| path.display().to_string());
        CodemodeExtension {
            runner: Arc::new(run::Runner::new(cache_dir, shown)),
            session: Arc::default(),
            docs,
        }
    }

    fn docs_text(&self) -> Option<String> {
        self.docs.as_ref().map(|path| path.display().to_string())
    }
}

impl Extension for CodemodeExtension {
    fn source(&self) -> SourceInfo {
        builtin_source(NAME)
    }

    /// Registers the tool unless an extension registered its own `codemode`,
    /// which replaces the built-in as in pi.
    fn load(&self, tools: &Tools) {
        if tools.all().iter().any(|tool| tool.name == NAME) {
            return;
        }
        let tool: Arc<dyn Tool> = Arc::new(CodemodeTool {
            declaration: ToolDeclaration {
                name: NAME.into(),
                description: declarations::description(
                    &[],
                    &HashSet::new(),
                    None,
                    self.docs_text().as_deref(),
                ),
                parameters: codemode::parameters(),
                constrained_sampling: Some(constrained_sampling()),
            },
            runner: self.runner.clone(),
            session: self.session.clone(),
        });
        tools.register(RegisteredTool {
            tool,
            snippet: Some(codemode::SNIPPET.into()),
            guidelines: vec![codemode::GUIDELINE.into()],
            exposure: Exposure::ModelOnly,
            namespace: None,
            default_active: false,
        });
    }

    fn session_start<'a>(&'a self, ctx: &'a Context) -> BoxFuture<'a, ()> {
        *lock(&self.session) = ctx.session.clone();
        Box::pin(async {})
    }

    /// pi's `prepareCodemodeLoadout` in `on` mode: declared tools that scripts
    /// may call say how, and the description lists the callable tools that
    /// are not declared directly.
    fn prepare_loadout(&self, loadout: &Loadout) -> HashMap<String, String> {
        let mut descriptions = HashMap::new();
        let Some(own) = loadout.declared.iter().find(|tool| tool.name() == NAME) else {
            return descriptions;
        };
        if !is_codemode(own) {
            return descriptions;
        }
        let settings = lock(&self.session)
            .upgrade()
            .map(|session| session.settings())
            .unwrap_or_default();
        let budget = settings
            .codemode
            .and_then(|codemode| codemode.inline_budget)
            .unwrap_or(declarations::DEFAULT_INLINE_BUDGET);
        let callable: Vec<&RegisteredTool> = loadout
            .callable
            .iter()
            .filter(|tool| tool.name() != NAME)
            .collect();
        for tool in &loadout.declared {
            if callable
                .iter()
                .any(|callable| callable.name() == tool.name())
            {
                descriptions.insert(
                    tool.name().to_owned(),
                    declarations::script_call(&declaration(tool)),
                );
            }
        }
        let listed: Vec<&RegisteredTool> = callable
            .into_iter()
            .filter(|tool| tool.exposure != Exposure::Direct)
            .collect();
        let deferred: HashSet<String> = listed
            .iter()
            .filter(|tool| tool.exposure == Exposure::Deferred)
            .map(|tool| tool.name().to_owned())
            .collect();
        let listed: Vec<Declaration> = listed.into_iter().map(declaration).collect();
        // Codemode is active, so the model may read the reference.
        if let Some(path) = &self.docs {
            write_reference(path);
        }
        descriptions.insert(
            NAME.to_owned(),
            declarations::description(
                &listed,
                &deferred,
                Some(budget),
                self.docs_text().as_deref(),
            ),
        );
        descriptions
    }
}
