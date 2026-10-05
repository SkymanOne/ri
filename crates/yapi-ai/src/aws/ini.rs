//! The shared AWS `config` and `credentials` files, read as
//! `@smithy/shared-ini-file-loader` reads them.

use std::path::PathBuf;

use indexmap::IndexMap;

use super::AwsEnv;

/// Settings by profile name, and `[sso-session]` sections by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profiles {
    /// Profiles, with the credentials file's values over the config file's.
    pub profiles: IndexMap<String, IndexMap<String, String>>,
    /// `[sso-session name]` sections of the config file.
    pub sso_sessions: IndexMap<String, IndexMap<String, String>>,
}

/// Sections of an INI file, by their header text.
fn parse(text: &str) -> IndexMap<String, IndexMap<String, String>> {
    let mut sections: IndexMap<String, IndexMap<String, String>> = IndexMap::new();
    let mut current: Option<String> = None;
    let mut parent: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some(header) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            let name = header.trim().to_owned();
            sections.entry(name.clone()).or_default();
            current = Some(name);
            parent = None;
            continue;
        }
        let Some(section) = &current else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let indented = line.starts_with([' ', '\t']);
        let entries = sections.entry(section.clone()).or_default();
        if indented && let Some(parent) = &parent {
            entries.insert(format!("{parent}.{key}"), value.to_owned());
        } else if value.is_empty() {
            parent = Some(key.to_owned());
        } else {
            parent = None;
            entries.insert(key.to_owned(), value.to_owned());
        }
    }
    sections
}

/// A path setting, with a leading `~` as the home directory.
fn expand(path: &str, env: &AwsEnv) -> PathBuf {
    match path
        .strip_prefix("~/")
        .or_else(|| (path == "~").then_some(""))
    {
        Some(rest) => home(env).join(rest),
        None => PathBuf::from(path),
    }
}

/// The home directory: `HOME`, else `USERPROFILE`.
pub fn home(env: &AwsEnv) -> PathBuf {
    env.get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .map_or_else(PathBuf::new, PathBuf::from)
}

/// The config file: `AWS_CONFIG_FILE`, else `~/.aws/config`.
pub fn config_path(env: &AwsEnv) -> PathBuf {
    expand(env.get("AWS_CONFIG_FILE").unwrap_or("~/.aws/config"), env)
}

/// The credentials file: `AWS_SHARED_CREDENTIALS_FILE`, else
/// `~/.aws/credentials`.
pub fn credentials_path(env: &AwsEnv) -> PathBuf {
    expand(
        env.get("AWS_SHARED_CREDENTIALS_FILE")
            .unwrap_or("~/.aws/credentials"),
        env,
    )
}

/// Reads both files; a missing file counts as empty.
pub async fn load(env: &AwsEnv) -> Profiles {
    let read = |path: PathBuf| async move {
        tokio::fs::read_to_string(path)
            .await
            .map(|text| parse(&text))
            .unwrap_or_default()
    };
    let config = read(config_path(env)).await;
    let credentials = read(credentials_path(env)).await;
    merge(config, credentials)
}

fn merge(
    config: IndexMap<String, IndexMap<String, String>>,
    credentials: IndexMap<String, IndexMap<String, String>>,
) -> Profiles {
    let mut profiles = Profiles::default();
    for (header, values) in config {
        let header = header.split_whitespace().collect::<Vec<_>>();
        match header.as_slice() {
            ["default"] => {
                profiles
                    .profiles
                    .entry("default".into())
                    .or_default()
                    .extend(values);
            }
            ["profile", name] => {
                profiles
                    .profiles
                    .entry((*name).to_owned())
                    .or_default()
                    .extend(values);
            }
            ["sso-session", name] => {
                profiles.sso_sessions.insert((*name).to_owned(), values);
            }
            _ => {}
        }
    }
    for (name, values) in credentials {
        profiles
            .profiles
            .entry(name.trim().to_owned())
            .or_default()
            .extend(values);
    }
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_config_and_credentials_like_the_sdk() {
        let config = parse(
            "[default]\nregion = us-west-2\n\n[profile dev]\nrole_arn = arn:aws:iam::1:role/r\nsource_profile = default\ns3 =\n  max_concurrent_requests = 10\n[sso-session corp]\nsso_region = eu-west-1\n[ignored]\nx = 1\n",
        );
        let credentials = parse(
            "# keys\n[default]\naws_access_key_id = AKID\naws_secret_access_key = secret\nregion = eu-central-1\n",
        );
        let profiles = merge(config, credentials);
        let default = &profiles.profiles["default"];
        assert_eq!(default["region"], "eu-central-1");
        assert_eq!(default["aws_access_key_id"], "AKID");
        assert_eq!(profiles.profiles["dev"]["source_profile"], "default");
        assert_eq!(profiles.profiles["dev"]["s3.max_concurrent_requests"], "10");
        assert_eq!(profiles.sso_sessions["corp"]["sso_region"], "eu-west-1");
        assert!(!profiles.profiles.contains_key("ignored"));
    }
}
