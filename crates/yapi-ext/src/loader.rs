//! The host-side module loader: Node resolution, TypeScript, and the module
//! format of each file, as pi's jiti loader treats them.
//!
//! ES modules reach the guest as source text. CommonJS files run in the guest
//! through its `require`; ES modules that import them get their exports.
//! Both may use `require`, `__filename` and `__dirname` as under jiti.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};
use oxc_resolver::{ResolveOptions, Resolver};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// Bumped when the output of [`Loader::transpile`] changes for the same input.
const TRANSPILE_VERSION: &str = "4";

/// How a file runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// An ES module.
    Module,
    /// A CommonJS module.
    CommonJs,
    /// JSON data.
    Json,
}

/// A file prepared for the guest.
#[derive(Clone, Debug)]
struct Prepared {
    format: Format,
    source: String,
}

/// Resolves and prepares modules for one instance.
pub(crate) struct Loader {
    import: Resolver,
    require: Resolver,
    cwd: PathBuf,
    cache_dir: Option<PathBuf>,
    prepared: Mutex<HashMap<PathBuf, Prepared>>,
}

fn resolver(conditions: &[&str]) -> Resolver {
    let extensions = [
        ".ts", ".tsx", ".mts", ".cts", ".js", ".mjs", ".cjs", ".json",
    ];
    Resolver::new(ResolveOptions {
        extensions: extensions.iter().map(|ext| (*ext).to_owned()).collect(),
        // TypeScript sources import their siblings by the emitted name.
        extension_alias: vec![
            (
                ".js".into(),
                vec![".ts".into(), ".tsx".into(), ".js".into()],
            ),
            (".mjs".into(), vec![".mts".into(), ".mjs".into()]),
            (".cjs".into(), vec![".cts".into(), ".cjs".into()]),
        ],
        condition_names: conditions.iter().map(|name| (*name).to_owned()).collect(),
        // Node reads only `main`; `module` is a bundler convention.
        main_fields: vec!["main".into()],
        ..ResolveOptions::default()
    })
}

fn json_text(value: &str) -> String {
    yapi_types::json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

impl Loader {
    /// A loader resolving bare paths against `cwd` and caching transpiled
    /// TypeScript under `cache_dir`.
    pub(crate) fn new(cwd: PathBuf, cache_dir: Option<PathBuf>) -> Loader {
        Loader {
            import: resolver(&["node", "import", "default"]),
            require: resolver(&["node", "require", "default"]),
            cwd,
            cache_dir,
            prepared: Mutex::new(HashMap::new()),
        }
    }

    /// The file `specifier` names when `referrer` imports (or, with
    /// `require`, requires) it.
    pub(crate) fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        require: bool,
    ) -> Result<String, String> {
        let specifier = specifier.strip_prefix("file://").unwrap_or(specifier);
        let referrer_path = referrer.strip_prefix("file://").unwrap_or(referrer);
        let base = if Path::new(referrer_path).is_absolute() {
            Path::new(referrer_path)
                .parent()
                .map_or_else(|| self.cwd.clone(), Path::to_path_buf)
        } else {
            self.cwd.clone()
        };
        let resolver = if require { &self.require } else { &self.import };
        let resolved = if Path::new(specifier).is_absolute() {
            resolver.resolve(Path::new(specifier).parent().unwrap_or(&base), specifier)
        } else {
            resolver.resolve(&base, specifier)
        };
        match resolved {
            Ok(resolution) => Ok(resolution.full_path().to_string_lossy().into_owned()),
            // jiti's message, as pi reports it.
            Err(_) => Err(format!(
                "Cannot find module '{specifier}'\nRequire stack:\n- {referrer}"
            )),
        }
    }

    /// What the guest's ES module loader gets for `path`: `{"source"}` for an
    /// ES module, or `{"kind": "cjs"}` for a file that runs through `require`.
    pub(crate) fn load(&self, path: &str) -> Result<Value, String> {
        let prepared = self.prepare(Path::new(path))?;
        Ok(match prepared.format {
            Format::Module => json!({ "source": prepared.source }),
            Format::Json => json!({ "source": format!("export default {};", prepared.source) }),
            Format::CommonJs => json!({ "kind": "cjs" }),
        })
    }

    /// What the guest's `require` gets for `path`: `{"source", "kind"}` with
    /// kind `cjs` or `json`. An ES module required this way runs as CommonJS
    /// only if it has no `import` or `export`, which the guest reports.
    pub(crate) fn source(&self, path: &str) -> Result<Value, String> {
        let prepared = self.prepare(Path::new(path))?;
        Ok(match prepared.format {
            Format::Json => json!({ "source": prepared.source, "kind": "json" }),
            Format::CommonJs => json!({ "source": prepared.source, "kind": "cjs" }),
            Format::Module => {
                return Err(format!(
                    "require() of ES module {path} is not supported in yapi extensions; use import"
                ));
            }
        })
    }

    fn prepare(&self, path: &Path) -> Result<Prepared, String> {
        let mut prepared = self
            .prepared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(found) = prepared.get(path) {
            return Ok(found.clone());
        }
        let file = file_of(path);
        if file.extension().is_some_and(|ext| ext == "node") {
            return Err(format!(
                "Native addon {} cannot be loaded in yapi extensions",
                file.display()
            ));
        }
        let text = std::fs::read_to_string(file)
            .map_err(|err| format!("Cannot read module {}: {err}", file.display()))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        // Node ignores a hashbang line; keep the line so positions hold.
        let text = match text.strip_prefix("#!") {
            Some(rest) => &rest[rest.find('\n').unwrap_or(rest.len())..],
            None => text,
        };
        let found = if file.extension().is_some_and(|ext| ext == "json") {
            serde_json::from_str::<Value>(text)
                .map_err(|err| format!("{}: {err}", file.display()))?;
            Prepared {
                format: Format::Json,
                source: text.to_owned(),
            }
        } else {
            self.transpile_cached(file, text)?
        };
        prepared.insert(path.to_path_buf(), found.clone());
        Ok(found)
    }

    fn transpile_cached(&self, path: &Path, text: &str) -> Result<Prepared, String> {
        let Some(dir) = &self.cache_dir else {
            return transpile(path, text);
        };
        let mut digest = Sha256::new();
        for part in [TRANSPILE_VERSION, &path.to_string_lossy(), text] {
            digest.update(part.as_bytes());
            digest.update([0]);
        }
        let hex: String = digest
            .finalize()
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let cached = dir.join(format!("{hex}.js"));
        if let Ok(source) = std::fs::read_to_string(&cached)
            && let Some((tag, source)) = source.split_once('\n')
        {
            let format = if tag == "//cjs" {
                Format::CommonJs
            } else {
                Format::Module
            };
            return Ok(Prepared {
                format,
                source: source.to_owned(),
            });
        }
        let prepared = transpile(path, text)?;
        let tag = if prepared.format == Format::CommonJs {
            "//cjs"
        } else {
            "//esm"
        };
        // A missing cache only costs the next load a transpile.
        if std::fs::create_dir_all(dir).is_ok() {
            let partial = cached.with_extension(format!("{}.tmp", std::process::id()));
            if std::fs::write(&partial, format!("{tag}\n{}", prepared.source)).is_ok() {
                let _ = std::fs::rename(&partial, &cached);
            }
        }
        Ok(prepared)
    }
}

