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

use serde_json::Value;
use yapi_types::message::{
    BranchSummaryMessage, CompactionSummaryMessage, Content, ContentBlock, CustomMessage, Message,
    Usage,
};
use yapi_types::session::{
    BranchSummaryEntry, CURRENT_VERSION, CompactionEntry, ContextEditEntry, CustomEntry,
    CustomMessageEntry, EntryMeta, FileEntry, LabelEntry, MessageEntry, ModelChangeEntry,
    Replacement, SessionHeader, SessionInfoEntry, ThinkingLevelChangeEntry,
};

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
    #[error("Session file is not a valid yapi session: {0}")]
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

#[derive(Clone)]
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
    /// Text of every user and assistant message, space-separated, for search.
    pub all_messages_text: String,
    /// The session this one was forked from.
    pub parent_session: Option<String>,
    /// Number of messages.
    pub message_count: usize,
    /// Modification time, Unix milliseconds.
    pub modified_ms: u64,
}

/// One entry of a [`SessionTree`].
#[derive(Clone, Debug, PartialEq)]
pub struct TreeNode {
    /// The entry.
    pub entry: FileEntry,
    /// Indexes of the children, oldest first.
    pub children: Vec<usize>,
    /// The entry's label.
    pub label: Option<String>,
    /// When the label was last set.
    pub label_timestamp: Option<String>,
}

