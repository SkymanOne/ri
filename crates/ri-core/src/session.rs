//! Session files: a JSONL tree of entries with a movable leaf.
//!
//! Port of `packages/coding-agent/src/core/session-manager.ts` in pi `v1.0.0`.
//! Entries are kept as order-preserving documents so a migration rewrites files the
//! way pi does; typed views are parsed alongside. A new session reaches disk only
//! once it has a user or assistant message.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use ri_types::message::{
    BranchSummaryMessage, CompactionSummaryMessage, Content, ContentBlock, CustomMessage, Message,
    TextContent, Usage,
};
use ri_types::session::{
    BranchSummaryEntry, CURRENT_VERSION, CompactionEntry, ContextEditEntry, CustomEntry,
    CustomMessageEntry, EntryMeta, FileEntry, LabelEntry, MessageEntry, ModelChangeEntry,
    Replacement, SessionHeader, SessionInfoEntry, ThinkingLevelChangeEntry,
};
use serde_json::Value;

use crate::time::{now_iso, parse_iso, uuid_v4, uuid_v7};

/// Session file errors.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Reading or writing the file failed.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file is not a session.
    #[error("Session file is not a valid ri session: {0}")]
    Invalid(PathBuf),
    /// An entry id that does not exist.
    #[error("Entry {0} not found")]
    NotFound(String),
    /// An operation the target entry does not allow.
    #[error("{0}")]
    Rejected(String),
    /// A JSON conversion failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, SessionError>;

struct Entry {
    doc: Value,
    view: Option<FileEntry>,
}

impl Entry {
    fn new(doc: Value) -> Entry {
        let view = serde_json::from_value(doc.clone()).ok();
        Entry { doc, view }
    }

    fn kind(&self) -> &str {
        self.doc["type"].as_str().unwrap_or_default()
    }

    fn id(&self) -> Option<&str> {
        self.doc["id"].as_str()
    }

    fn parent_id(&self) -> Option<&str> {
        self.doc["parentId"].as_str()
    }
}

/// The model context a branch produces.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionContext {
    /// Messages, with compaction and branch summaries and context edits applied.
    pub messages: Vec<Message>,
    /// The last thinking level set on the branch; `off` when none.
    pub thinking_level: String,
    /// The last model used or selected on the branch, as (provider, model id).
    pub model: Option<(String, String)>,
}

/// Summary of a session file, for listings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    /// The file.
    pub path: PathBuf,
    /// The session id.
    pub id: String,
    /// The working directory recorded in the header.
    pub cwd: String,
    /// The latest name, if any.
    pub name: Option<String>,
    /// Text of the first user message.
    pub first_message: String,
    /// Number of messages.
    pub message_count: usize,
    /// Modification time, Unix milliseconds.
    pub modified_ms: u64,
}

/// A session: its entries, where it is stored, and the current leaf.
pub struct SessionManager {
    session_id: String,
    file: Option<PathBuf>,
    dir: PathBuf,
    cwd: PathBuf,
    persist: bool,
    flushed: bool,
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    labels: HashMap<String, (String, String)>,
    leaf: Option<String>,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> SessionError + '_ {
    move |source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn to_doc(entry: &FileEntry) -> Value {
    serde_json::to_value(entry).unwrap_or(Value::Null)
}

fn line(doc: &Value) -> String {
    ri_types::json::to_string(doc).unwrap_or_default() + "\n"
}

/// Reads the entries of a session file; empty when the file is missing or does not
/// start with a session header.
fn load_docs(path: &Path) -> Result<Vec<Value>> {
    let Ok(bytes) = fs::read(path) else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8_lossy(&bytes);
    let docs: Vec<Value> = text
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let valid = docs
        .first()
        .is_some_and(|header| header["type"] == "session" && header["id"].is_string());
    if !valid {
        return Ok(Vec::new());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(b"\n"))
            .map_err(io(path))?;
    }
    Ok(docs)
}

fn short_id(taken: &dyn Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let id = uuid_v4()[..8].to_owned();
        if !taken(&id) {
            return id;
        }
    }
    uuid_v4()
}

