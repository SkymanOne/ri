# {{project-name}}

A native extension for [yapi](https://github.com/SkymanOne/yapi), written in Rust with the `yapi-extension-api` crate. `src/lib.rs` starts as yapi's `hello` example: a `shout` tool, a `/hello` command, a `--shout-suffix` flag and two event handlers. Replace them with your own.

The project is a yapi package. `package.json` names the built extension, `extensions/{{crate_name}}.wasm`.

## Build

```sh
rustup target add wasm32-wasip2    # once
cargo build --release
cp target/wasm32-wasip2/release/{{crate_name}}.wasm extensions/
```

`.cargo/config.toml` makes `wasm32-wasip2` the default target, so `cargo build` builds the WebAssembly component.

## Try it

Load the package for one run without installing it:

```sh
yapi -e .
```

## Install it

```sh
yapi install .       # for every project
yapi install . -l    # for this project only
```

yapi loads the `.wasm` file from this folder, so a rebuild takes effect the next time yapi starts.

## Share it

Commit `extensions/{{crate_name}}.wasm` along with the sources. yapi installs prebuilt files and never compiles code. Then push the repository, and others install it by its URL:

```sh
yapi install git:github.com/<you>/{{project-name}}
```

To publish on npm, run `npm publish`, and others install it with `yapi install npm:{{project-name}}`.

The [native extension guide](https://skymanone.github.io/yapi/native-extensions.html) documents the API.
