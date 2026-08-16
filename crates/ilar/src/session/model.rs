//! Neutral, provider-agnostic message model.
//!
//! Providers translate to/from their wire formats; the agent loop, session
//! store and tool executor speak only these types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// Extended-thinking block. Anthropic-style APIs require passing these
    /// back on the next request when thinking interleaves with tool use.
    Thinking {
        text: String,
        /// Signature for verifiable thinking (Anthropic-style); providers
        /// that don't sign leave it out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Opaque provider state required to continue stateless reasoning turns.
    Reasoning {
        item: serde_json::Value,
    },
    /// Exact provider assistant content retained alongside neutral display
    /// blocks when a paused response had to be resumed.
    ProviderReplay {
        provider: String,
        content: serde_json::Value,
    },
    /// Locally visible provider diagnostics that must never be replayed.
    Diagnostic {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl ChatMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

/// Token usage as reported by the provider on a completed turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTokenAccounting {
    IncludesCached,
    ExcludesCached,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input tokens. Provider adapters normalize totals into this
    /// field plus the cache fields below.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt-caching accounting (Anthropic-style providers).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Absent in legacy sessions, whose provider-specific cache semantics are
    /// ambiguous and therefore unsuitable as an exact context estimate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_token_accounting: Option<InputTokenAccounting>,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn context_tokens(&self) -> u64 {
        let cached = match self.input_token_accounting {
            Some(InputTokenAccounting::IncludesCached) => 0,
            Some(InputTokenAccounting::ExcludesCached) | None => self
                .cache_read_input_tokens
                .saturating_add(self.cache_creation_input_tokens),
        };
        self.input_tokens
            .saturating_add(cached)
            .saturating_add(self.output_tokens)
    }
}