/// Upgrades version 1 and 2 entries in place. Returns whether anything changed.
fn migrate(docs: &mut [Value]) -> bool {
    let version = docs
        .iter()
        .find(|doc| doc["type"] == "session")
        .and_then(|header| header["version"].as_u64())
        .unwrap_or(1);
    if version >= u64::from(CURRENT_VERSION) {
        return false;
    }
    if version < 2 {
        let mut ids: Vec<String> = Vec::new();
        let mut previous: Option<String> = None;
        for doc in docs.iter_mut() {
            if doc["type"] == "session" {
                doc["version"] = Value::from(2);
                continue;
            }
            let id = short_id(&|candidate| ids.iter().any(|id| id == candidate));
            ids.push(id.clone());
            doc["id"] = Value::from(id.clone());
            doc["parentId"] = previous.map_or(Value::Null, Value::from);
            previous = Some(id);
        }
        for index in 0..docs.len() {
            if docs[index]["type"] != "compaction" {
                continue;
            }
            if let Some(kept) = docs[index]["firstKeptEntryIndex"].as_u64() {
                let target = docs
                    .get(kept as usize)
                    .filter(|target| target["type"] != "session")
                    .map(|target| target["id"].clone());
                if let Some(object) = docs[index].as_object_mut() {
                    if let Some(id) = target {
                        object.insert("firstKeptEntryId".into(), id);
                    }
                    object.shift_remove("firstKeptEntryIndex");
                }
            }
        }
    }
    for doc in docs.iter_mut() {
        if doc["type"] == "session" {
            doc["version"] = Value::from(CURRENT_VERSION);
        } else if doc["type"] == "message" && doc["message"]["role"] == "hookMessage" {
            doc["message"]["role"] = Value::from("custom");
        }
    }
    true
}

impl SessionManager {
    fn blank(cwd: &Path, dir: &Path, persist: bool) -> SessionManager {
        SessionManager {
            session_id: String::new(),
            file: None,
            dir: dir.to_path_buf(),
            cwd: cwd.to_path_buf(),
            persist,
            flushed: false,
            entries: Vec::new(),
            by_id: HashMap::new(),
            labels: HashMap::new(),
            leaf: None,
        }
    }

    /// A new session stored in `dir`, created on disk once it has a conversation.
    pub fn create(cwd: &Path, dir: &Path) -> Result<SessionManager> {
        fs::create_dir_all(dir).map_err(io(dir))?;
        let mut manager = SessionManager::blank(cwd, dir, true);
        manager.new_session(None, None);
        Ok(manager)
    }

    /// A session that is never written.
    pub fn in_memory(cwd: &Path) -> SessionManager {
        let mut manager = SessionManager::blank(cwd, Path::new(""), false);
        manager.new_session(None, None);
        manager
    }

