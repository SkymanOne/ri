//! `auth.json`: stored credentials keyed by provider id.
//!
//! Mirrors `packages/ai/src/auth/types.ts` in pi `v1.0.0`.
#![allow(
    missing_docs,
    reason = "fields mirror pi's TypeScript types; contracts are noted where they differ from the name"
)]

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The whole file: provider id to credential.
pub type AuthFile = IndexMap<String, Credential>;

/// A stored credential, tagged by `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

/// Values may be literals, `$VAR` templates or `!command`s; see pi's
/// `resolve-config-value.ts`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Extra environment for the provider, such as a region.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<IndexMap<String, String>>,
}

/// OAuth tokens. Providers store additional fields, such as `accountId` or
/// `enterpriseUrl`, which are kept in `extra`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access: String,
    pub refresh: String,
    /// Expiry as Unix time in milliseconds.
    pub expires: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