/// Every entry of a session as a tree, stored flat.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionTree {
    /// The nodes, in file order.
    pub nodes: Vec<TreeNode>,
    /// Indexes of entries without a known parent.
    pub roots: Vec<usize>,
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
    yapi_types::json::stringify(doc) + "\n"
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
    /// `id` must be a valid session id.
    pub fn create(cwd: &Path, dir: &Path, id: Option<String>) -> Result<SessionManager> {
        if let Some(id) = &id {
            validate_session_id(id)?;
        }
        fs::create_dir_all(dir).map_err(io(dir))?;
        let mut manager = SessionManager::blank(cwd, dir, true);
        manager.new_session(id, None);
        Ok(manager)
    }

    /// A copy at the same leaf that is never written, to try appends on.
    pub fn preview(&self) -> SessionManager {
        let mut copy = SessionManager::blank(&self.cwd, &self.dir, false);
        copy.session_id.clone_from(&self.session_id);
        copy.entries = self.entries.clone();
        copy.index();
        copy.leaf.clone_from(&self.leaf);
        copy
    }

    /// A session that is never written.
    pub fn in_memory(cwd: &Path) -> SessionManager {
        let mut manager = SessionManager::blank(cwd, Path::new(""), false);
        manager.new_session(None, None);
        manager
    }

    /// Opens a session file; a missing file starts a new session at that path. The
    /// working directory comes from the header unless given; new sessions go to
    /// `dir`, else the file's directory.
    pub fn open(path: &Path, dir: Option<&Path>, cwd: Option<&Path>) -> Result<SessionManager> {
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
        let dir = dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.parent().unwrap_or(Path::new("")).to_path_buf());
        let mut manager = SessionManager::blank(&cwd, &dir, true);
        manager.set_file(path, docs)?;
        Ok(manager)
    }

    /// The most recently modified session file in `dir` (one from `cwd` when
    /// filtering), or a new session.
    pub fn continue_recent(cwd: &Path, dir: &Path, filter_cwd: bool) -> Result<SessionManager> {
        fs::create_dir_all(dir).map_err(io(dir))?;
        match find_most_recent(dir, filter_cwd.then_some(cwd)) {
            Some(recent) => SessionManager::open(&recent, Some(dir), Some(cwd)),
            None => SessionManager::create(cwd, dir, None),
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
            write_new(&file, &text)?;
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
        let system = yapi_ai::transcript::current_system_message(&self.build_context().messages)
            .map(|mut system| {
                system.timestamp = parse_iso(&meta.timestamp).unwrap_or_default();
                Box::new(Message::System(system))
            });
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
                Content::Blocks(vec![ContentBlock::text(text)])
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

    /// The header and the other entries as stored, in file order: what pi's
    /// HTML export embeds.
    pub fn documents(&self) -> (Option<&Value>, Vec<&Value>) {
        let header = self
            .entries
            .first()
            .filter(|entry| entry.kind() == "session")
            .map(|entry| &entry.doc);
        let entries = self
            .entries
            .iter()
            .filter(|entry| entry.kind() != "session")
            .map(|entry| &entry.doc)
            .collect();
        (header, entries)
    }

    /// Every entry except the header, in file order.
    pub fn entries(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind() != "session")
            .filter_map(|entry| entry.view.as_ref())
    }

    /// pi's `getTree`: entries under their parents, children oldest first;
    /// entries whose parent is missing become roots.
    pub fn tree(&self) -> SessionTree {
        let entries: Vec<&FileEntry> = self.entries().collect();
        let index: HashMap<&str, usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(position, entry)| entry.meta().map(|meta| (meta.id.as_str(), position)))
            .collect();
        let mut tree = SessionTree {
            nodes: entries
                .iter()
                .map(|entry| {
                    let id = entry
                        .meta()
                        .map(|meta| meta.id.as_str())
                        .unwrap_or_default();
                    let label = self.labels.get(id);
                    TreeNode {
                        entry: (*entry).clone(),
                        children: Vec::new(),
                        label: label.map(|(label, _)| label.clone()),
                        label_timestamp: label.map(|(_, timestamp)| timestamp.clone()),
                    }
                })
                .collect(),
            roots: Vec::new(),
        };
        for (position, entry) in entries.iter().enumerate() {
            let meta = entry.meta();
            let parent = meta
                .and_then(|meta| meta.parent_id.as_deref())
                .filter(|parent| Some(*parent) != meta.map(|meta| meta.id.as_str()))
                .and_then(|parent| index.get(parent));
            match parent {
                Some(&parent) => tree.nodes[parent].children.push(position),
                None => tree.roots.push(position),
            }
        }
        let time = |node: &TreeNode| {
            node.entry
                .meta()
                .and_then(|meta| crate::time::parse_iso(&meta.timestamp))
                .unwrap_or(0)
        };
        for position in 0..tree.nodes.len() {
            let mut children = std::mem::take(&mut tree.nodes[position].children);
            children.sort_by_key(|child| time(&tree.nodes[*child]));
            tree.nodes[position].children = children;
        }
        tree
    }

    /// Positions in `entries` of the path from the root to `from` (default:
    /// the leaf).
    fn branch_positions(&self, from: Option<&str>) -> Vec<usize> {
        let mut positions = Vec::new();
        let mut current = from
            .or(self.leaf.as_deref())
            .and_then(|id| self.by_id.get(id));
        while let Some(&position) = current {
            positions.push(position);
            current = self.entries[position]
                .parent_id()
                .and_then(|parent| self.by_id.get(parent));
        }
        positions.reverse();
        positions
    }

    /// The path from the root to `from` (default: the leaf).
    pub fn branch_path(&self, from: Option<&str>) -> Vec<&FileEntry> {
        self.branch_positions(from)
            .into_iter()
            .filter_map(|position| self.entries[position].view.as_ref())
            .collect()
    }

    /// pi's `serializeSessionBranch`: a fresh header and the current branch with
    /// parent ids rechained, as JSONL.
    pub fn serialize_branch(&self) -> String {
        let positions = self.branch_positions(None);
        let header = serde_json::json!({
            "type": "session",
            "version": CURRENT_VERSION,
            "id": self.session_id,
            "timestamp": crate::time::now_iso(),
            "cwd": self.cwd.display().to_string(),
        });
        let mut out = line(&header);
        let mut parent = Value::Null;
        for position in positions {
            let mut doc = self.entries[position].doc.clone();
            if let Some(object) = doc.as_object_mut() {
                object.insert("parentId".into(), parent.clone());
            }
            parent = doc["id"].clone();
            out.push_str(&line(&doc));
        }
        out
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
    pub fn fork_from(
        source: &Path,
        cwd: &Path,
        dir: &Path,
        id: Option<String>,
    ) -> Result<SessionManager> {
        let docs = load_docs(source)?;
        if docs.is_empty() {
            return Err(SessionError::Rejected(format!(
                "Cannot fork: source session file is empty or invalid: {}",
                source.display()
            )));
        }
        if let Some(id) = &id {
            validate_session_id(id)?;
        }
        fs::create_dir_all(dir).map_err(io(dir))?;
        let mut manager = SessionManager::blank(cwd, dir, true);
        let file = manager
            .new_session(id, Some(source.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let mut text = line(&manager.entries[0].doc);
        for doc in docs.iter().filter(|doc| doc["type"] != "session") {
            text += &line(doc);
        }
        write_new(&file, &text)?;
        SessionManager::open(&file, Some(dir), Some(cwd))
    }
}

/// The messages an entry contributes to the context before edits.
pub(crate) fn entry_messages(entry: &FileEntry) -> Vec<Message> {
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
    replacement: Option<&yapi_types::session::Replacement>,
) -> Vec<Message> {
    let Some(replacement) = replacement else {
        return Vec::new();
    };
    let as_blocks = || replacement.content.clone().into_blocks();
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

    let mut edits: HashMap<&str, Option<&yapi_types::session::Replacement>> = HashMap::new();
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

/// Whether a session id is valid: alphanumeric at both ends, with `.`, `_` and
/// `-` inside.
pub fn validate_session_id(id: &str) -> Result<()> {
    let inner = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let valid = id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id.ends_with(|c: char| c.is_ascii_alphanumeric())
        && id.chars().all(inner);
    if valid {
        Ok(())
    } else {
        Err(SessionError::Rejected(
            "Session id must be non-empty, contain only alphanumeric characters, '-', '_', and '.', and start and end with an alphanumeric character".into(),
        ))
    }
}

fn header(path: &Path) -> Option<Value> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut line).ok()?;
    let header: Value = yapi_types::json::parse(&line).ok()?;
    (header["type"] == "session").then_some(header)
}

/// Whether a session recorded with working directory `recorded` belongs to
/// `cwd`.
fn cwd_matches(recorded: &str, cwd: &Path) -> bool {
    !recorded.is_empty() && crate::tools::path::resolve_lexically(cwd, Path::new(recorded)) == cwd
}

/// Writes `text` to a new file at `path`, failing when one exists.
fn write_new(path: &Path, text: &str) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut handle| handle.write_all(text.as_bytes()))
        .map_err(io(path))
}

