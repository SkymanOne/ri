# Install

yapi is one executable. Releases have it for Linux and macOS on x86_64 and arm64, and every platform with a recent Rust toolchain can build it from source.

The first release is not tagged yet. Until it is, the install script and cargo-binstall report that no release exists, and [building from source](#from-source) works.

## Install script

```sh
curl -fsSL https://raw.githubusercontent.com/SkymanOne/ri/main/install.sh | sh
```

The script downloads the latest release binary for your system from GitHub, checks it against its published SHA-256 checksum, and installs it as `~/.local/bin/yapi`. Options go after `sh -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/SkymanOne/ri/main/install.sh | sh -s -- --version v0.1.0 --to /usr/local/bin
```

`YAPI_VERSION` and `YAPI_INSTALL_DIR` set the same as `--version` and `--to`. Running the script again replaces the installed binary, which is how yapi updates.

## cargo-binstall

[cargo-binstall](https://github.com/cargo-bins/cargo-binstall) installs the same release binary through Cargo, into `~/.cargo/bin`:

```sh
cargo binstall --git https://github.com/SkymanOne/ri yapi
```

Keep `--git`. On crates.io, `yapi` names an unrelated crate. With `--git`, cargo-binstall reads yapi's manifest from this repository and downloads the release binary for your system. yapi's manifest turns off cargo-binstall's other ways of installing, which would fetch `yapi` from crates.io, so a system without a release binary gets an error rather than another program.

## Release binaries

Each [release](https://github.com/SkymanOne/ri/releases) has the binary for each platform as `yapi-<target>.tar.gz`, which holds only `yapi`, next to a `.sha256` checksum. The names carry no version, so the latest release's binary is always at `https://github.com/SkymanOne/ri/releases/latest/download/yapi-<target>.tar.gz`.

| Target | Systems |
|---|---|
| `x86_64-unknown-linux-gnu` | Linux on x86_64 with glibc 2.35 or newer, such as Ubuntu 22.04, Debian 12 and Fedora 36 |
| `aarch64-unknown-linux-gnu` | Linux on arm64 with glibc 2.35 or newer |
| `x86_64-apple-darwin` | macOS on Intel |
| `aarch64-apple-darwin` | macOS on Apple silicon |

Without the script, unpack the binary straight into a directory on `PATH`:

```sh
curl -fsSL https://github.com/SkymanOne/ri/releases/latest/download/yapi-aarch64-apple-darwin.tar.gz | tar xzf - -C ~/.local/bin
```

To check the download first, fetch the archive and its checksum:

```sh
curl -fsSLO https://github.com/SkymanOne/ri/releases/latest/download/yapi-x86_64-unknown-linux-gnu.tar.gz
curl -fsSLO https://github.com/SkymanOne/ri/releases/latest/download/yapi-x86_64-unknown-linux-gnu.tar.gz.sha256
sha256sum -c yapi-x86_64-unknown-linux-gnu.tar.gz.sha256
tar xzf yapi-x86_64-unknown-linux-gnu.tar.gz -C ~/.local/bin
```

On macOS, `shasum -a 256 -c` checks the file. For a particular release, replace `latest/download` with `download/<tag>`, such as `download/v0.1.0`. Systems with an older glibc, or with musl such as Alpine, build from source.

## From source

yapi builds with Rust 1.99 or newer. Install Rust with [rustup](https://rustup.rs), or update it with `rustup update`, then build and install yapi from the repository:

```sh
cargo install --locked --git https://github.com/SkymanOne/ri yapi
```

Cargo places `yapi` in `~/.cargo/bin`. To build from a clone instead, as for development:

```sh
git clone https://github.com/SkymanOne/ri yapi
cd yapi
cargo install --locked --path crates/yapi
```

Inside the clone, rustup uses the toolchain pinned in `rust-toolchain.toml` and installs it on the first build.

## Check the installation

```sh
yapi --version
```

## Platforms

Linux and macOS are fully supported. Windows builds are best effort.