/// The file module `path` names, without the query or fragment an import
/// such as `./a.js?v=1` adds and the module keeps in its name.
fn file_of(path: &Path) -> &Path {
    if path.exists() {
        return path;
    }
    match path
        .to_str()
        .and_then(|text| text.find(['?', '#']).map(|at| &text[..at]))
    {
        Some(file) => Path::new(file),
        None => path,
    }
}

/// Strips TypeScript, decides the module format and adds what jiti provides
/// to ES modules: `require`, `__filename`, `__dirname` and `import.meta` paths.
fn transpile(path: &Path, text: &str) -> Result<Prepared, String> {
    // QuickJS takes sources as C strings. A NUL is legal only in strings,
    // templates, regular expressions and comments, where `\u0000` stands for
    // the same character.
    let text = &text.replace('\0', "\\u0000");
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, text, source_type).parse();
    // pi's loader strips types without checking them, so only syntax errors
    // fail, not the TypeScript grammar checks that oxc reports with `TS` codes.
    if let Some(error) = parsed
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code.scope.as_deref() != Some("TS"))
    {
        return Err(format!("{}: {error}", path.display()));
    }
    let has_module_syntax = parsed.module_record.has_module_syntax;
    let mut program = parsed.program;
    // The TypeScript transform needs enum members evaluated.
    let semantic = SemanticBuilder::new()
        .with_enum_eval(true)
        .build(&program)
        .semantic;
    let scoping = semantic.into_scoping();
    let unresolved = |name: &str| {
        scoping
            .root_unresolved_references()
            .keys()
            .any(|key| key.as_str() == name)
    };
    // As in Node, a `.js` file without module syntax is CommonJS: a script
    // such as a polyfill runs when it is required.
    let explicit_module = path
        .extension()
        .is_some_and(|ext| ext == "mjs" || ext == "mts");
    let common_js = source_type.is_commonjs() || (!has_module_syntax && !explicit_module);
    let uses_require = unresolved("require");
    let uses_filename = unresolved("__filename");
    let uses_dirname = unresolved("__dirname");
    let typescript = source_type.is_typescript() || source_type.is_jsx();
    let mut source = if typescript {
        let options = TransformOptions::default();
        let transformed =
            Transformer::new(&allocator, path, &options).build_with_scoping(scoping, &mut program);
        if let Some(error) = transformed.diagnostics.first() {
            return Err(format!("{}: {error}", path.display()));
        }
        Codegen::new().build(&program).code
    } else {
        text.to_owned()
    };
    if common_js {
        return Ok(Prepared {
            format: Format::CommonJs,
            source,
        });
    }
    // One line, so stack traces keep the file's line numbers.
    let file = path.to_string_lossy();
    let dir = path
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut prelude = format!(
        "import.meta.url = {}; import.meta.filename = {}; import.meta.dirname = {}; import.meta.resolve = globalThis.__yapi_import_meta_resolve({});",
        json_text(&format!("file://{file}")),
        json_text(&file),
        json_text(&dir),
        json_text(&file),
    );
    if uses_require {
        prelude.push_str(&format!(
            " const require = globalThis.__yapi_require_for({});",
            json_text(&file)
        ));
    }
    if uses_filename {
        prelude.push_str(&format!(" const __filename = {};", json_text(&file)));
    }
    if uses_dirname {
        prelude.push_str(&format!(" const __dirname = {};", json_text(&dir)));
    }
    source.insert_str(0, &prelude);
    Ok(Prepared {
        format: Format::Module,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("yapi-ext-loader-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn strips_types_and_keeps_modules() {
        let dir = scratch("ts");
        let path = write(
            &dir,
            "a.ts",
            "import x from './b.js';\nexport default function f(n: number): string { return String(n); }\n",
        );
        let loader = Loader::new(dir.clone(), None);
        let loaded = loader.load(path.to_str().unwrap()).unwrap();
        let source = loaded["source"].as_str().unwrap();
        assert!(source.contains("function f(n)"), "{source}");
        assert!(
            source.starts_with("import.meta.url = \"file://"),
            "{source}"
        );
        assert_eq!(source.lines().count(), 3, "{source}");
    }

    #[test]
    fn detects_common_js_and_jiti_globals() {
        let dir = scratch("cjs");
        let cjs = write(&dir, "c.js", "module.exports = { a: require('./d') };\n");
        let esm = write(
            &dir,
            "e.ts",
            "export const here = __dirname + require('x');\n",
        );
        let loader = Loader::new(dir.clone(), None);
        assert_eq!(
            loader.load(cjs.to_str().unwrap()).unwrap(),
            json!({"kind": "cjs"})
        );
        assert_eq!(
            loader.source(cjs.to_str().unwrap()).unwrap()["kind"],
            json!("cjs")
        );
        let source = loader.load(esm.to_str().unwrap()).unwrap()["source"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            source.contains("const require = globalThis.__yapi_require_for("),
            "{source}"
        );
        assert!(source.contains("const __dirname = "), "{source}");
        assert!(!source.contains("const __filename"), "{source}");
    }

    #[test]
    fn resolves_typescript_siblings_and_packages() {
        let dir = scratch("resolve");
        write(&dir, "src/util.ts", "export const x = 1;\n");
        let main = write(&dir, "src/main.ts", "import { x } from './util.js';\n");
        write(
            &dir,
            "node_modules/pkg/package.json",
            r#"{"name":"pkg","exports":{".":{"import":"./esm.js","require":"./cjs.js"}}}"#,
        );
        write(&dir, "node_modules/pkg/esm.js", "export default 1;\n");
        write(&dir, "node_modules/pkg/cjs.js", "module.exports = 1;\n");
        let loader = Loader::new(dir.clone(), None);
        let referrer = main.to_str().unwrap();
        assert!(
            loader
                .resolve("./util.js", referrer, false)
                .unwrap()
                .ends_with("src/util.ts")
        );
        assert!(
            loader
                .resolve("pkg", referrer, false)
                .unwrap()
                .ends_with("pkg/esm.js")
        );
        assert!(
            loader
                .resolve("pkg", referrer, true)
                .unwrap()
                .ends_with("pkg/cjs.js")
        );
        let missing = loader.resolve("nope", referrer, false).unwrap_err();
        assert!(
            missing.starts_with("Cannot find module 'nope'"),
            "{missing}"
        );
    }

    #[test]
    fn caches_transpiled_sources() {
        let dir = scratch("cache");
        let cache = dir.join("cache");
        let path = write(&dir, "a.ts", "export const a: number = 1;\n");
        let first = Loader::new(dir.clone(), Some(cache.clone()))
            .load(path.to_str().unwrap())
            .unwrap();
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
        let second = Loader::new(dir.clone(), Some(cache))
            .load(path.to_str().unwrap())
            .unwrap();
        assert_eq!(first, second);
    }
}
