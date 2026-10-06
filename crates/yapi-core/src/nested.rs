//! Tool calls a tool makes while it runs (`ctx.executeTool()`), for example
//! from codemode scripts. Port of `core/nested-tool-calls.ts` in pi `v1.0.0`.
//!
//! The session runs each call through the agent's tool pipeline with its own
//! hooks and emits `tool_execution_*` events with `parentToolCallId`. Calls are
//! recorded per model-issued call and attached to its tool result message as
//! `nestedCalls`, with their summed usage.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use yapi_types::message::{NestedToolCall, NestedToolCallStatus, NestedToolCalls, ToolCall, Usage};
use yapi_types::sync::lock;

/// Calls beyond this count are dropped from the record.
const MAX_CALLS: usize = 256;
/// Arguments larger than this, in bytes of JSON, are omitted.
const MAX_ARGUMENT_BYTES_PER_CALL: usize = 8 * 1024;
/// Arguments past this total, in bytes of JSON, are omitted.
const MAX_ARGUMENT_BYTES_TOTAL: usize = 32 * 1024;
/// Error texts are cut to this many UTF-16 code units.
const MAX_ERROR_CHARS: usize = 500;

/// The record of one model-issued call's nested calls.
#[derive(Default)]
struct Recorder {
    calls: Vec<(NestedToolCall, Option<Instant>)>,
    incomplete: bool,
    argument_bytes: usize,
    usage: Option<Usage>,
}

impl Recorder {
    /// Records a call as it starts; `None` when it is dropped.
    fn start(&mut self, call: &ToolCall) -> Option<usize> {
        if self.calls.len() >= MAX_CALLS {
            self.incomplete = true;
            return None;
        }
        let json = yapi_types::json::to_string(&call.arguments).unwrap_or_default();
        let bytes = json.len();
        let mut record = NestedToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: None,
            arguments_bytes: None,
            status: NestedToolCallStatus::Unfinished,
            duration_ms: None,
            error: None,
        };
        if bytes > MAX_ARGUMENT_BYTES_PER_CALL
            || self.argument_bytes + bytes > MAX_ARGUMENT_BYTES_TOTAL
        {
            record.arguments_bytes = Some(bytes as u64);
            self.incomplete = true;
        } else {
            record.arguments = Some(call.arguments.clone());
            self.argument_bytes += bytes;
        }
        self.calls.push((record, Some(Instant::now())));
        Some(self.calls.len() - 1)
    }

    fn finish(&mut self, index: Option<usize>, is_error: bool, error: &str) {
        let Some((record, started)) = index.and_then(|index| self.calls.get_mut(index)) else {
            return;
        };
        record.status = if is_error {
            NestedToolCallStatus::Error
        } else {
            NestedToolCallStatus::Ok
        };
        let elapsed = started
            .take()
            .map_or(0.0, |started| started.elapsed().as_secs_f64() * 1000.0);
        record.duration_ms = Some(yapi_types::js::round(elapsed) as u64);
        if is_error && !error.is_empty() {
            record.error = Some(yapi_types::js::slice(error, 0, MAX_ERROR_CHARS));
        }
    }

    fn snapshot(&self) -> Option<NestedToolCalls> {
        if self.calls.is_empty() && !self.incomplete {
            return None;
        }
        let calls: Vec<NestedToolCall> = self.calls.iter().map(|(call, _)| call.clone()).collect();
        let complete = !self.incomplete
            && calls
                .iter()
                .all(|call| call.status != NestedToolCallStatus::Unfinished);
        Some(NestedToolCalls { calls, complete })
    }
}

/// Calls below one model-issued call share its recorder.
struct Scope {
    recorder: Arc<Mutex<Recorder>>,
    next_id: u64,
    /// Set inside a call that holds the exclusive queue, so its own nested
    /// calls do not wait on it.
    holds_queue: bool,
}

/// A nested call that started: its id and where it is recorded.
pub(crate) struct Started {
    /// `<caller id>/<n>`.
    pub id: String,
    recorder: Arc<Mutex<Recorder>>,
    record: Option<usize>,
    /// Whether the caller's scope already holds the exclusive queue.
    pub holds_queue: bool,
}

