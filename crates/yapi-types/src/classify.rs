//! Classifier and image-generation requests and results.
//!
//! Mirrors `ClassifierContext`, `ClassifierResult`, `ImagesContext` and
//! `AssistantImages` in `packages/ai/src/types.ts` in pi `v1.0.0`.
#![allow(
    missing_docs,
    reason = "fields mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::message::{ImageContent, TextContent, Usage};

/// The meaning of each answer of a `bool` question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoolCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

/// One question about the state, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierQuestion {
    /// Pick one key of `criteria`; each value describes its option.
    Choice {
        instructions: String,
        criteria: IndexMap<String, String>,
    },
    /// A level index into `criteria`, lowest first.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
    /// Yes or no.
    Bool {
        instructions: String,
        criteria: BoolCriteria,
    },
}

impl ClassifierQuestion {
    /// What the question asks.
    pub fn instructions(&self) -> &str {
        match self {
            ClassifierQuestion::Choice { instructions, .. }
            | ClassifierQuestion::Score { instructions, .. }
            | ClassifierQuestion::Bool { instructions, .. } => instructions,
        }
    }
}

/// What a classifier judges, and the questions to answer about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClassifierContext {
    pub state: Map<String, Value>,
    pub questions: IndexMap<String, ClassifierQuestion>,
}

/// One answer, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierAnswer {
    Choice {
        choice: String,
        probabilities: IndexMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        confidence: f64,
    },
    Bool {
        probability: f64,
    },
}

/// How a classification or image request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeReason {
    Stop,
    Error,
    Aborted,
}

/// A classification's answers, by question id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierResult {
    pub api: String,
    pub provider: String,
    pub model: String,
    pub answers: IndexMap<String, ClassifierAnswer>,
    /// Token usage priced from the catalog, when the service reports tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: OutcomeReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: u64,
}

/// A text or image block of an image request or result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ImagesContent {
    Text(TextContent),
    Image(ImageContent),
}

/// The prompt and reference images of an image request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImagesContext {
    pub input: Vec<ImagesContent>,
}

/// Images, and any text, an image model returned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: String,
    pub provider: String,
    pub model: String,
    pub output: Vec<ImagesContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub stop_reason: OutcomeReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: u64,
}
