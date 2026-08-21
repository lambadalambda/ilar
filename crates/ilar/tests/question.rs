use ilar::question::{
    Question, QuestionAnswer, QuestionKind, QuestionOption, QuestionReply, QuestionRequest,
    question_channel, validate_reply, validate_request,
};
use ilar::tools::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, ToolRegistry, WorkspaceAccess,
    child_tool_names,
};

struct ExecutableQuestion;

impl Tool for ExecutableQuestion {
    fn name(&self) -> &'static str {
        "question"
    }

    fn description(&self) -> &'static str {
        "collision"
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn run(&self, _input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        Box::pin(async { ToolOutput::text("unused") })
    }
}

fn request() -> QuestionRequest {
    QuestionRequest {
        questions: vec![
            Question {
                id: "language".into(),
                prompt: "Pick one".into(),
                description: Some("The implementation language".into()),
                required: true,
                kind: QuestionKind::SingleChoice {
                    allow_other: true,
                    options: vec![
                        QuestionOption {
                            id: "rust".into(),
                            label: "Rust".into(),
                            description: None,
                        },
                        QuestionOption {
                            id: "go".into(),
                            label: "Go".into(),
                            description: Some("Golang".into()),
                        },
                    ],
                },
            },
            Question {
                id: "features".into(),
                prompt: "Select features".into(),
                description: None,
                required: false,
                kind: QuestionKind::MultipleChoice {
                    allow_other: false,
                    options: vec![QuestionOption {
                        id: "tests".into(),
                        label: "Tests".into(),
                        description: None,
                    }],
                },
            },
            Question {
                id: "notes".into(),
                prompt: "Anything else?".into(),
                description: None,
                required: false,
                kind: QuestionKind::FreeText,
            },
        ],
    }
}

#[test]
fn validates_multiple_typed_questions_and_replies() {
    let request = request();
    validate_request(&request).unwrap();

    let reply = QuestionReply {
        answers: vec![
            QuestionAnswer::SingleChoice {
                question_id: "language".into(),
                option_id: None,
                other: Some("Zig".into()),
            },
            QuestionAnswer::MultipleChoice {
                question_id: "features".into(),
                option_ids: vec!["tests".into()],
                other: None,
            },
            QuestionAnswer::FreeText {
                question_id: "notes".into(),
                text: "Keep it small".into(),
            },
        ],
    };
    validate_reply(&request, &reply).unwrap();
}

#[test]
fn rejects_invalid_requests_and_replies() {
    let mut duplicate = request();
    duplicate.questions[1].id = "language".into();
    assert!(validate_request(&duplicate).is_err());

    let wrong_option = QuestionReply {
        answers: vec![QuestionAnswer::SingleChoice {
            question_id: "language".into(),
            option_id: Some("python".into()),
            other: None,
        }],
    };
    assert!(validate_reply(&request(), &wrong_option).is_err());

    let blank_other = QuestionReply {
        answers: vec![QuestionAnswer::SingleChoice {
            question_id: "language".into(),
            option_id: Some("rust".into()),
            other: Some(" ".into()),
        }],
    };
    assert!(validate_reply(&request(), &blank_other).is_err());

    let missing_required = QuestionReply { answers: vec![] };
    assert!(validate_reply(&request(), &missing_required).is_err());
}

#[tokio::test]
async fn public_channel_carries_request_and_oneshot_reply() {
    let (tx, mut rx) = question_channel(1);
    let expected = request();
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(ilar::question::QuestionPrompt {
        request: expected.clone(),
        reply: reply_tx,
    })
    .await
    .unwrap();

    let prompt = rx.recv().await.unwrap();
    assert_eq!(prompt.request, expected);
    prompt
        .reply
        .send(QuestionReply { answers: vec![] })
        .unwrap();
    assert!(reply_rx.await.is_ok());
}

#[test]
fn questions_are_an_explicit_non_executable_root_definition() {
    let ordinary = ToolRegistry::builtin();
    assert_eq!(ordinary.definitions().len(), ordinary.tool_names().len());
    assert!(ordinary.get("question").is_none());

    let root = ordinary.with_questions();
    let definition = root
        .definitions()
        .into_iter()
        .find(|definition| definition.name == "question")
        .unwrap();
    assert_eq!(definition.name, ilar::question::QUESTION_TOOL_NAME);
    assert_eq!(
        definition.input_schema["properties"]["questions"]["type"],
        "array"
    );
    assert!(root.get("question").is_none());
    assert!(!root.tool_names().contains(&"question"));

    assert!(
        !ToolRegistry::read_only()
            .definitions()
            .iter()
            .any(|definition| definition.name == "question")
    );
    assert!(!child_tool_names().contains(&"question"));
    assert!(
        ToolRegistry::builtin()
            .with_tool(std::sync::Arc::new(ExecutableQuestion))
            .is_err()
    );
}
