// CommonJS for npm packages: `require` resolves through the host and runs
// module sources synchronously. ES modules that import a CommonJS file get a
// host-generated wrapper that calls `__yapi_cjs`.
(() => {
	"use strict";
	const yapi = globalThis.__yapi;
	const builtins = globalThis.__yapi_builtins;
	const cache = new Map();

	const builtinName = yapi.builtinName;
	const dirname = (path) => builtins.path.dirname(path);

	function requireFor(referrer) {
		const require = (specifier) => {
			const builtin = builtinName(specifier);
			if (builtin) return builtins[builtin];
			return load(yapi.request("module.resolve", { specifier, referrer, kind: "require" }));
		};
		require.resolve = (specifier) => {
			const builtin = builtinName(specifier);
			if (builtin) return builtin;
			return yapi.request("module.resolve", { specifier, referrer, kind: "require" });
		};
		require.cache = Object.create(null);
		require.main = undefined;
		return require;
	}

	function load(path) {
		const cached = cache.get(path);
		if (cached) return cached.exports;
		const { source, kind } = yapi.request("module.source", { path });
		const module = { id: path, filename: path, path: dirname(path), exports: {}, loaded: false, children: [], paths: [], parent: null };
		module.require = requireFor(path);
		cache.set(path, module);
		try {
			if (kind === "json") module.exports = JSON.parse(source);
			else {
				// Node's wrapper, on the first line so positions hold.
				const run = globalThis.__yapi_native.compile(`(function (exports, require, module, __filename, __dirname) {${source}\n})`, path);
				run.call(module.exports, module.exports, module.require, module, path, dirname(path));
			}
		} catch (error) {
			cache.delete(path);
			throw error;
		}
		module.loaded = true;
		return module.exports;
	}

	globalThis.__yapi_cjs = load;
	globalThis.__yapi_require_for = requireFor;
	// Node's synchronous `import.meta.resolve`: the URL an import of
	// `specifier` from `referrer` would load.
	globalThis.__yapi_import_meta_resolve = (referrer) => (specifier) => {
		const builtin = builtinName(specifier);
		if (builtin) return `node:${builtin}`;
		return builtins.url.pathToFileURL(yapi.request("module.resolve", { specifier: String(specifier), referrer, kind: "import" })).href;
	};
})();

// The names an ES module wrapper of CommonJS module `path` exports; loads it.
globalThis.__yapi.cjsExports = (path) => {
	const exports = globalThis.__yapi_cjs(path);
	if (exports === null || (typeof exports !== "object" && typeof exports !== "function")) return [];
	return globalThis.__yapi.exportNames(exports);
};
