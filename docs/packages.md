# Packages

A package bundles extensions, skills, prompt templates and themes. ri installs pi packages from npm, git or a local folder without Node.js.

```sh
ri install npm:@scope/some-pi-package     # from npm
ri install git:github.com/user/repo       # from git
ri install ./my-package -l                 # a local folder, for this project only
ri install ./shout.wasm                    # a single native extension
ri list
ri update --extensions
ri remove npm:@scope/some-pi-package
```

Installed packages are recorded in `settings.json` as pi records them. `-l` installs into the project's `.ri` folder instead of the global one. Local folders and files are used where they are, not copied.

`ri list` shows each package with its install location. A tag after the source says what kind of extensions it contains: `[npm]` for pi extensions in JavaScript or TypeScript, `[wasm]` for native extensions, and `[npm, wasm]` for both. Packages that hold only skills, prompt templates or themes, and packages that are not installed yet, have no tag.

```
User packages:
  npm:pi-mcp-adapter [npm]
    /home/you/.ri/agent/npm/node_modules/pi-mcp-adapter
  /home/you/extensions/shout.wasm [wasm]
    /home/you/extensions/shout.wasm
```

## Native extensions

Packages can contain [native extensions](native-extensions.md) as `.wasm` files next to, or instead of, JavaScript ones. ri loads `.wasm` files from a package's `extensions` folder and from the files its manifest names. `ri install` also accepts a single `.wasm` file.

## The npm client

ri has a built-in npm registry client. It resolves versions with npm's range syntax, checks integrity hashes and lays out `node_modules` as npm does. It does not run lifecycle scripts. Those mostly build native addons, which the WebAssembly runtime cannot load, so such addons are installed unbuilt and fail only if the extension loads one.

pi's `npmCommand` setting still overrides the built-in client.

## Package manifests

ri reads the `pi` key of `package.json` to find a package's resources. An optional `ri` key with the same shape takes precedence in ri, which lets one package ship a [native build](native-extensions.md) for ri and JavaScript for pi. Packages without a manifest use the conventional folders: `extensions`, `skills`, `prompts` and `themes`.

pi's [package documentation](https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs/packages.md) covers the manifest format.

## Turning resources on and off

`ri config` lists every extension, skill, prompt template and theme that your packages, settings and folders provide. Space toggles one, and ri saves the choice as a pattern in `settings.json`. Tab switches between global settings and overrides for the current project. `ri config -l` starts with the project.
