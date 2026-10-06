//! `rg` and `fd` for the `grep` and `find` tools: the agent's copy, else the
//! system's, else a download of the latest release.
//!
//! Port of `packages/coding-agent/src/utils/tools-manager.ts` in pi `v1.0.0`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// A helper binary the tools run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalTool {
    /// `fd`, for `find`.
    Fd,
    /// `rg`, for `grep`.
    Rg,
}

impl ExternalTool {
    fn binary(self) -> &'static str {
        match self {
            ExternalTool::Fd => "fd",
            ExternalTool::Rg => "rg",
        }
    }

    fn system_names(self) -> &'static [&'static str] {
        match self {
            ExternalTool::Fd => &["fd", "fdfind"],
            ExternalTool::Rg => &["rg"],
        }
    }

    fn repo(self) -> &'static str {
        match self {
            ExternalTool::Fd => "sharkdp/fd",
            ExternalTool::Rg => "BurntSushi/ripgrep",
        }
    }

    fn asset(self, version: &str) -> Option<String> {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "aarch64",
            "x86_64" => "x86_64",
            _ => return None,
        };
        let target = match std::env::consts::OS {
            "macos" => format!("{arch}-apple-darwin.tar.gz"),
            "linux" => format!("{arch}-unknown-linux-musl.tar.gz"),
            "windows" => format!("{arch}-pc-windows-msvc.zip"),
            _ => return None,
        };
        Some(match self {
            ExternalTool::Fd => format!("fd-v{version}-{target}"),
            ExternalTool::Rg => format!("ripgrep-{version}-{target}"),
        })
    }

    fn tag(self, version: &str) -> String {
        match self {
            ExternalTool::Fd => format!("v{version}"),
            ExternalTool::Rg => version.to_owned(),
        }
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// Whether `PI_OFFLINE` asks to skip network operations.
pub fn offline() -> bool {
    crate::config::env_flag("PI_OFFLINE")
}

async fn command_exists(name: &str) -> bool {
    tokio::process::Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok()
}

/// The installed tool: the agent's copy, else a system command name.
pub async fn tool_path(tool: ExternalTool, bin_dir: &Path) -> Option<PathBuf> {
    let local = bin_dir.join(exe(tool.binary()));
    if local.exists() {
        return Some(local);
    }
    for name in tool.system_names() {
        if command_exists(name).await {
            return Some(PathBuf::from(name));
        }
    }
    None
}

/// The tool, downloading the latest release into `bin_dir` when it is missing and
/// network use is allowed. `None` when it cannot be had.
pub async fn ensure_tool(tool: ExternalTool, bin_dir: &Path) -> Option<PathBuf> {
    if let Some(path) = tool_path(tool, bin_dir).await {
        return Some(path);
    }
    if offline() {
        return None;
    }
    // pi reports download failures only to an interactive status line.
    download(tool, bin_dir).await.ok()
}

/// The latest release version, from the redirect of the release page; the API
/// endpoint's anonymous quota is often exhausted behind shared egress.
async fn latest_version(repo: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .user_agent("yapi-coding-agent")
        .build()
        .map_err(|err| err.to_string())?;
    let response = client
        .get(format!("https://github.com/{repo}/releases/latest"))
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .filter(|_| status.is_redirection())
        .ok_or_else(|| {
            format!(
                "Failed to resolve latest {repo} release: HTTP {} without redirect",
                status.as_u16()
            )
        })?;
    if !location.contains("/releases/tag/") {
        return Err(format!(
            "Failed to resolve latest {repo} release: unexpected redirect to {location}"
        ));
    }
    let tag = location
        .split(['?', '#'])
        .next()
        .and_then(|path| path.rsplit('/').next())
        .unwrap_or_default();
    Ok(tag.strip_prefix('v').unwrap_or(tag).to_owned())
}

fn find_binary(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            let kind = entry.file_type().ok()?;
            if kind.is_file() && entry.file_name() == name {
                return Some(path);
            }
            if kind.is_dir() {
                stack.push(path);
            }
        }
    }
    None
}

async fn run(command: &str, args: &[&std::ffi::OsStr]) -> Result<(), String> {
    let output = tokio::process::Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|err| format!("{command}: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(format!(
        "{command}: {}",
        if stderr.is_empty() {
            format!("exit status {}", output.status.code().unwrap_or(-1))
        } else {
            stderr
        }
    ))
}

/// The body of `url`, a release download.
pub(crate) async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let response = yapi_ai::http::client()
        .get(url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "Download failed with HTTP {}: {url}",
            response.status().as_u16()
        ));
    }
    let bytes = response.bytes().await.map_err(|err| err.to_string())?;
    Ok(bytes.to_vec())
}

async fn download(tool: ExternalTool, bin_dir: &Path) -> Result<PathBuf, String> {
    let version = latest_version(tool.repo()).await?;
    let asset = tool.asset(&version).ok_or_else(|| {
        format!(
            "Unsupported platform: {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    std::fs::create_dir_all(bin_dir).map_err(|err| err.to_string())?;
    let url = format!(
        "https://github.com/{}/releases/download/{}/{asset}",
        tool.repo(),
        tool.tag(&version)
    );
    let archive = bin_dir.join(&asset);
    let binary = bin_dir.join(exe(tool.binary()));
    let bytes = fetch(&url).await?;
    std::fs::write(&archive, &bytes).map_err(|err| err.to_string())?;
    let extract = bin_dir.join(format!(
        "extract_tmp_{}_{}_{}",
        tool.binary(),
        std::process::id(),
        crate::time::random_hex(4)
    ));
    let result = async {
        std::fs::create_dir_all(&extract).map_err(|err| err.to_string())?;
        if asset.ends_with(".tar.gz") {
            run(
                "tar",
                &[
                    "xzf".as_ref(),
                    archive.as_os_str(),
                    "-C".as_ref(),
                    extract.as_os_str(),
                ],
            )
            .await
            .map_err(|err| format!("Failed to extract {asset}: {err}"))?;
        } else if let Err(unzip) = run(
            "unzip",
            &[
                "-q".as_ref(),
                archive.as_os_str(),
                "-d".as_ref(),
                extract.as_os_str(),
            ],
        )
        .await
        {
            run(
                "tar",
                &[
                    "xf".as_ref(),
                    archive.as_os_str(),
                    "-C".as_ref(),
                    extract.as_os_str(),
                ],
            )
            .await
            .map_err(|tar| format!("Failed to extract {asset}: {unzip}; {tar}"))?;
        }
        let name = exe(tool.binary());
        let found = find_binary(&extract, &name).ok_or_else(|| {
            format!(
                "Binary not found in archive: expected {name} under {}",
                extract.display()
            )
        })?;
        std::fs::rename(found, &binary).map_err(|err| err.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
                .map_err(|err| err.to_string())?;
        }
        Ok(binary.clone())
    }
    .await;
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&extract);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_release_assets() {
        if std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64" {
            assert_eq!(
                ExternalTool::Fd.asset("10.2.0").as_deref(),
                Some("fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz")
            );
            assert_eq!(
                ExternalTool::Rg.asset("14.1.1").as_deref(),
                Some("ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz")
            );
        }
        assert_eq!(ExternalTool::Fd.tag("10.2.0"), "v10.2.0");
        assert_eq!(ExternalTool::Rg.tag("14.1.1"), "14.1.1");
    }
}
