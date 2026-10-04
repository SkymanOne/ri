//! The `codemode` tool: JavaScript the model writes that calls other tools,
//! run in a fresh `ri-js` instance without grants. Port of pi's built-in
//! codemode extension (`extensions/codemode` in pi-coding-agent `v1.0.0`).
//!
//! The tool is registered inactive. While it is active, its description
//! lists the tools scripts may call and the declared tools say how scripts
//! call them.

mod declarations;
mod run;

pub(crate) use run::Runner;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use ri_agent::{Tool, UpdateSink};
use ri_core::agent_session::WeakSession;
use ri_core::extensions::{Context, Extension, Loadout, Tools, builtin_source, codemode};
use ri_core::tools::{Exposure, RegisteredTool};
use ri_types::event::ToolResult;
use ri_types::message::ToolDeclaration;
use ri_types::rpc::SourceInfo;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use declarations::Declaration;

pub use ri_core::extensions::codemode::NAME;

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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        "description": declarations::description(&[], &HashSet::new(), None),
        "promptSnippet": codemode::SNIPPET,
        "promptGuidelines": [codemode::GUIDELINE],
        "parameters": codemode::parameters(),
        "constrainedSampling": constrained_sampling(),
    })
}

fn constrained_sampling() -> Value {
    json!({"type": "grammar", "variants": {"openai_lark": GRAMMAR}})
}

/// Whether `tool` is codemode: ri's, or the facade's that a package
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
}

impl CodemodeExtension {
    /// The extension; compiled runtimes are cached in `cache_dir`.
    pub fn new(cache_dir: Option<PathBuf>) -> CodemodeExtension {
        CodemodeExtension {
            runner: Arc::new(run::Runner::new(cache_dir)),
            session: Arc::default(),
        }
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
                description: declarations::description(&[], &HashSet::new(), None),
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
        descriptions.insert(
            NAME.to_owned(),
            declarations::description(&listed, &deferred, Some(budget)),
        );
        descriptions
    }
}
