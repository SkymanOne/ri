# Packages

A package bundles extensions, skills, prompt templates and themes. yapi installs Pi packages from npm, git or a local folder without Node.js.

```sh
yapi install npm:@scope/some-pi-package     # from npm
yapi install git:github.com/user/repo       # from git
yapi install ./my-package -l                 # a local folder, for this project only
yapi install ./shout.wasm                    # a single native extension
yapi list
yapi update --extensions
yapi remove npm:@scope/some-pi-package
```

Installed packages are recorded in `settings.json` as Pi records them. `-l` installs into the project's `.yapi` folder instead of the global one. Local folders and files are used where they are, not copied.

To try a package for one run without adding it to settings, pass it to `-e`:

```sh
yapi -e npm:@scope/some-pi-package
yapi -e git:github.com/user/repo
```

yapi installs these into `~/.yapi/agent/tmp/extensions` and reuses them on later runs. An npm package is installed again when its version no longer matches the source's range, and a git source without a ref is updated on each run. With `--offline` or `PI_OFFLINE`, nothing is installed or updated, and a source that is not installed yet is skipped.

`yapi list` shows each package with its install location. A tag after the source says what kind of extensions it contains: `[npm]` for Pi extensions in JavaScript or TypeScript, `[wasm]` for native extensions, and `[npm, wasm]` for both. Packages that hold only skills, prompt templates or themes, and packages that are not installed yet, have no tag.

```
User packages:
  npm:pi-mcp-adapter [npm]
    /home/you/.yapi/agent/npm/node_modules/pi-mcp-adapter
  /home/you/extensions/shout.wasm [wasm]
    /home/you/extensions/shout.wasm
```

## Native extensions

Packages can contain [native extensions](native-extensions.md) as `.wasm` files next to, or instead of, JavaScript ones. yapi loads `.wasm` files from a package's `extensions` folder and from the files its manifest names. `yapi install` also accepts a single `.wasm` file.

## The npm client

yapi has a built-in npm registry client. It resolves versions with npm's range syntax, checks integrity hashes and lays out `node_modules` as npm does. It does not run lifecycle scripts. Those mostly build native addons, which the WebAssembly runtime cannot load, so such addons are installed unbuilt and fail only if the extension loads one.

The client reads registries and credentials as npm does, from `npm_config_*` environment variables and then from `~/.npmrc` or the file `npm_config_userconfig` names. It reads `registry`, `@scope:registry` and each registry's `_authToken`, `_auth`, or `username` and `_password`, and expands `${VAR}` references. As with npm, credentials go only to the registry their key names.

```ini
@acme:registry=https://npm.acme.dev/
//npm.acme.dev/:_authToken=${ACME_NPM_TOKEN}
```

Pi's `npmCommand` setting replaces the built-in client for installing and updating npm sources. Removals and the dependencies of git packages always use the built-in client.

## Package manifests

yapi reads the `pi` key of `package.json` to find a package's resources. An optional `yapi` key with the same shape takes precedence in yapi, which lets one package ship a [native build](native-extensions.md) for yapi and JavaScript for Pi. Packages without a manifest use the conventional folders: `extensions`, `skills`, `prompts` and `themes`.

Pi's [package documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/packages.md) covers the manifest format.

## Planned: manifests and registry

These features are planned and not available yet.

- **Package manifests with capabilities.** A WebAssembly package declares in its own manifest the grants it needs, such as file, process, network or environment access. You approve them, and yapi runs the package with only those grants. Events on Pi's `pi.events` bus then reach only packages with the same grants, unless your settings let a package reach the others. A package's own manifest cannot allow it.
- **Project manifests.** A `yapi.toml` file at a project's root lists the packages, extensions and skills the project uses. Anyone who checks out the project installs them in one step and gets the same versions, as `Cargo.toml` does for a Rust project.
- **A package registry.** A central registry of yapi packages, so manifests can name packages and versions instead of npm or git sources.

## Turning resources on and off

`yapi config` lists every extension, skill, prompt template and theme that your packages, settings and folders provide. Space toggles one, and yapi saves the choice as a pattern in `settings.json`. Tab switches between global settings and overrides for the current project. `yapi config -l` starts with the project.
