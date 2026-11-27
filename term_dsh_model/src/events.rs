//! Answerable DSH event outcomes; serializable data only.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Outcome sent back to the host for one answerable `$events` waterfall
/// (approval / user-questions), via `POST /api/$events/result`.
///
/// Serializes to `{kind:'next'}` (delegate to the next answerer),
/// `{kind:'result', value?}` (the answer; `value` is an `ApprovalOutcome`
/// string for `approval/request`, an `AskUserQuestionAnswer` object for
/// `user-questions/request`), or `{kind:'rejected', error:{…}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum EventOutcome {
    /// Pass the waterfall on to the next composed answerer.
    Next,
    /// Claim the waterfall with an answer value.
    Result {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
    },
    /// Fail the waterfall with an error the host rethrows to the asker.
    Rejected { error: EventRejection },
}

impl EventOutcome {
    /// Claim the waterfall with `value`.
    pub fn result(value: Value) -> Self {
        EventOutcome::Result { value: Some(value) }
    }

    /// Fail the waterfall with a plain error (no `code`/`details`).
    pub fn rejected(name: impl Into<String>, message: impl Into<String>) -> Self {
        EventOutcome::Rejected {
            error: EventRejection {
                name: name.into(),
                message: message.into(),
                code: None,
                details: None,
            },
        }
    }
}

/// Wire-safe rejection fields of [`EventOutcome::Rejected`]
/// (`{name, message, code?, details?}`; `details` must be lossless JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRejection {
    pub name: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}
