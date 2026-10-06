# Install

yapi is one executable. Releases have it for Linux and macOS on x86_64 and arm64, and every platform with a recent Rust toolchain can build it from source.

## Install script

```sh
curl -fsSL https://raw.githubusercontent.com/SkymanOne/yapi/main/install.sh | sh
```

The script downloads the latest release binary for your system from GitHub, checks it against its published SHA-256 checksum, and installs it as `~/.local/bin/yapi`. Options go after `sh -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/SkymanOne/yapi/main/install.sh | sh -s -- --version v0.1.0 --to /usr/local/bin
```

`YAPI_VERSION` and `YAPI_INSTALL_DIR` set the same as `--version` and `--to`.

## cargo-binstall

[cargo-binstall](https://github.com/cargo-bins/cargo-binstall) installs the same release binary through Cargo, into `~/.cargo/bin`:

```sh
cargo binstall --git https://github.com/SkymanOne/yapi yapi
```

Keep `--git`. On crates.io, `yapi` names an unrelated crate. With `--git`, cargo-binstall reads yapi's manifest on the `main` branch and downloads the release binary for the version it names, so it works when `main` names a released version. If it reports that the release does not exist, use the install script. yapi's manifest turns off cargo-binstall's other ways of installing, which would fetch `yapi` from crates.io, so a system without a release binary gets an error rather than another program.

## Release binaries

Each [release](https://github.com/SkymanOne/yapi/releases) has the binary for each platform as `yapi-<target>.tar.gz`, next to a `.sha256` checksum. The archive holds `yapi` and its license files, `LICENSE-MIT`, `LICENSE-APACHE` and `THIRD-PARTY-NOTICES`. The install script installs only `yapi`. The names carry no version, so the latest release's binary is always at `https://github.com/SkymanOne/yapi/releases/latest/download/yapi-<target>.tar.gz`.

| Target | Systems |
|---|---|
| `x86_64-unknown-linux-gnu` | Linux on x86_64 with glibc 2.35 or newer, such as Ubuntu 22.04, Debian 12 and Fedora 36 |
| `aarch64-unknown-linux-gnu` | Linux on arm64 with glibc 2.35 or newer |
| `x86_64-apple-darwin` | macOS on Intel |
| `aarch64-apple-darwin` | macOS on Apple silicon |

Without the script, unpack the binary straight into a directory on `PATH`:

```sh
curl -fsSL https://github.com/SkymanOne/yapi/releases/latest/download/yapi-aarch64-apple-darwin.tar.gz | tar xzf - -C ~/.local/bin yapi
```

To check the download first, fetch the archive and its checksum:

```sh
curl -fsSLO https://github.com/SkymanOne/yapi/releases/latest/download/yapi-x86_64-unknown-linux-gnu.tar.gz
curl -fsSLO https://github.com/SkymanOne/yapi/releases/latest/download/yapi-x86_64-unknown-linux-gnu.tar.gz.sha256
sha256sum -c yapi-x86_64-unknown-linux-gnu.tar.gz.sha256
tar xzf yapi-x86_64-unknown-linux-gnu.tar.gz -C ~/.local/bin yapi
```

On macOS, `shasum -a 256 -c` checks the file. For a particular release, replace `latest/download` with `download/<tag>`, such as `download/v0.1.0`. Systems with an older glibc, or with musl such as Alpine, build from source. If macOS refuses to open a binary from an archive downloaded in a browser, see [Troubleshooting](troubleshooting.md#macos-blocks-a-downloaded-binary).

## From source

yapi builds with Rust 1.99 or newer. Install Rust with [rustup](https://rustup.rs), or update it with `rustup update`, then build and install a release from the repository:

```sh
cargo install --locked --git https://github.com/SkymanOne/yapi --tag v0.1.0 yapi
```

Cargo places `yapi` in `~/.cargo/bin`. Without `--tag`, Cargo builds the latest commit on `main`. To build from a clone instead, as for development:

```sh
git clone https://github.com/SkymanOne/yapi yapi
cd yapi
cargo install --locked --path crates/yapi
```

Inside the clone, `rust-toolchain.toml` selects the stable toolchain with Clippy and rustfmt. rustup installs it on the first build if needed, but does not update an older one, so run `rustup update` if Cargo reports that rustc is too old.

## Check the installation

```sh
yapi --version
```

## Upgrade

yapi does not update itself. `yapi update` updates packages and model catalogs only. Upgrade yapi the way you installed it:

| Installed with | Upgrade |
|---|---|
| Install script | Run the script again. It replaces the binary with the latest release, or the one `--version` names. |
| cargo-binstall | Run the same `cargo binstall` command again. |
| Release archive | Unpack the new archive over the old binary. |
| Cargo, from source | Run `cargo install` again with the new release's tag. |

Settings, credentials, sessions and packages are kept.

## Uninstall

Remove the binary the way you installed it:

- Install script or release archive: `rm ~/.local/bin/yapi`, or the file in the directory you chose.
- cargo-binstall or Cargo: `cargo uninstall yapi`.

yapi keeps its settings, credentials, sessions, packages and caches in `~/.yapi`, or in `YAPI_CODING_AGENT_DIR` when it is set, and project resources in each project's `.yapi` folder. Delete them to remove that state as well.

## Platforms

Linux and macOS are fully supported. Windows builds are best effort.
