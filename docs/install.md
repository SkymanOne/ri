# Install

yapi builds from source with stable Rust. Install Rust with [rustup](https://rustup.rs), then build and install yapi:

```sh
git clone https://github.com/SkymanOne/ri yapi
cd yapi
cargo install --locked --path crates/yapi
```

Cargo places `yapi` in `~/.cargo/bin`. The repository pins its toolchain in `rust-toolchain.toml`, and rustup installs that version on the first build.

Check the installation:

```sh
yapi --version
```

## Release binaries

Tagged releases will publish binaries for Linux and macOS on x86_64 and arm64, each with a `.sha256` checksum. No release is tagged yet.

## Platforms

Linux and macOS are fully supported. Windows builds are best effort.
