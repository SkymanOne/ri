//! `auth.json`: stored credentials keyed by provider id.
//!
//! Port of `core/auth-storage.ts` in pi `v1.0.0`. Reads use a cached
//! document that reloads when the file's revision changes; changes re-read
//! the file under the lock and write it back as pi does, so other entries keep
//! their bytes.

use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use yapi_types::auth::Credential;
use yapi_types::config::ConfigFile;

use super::AuthError;
use super::lock::FileLock;

type Document = Map<String, Value>;
type Revision = (Option<SystemTime>, u64);

#[derive(Default)]
struct State {
    document: Document,
    revision: Option<Revision>,
    /// Why the file at this revision could not be read, if it could not.
    error: Option<String>,
}

/// The credential store: `auth.json` in the agent directory, or memory.
#[derive(Default)]
pub struct CredentialStore {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Which kind of credential is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    /// `"api_key"`.
    ApiKey,
    /// `"oauth"`.
    OAuth,
}

fn revision(path: &Path) -> Option<Revision> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok(), metadata.len()))
}

fn parse(text: &str) -> Result<Document, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.trim().is_empty() {
        return Ok(Document::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(document)) => Ok(document),
        Ok(_) => Err("Invalid auth.json: expected an object".into()),
        Err(err) => Err(err.to_string()),
    }
}

fn failed(err: impl std::fmt::Display) -> AuthError {
    AuthError::Failed(err.to_string())
}

impl CredentialStore {
    /// The store backed by `path`, loaded now. An unreadable file reads as empty.
    pub fn open(path: impl Into<PathBuf>) -> CredentialStore {
        let store = CredentialStore {
            path: Some(path.into()),
            state: Mutex::default(),
        };
        drop(store.state());
        store
    }

    /// A store that lives in memory only.
    pub fn in_memory(document: Map<String, Value>) -> CredentialStore {
        CredentialStore {
            path: None,
            state: Mutex::new(State {
                document,
                revision: None,
                error: None,
            }),
        }
    }

    /// The file behind the store, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The cached state, reloaded first when the file changed. A file that no
    /// longer parses keeps the last good document, as pi does.
    fn state(&self) -> MutexGuard<'_, State> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(path) = &self.path {
            let current = revision(path);
            if current.is_none() || current != state.revision {
                state.error = None;
                match std::fs::read_to_string(path) {
                    Ok(text) => match parse(&text) {
                        Ok(document) => state.document = document,
                        Err(error) => state.error = Some(error),
                    },
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        state.document = Document::new();
                    }
                    Err(error) => state.error = Some(error.to_string()),
                }
                state.revision = current;
            }
        }
        state
    }

    /// Why the file cannot be read now, as pi's store reports it; reads
    /// meanwhile see the last document that could be.
    pub fn read_error(&self) -> Option<String> {
        self.state().error.clone()
    }

    /// The stored credential for `provider`, if it is a valid one.
    pub fn get(&self, provider: &str) -> Option<Credential> {
        let value = self.state().document.get(provider).cloned()?;
        serde_json::from_value(value).ok()
    }

    /// Stored providers and their credential kinds, in file order.
    pub fn list(&self) -> Vec<(String, CredentialKind)> {
        self.state()
            .document
            .iter()
            .filter_map(|(provider, value)| {
                let kind = match value.get("type").and_then(Value::as_str)? {
                    "api_key" => CredentialKind::ApiKey,
                    "oauth" => CredentialKind::OAuth,
                    _ => return None,
                };
                Some((provider.clone(), kind))
            })
            .collect()
    }

    /// Replaces `provider`'s credential with what `change` returns for the
    /// current one, under the lock; `None` leaves the file untouched. Returns the
    /// credential now stored.
    pub async fn modify<F, Fut>(
        &self,
        provider: &str,
        change: F,
        cancel: &CancellationToken,
    ) -> Result<Option<Credential>, AuthError>
    where
        F: FnOnce(Option<Credential>) -> Fut,
        Fut: Future<Output = Result<Option<Credential>, AuthError>>,
    {
        let Some(path) = self.path.clone() else {
            let current = self.get(provider);
            let next = change(current.clone()).await?;
            if let Some(next) = &next {
                let value = serde_json::to_value(next).map_err(failed)?;
                self.state().document.insert(provider.to_owned(), value);
            }
            return Ok(next.or(current));
        };
        prepare(&path)?;
        let _lock = FileLock::acquire(&path, cancel).await.map_err(|err| {
            if cancel.is_cancelled() {
                AuthError::Cancelled
            } else {
                failed(err)
            }
        })?;
        let mut document = read(&path)?;
        let current = document
            .get(provider)
            .cloned()
            .and_then(|value| serde_json::from_value::<Credential>(value).ok());
        let Some(next) = change(current.clone()).await? else {
            self.remember(&path, document);
            return Ok(current);
        };
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let value = serde_json::to_value(&next).map_err(failed)?;
        document.insert(provider.to_owned(), value);
        write(&path, &document)?;
        self.remember(&path, document);
        Ok(Some(next))
    }

    /// Stores `credential` for `provider`.
    pub async fn set(
        &self,
        provider: &str,
        credential: Credential,
        cancel: &CancellationToken,
    ) -> Result<(), AuthError> {
        self.modify(provider, |_| async { Ok(Some(credential)) }, cancel)
            .await
            .map(drop)
    }

    /// Removes `provider`'s credential.
    pub async fn delete(
        &self,
        provider: &str,
        cancel: &CancellationToken,
    ) -> Result<(), AuthError> {
        let Some(path) = self.path.clone() else {
            self.state().document.shift_remove(provider);
            return Ok(());
        };
        prepare(&path)?;
        let _lock = FileLock::acquire(&path, cancel).await.map_err(failed)?;
        let mut document = read(&path)?;
        document.shift_remove(provider);
        write(&path, &document)?;
        self.remember(&path, document);
        Ok(())
    }

    fn remember(&self, path: &Path, document: Document) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.document = document;
        state.revision = revision(path);
    }
}