    /// Opens a session file; a missing file starts a new session at that path. The
    /// working directory comes from the header unless given.
    pub fn open(path: &Path, cwd: Option<&Path>) -> Result<SessionManager> {
        let docs = load_docs(path)?;
        let header_cwd = docs
            .first()
            .and_then(|header| header["cwd"].as_str())
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from);
        let cwd = cwd
            .map(Path::to_path_buf)
            .or(header_cwd)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let mut manager = SessionManager::blank(&cwd, &dir, true);
        manager.set_file(path, docs)?;
        Ok(manager)
    }

    /// The most recently modified session in `dir`, or a new one.
    pub fn continue_recent(cwd: &Path, dir: &Path) -> Result<SessionManager> {
        fs::create_dir_all(dir).map_err(io(dir))?;
        match list(dir)?.into_iter().next() {
            Some(recent) => SessionManager::open(&recent.path, Some(cwd)),
            None => SessionManager::create(cwd, dir),
        }
    }

    fn set_file(&mut self, path: &Path, docs: Vec<Value>) -> Result<()> {
        if docs.is_empty() {
            if fs::metadata(path).is_ok_and(|meta| meta.len() > 0) {
                return Err(SessionError::Invalid(path.to_path_buf()));
            }
            let existed = path.exists();
            self.new_session(None, None);
            self.file = Some(path.to_path_buf());
            if existed {
                self.rewrite()?;
                self.flushed = true;
            }
            return Ok(());
        }
        self.file = Some(path.to_path_buf());
        self.load(docs)?;
        self.flushed = true;
        Ok(())
    }

    fn load(&mut self, mut docs: Vec<Value>) -> Result<()> {
        let migrated = migrate(&mut docs);
        self.session_id = docs[0]["id"].as_str().unwrap_or_default().to_owned();
        self.entries = docs.into_iter().map(Entry::new).collect();
        if migrated {
            self.rewrite()?;
        }
        self.index();
        Ok(())
    }

    fn index(&mut self) {
        self.by_id.clear();
        self.labels.clear();
        self.leaf = None;
        for (position, entry) in self.entries.iter().enumerate() {
            if entry.kind() == "session" {
                continue;
            }
            let Some(id) = entry.id() else { continue };
            self.by_id.insert(id.to_owned(), position);
            self.leaf = Some(id.to_owned());
            if let Some(FileEntry::Label(label)) = &entry.view {
                match &label.label {
                    Some(text) if !text.is_empty() => {
                        self.labels.insert(
                            label.target_id.clone(),
                            (text.clone(), label.meta.timestamp.clone()),
                        );
                    }
                    _ => {
                        self.labels.remove(&label.target_id);
                    }
                }
            }
        }
    }

    /// Starts over with a fresh header; returns the new file path when persisted.
    pub fn new_session(&mut self, id: Option<String>, parent: Option<String>) -> Option<PathBuf> {
        self.session_id = id.unwrap_or_else(uuid_v7);
        let timestamp = now_iso();
        let header = FileEntry::Session(SessionHeader {
            version: Some(CURRENT_VERSION),
            id: self.session_id.clone(),
            timestamp: timestamp.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            parent_session: parent,
            provider: None,
            model_id: None,
            thinking_level: None,
            branched_from: None,
        });
        self.entries = vec![Entry::new(to_doc(&header))];
        self.by_id.clear();
        self.labels.clear();
        self.leaf = None;
        self.flushed = false;
        if self.persist {
            let file_timestamp = timestamp.replace([':', '.'], "-");
            self.file = Some(
                self.dir
                    .join(format!("{file_timestamp}_{}.jsonl", self.session_id)),
            );
        }
        self.file.clone()
    }

    fn rewrite(&self) -> Result<()> {
        let (true, Some(file)) = (self.persist, &self.file) else {
            return Ok(());
        };
        let text: String = self.entries.iter().map(|entry| line(&entry.doc)).collect();
        fs::write(file, text).map_err(io(file))
    }

    fn has_conversation(&self) -> bool {
        self.entries.iter().any(|entry| {
            entry.kind() == "message"
                && matches!(
                    entry.doc["message"]["role"].as_str(),
                    Some("user" | "assistant")
                )
        })
    }

    fn persist_entry(&mut self, doc: &Value) -> Result<()> {
        let (true, Some(file)) = (self.persist, self.file.clone()) else {
            return Ok(());
        };
        if !self.flushed {
            if !self.has_conversation() {
                return Ok(());
            }
            if let Some(dir) = file.parent() {
                fs::create_dir_all(dir).map_err(io(dir))?;
            }
            let text: String = self.entries.iter().map(|entry| line(&entry.doc)).collect();
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)
                .and_then(|mut handle| handle.write_all(text.as_bytes()))
                .map_err(io(&file))?;
            self.flushed = true;
        } else {
            OpenOptions::new()
                .append(true)
                .open(&file)
                .and_then(|mut handle| handle.write_all(line(doc).as_bytes()))
                .map_err(io(&file))?;
        }
        Ok(())
    }

    fn new_meta(&self) -> EntryMeta {
        EntryMeta {
            id: short_id(&|id| self.by_id.contains_key(id)),
            parent_id: self.leaf.clone(),
            timestamp: now_iso(),
        }
    }

    fn append(&mut self, entry: FileEntry) -> Result<String> {
        let id = entry.meta().map(|meta| meta.id.clone()).unwrap_or_default();
        let doc = to_doc(&entry);
        self.entries.push(Entry {
            doc: doc.clone(),
            view: Some(entry),
        });
        self.by_id.insert(id.clone(), self.entries.len() - 1);
        self.leaf = Some(id.clone());
        self.persist_entry(&doc)?;
        Ok(id)
    }

    /// Appends a message on the current branch; returns its entry id.
    pub fn append_message(&mut self, message: Message) -> Result<String> {
        let meta = self.new_meta();
        self.append(FileEntry::Message(MessageEntry { meta, message }))
    }

    /// Records a thinking level change.
    pub fn append_thinking_level_change(&mut self, level: &str) -> Result<String> {
        let meta = self.new_meta();
        self.append(FileEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
            meta,
            thinking_level: level.to_owned(),
        }))
    }

    /// Records a model change.
    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> Result<String> {
        let meta = self.new_meta();
        self.append(FileEntry::ModelChange(ModelChangeEntry {
            meta,
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        }))
    }

    /// Records a compaction. The current system message travels with it, so the
    /// prompt survives when the messages before `firstKeptEntryId` drop out.
    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: Option<String>,
        tokens_before: u64,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> Result<String> {
        let meta = self.new_meta();
        let system = ri_ai::transcript::current_system_message(&self.build_context().messages).map(
            |mut system| {
                system.timestamp = parse_iso(&meta.timestamp).unwrap_or_default();
                Box::new(Message::System(system))
            },
        );
        let first_kept = first_kept_entry_id.unwrap_or_else(|| meta.id.clone());
        self.append(FileEntry::Compaction(CompactionEntry {
            meta,
            summary,
            first_kept_entry_id: Some(Some(first_kept)),
            tokens_before,
            details,
            usage,
            from_hook,
            system_message: system,
        }))
    }

    /// Stores extension state outside the model context.
    pub fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: Option<Value>,
    ) -> Result<String> {
        let meta = self.new_meta();
        self.append(FileEntry::Custom(CustomEntry {
            custom_type: custom_type.to_owned(),
            data,
            meta,
        }))
    }

    /// Stores an extension message that is part of the model context.
    pub fn append_custom_message(
        &mut self,
        custom_type: &str,
        content: Content,
        display: bool,
        details: Option<Value>,
    ) -> Result<String> {
        let meta = self.new_meta();
        self.append(FileEntry::CustomMessage(CustomMessageEntry {
            custom_type: custom_type.to_owned(),
            content,
            display,
            details,
            meta,
        }))
    }

    /// Names the session; line breaks become spaces.
    pub fn append_session_info(&mut self, name: &str) -> Result<String> {
        let meta = self.new_meta();
        let name = name
            .split(['\r', '\n'])
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_owned();
        self.append(FileEntry::SessionInfo(SessionInfoEntry {
            meta,
            name: Some(name),
        }))
    }

    /// Edits an earlier model-visible entry on the active branch: `None` drops it
    /// from the context, a replacement swaps its content. Text replacing an
    /// assistant or tool result becomes a text block.
    pub fn append_context_edit(
        &mut self,
        target_id: &str,
        replacement: Option<Content>,
    ) -> Result<String> {
        let target = self
            .entry(target_id)
            .ok_or_else(|| SessionError::NotFound(target_id.to_owned()))?;
        if !self
            .branch_path(None)
            .iter()
            .any(|entry| entry.meta().is_some_and(|meta| meta.id == target_id))
        {
            return Err(SessionError::Rejected(format!(
                "Entry {target_id} is not on the active branch"
            )));
        }
        let role = match target {
            FileEntry::CustomMessage(_) => "custom",
            FileEntry::Message(entry) => match &entry.message {
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::ToolResult(_) => "toolResult",
                _ => "",
            },
            _ => "",
        };
        if role.is_empty() {
            return Err(SessionError::Rejected(format!(
                "Entry {target_id} does not contribute editable model content"
            )));
        }
        let replacement = replacement.map(|content| match content {
            Content::Text(text) if matches!(role, "assistant" | "toolResult") => {
                Content::Blocks(vec![ContentBlock::Text(TextContent {
                    text,
                    text_signature: None,
                })])
            }
            content => content,
        });
        let meta = self.new_meta();
        self.append(FileEntry::ContextEdit(ContextEditEntry {
            meta,
            target_id: target_id.to_owned(),
            replacement: replacement.map(|content| Replacement { content }),
        }))
    }

    /// Sets or clears the label of an entry.
    pub fn append_label(&mut self, target_id: &str, label: Option<String>) -> Result<String> {
        if !self.by_id.contains_key(target_id) {
            return Err(SessionError::NotFound(target_id.to_owned()));
        }
        let meta = self.new_meta();
        let timestamp = meta.timestamp.clone();
        let id = self.append(FileEntry::Label(LabelEntry {
            meta,
            target_id: target_id.to_owned(),
            label: label.clone(),
        }))?;
        match label.filter(|label| !label.is_empty()) {
            Some(label) => {
                self.labels.insert(target_id.to_owned(), (label, timestamp));
            }
            None => {
                self.labels.remove(target_id);
            }
        }
        Ok(id)
    }

    /// The latest session name.
    pub fn name(&self) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.view {
                Some(FileEntry::SessionInfo(info)) => Some(
                    info.name
                        .as_deref()
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                ),
                _ => None,
            })?
    }

    /// The session id.
    pub fn id(&self) -> &str {
        &self.session_id
    }

    /// The session file, when persisted.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref().filter(|_| self.persist)
    }

    /// Whether the session is written to disk.
    pub fn is_persisted(&self) -> bool {
        self.persist
    }

    /// The directory new session files go to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The session's working directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The header.
    pub fn header(&self) -> Option<&SessionHeader> {
        match self.entries.first().and_then(|entry| entry.view.as_ref()) {
            Some(FileEntry::Session(header)) => Some(header),
            _ => None,
        }
    }

    /// The current leaf entry id.
    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    /// An entry by id.
    pub fn entry(&self, id: &str) -> Option<&FileEntry> {
        self.by_id
            .get(id)
            .and_then(|position| self.entries[*position].view.as_ref())
    }

    /// The label of an entry.
    pub fn label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(|(label, _)| label.as_str())
    }

    /// Every entry except the header, in file order.
    pub fn entries(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind() != "session")
            .filter_map(|entry| entry.view.as_ref())
    }

    /// The path from the root to `from` (default: the leaf).
    pub fn branch_path(&self, from: Option<&str>) -> Vec<&FileEntry> {
        let mut path = Vec::new();
        let mut current = from
            .or(self.leaf.as_deref())
            .and_then(|id| self.by_id.get(id));
        while let Some(position) = current {
            let entry = &self.entries[*position];
            if let Some(view) = &entry.view {
                path.push(view);
            }
            current = entry.parent_id().and_then(|parent| self.by_id.get(parent));
        }
        path.reverse();
        path
    }

    /// Moves the leaf to an existing entry; the next append starts a branch there.
    pub fn branch(&mut self, id: &str) -> Result<()> {
        if !self.by_id.contains_key(id) {
            return Err(SessionError::NotFound(id.to_owned()));
        }
        self.leaf = Some(id.to_owned());
        Ok(())
    }

    /// Moves the leaf before the first entry.
    pub fn reset_leaf(&mut self) {
        self.leaf = None;
    }

    /// Branches to `from` (or the root) and records a summary of the branch left.
    pub fn branch_with_summary(
        &mut self,
        from: Option<&str>,
        summary: String,
        details: Option<Value>,
        from_hook: Option<bool>,
        usage: Option<Usage>,
    ) -> Result<String> {
        if let Some(id) = from
            && !self.by_id.contains_key(id)
        {
            return Err(SessionError::NotFound(id.to_owned()));
        }
        let from_id = self.leaf.clone().unwrap_or_else(|| "root".into());
        self.leaf = from.map(str::to_owned);
        let meta = self.new_meta();
        self.append(FileEntry::BranchSummary(BranchSummaryEntry {
            meta,
            from_id,
            summary,
            details,
            usage,
            from_hook,
        }))
    }

    /// The model context of the current branch.
    pub fn build_context(&self) -> SessionContext {
        build_context(&self.branch_path(None))
    }

    /// The current branch's context, entry by entry.
    pub fn build_projection(&self) -> Projection<'_> {
        build_projection(&self.branch_path(None))
    }

    /// Starts a new session file holding the branch up to `leaf_id`, with labels on
    /// it, and switches to it. Returns the new file when persisted.
    pub fn create_branched_session(&mut self, leaf_id: &str) -> Result<Option<PathBuf>> {
        let previous_file = self.file.clone();
        let path: Vec<FileEntry> = self
            .branch_path(Some(leaf_id))
            .into_iter()
            .cloned()
            .collect();
        if path.is_empty() {
            return Err(SessionError::NotFound(leaf_id.to_owned()));
        }
        let mut kept: Vec<Value> = Vec::new();
        let mut label_targets: HashMap<String, String> = HashMap::new();
        let mut pending_labels: Vec<String> = Vec::new();
        let mut parent: Option<String> = None;
        for entry in &path {
            let Some(meta) = entry.meta() else { continue };
            if matches!(entry, FileEntry::Label(_)) {
                pending_labels.push(meta.id.clone());
                continue;
            }
            for label in pending_labels.drain(..) {
                label_targets.insert(label, meta.id.clone());
            }
            let position = self.by_id[&meta.id];
            let mut doc = self.entries[position].doc.clone();
            doc["parentId"] = parent.clone().map_or(Value::Null, Value::from);
            if let FileEntry::Compaction(compaction) = entry
                && let Some(Some(first_kept)) = &compaction.first_kept_entry_id
                && first_kept != &meta.id
                && let Some(target) = label_targets.get(first_kept)
            {
                doc["firstKeptEntryId"] = Value::from(target.clone());
            }
            parent = Some(meta.id.clone());
            kept.push(doc);
        }

        let kept_ids: Vec<String> = kept
            .iter()
            .filter_map(|doc| doc["id"].as_str().map(str::to_owned))
            .collect();
        let mut labels: Vec<(String, String, String)> = self
            .labels
            .iter()
            .filter(|(target, _)| kept_ids.contains(target))
            .map(|(target, (label, timestamp))| (target.clone(), label.clone(), timestamp.clone()))
            .collect();
        labels.sort_by(|a, b| a.2.cmp(&b.2));

        let previous_dir = self.dir.clone();
        let persist = self.persist;
        self.new_session(
            None,
            if persist {
                previous_file.map(|f| f.to_string_lossy().into_owned())
            } else {
                None
            },
        );
        self.dir = previous_dir;
        let mut all_ids = kept_ids.clone();
        let mut docs: Vec<Value> = vec![self.entries[0].doc.clone()];
        docs.extend(kept);
        let mut label_parent = kept_ids.last().cloned();
        for (target, label, timestamp) in labels {
            let id = short_id(&|candidate| all_ids.iter().any(|id| id == candidate));
            all_ids.push(id.clone());
            let entry = FileEntry::Label(LabelEntry {
                meta: EntryMeta {
                    id: id.clone(),
                    parent_id: label_parent.clone(),
                    timestamp,
                },
                target_id: target,
                label: Some(label),
            });
            docs.push(to_doc(&entry));
            label_parent = Some(id);
        }
        self.entries = docs.into_iter().map(Entry::new).collect();
        self.index();
        if persist && self.has_conversation() {
            self.rewrite()?;
            self.flushed = true;
        }
        Ok(self.file.clone().filter(|_| persist))
    }

    /// Copies every entry of `source` into a new session file for `cwd` in `dir`,
    /// with `parentSession` pointing at the source.
    pub fn fork_from(source: &Path, cwd: &Path, dir: &Path) -> Result<SessionManager> {
        let docs = load_docs(source)?;
        if docs.is_empty() {
            return Err(SessionError::Invalid(source.to_path_buf()));
        }
        fs::create_dir_all(dir).map_err(io(dir))?;
        let mut manager = SessionManager::blank(cwd, dir, true);
        let file = manager
            .new_session(None, Some(source.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let mut text = line(&manager.entries[0].doc);
        for doc in docs.iter().filter(|doc| doc["type"] != "session") {
            text += &line(doc);
        }
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
            .and_then(|mut handle| handle.write_all(text.as_bytes()))
            .map_err(io(&file))?;
        SessionManager::open(&file, Some(cwd))
    }
}

fn entry_messages(entry: &FileEntry) -> Vec<Message> {
    match entry {
        FileEntry::Message(entry) => vec![entry.message.clone()],
        FileEntry::CustomMessage(entry) => vec![Message::Custom(CustomMessage {
            custom_type: entry.custom_type.clone(),
            content: entry.content.clone(),
            display: entry.display,
            details: entry.details.clone(),
            timestamp: parse_iso(&entry.meta.timestamp).unwrap_or_default(),
        })],
        FileEntry::BranchSummary(entry) if !entry.summary.is_empty() => {
            vec![Message::BranchSummary(BranchSummaryMessage {
                summary: entry.summary.clone(),
                from_id: Some(entry.from_id.clone()),
                timestamp: parse_iso(&entry.meta.timestamp).unwrap_or_default(),
            })]
        }
        FileEntry::Compaction(entry) => {
            let summary = Message::CompactionSummary(CompactionSummaryMessage {
                summary: entry.summary.clone(),
                tokens_before: entry.tokens_before,
                timestamp: parse_iso(&entry.meta.timestamp).unwrap_or_default(),
            });
            match &entry.system_message {
                Some(system) => vec![(**system).clone(), summary],
                None => vec![summary],
            }
        }
        _ => Vec::new(),
    }
}

fn edited(
    messages: Vec<Message>,
    replacement: Option<&ri_types::session::Replacement>,
) -> Vec<Message> {
    let Some(replacement) = replacement else {
        return Vec::new();
    };
    let as_blocks = || match &replacement.content {
        Content::Text(text) => vec![ContentBlock::Text(TextContent {
            text: text.clone(),
            text_signature: None,
        })],
        Content::Blocks(blocks) => blocks.clone(),
    };
    messages
        .into_iter()
        .map(|message| match message {
            Message::User(mut user) => {
                user.content = replacement.content.clone();
                Message::User(user)
            }
            Message::Custom(mut custom) => {
                custom.content = replacement.content.clone();
                Message::Custom(custom)
            }
            Message::Assistant(mut assistant) => {
                assistant.content = as_blocks();
                Message::Assistant(assistant)
            }
            Message::ToolResult(mut result) => {
                result.content = as_blocks();
                Message::ToolResult(result)
            }
            other => other,
        })
        .collect()
}

/// One context entry of a branch and the messages it contributes after edits.
#[derive(Clone, Debug)]
pub struct ProjectedEntry<'a> {
    /// The entry.
    pub source: &'a FileEntry,
    /// Its model-visible messages; empty when edited out or not visible.
    pub messages: Vec<Message>,
}