fn session_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .map(|item| item.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect()
}

fn modified_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The most recently modified session file in `dir`, from `cwd` when given.
pub fn find_most_recent(dir: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let mut files: Vec<(u64, PathBuf)> = session_paths(dir)
        .into_iter()
        .map(|path| (modified_ms(&path), path))
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.into_iter().map(|(_, path)| path).find(|path| {
        header(path).is_some_and(|header| {
            cwd.is_none_or(|cwd| cwd_matches(header["cwd"].as_str().unwrap_or_default(), cwd))
        })
    })
}

/// The session file in `dir` with exactly this id, from `cwd` when given.
pub fn find_by_id(dir: &Path, id: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    session_paths(dir).into_iter().find(|path| {
        header(path).is_some_and(|header| {
            header["id"] == id
                && cwd
                    .is_none_or(|cwd| cwd_matches(header["cwd"].as_str().unwrap_or_default(), cwd))
        })
    })
}

fn summary(path: &Path) -> Option<SessionSummary> {
    let docs = load_docs(path).ok()?;
    let header = docs.first().filter(|header| header["type"] == "session")?;
    let mut summary = SessionSummary {
        path: path.to_path_buf(),
        id: header["id"].as_str().unwrap_or_default().to_owned(),
        cwd: header["cwd"].as_str().unwrap_or_default().to_owned(),
        name: None,
        first_message: String::new(),
        all_messages_text: String::new(),
        parent_session: header["parentSession"].as_str().map(str::to_owned),
        message_count: 0,
        modified_ms: 0,
    };
    let mut last_activity: Option<u64> = None;
    let mut all_messages: Vec<String> = Vec::new();
    for doc in &docs[1..] {
        match doc["type"].as_str() {
            Some("session_info") => {
                summary.name = doc["name"]
                    .as_str()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
            }
            Some("message") => {
                summary.message_count += 1;
                let message = &doc["message"];
                if !matches!(message["role"].as_str(), Some("user" | "assistant"))
                    || message.get("content").is_none()
                {
                    continue;
                }
                let activity = message["timestamp"]
                    .as_f64()
                    .map(|ms| ms as u64)
                    .or_else(|| doc["timestamp"].as_str().and_then(parse_iso));
                if let Some(activity) = activity {
                    last_activity = Some(last_activity.unwrap_or(0).max(activity));
                }
                let text = match &message["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(blocks) => blocks
                        .iter()
                        .filter(|block| block["type"] == "text")
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => String::new(),
                };
                if text.is_empty() {
                    continue;
                }
                if summary.first_message.is_empty() && message["role"] == "user" {
                    summary.first_message = text.clone();
                }
                all_messages.push(text);
            }
            _ => {}
        }
    }
    if summary.first_message.is_empty() {
        summary.first_message = "(no messages)".into();
    }
    summary.all_messages_text = all_messages.join(" ");
    summary.modified_ms = match last_activity.filter(|time| *time > 0) {
        Some(time) => time,
        None => header["timestamp"]
            .as_str()
            .and_then(parse_iso)
            .unwrap_or_else(|| modified_ms(path)),
    };
    Some(summary)
}

fn sorted(mut sessions: Vec<SessionSummary>) -> Vec<SessionSummary> {
    sessions.sort_by_key(|summary| std::cmp::Reverse(summary.modified_ms));
    sessions
}

/// Sessions in `dir`, latest activity first; only those from `cwd` when given.
pub fn list(dir: &Path, cwd: Option<&Path>) -> Vec<SessionSummary> {
    sorted(
        session_paths(dir)
            .iter()
            .filter_map(|path| summary(path))
            .filter(|summary| cwd.is_none_or(|cwd| cwd_matches(&summary.cwd, cwd)))
            .collect(),
    )
}

/// Sessions of every project: the session files in each directory under `root`,
/// latest activity first.
pub fn list_all(root: &Path) -> Vec<SessionSummary> {
    let Ok(read) = fs::read_dir(root) else {
        return Vec::new();
    };
    sorted(
        read.flatten()
            .map(|item| item.path())
            .filter(|path| path.is_dir())
            .flat_map(|dir| session_paths(&dir))
            .filter_map(|path| summary(&path))
            .collect(),
    )
}
