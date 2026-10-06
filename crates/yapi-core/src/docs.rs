//! The documentation the model reads about yapi: the yapi book and pi's
//! README, docs and examples, as each release's `yapi-docs.tar.gz` packs
//! them. install.sh unpacks the release's copy into `<agent dir>/docs`, and
//! yapi downloads it on its first run when installed another way.

use std::path::Path;

use sha2::Digest as _;

/// Where yapi's releases are published.
pub const RELEASES_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases");
/// The docs archive in each release.
const ARCHIVE: &str = "yapi-docs.tar.gz";
/// The file in the docs naming the yapi version they document.
const VERSION_FILE: &str = ".version";

/// Whether `YAPI_NO_DOCS` turns the download off.
pub fn opted_out() -> bool {
    crate::config::env_flag("YAPI_NO_DOCS")
}

/// Downloads this version's docs from the release in `releases` into
/// `<agent_dir>/docs`, unless the docs there are this version's. The archive
/// must match its published SHA-256 and name this version. It is unpacked
/// beside the old copy, which it then replaces.
pub async fn download(releases: &str, agent_dir: &Path) -> Result<(), String> {
    let version = env!("CARGO_PKG_VERSION");
    let dir = crate::config::docs_dir(agent_dir);
    if std::fs::read_to_string(dir.join(VERSION_FILE)).is_ok_and(|text| text.trim() == version) {
        return Ok(());
    }
    let url = format!("{releases}/download/v{version}/{ARCHIVE}");
    let archive = crate::tools::external::fetch(&url).await?;
    let sums = crate::tools::external::fetch(&format!("{url}.sha256")).await?;
    let expected = String::from_utf8_lossy(&sums)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if crate::time::hex(&sha2::Sha256::digest(&archive)) != expected {
        return Err(format!("{url} does not match its SHA-256 checksum"));
    }
    tokio::task::spawn_blocking(move || replace(&archive, &dir, version))
        .await
        .map_err(|err| err.to_string())?
}

/// Unpacks `archive` next to `dir` and swaps it in for `dir` when it
/// documents `version`.
fn replace(archive: &[u8], dir: &Path, version: &str) -> Result<(), String> {
    let parent = dir.parent().unwrap_or(Path::new("."));
    let unique = format!("{}.{}", std::process::id(), crate::time::random_hex(4));
    let fresh = parent.join(format!(".docs.{unique}"));
    let old = parent.join(format!(".docs-old.{unique}"));
    let result = (|| {
        tar::Archive::new(flate2::read::GzDecoder::new(archive))
            .unpack(&fresh)
            .map_err(|err| format!("could not unpack {ARCHIVE}: {err}"))?;
        let found = std::fs::read_to_string(fresh.join(VERSION_FILE)).unwrap_or_default();
        if found.trim() != version {
            return Err(format!(
                "{ARCHIVE} documents {}, not {version}",
                found.trim()
            ));
        }
        if dir.exists() {
            std::fs::rename(dir, &old).map_err(|err| err.to_string())?;
        }
        std::fs::rename(&fresh, dir).map_err(|err| {
            // The old copy goes back rather than leaving no docs.
            let _ = std::fs::rename(&old, dir);
            err.to_string()
        })
    })();
    let _ = std::fs::remove_dir_all(&fresh);
    let _ = std::fs::remove_dir_all(&old);
    result
}
