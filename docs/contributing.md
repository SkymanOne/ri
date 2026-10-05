# Contributing

yapi is a Cargo workspace whose crates mirror Pi's packages, so each behavior traces back to its Pi source. [AGENTS.md](https://github.com/SkymanOne/ri/blob/main/AGENTS.md) describes the architecture, the design decisions and the rules for changes. [dev/status.md](https://github.com/SkymanOne/ri/blob/main/dev/status.md) tracks progress and deferred work.

## Build and run

```sh
git clone https://github.com/SkymanOne/ri yapi
cd yapi
cargo run -p yapi --
```

## Checks

Run these before every commit. CI runs the same checks on Linux and macOS.

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

`cargo test` includes the end-to-end scenarios, which replay recorded provider streams against goldens recorded from Pi. They need `rg`, `fd` and `python3` on `PATH`.

## Comparing with Pi

```sh
cargo xtask e2e --differential         # run pi and yapi side by side on every scenario
cargo xtask e2e --record-pi            # rewrite the goldens from pi
cargo xtask bench --pi <path-to-pi>    # time and memory against pi
```

Pi installs from `tests/fixtures/pi/generator` with `npm ci`, which needs Node.js 22.19 or newer.

## Documentation

This site lives in `docs/`, is configured in `book.toml` and builds with [mdBook](https://rust-lang.github.io/mdBook/):

```sh
mdbook serve
```

A workflow publishes it to GitHub Pages on every push to `main`.