/// Creates the directory (mode 0700) and an empty file (mode 0600) when missing.
fn prepare(path: &Path) -> Result<(), AuthError> {
    if let Some(dir) = path.parent()
        && !dir.exists()
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir).map_err(failed)?;
    }
    if !path.exists() {
        write_text(path, "{}")?;
    }
    Ok(())
}

fn read(path: &Path) -> Result<Document, AuthError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(AuthError::Failed),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Document::new()),
        Err(err) => Err(failed(err)),
    }
}

fn write(path: &Path, document: &Document) -> Result<(), AuthError> {
    let text = ConfigFile::Auth.render(document).map_err(failed)?;
    write_text(path, &text)
}

/// Writes with mode 0600 when creating; an existing file keeps its mode.
fn write_text(path: &Path, text: &str) -> Result<(), AuthError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)
}

#[cfg(test)]
mod tests {
    use yapi_types::auth::ApiKeyCredential;

    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yapi-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("agent").join("auth.json")
    }

    fn key(value: &str) -> Credential {
        Credential::ApiKey(ApiKeyCredential {
            key: Some(value.into()),
            env: None,
        })
    }

    #[test]
    fn reports_a_file_that_does_not_parse() {
        let path = temp("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{bad").unwrap();
        let store = CredentialStore::open(&path);
        assert!(store.read_error().is_some());
        assert_eq!(store.get("anthropic"), None);
        std::fs::write(
            &path,
            "{\"anthropic\": {\"type\": \"api_key\", \"key\": \"k\"}}",
        )
        .unwrap();
        // A rewrite of the same length may share the old modification time.
        let store = CredentialStore::open(&path);
        assert_eq!(store.read_error(), None);
        assert_eq!(store.get("anthropic"), Some(key("k")));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn keeps_other_entries_byte_identical() {
        let path = temp("bytes");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "{\n  \"zeta\": {\n    \"type\": \"oauth\",\n    \"refresh\": \"r\",\n    \"access\": \"a\",\n    \"expires\": 1700000000000,\n    \"accountId\": \"x\"\n  },\n  \"alpha\": {\n    \"type\": \"api_key\",\n    \"key\": \"$KEY\"\n  }\n}";
        std::fs::write(&path, original).unwrap();
        let store = CredentialStore::open(&path);
        assert_eq!(
            store.list(),
            [
                ("zeta".to_owned(), CredentialKind::OAuth),
                ("alpha".to_owned(), CredentialKind::ApiKey)
            ]
        );
        let cancel = CancellationToken::new();
        store.set("beta", key("k"), &cancel).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            original.replace(
                "\n}",
                ",\n  \"beta\": {\n    \"type\": \"api_key\",\n    \"key\": \"k\"\n  }\n}"
            )
        );
        store.delete("beta", &cancel).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!path.with_file_name("auth.json.lock").exists());
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn sees_external_changes_and_creates_files() {
        let path = temp("reload");
        let store = CredentialStore::open(&path);
        assert!(store.get("openai").is_none());
        let cancel = CancellationToken::new();
        store.set("openai", key("one"), &cancel).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(
            &path,
            "{\"openai\": {\"type\": \"api_key\", \"key\": \"two-longer\"}}",
        )
        .unwrap();
        assert_eq!(store.get("openai"), Some(key("two-longer")));
        let unchanged = store
            .modify("openai", |_| async { Ok(None) }, &cancel)
            .await
            .unwrap();
        assert_eq!(unchanged, Some(key("two-longer")));
        std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}
