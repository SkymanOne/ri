//! Which files load as extensions; pi's extension entry rules
//! (`core/extensions/loader.ts` in pi `v1.0.0`).

use std::path::{Path, PathBuf};

use crate::packages::resolve::{Manifest, ResourceType};

/// Whether the extension entry `path` is a native extension, a WebAssembly
/// component, rather than a pi extension that runs in yapi-js.
pub fn is_native(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "wasm")
}

/// The entry points of extension directory `dir`: what its manifest declares,
/// else `index.ts` or `index.js`.
pub fn entries(dir: &Path) -> Option<Vec<PathBuf>> {
    if let Some(manifest) = Manifest::read(dir) {
        let declared: Vec<PathBuf> = manifest
            .get(ResourceType::Extensions)
            .into_iter()
            .flatten()
            .map(|entry| crate::tools::path::resolve_lexically(dir, Path::new(entry)))
            .filter(|path| path.exists())
            .collect();
        if !declared.is_empty() {
            return Some(declared);
        }
    }
    ["index.ts", "index.js"]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.exists())
        .map(|path| vec![path])
}