/// The context of a branch, entry by entry.
#[derive(Clone, Debug)]
pub struct Projection<'a> {
    /// Context entries: the last compaction first, then the entries it keeps and
    /// the later ones.
    pub entries: Vec<ProjectedEntry<'a>>,
    /// Every projected message, in order.
    pub messages: Vec<Message>,
    /// The last thinking level set on the branch; `off` when none.
    pub thinking_level: String,
    /// The last model used or selected on the branch, as (provider, model id).
    pub model: Option<(String, String)>,
}

/// The messages an entry contributes to the context before edits.
pub fn entry_context_messages(entry: &FileEntry) -> Vec<Message> {
    entry_messages(entry)
}

/// Projects a branch path: from the last compaction on, entries kept by it, then
/// later entries; context edits applied; model and thinking level as last set.
pub fn build_projection<'a>(path: &[&'a FileEntry]) -> Projection<'a> {
    let mut thinking_level = "off".to_owned();
    let mut model = None;
    for entry in path {
        match entry {
            FileEntry::ThinkingLevelChange(change) => {
                thinking_level = change.thinking_level.clone()
            }
            FileEntry::ModelChange(change) => {
                model = Some((change.provider.clone(), change.model_id.clone()));
            }
            FileEntry::Message(entry) => {
                if let Message::Assistant(assistant) = &entry.message {
                    model = Some((assistant.provider.clone(), assistant.model.clone()));
                }
            }
            _ => {}
        }
    }

    let compaction = path
        .iter()
        .rposition(|entry| matches!(entry, FileEntry::Compaction(_)));
    let context: Vec<&'a FileEntry> = match compaction {
        None => path.to_vec(),
        Some(index) => {
            let FileEntry::Compaction(entry) = path[index] else {
                unreachable!("position matched a compaction")
            };
            let first_kept = entry.first_kept_entry_id.clone().flatten();
            let mut context = vec![path[index]];
            let mut found = false;
            for candidate in &path[..index] {
                if candidate.meta().map(|meta| &meta.id) == first_kept.as_ref() {
                    found = true;
                }
                let is_system = matches!(candidate, FileEntry::Message(m) if matches!(m.message, Message::System(_)));
                if found && !is_system {
                    context.push(candidate);
                }
            }
            context.extend(&path[index + 1..]);
            context
        }
    };

    let mut edits: HashMap<&str, Option<&ri_types::session::Replacement>> = HashMap::new();
    for entry in &context {
        if let FileEntry::ContextEdit(edit) = entry {
            edits.insert(&edit.target_id, edit.replacement.as_ref());
        }
    }
    let entries: Vec<ProjectedEntry<'a>> = context
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            // Only the newest compaction, first in the context, contributes.
            let messages = if matches!(entry, FileEntry::Compaction(_)) && index > 0 {
                Vec::new()
            } else {
                let messages = entry_messages(entry);
                match entry.meta().and_then(|meta| edits.get(meta.id.as_str())) {
                    Some(replacement) => edited(messages, *replacement),
                    None => messages,
                }
            };
            ProjectedEntry {
                source: entry,
                messages,
            }
        })
        .collect();
    Projection {
        messages: entries
            .iter()
            .flat_map(|entry| entry.messages.iter().cloned())
            .collect(),
        entries,
        thinking_level,
        model,
    }
}

