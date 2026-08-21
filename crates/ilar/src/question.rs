//! Typed protocol for asking an interactive client structured questions.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::provider::ToolDefinition;

/// Provider-facing name used to mark a structured question request.
pub const QUESTION_TOOL_NAME: &str = "question";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
    #[serde(flatten)]
    pub kind: QuestionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionKind {
    SingleChoice {
        allow_other: bool,
        options: Vec<QuestionOption>,
    },
    MultipleChoice {
        allow_other: bool,
        options: Vec<QuestionOption>,
    },
    FreeText,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A validated answer set for a structured question request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionReply {
    pub answers: Vec<QuestionAnswer>,
}

/// Frontend response to a structured question prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum QuestionResponse {
    Answered { answers: Vec<QuestionAnswer> },
    Cancelled,
}

impl QuestionResponse {
    pub fn answered(reply: QuestionReply) -> Self {
        Self::Answered {
            answers: reply.answers,
        }
    }

    pub fn validate(&self, request: &QuestionRequest) -> Result<(), QuestionValidationError> {
        match self {
            Self::Answered { answers } => validate_reply(
                request,
                &QuestionReply {
                    answers: answers.clone(),
                },
            ),
            Self::Cancelled => validate_request(request),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionAnswer {
    SingleChoice {
        question_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        other: Option<String>,
    },
    MultipleChoice {
        question_id: String,
        option_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        other: Option<String>,
    },
    FreeText {
        question_id: String,
        text: String,
    },
}

impl QuestionAnswer {
    pub fn question_id(&self) -> &str {
        match self {
            Self::SingleChoice { question_id, .. }
            | Self::MultipleChoice { question_id, .. }
            | Self::FreeText { question_id, .. } => question_id,
        }
    }
}

/// A request delivered over [`QuestionSender`], with its one-shot reply path.
#[derive(Debug)]
pub struct QuestionPrompt {
    pub session_id: String,
    pub tool_call_id: String,
    pub request: QuestionRequest,
    pub reply: oneshot::Sender<QuestionResponse>,
}

pub type QuestionSender = mpsc::Sender<QuestionPrompt>;
pub type QuestionReceiver = mpsc::Receiver<QuestionPrompt>;

pub fn question_channel(capacity: usize) -> (QuestionSender, QuestionReceiver) {
    mpsc::channel(capacity)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct QuestionValidationError {
    message: String,
}

impl QuestionValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub fn validate_request(request: &QuestionRequest) -> Result<(), QuestionValidationError> {
    if request.questions.is_empty() {
        return Err(QuestionValidationError::new(
            "a question request must contain at least one question",
        ));
    }

    let mut question_ids = HashSet::new();
    for question in &request.questions {
        require_text(&question.id, "question id")?;
        require_text(&question.prompt, "question prompt")?;
        if !question_ids.insert(question.id.as_str()) {
            return Err(QuestionValidationError::new(format!(
                "duplicate question id {:?}",
                question.id
            )));
        }
        if let Some(description) = &question.description {
            require_text(description, "question description")?;
        }
        if let QuestionKind::SingleChoice { options, .. }
        | QuestionKind::MultipleChoice { options, .. } = &question.kind
        {
            validate_options(&question.id, options)?;
        }
    }
    Ok(())
}

pub fn validate_reply(
    request: &QuestionRequest,
    reply: &QuestionReply,
) -> Result<(), QuestionValidationError> {
    validate_request(request)?;

    let mut answered = HashSet::new();
    for answer in &reply.answers {
        let id = answer.question_id();
        if !answered.insert(id) {
            return Err(QuestionValidationError::new(format!(
                "duplicate answer for question {id:?}"
            )));
        }
        let question = request
            .questions
            .iter()
            .find(|question| question.id == id)
            .ok_or_else(|| QuestionValidationError::new(format!("unknown question id {id:?}")))?;
        validate_answer(question, answer)?;
    }

    if let Some(question) = request
        .questions
        .iter()
        .find(|question| question.required && !answered.contains(question.id.as_str()))
    {
        return Err(QuestionValidationError::new(format!(
            "required question {:?} was not answered",
            question.id
        )));
    }
    Ok(())
}

fn validate_options(
    question_id: &str,
    options: &[QuestionOption],
) -> Result<(), QuestionValidationError> {
    if options.is_empty() {
        return Err(QuestionValidationError::new(format!(
            "choice question {question_id:?} must contain at least one option"
        )));
    }
    let mut ids = HashSet::new();
    for option in options {
        require_text(&option.id, "option id")?;
        require_text(&option.label, "option label")?;
        if !ids.insert(option.id.as_str()) {
            return Err(QuestionValidationError::new(format!(
                "duplicate option id {:?} in question {question_id:?}",
                option.id
            )));
        }
        if let Some(description) = &option.description {
            require_text(description, "option description")?;
        }
    }
    Ok(())
}

fn validate_answer(
    question: &Question,
    answer: &QuestionAnswer,
) -> Result<(), QuestionValidationError> {
    match (&question.kind, answer) {
        (
            QuestionKind::SingleChoice {
                allow_other,
                options,
            },
            QuestionAnswer::SingleChoice {
                option_id, other, ..
            },
        ) => {
            if other.is_some() && !nonempty(other.as_deref()) {
                return Err(invalid_answer(question, "contains empty other text"));
            }
            let has_option = option_id.is_some();
            let has_other = nonempty(other.as_deref());
            if has_option == has_other {
                return Err(invalid_answer(
                    question,
                    "select exactly one option or provide other text",
                ));
            }
            if let Some(id) = option_id
                && !options.iter().any(|option| option.id == *id)
            {
                return Err(invalid_answer(question, "selected an unknown option"));
            }
            if has_other && !allow_other {
                return Err(invalid_answer(question, "does not allow other text"));
            }
            Ok(())
        }
        (
            QuestionKind::MultipleChoice {
                allow_other,
                options,
            },
            QuestionAnswer::MultipleChoice {
                option_ids, other, ..
            },
        ) => {
            let mut selected = HashSet::new();
            for id in option_ids {
                if !selected.insert(id) {
                    return Err(invalid_answer(
                        question,
                        "selected an option more than once",
                    ));
                }
                if !options.iter().any(|option| option.id == *id) {
                    return Err(invalid_answer(question, "selected an unknown option"));
                }
            }
            let has_other = nonempty(other.as_deref());
            if other.is_some() && !has_other {
                return Err(invalid_answer(question, "contains empty other text"));
            }
            if has_other && !allow_other {
                return Err(invalid_answer(question, "does not allow other text"));
            }
            if question.required && option_ids.is_empty() && !has_other {
                return Err(invalid_answer(question, "requires at least one selection"));
            }
            Ok(())
        }
        (QuestionKind::FreeText, QuestionAnswer::FreeText { text, .. }) => {
            if question.required && text.trim().is_empty() {
                return Err(invalid_answer(question, "requires non-empty text"));
            }
            Ok(())
        }
        _ => Err(invalid_answer(question, "has the wrong answer type")),
    }
}

fn invalid_answer(question: &Question, reason: &str) -> QuestionValidationError {
    QuestionValidationError::new(format!("answer for question {:?} {reason}", question.id))
}

fn require_text(value: &str, field: &str) -> Result<(), QuestionValidationError> {
    if value.trim().is_empty() {
        Err(QuestionValidationError::new(format!(
            "{field} must not be empty"
        )))
    } else {
        Ok(())
    }
}

fn nonempty(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

/// Non-executable provider definition recognized by the agent as a question.
pub fn question_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: QUESTION_TOOL_NAME.into(),
        description: "Ask the user one or more structured questions. Use stable IDs. Choice questions may permit a custom other answer.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["questions"],
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "oneOf": [
                            question_schema("single_choice", true),
                            question_schema("multiple_choice", true),
                            question_schema("free_text", false)
                        ]
                    }
                }
            }
        }),
    }
}

fn question_schema(kind: &str, choice: bool) -> serde_json::Value {
    let mut properties = serde_json::json!({
        "id": {"type": "string", "minLength": 1, "description": "Stable question ID"},
        "type": {"type": "string", "const": kind},
        "prompt": {"type": "string", "minLength": 1},
        "description": {"type": "string", "minLength": 1},
        "required": {"type": "boolean"}
    });
    let mut required = vec!["id", "type", "prompt", "required"];
    if choice {
        properties["allow_other"] = serde_json::json!({"type": "boolean"});
        properties["options"] = serde_json::json!({
            "type": "array",
            "minItems": 1,
            "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "label"],
                "properties": {
                    "id": {"type": "string", "minLength": 1, "description": "Stable option ID"},
                    "label": {"type": "string", "minLength": 1},
                    "description": {"type": "string", "minLength": 1}
                }
            }
        });
        required.extend(["allow_other", "options"]);
    }
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": properties
    })
}
