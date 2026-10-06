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

/// The published yapi book.
const BOOK_URL: &str = "https://nikolish.in/yapi/";
/// pi's docs and examples at the release yapi follows.
const PI_URL: &str = "https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent";

/// Where the model reads the docs: the local copy in the agent directory
/// when there is one, else the published pages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Locations {
    /// yapi's main page, in place of pi's `getReadmePath`.
    pub main: String,
    /// pi's docs, pi's `getDocsPath`.
    pub pi_docs: String,
    /// pi's examples, pi's `getExamplesPath`.
    pub pi_examples: String,
}

impl Locations {
    /// The local docs in `agent_dir` when they are installed, else the
    /// published ones.
    pub fn find(agent_dir: &Path) -> Locations {
        let dir = crate::config::docs_dir(agent_dir);
        if dir.join(VERSION_FILE).is_file() {
            Locations {
                main: dir.join("index.md").display().to_string(),
                pi_docs: dir.join("pi").join("docs").display().to_string(),
                pi_examples: dir.join("pi").join("examples").display().to_string(),
            }
        } else {
            Locations {
                main: BOOK_URL.to_owned(),
                pi_docs: format!("{PI_URL}/docs"),
                pi_examples: format!("{PI_URL}/examples"),
            }
        }
    }
}

/// Downloads this version's docs from the release in `releases` into
/// `<agent_dir>/docs`, unless the docs there are this version's. The archive
/// must match its published SHA-256. It is unpacked beside the old copy,
/// which it then replaces.
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
    tokio::task::spawn_blocking(move || replace(&archive, &dir))
        .await
        .map_err(|err| err.to_string())?
}

/// Unpacks `archive` next to `dir`, then puts it in place of `dir`. Should
/// the last step fail, the model reads the published docs.
fn replace(archive: &[u8], dir: &Path) -> Result<(), String> {
    let fresh = dir.with_file_name(format!(".docs.{}", std::process::id()));
    let result = tar::Archive::new(flate2::read::GzDecoder::new(archive))
        .unpack(&fresh)
        .map_err(|err| format!("could not unpack {ARCHIVE}: {err}"))
        .and_then(|()| {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::rename(&fresh, dir).map_err(|err| err.to_string())
        });
    let _ = std::fs::remove_dir_all(&fresh);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_local_docs_else_the_published_ones() {
        let agent = std::env::temp_dir().join(format!("yapi-docs-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&agent);
        let published = Locations::find(&agent);
        assert_eq!(published.main, "https://nikolish.in/yapi/");
        assert_eq!(
            published.pi_docs,
            "https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/docs"
        );
        assert_eq!(
            published.pi_examples,
            "https://github.com/earendil-works/pi/blob/v1.0.0/packages/coding-agent/examples"
        );
        std::fs::create_dir_all(agent.join("docs")).unwrap();
        std::fs::write(agent.join("docs").join(VERSION_FILE), "0.1.0\n").unwrap();
        let local = Locations::find(&agent);
        let docs = agent.join("docs");
        assert_eq!(local.main, docs.join("index.md").display().to_string());
        assert_eq!(
            local.pi_docs,
            docs.join("pi").join("docs").display().to_string()
        );
        assert_eq!(
            local.pi_examples,
            docs.join("pi").join("examples").display().to_string()
        );
        std::fs::remove_dir_all(&agent).unwrap();
    }
}