/// The model context of a branch path; see [`build_projection`].
pub fn build_context(path: &[&FileEntry]) -> SessionContext {
    let projection = build_projection(path);
    SessionContext {
        messages: projection.messages,
        thinking_level: projection.thinking_level,
        model: projection.model,
    }
}

/// Sessions in `dir`, most recently modified first.
pub fn list(dir: &Path) -> Result<Vec<SessionSummary>> {
    let Ok(read) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut sessions = Vec::new();
    for item in read.flatten() {
        let path = item.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let modified_ms = item
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        let docs = load_docs(&path)?;
        let Some(header) = docs.first() else { continue };
        let mut summary = SessionSummary {
            path: path.clone(),
            id: header["id"].as_str().unwrap_or_default().to_owned(),
            cwd: header["cwd"].as_str().unwrap_or_default().to_owned(),
            name: None,
            first_message: String::new(),
            message_count: 0,
            modified_ms,
        };
        for doc in &docs[1..] {
            match doc["type"].as_str() {
                Some("message") => {
                    summary.message_count += 1;
                    if summary.first_message.is_empty() && doc["message"]["role"] == "user" {
                        summary.first_message = match &doc["message"]["content"] {
                            Value::String(text) => text.clone(),
                            Value::Array(blocks) => blocks
                                .iter()
                                .filter_map(|block| block["text"].as_str())
                                .collect::<Vec<_>>()
                                .join(" "),
                            _ => String::new(),
                        };
                    }
                }
                Some("session_info") => {
                    summary.name = doc["name"]
                        .as_str()
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned);
                }
                _ => {}
            }
        }
        sessions.push(summary);
    }
    sessions.sort_by_key(|summary| std::cmp::Reverse(summary.modified_ms));
    Ok(sessions)
}

/// Finds a session in `dir` by full id or unique id prefix.
pub fn find_by_id(dir: &Path, id: &str) -> Result<Option<PathBuf>> {
    let matches: Vec<PathBuf> = list(dir)?
        .into_iter()
        .filter(|summary| summary.id == id || summary.id.starts_with(id))
        .map(|summary| summary.path)
        .collect();
    Ok(if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    })
}