/// What the nested calls of one model-issued call leave on its tool result.
pub(crate) struct Summary {
    /// Becomes `nestedCalls`.
    pub calls: Option<NestedToolCalls>,
    /// Added to the message's `usage`.
    pub usage: Option<Usage>,
}

/// The session's nested calls, by the id of the calling tool call.
#[derive(Default)]
pub(crate) struct NestedCalls {
    scopes: Mutex<HashMap<String, Scope>>,
    /// Serializes nested calls that must not run concurrently.
    pub queue: Arc<tokio::sync::Mutex<()>>,
}

impl NestedCalls {
    /// Assigns the next id below `caller` and records `call` under it.
    pub fn start(&self, caller: &str, call: &mut ToolCall) -> Started {
        let mut scopes = lock(&self.scopes);
        let scope = scopes.entry(caller.to_owned()).or_insert_with(|| Scope {
            recorder: Arc::default(),
            next_id: 1,
            holds_queue: false,
        });
        call.id = format!("{caller}/{}", scope.next_id);
        scope.next_id += 1;
        let recorder = scope.recorder.clone();
        let holds_queue = scope.holds_queue;
        let record = lock(&recorder).start(call);
        Started {
            id: call.id.clone(),
            recorder,
            record,
            holds_queue,
        }
    }

    /// Opens the scope of a running nested call, for the calls it makes.
    pub fn enter(&self, started: &Started, holds_queue: bool) {
        lock(&self.scopes).insert(
            started.id.clone(),
            Scope {
                recorder: started.recorder.clone(),
                next_id: 1,
                holds_queue,
            },
        );
    }

    /// Closes a nested call's scope and records how it ended.
    pub fn finish(&self, started: Started, is_error: bool, error: &str, usage: Option<&Usage>) {
        lock(&self.scopes).remove(&started.id);
        let mut recorder = lock(&started.recorder);
        recorder.finish(started.record, is_error, error);
        if let Some(usage) = usage {
            recorder.usage = Some(match &recorder.usage {
                Some(total) => total.combine(usage),
                None => usage.clone(),
            });
        }
    }

    /// Removes and returns the record of the calls `tool_call_id` made.
    pub fn take(&self, tool_call_id: &str) -> Option<Summary> {
        let scope = lock(&self.scopes).remove(tool_call_id)?;
        let recorder = lock(&scope.recorder);
        Some(Summary {
            calls: recorder.snapshot(),
            usage: recorder.usage.clone(),
        })
    }

    /// Forgets every record, when a run ends.
    pub fn clear(&self) {
        lock(&self.scopes).clear();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::*;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: String::new(),
            name: name.into(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            thought_signature: None,
            namespace: None,
        }
    }

    #[test]
    fn numbers_calls_per_caller_and_records_them() {
        let nested = NestedCalls::default();
        let mut first = call("read", json!({"path": "a.txt"}));
        let started = nested.start("toolu_1", &mut first);
        assert_eq!(first.id, "toolu_1/1");
        nested.enter(&started, false);
        let mut inner = call("ls", json!({}));
        let inner_started = nested.start("toolu_1/1", &mut inner);
        assert_eq!(inner.id, "toolu_1/1/1");
        nested.finish(inner_started, true, &"x".repeat(600), None);
        nested.finish(started, false, "", None);
        let mut second = call("read", Value::Object(Map::new()));
        let started = nested.start("toolu_1", &mut second);
        assert_eq!(second.id, "toolu_1/2");

        let summary = nested.take("toolu_1").unwrap().calls.unwrap();
        assert!(!summary.complete);
        let statuses: Vec<_> = summary.calls.iter().map(|call| call.status).collect();
        assert_eq!(
            statuses,
            [
                NestedToolCallStatus::Ok,
                NestedToolCallStatus::Error,
                NestedToolCallStatus::Unfinished
            ]
        );
        assert_eq!(summary.calls[1].error.as_ref().unwrap().len(), 500);
        assert_eq!(summary.calls[0].arguments, Some(first.arguments));
        drop(started);
    }
}
