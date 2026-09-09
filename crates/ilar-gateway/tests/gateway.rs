use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ilar::config::{Config, Loader};
use ilar::provider::{FixedProviderResolver, MockProvider, ProviderEvent, StopReason};
use ilar::session::Usage;
use ilar_gateway::channel::FakeChannel;
use ilar_gateway::config::GatewayConfig;
use ilar_gateway::gateway::{BUSY_REPLY, Gateway};
use ilar_gateway::routes::RouteStore;

fn config(dir: &Path) -> Config {
    for sub in ["config", "state", "project", "workspace"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    Loader::with_env(vec![("ILAR_ZAI_API_KEY", "zk".to_string())])
        .config_dir(dir.join("config"))
        .state_dir(dir.join("state"))
        .project_dir(dir.join("project"))
        .resolve()
        .unwrap()
}

fn says(text: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta(text.into()),
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]
}

fn delegates(prompt: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCallStarted {
            id: "task-1".into(),
            name: "task".into(),
            item_id: None,
        },
        ProviderEvent::ToolCallCompleted {
            id: "task-1".into(),
            name: "task".into(),
            input: serde_json::json!({
                "description": "survey",
                "prompt": prompt,
                "subagent_type": "explore",
                "background": true,
            }),
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]
}

fn messages(text: &str, chat: Option<&str>) -> Vec<ProviderEvent> {
    let mut input = serde_json::json!({"text": text});
    if let Some(chat) = chat {
        input["chat"] = serde_json::json!(chat);
    }
    vec![
        ProviderEvent::ToolCallStarted {
            id: "msg-1".into(),
            name: "message".into(),
            item_id: None,
        },
        ProviderEvent::ToolCallCompleted {
            id: "msg-1".into(),
            name: "message".into(),
            input,
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]
}

/// A gateway on a fake channel and a scripted provider, running.
fn gateway(dir: &Path, turns: Vec<Vec<ProviderEvent>>) -> (Arc<Gateway>, Arc<FakeChannel>) {
    gateway_with(dir, turns, GatewayConfig::default())
}

fn gateway_with(
    dir: &Path,
    turns: Vec<Vec<ProviderEvent>>,
    settings: GatewayConfig,
) -> (Arc<Gateway>, Arc<FakeChannel>) {
    let config = config(dir);
    let settings = GatewayConfig {
        workspace: Some(dir.join("workspace")),
        ..settings
    };
    let resolver = Arc::new(FixedProviderResolver::new(Arc::new(MockProvider::new(
        turns,
    ))));
    let fake = FakeChannel::new("fake");
    let gateway = Gateway::new(config, settings, resolver, vec![fake.clone()]).unwrap();
    tokio::spawn(gateway.clone().run());
    (gateway, fake)
}

const WAIT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn a_message_round_trips_through_a_channel() {
    let dir = tempfile::tempdir().unwrap();
    let (gateway, fake) = gateway(dir.path(), vec![says("hello there")]);

    fake.inject("hi", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(1, WAIT).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].text, "hello there");
    assert_eq!(sent[0].chat_id, "chat-1");
    gateway.cancel();
}

#[tokio::test]
async fn two_chats_are_two_sessions_and_the_routes_remember_them() {
    let dir = tempfile::tempdir().unwrap();
    let (gateway, fake) = gateway(dir.path(), vec![says("one"), says("two")]);

    fake.inject("first", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    fake.inject("second", "chat-2", "bob").await;
    let sent = fake.wait_for_sent(2, WAIT).await;
    let mut chats: Vec<&str> = sent.iter().map(|m| m.chat_id.as_str()).collect();
    chats.sort();
    assert_eq!(chats, ["chat-1", "chat-2"]);

    let routes = RouteStore::open(dir.path().join("state/gateway/routes.json"))
        .unwrap()
        .snapshot();
    let one = routes.session_for("fake:chat-1").expect("route");
    let two = routes.session_for("fake:chat-2").expect("route");
    assert_ne!(one, two);
    assert_eq!(routes.last_active.as_deref(), Some("fake:chat-2"));
    let store = ilar::runtime::session_store(&config(dir.path()));
    assert_eq!(store.list().len(), 2);
    gateway.cancel();
}

#[tokio::test]
async fn an_inbox_message_reaches_the_last_active_chat() {
    let dir = tempfile::tempdir().unwrap();
    let (gateway, fake) = gateway(dir.path(), vec![says("hi alice"), says("noted the build")]);

    fake.inject("hi", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    ilar_gateway::inbox::write(
        gateway.inbox_dir(),
        &ilar_gateway::inbox::InboxMessage {
            source: "ci".into(),
            text: "build green".into(),
            to: None,
        },
    )
    .unwrap();
    let sent = fake.wait_for_sent(2, WAIT).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1].text, "noted the build");
    assert_eq!(sent[1].chat_id, "chat-1");
    assert!(
        ilar_gateway::inbox::drain(gateway.inbox_dir())
            .unwrap()
            .is_empty()
    );
    gateway.cancel();
}

#[tokio::test]
async fn a_message_sent_by_the_tool_replaces_the_final_text() {
    let dir = tempfile::tempdir().unwrap();
    let (gateway, fake) = gateway(
        dir.path(),
        vec![
            messages("via tool", None),
            says("final words"),
            says("second reply"),
        ],
    );

    fake.inject("hi", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(1, WAIT).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].text, "via tool");
    assert_eq!(sent[0].chat_id, "chat-1");
    // The final text never follows: the next turn's reply is the next
    // thing the chat sees.
    fake.inject("and?", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, WAIT).await;
    let texts: Vec<&str> = sent.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["via tool", "second reply"], "{sent:?}");
    gateway.cancel();
}

#[tokio::test]
async fn a_message_to_an_unknown_chat_is_refused_and_the_final_text_still_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let (gateway, fake) = gateway(
        dir.path(),
        vec![messages("psst", Some("stranger")), says("sorry, cannot")],
    );

    fake.inject("hi", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(1, WAIT).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].text, "sorry, cannot");
    assert_eq!(sent[0].chat_id, "chat-1");
    // The tool told the model why.
    let routes = RouteStore::open(dir.path().join("state/gateway/routes.json"))
        .unwrap()
        .snapshot();
    let session_id = routes.session_for("fake:chat-1").unwrap().to_string();
    let store = ilar::runtime::session_store(&config(dir.path()));
    let refused = store
        .load(&session_id)
        .unwrap()
        .events()
        .iter()
        .any(|event| {
            matches!(event, ilar::session::SessionEvent::ToolResult { content, is_error, .. }
                if *is_error && content.contains("no chat fake:stranger"))
        });
    assert!(refused);
    gateway.cancel();
}

/// One tool call, then the turn yields for its result. Ids are
/// unique across a test: a session refuses a repeated one.
fn calls(name: &str, input: serde_json::Value) -> Vec<ProviderEvent> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    let id = format!(
        "{name}-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    vec![
        ProviderEvent::ToolCallStarted {
            id: id.clone(),
            name: name.into(),
            item_id: None,
        },
        ProviderEvent::ToolCallCompleted {
            id,
            name: name.into(),
            input,
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]
}

/// Whether a chat's session log holds a tool result matching `check`.
fn has_tool_result(dir: &Path, key: &str, check: impl Fn(&str, bool) -> bool) -> bool {
    let routes = RouteStore::open(dir.join("state/gateway/routes.json"))
        .unwrap()
        .snapshot();
    let session_id = routes.session_for(key).unwrap().to_string();
    let store = ilar::runtime::session_store(&config(dir));
    store
        .load(&session_id)
        .unwrap()
        .events()
        .iter()
        .any(|event| {
            matches!(event, ilar::session::SessionEvent::ToolResult { content, is_error, .. }
                if check(content, *is_error))
        })
}

#[tokio::test]
async fn a_fact_kept_in_one_chat_is_found_from_another_and_the_core_reaches_a_new_private_chat() {
    let dir = tempfile::tempdir().unwrap();
    let (gateway, fake) = gateway(
        dir.path(),
        vec![
            calls(
                "memory",
                serde_json::json!({
                    "action": "note", "kind": "preference", "title": "Tea",
                    "summary": "the person likes earl grey", "body": "Never coffee after noon.",
                }),
            ),
            calls(
                "memory",
                serde_json::json!({
                    "action": "add", "file": "user", "text": "Likes earl grey",
                }),
            ),
            says("kept"),
            calls("memory_search", serde_json::json!({"query": "earl grey"})),
            says("found it"),
            says("hello new chat"),
            says("hello group"),
        ],
    );
    fake.inject("remember I like earl grey", "chat-1", "alice")
        .await;
    fake.wait_for_sent(1, WAIT).await;
    fake.inject("what tea do I like?", "chat-2", "alice").await;
    fake.wait_for_sent(2, WAIT).await;
    assert!(has_tool_result(
        dir.path(),
        "fake:chat-2",
        |content, is_error| {
            !is_error && content.contains("[preference]") && content.contains("earl grey")
        }
    ));
    // A chat opened after the core was written sees it; a group does not.
    fake.inject("hi", "chat-3", "alice").await;
    fake.wait_for_sent(3, WAIT).await;
    let prompt = gateway.system_prompt("fake:chat-3").unwrap();
    assert!(
        prompt.contains("# Memory") && prompt.contains("Likes earl grey"),
        "{prompt}"
    );
    fake.inject_in_group("hi all", "room-1", "alice").await;
    fake.wait_for_sent(4, WAIT).await;
    let prompt = gateway.system_prompt("fake:room-1").unwrap();
    assert!(!prompt.contains("# Memory"), "{prompt}");
    // The first two chats opened before the core existed: frozen prompts.
    assert!(
        !gateway
            .system_prompt("fake:chat-1")
            .unwrap()
            .contains("# Memory")
    );
    gateway.cancel();
}

fn schedules_once(secs_from_now: i64, prompt: &str) -> Vec<ProviderEvent> {
    let at = (chrono::Utc::now() + chrono::Duration::seconds(secs_from_now)).to_rfc3339();
    vec![
        ProviderEvent::ToolCallStarted {
            id: "cron-1".into(),
            name: "cron".into(),
            item_id: None,
        },
        ProviderEvent::ToolCallCompleted {
            id: "cron-1".into(),
            name: "cron".into(),
            input: serde_json::json!({
                "action": "add", "name": "ping", "prompt": prompt, "at": at,
            }),
        },
        ProviderEvent::TurnComplete {
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]
}

#[tokio::test]
async fn a_scheduled_turn_speaks_only_through_the_message_tool_and_a_one_shot_retires() {
    let dir = tempfile::tempdir().unwrap();
    let settings = GatewayConfig {
        scheduler_tick_secs: 1,
        ..GatewayConfig::default()
    };
    // The chat asks for a reminder; the reminder's turn sends one
    // message and then says something that must not be delivered.
    let (gateway, fake) = gateway_with(
        dir.path(),
        vec![
            schedules_once(1, "remind them"),
            says("scheduled"),
            messages("scheduled hi", None),
            says("(not for the chat)"),
        ],
        settings,
    );
    fake.inject("remind me", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, Duration::from_secs(15)).await;
    let texts: Vec<&str> = sent.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["scheduled", "scheduled hi"], "{sent:?}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fake.sent().len(), 2, "{:?}", fake.sent());
    let jobs = ilar_gateway::cron::CronStore::open(dir.path().join("state/gateway/cron.json"))
        .unwrap()
        .list();
    // Only the gateway's own weekly job remains.
    let ids: Vec<&str> = jobs.iter().map(|job| job.id.as_str()).collect();
    assert_eq!(ids, ["weekly"], "{jobs:?}");
    gateway.cancel();
}

#[tokio::test]
async fn a_heartbeat_with_nothing_to_say_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let settings = GatewayConfig {
        scheduler_tick_secs: 1,
        heartbeat: ilar_gateway::config::Heartbeat {
            every_secs: 1,
            prompt: "anything?".into(),
            chats: vec!["fake:chat-1".into()],
        },
        ..GatewayConfig::default()
    };
    // The first beat has something to say and says it through the
    // tool; its final text and the later, silent beats send nothing.
    let (gateway, fake) = gateway_with(
        dir.path(),
        vec![
            says("hi"),
            messages("beat!", None),
            says("(the beat's final text)"),
            says("nothing to report"),
            says("nothing to report"),
        ],
        settings,
    );
    fake.inject("hello", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, WAIT).await;
    let texts: Vec<&str> = sent.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["hi", "beat!"], "{sent:?}");
    assert!(gateway.tool_names("heartbeat:fake:chat-1").is_some());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(fake.sent().len(), 2, "{:?}", fake.sent());
    // A background session is nobody's address.
    let routes = RouteStore::open(dir.path().join("state/gateway/routes.json"))
        .unwrap()
        .snapshot();
    assert!(routes.session_for("heartbeat:fake:chat-1").is_none());
    gateway.cancel();
}

#[tokio::test]
async fn attachments_reach_the_model_and_files_go_back_out_by_path() {
    let dir = tempfile::tempdir().unwrap();
    let picture = dir.path().join("picture.png");
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0; 32]);
    std::fs::write(&picture, &png).unwrap();
    let report = dir.path().join("report.txt");
    std::fs::write(&report, "quarterly numbers").unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();
    std::fs::write(dir.path().join("workspace/out.txt"), "here you go").unwrap();
    let (gateway, fake) = gateway(
        dir.path(),
        vec![
            says("got them"),
            calls(
                "message",
                serde_json::json!({"text": "the file", "media": ["out.txt"]}),
            ),
            says("(sent)"),
            calls(
                "message",
                serde_json::json!({"text": "oops", "media": ["missing.bin"]}),
            ),
            says("no such file"),
        ],
    );
    // In: an image becomes a picture on the turn, a file a line naming it.
    fake.inject_with_media(
        "look at these",
        "chat-1",
        vec![picture.clone(), report.clone()],
    )
    .await;
    fake.wait_for_sent(1, WAIT).await;
    let routes = RouteStore::open(dir.path().join("state/gateway/routes.json"))
        .unwrap()
        .snapshot();
    let session_id = routes.session_for("fake:chat-1").unwrap().to_string();
    let store = ilar::runtime::session_store(&config(dir.path()));
    let reader = store.load(&session_id).unwrap();
    let user = reader
        .events()
        .iter()
        .find_map(|event| match event {
            ilar::session::SessionEvent::UserMessage { text, images, .. } => {
                Some((text.clone(), images.len()))
            }
            _ => None,
        })
        .expect("the user message");
    assert_eq!(user.1, 1, "{user:?}");
    assert!(user.0.contains("look at these"), "{}", user.0);
    assert!(
        user.0
            .contains(&format!("file attached: {}", report.display())),
        "{}",
        user.0
    );
    assert!(
        user.0.contains(&format!("also at {}", picture.display())),
        "{}",
        user.0
    );
    // Out: a relative path resolves against the workspace and goes out absolute.
    fake.inject("send me the file", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, WAIT).await;
    assert_eq!(sent[1].text, "the file");
    assert_eq!(sent[1].media, vec![dir.path().join("workspace/out.txt")]);
    // A file that is not there is refused, and the model says so.
    fake.inject("and the other", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(3, WAIT).await;
    assert_eq!(sent[2].text, "no such file");
    assert!(has_tool_result(
        dir.path(),
        "fake:chat-1",
        |content, is_error| { is_error && content.contains("no file at") }
    ));
    gateway.cancel();
}

#[tokio::test]
async fn a_channel_that_dies_is_started_again() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let settings = GatewayConfig {
        workspace: Some(dir.path().join("workspace")),
        ..GatewayConfig::default()
    };
    let resolver = Arc::new(FixedProviderResolver::new(Arc::new(MockProvider::new(
        vec![says("back")],
    ))));
    let fake = FakeChannel::new("fake");
    fake.fail_next_runs(1);
    let gateway = Gateway::new(config, settings, resolver, vec![fake.clone()]).unwrap();
    tokio::spawn(gateway.clone().run());
    // The first run dies at once; the restart comes after a pause, and
    // a message injected meanwhile is delivered by the second run.
    fake.inject("anyone there?", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(1, Duration::from_secs(20)).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].text, "back");
    assert!(fake.runs() >= 2, "{}", fake.runs());
    gateway.cancel();
}

#[tokio::test]
async fn a_soul_file_speaks_for_the_assistant_before_the_coding_instructions() {
    let dir = tempfile::tempdir().unwrap();
    // The terminal agent's configuration: not the assistant's.
    std::fs::create_dir_all(dir.path().join("config/skills/deploy")).unwrap();
    std::fs::write(
        dir.path().join("config/AGENTS.md"),
        "Be terse and commit often.\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("config/skills/deploy/SKILL.md"),
        "---\nname: deploy\ndescription: Deploys to production\n---\nrun the deploy script\n",
    )
    .unwrap();
    // The assistant's home: its own SOUL.md and skills.
    let home = dir.path().join("state/gateway");
    std::fs::create_dir_all(home.join("skills/greet")).unwrap();
    std::fs::write(
        home.join("SOUL.md"),
        "You are Sprocket, warm and a little dry.\n",
    )
    .unwrap();
    std::fs::write(
        home.join("skills/greet/SKILL.md"),
        "---\nname: greet\ndescription: Greets people warmly\n---\nsay hello\n",
    )
    .unwrap();
    let (gateway, fake) = gateway(dir.path(), vec![says("hi")]);
    fake.inject("hello", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    let prompt = gateway.system_prompt("fake:chat-1").unwrap();
    assert!(prompt.contains("You are Sprocket"), "{prompt}");
    assert!(prompt.contains("(from SOUL.md)"), "{prompt}");
    assert!(prompt.contains("Greets people warmly"), "{prompt}");
    assert!(!prompt.contains("commit often"), "{prompt}");
    assert!(!prompt.contains("Deploys to production"), "{prompt}");
    gateway.cancel();
}

#[tokio::test]
async fn slash_new_starts_a_fresh_session_and_slash_model_lists_and_switches() {
    let dir = tempfile::tempdir().unwrap();
    let (gateway, fake) = gateway(dir.path(), vec![says("one"), says("two"), says("three")]);
    let routes_path = dir.path().join("state/gateway/routes.json");
    let session_of = |key: &str| {
        RouteStore::open(routes_path.clone())
            .unwrap()
            .snapshot()
            .session_for(key)
            .map(str::to_string)
    };

    fake.inject("hi", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    let first = session_of("fake:chat-1").expect("a session");

    fake.inject("/new", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, WAIT).await;
    assert!(
        sent[1]
            .text
            .starts_with("Started a fresh chat on zai/glm-4.7."),
        "{sent:?}"
    );
    assert_eq!(session_of("fake:chat-1"), None);
    fake.inject("hi again", "chat-1", "alice").await;
    fake.wait_for_sent(3, WAIT).await;
    let second = session_of("fake:chat-1").expect("a new session");
    assert_ne!(first, second);

    fake.inject("/model", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(4, WAIT).await;
    assert!(
        sent[3]
            .text
            .starts_with("Current: zai/glm-4.7\nDefault for new chats: zai/glm-4.7\n"),
        "{sent:?}"
    );
    assert!(sent[3].text.contains("zai: "), "{}", sent[3].text);
    assert!(sent[3].text.contains("glm-4.5"), "{}", sent[3].text);

    fake.inject("/model nope/none", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(5, WAIT).await;
    assert!(
        sent[4].text.contains("no model nope/none"),
        "{}",
        sent[4].text
    );

    fake.inject("/model zai/glm-4.5", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(6, WAIT).await;
    assert_eq!(sent[5].text, "Switched to zai/glm-4.5.");
    fake.inject("and now?", "chat-1", "alice").await;
    fake.wait_for_sent(7, WAIT).await;
    let store = ilar::runtime::session_store(&config(dir.path()));
    let reader = store.load(&second).unwrap();
    assert_eq!(reader.effective_model(), "zai/glm-4.5");
    let on_new_model = reader.events().iter().any(|event| {
        matches!(event, ilar::session::SessionEvent::AssistantMessage { model, .. } if model == "zai/glm-4.5")
    });
    assert!(on_new_model);

    fake.inject("/help", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(8, WAIT).await;
    assert!(sent[7].text.contains("/new"), "{}", sent[7].text);
    gateway.cancel();
}

#[tokio::test]
async fn a_model_switch_outlives_a_restart_and_save_sets_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let quiet = GatewayConfig {
        announce: false,
        ..GatewayConfig::default()
    };
    let (gateway, fake) = gateway_with(dir.path(), vec![says("one")], quiet.clone());
    fake.inject("hi", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    fake.inject("/model zai/glm-4.5", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, WAIT).await;
    assert_eq!(sent[1].text, "Switched to zai/glm-4.5.");
    gateway.cancel();

    // Started again on the same home, the chat is still on it.
    let (gateway, fake) = gateway_with(dir.path(), vec![says("two")], quiet.clone());
    fake.inject("/model", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(1, WAIT).await;
    assert!(
        sent[0]
            .text
            .starts_with("Current: zai/glm-4.5\nDefault for new chats: zai/glm-4.7\n"),
        "{}",
        sent[0].text
    );
    // Saved, it is what a fresh chat starts on.
    fake.inject("/model zai/glm-4.5 --save", "chat-1", "alice")
        .await;
    let sent = fake.wait_for_sent(2, WAIT).await;
    assert_eq!(
        sent[1].text,
        "Switched to zai/glm-4.5. It is the default for new chats now."
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("state/gateway/model")).unwrap(),
        "zai/glm-4.5\n"
    );
    fake.inject("/new", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(3, WAIT).await;
    assert!(
        sent[2]
            .text
            .starts_with("Started a fresh chat on zai/glm-4.5."),
        "{}",
        sent[2].text
    );
    fake.inject("hello", "chat-1", "alice").await;
    fake.wait_for_sent(4, WAIT).await;
    fake.inject("/model", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(5, WAIT).await;
    assert!(
        sent[4].text.starts_with("Current: zai/glm-4.5\n"),
        "{}",
        sent[4].text
    );
    // `/model --save` alone saves what the chat is on.
    fake.inject("/model zai/glm-4.7", "chat-1", "alice").await;
    fake.wait_for_sent(6, WAIT).await;
    fake.inject("/model --save", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(7, WAIT).await;
    assert_eq!(
        sent[6].text,
        "zai/glm-4.7 is the default for new chats now."
    );
    gateway.cancel();
}

#[tokio::test]
async fn a_status_line_follows_the_turn_and_vanishes_before_the_reply() {
    use ilar_gateway::channel::Seen;
    let dir = tempfile::tempdir().unwrap();
    let settings = GatewayConfig {
        status_interval_secs: 0,
        ..GatewayConfig::default()
    };
    let (gateway, fake) = gateway_with(
        dir.path(),
        vec![
            calls("bash", serde_json::json!({"command": "echo hi"})),
            says("done"),
        ],
        settings,
    );
    fake.inject("run something", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    let seen = fake.seen();
    assert_eq!(
        seen.first(),
        Some(&Seen::StatusPosted("working…".into())),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|s| matches!(s, Seen::StatusEdited(line) if line.starts_with("running bash"))),
        "{seen:?}"
    );
    let cleared = seen
        .iter()
        .position(|s| *s == Seen::StatusCleared)
        .expect("cleared");
    let replied = seen
        .iter()
        .position(|s| *s == Seen::Sent("done".into()))
        .expect("replied");
    assert!(cleared < replied, "{seen:?}");
    assert_eq!(
        seen.iter().filter(|s| **s == Seen::StatusCleared).count(),
        1
    );
    gateway.cancel();
}

const REVIEW_PLAN: &str = "{\"memory\": [{\"file\": \"user\", \"action\": \"add\", \"text\": \
\"Likes earl grey\"}], \"notes\": [{\"kind\": \"preference\", \"title\": \"Tea\", \"summary\": \
\"prefers earl grey\"}]}";

#[tokio::test]
async fn the_review_after_an_idle_episode_keeps_what_was_worth_it() {
    let dir = tempfile::tempdir().unwrap();
    let settings = GatewayConfig {
        review: ilar_gateway::review::ReviewConfig {
            min_tool_calls: 1,
            after_idle_secs: Some(1),
            ..Default::default()
        },
        ..GatewayConfig::default()
    };
    // A turn with a tool call, then the aside answering the review.
    let (gateway, fake) = gateway_with(
        dir.path(),
        vec![
            calls("bash", serde_json::json!({"command": "true"})),
            says("done"),
            says(REVIEW_PLAN),
        ],
        settings,
    );
    fake.inject("run it", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, Duration::from_secs(20)).await;
    let texts: Vec<&str> = sent.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts[0], "done");
    assert!(
        texts[1].starts_with("💾 remembered: user: Likes earl grey"),
        "{texts:?}"
    );
    let user = std::fs::read_to_string(dir.path().join("state/gateway/memory/USER.md")).unwrap();
    assert_eq!(user, "Likes earl grey\n");
    let notes = std::fs::read_dir(dir.path().join("state/gateway/memory/notes")).unwrap();
    assert_eq!(notes.count(), 1);
    gateway.cancel();
}

#[tokio::test]
async fn a_quiet_episode_is_not_reviewed_and_approval_stages_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let settings = GatewayConfig {
        review: ilar_gateway::review::ReviewConfig {
            min_tool_calls: 1,
            after_idle_secs: Some(1),
            approval: true,
            ..Default::default()
        },
        ..GatewayConfig::default()
    };
    let (gateway, fake) = gateway_with(
        dir.path(),
        vec![
            says("just chatting"),
            calls("bash", serde_json::json!({"command": "true"})),
            says("done"),
            says(REVIEW_PLAN),
        ],
        settings,
    );
    // No tool call: nothing to review, nothing arrives.
    fake.inject("hi", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(fake.sent().len(), 1, "{:?}", fake.sent());
    // A tool call: the review runs and, with approval on, stages.
    fake.inject("run it", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(3, Duration::from_secs(20)).await;
    assert!(
        sent[2]
            .text
            .starts_with("📝 I would remember: user: Likes earl grey"),
        "{sent:?}"
    );
    assert!(!dir.path().join("state/gateway/memory/USER.md").exists());
    fake.inject("/pending", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(4, WAIT).await;
    assert!(sent[3].text.contains("Likes earl grey"), "{sent:?}");
    fake.inject("/approve all", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(5, WAIT).await;
    assert!(
        sent[4]
            .text
            .starts_with("💾 remembered: user: Likes earl grey"),
        "{sent:?}"
    );
    let user = std::fs::read_to_string(dir.path().join("state/gateway/memory/USER.md")).unwrap();
    assert_eq!(user, "Likes earl grey\n");
    fake.inject("/pending", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(6, WAIT).await;
    assert_eq!(sent[5].text, "Nothing pending.");
    gateway.cancel();
}

#[tokio::test]
async fn a_skill_the_assistant_writes_is_listed_next_session_and_loads_now() {
    let dir = tempfile::tempdir().unwrap();
    let (gateway, fake) = gateway(
        dir.path(),
        vec![
            calls(
                "skill_manage",
                serde_json::json!({
                    "action": "create", "name": "deploy-check",
                    "description": "Check a deploy is up",
                    "triggers": ["is it up"],
                    "body": "# Deploy check\n\n1. curl the health URL.\n2. Read the journal.",
                }),
            ),
            says("saved it"),
            calls("skill", serde_json::json!({"name": "deploy-check"})),
            says("loaded it"),
            says("fresh"),
        ],
    );
    fake.inject("remember how we check deploys", "chat-1", "alice")
        .await;
    fake.wait_for_sent(1, WAIT).await;
    let path = dir
        .path()
        .join("state/gateway/skills/deploy-check/SKILL.md");
    assert!(path.is_file(), "{}", path.display());
    // Loadable at once through the core's skill tool, and counted.
    fake.inject("use it", "chat-1", "alice").await;
    fake.wait_for_sent(2, WAIT).await;
    assert!(has_tool_result(
        dir.path(),
        "fake:chat-1",
        |content, is_error| { !is_error && content.contains("curl the health URL") }
    ));
    let ledger = ilar_gateway::skills::SkillLibrary::new(dir.path().join("state/gateway/skills"))
        .ledger()
        .unwrap();
    assert_eq!(ledger["deploy-check"].views, 1);
    // Listed in the prompt of the next session.
    assert!(
        !gateway
            .system_prompt("fake:chat-1")
            .unwrap()
            .contains("Check a deploy is up")
    );
    fake.inject("/new", "chat-1", "alice").await;
    fake.wait_for_sent(3, WAIT).await;
    fake.inject("hi", "chat-1", "alice").await;
    fake.wait_for_sent(4, WAIT).await;
    let prompt = gateway.system_prompt("fake:chat-1").unwrap();
    assert!(
        prompt.contains("deploy-check: Check a deploy is up (use when: is it up)"),
        "{prompt}"
    );
    gateway.cancel();
}

#[tokio::test]
async fn the_gateway_announces_its_stop_and_its_start_to_the_last_chat() {
    let dir = tempfile::tempdir().unwrap();
    let announcing = GatewayConfig {
        announce: true,
        ..GatewayConfig::default()
    };
    let (gateway, fake) = gateway_with(dir.path(), vec![says("hi")], announcing.clone());
    // Nothing at start: no chat has written yet.
    fake.inject("hello", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    gateway.cancel();
    let sent = fake.wait_for_sent(2, WAIT).await;
    let texts: Vec<&str> = sent.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["hi", "⏹ ilar-gateway stopping"], "{sent:?}");

    // The next gateway on the same home says hello to that chat.
    let (gateway, fake) = gateway_with(dir.path(), vec![], announcing);
    let sent = fake.wait_for_sent(1, WAIT).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].chat_id, "chat-1");
    assert!(
        sent[0].text.starts_with("▶ ilar-gateway 0.") && sent[0].text.contains("started · model "),
        "{}",
        sent[0].text
    );
    gateway.cancel();
    fake.wait_for_sent(2, WAIT).await;

    // And keeps quiet when told to.
    let quiet = GatewayConfig {
        announce: false,
        ..GatewayConfig::default()
    };
    let (gateway, fake) = gateway_with(dir.path(), vec![], quiet);
    tokio::time::sleep(Duration::from_millis(800)).await;
    gateway.cancel();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(fake.sent().is_empty(), "{:?}", fake.sent());
}

#[tokio::test]
async fn the_weekly_review_is_a_job_the_gateway_owns_and_speaks_to_the_last_chat() {
    let dir = tempfile::tempdir().unwrap();
    let settings = GatewayConfig {
        scheduler_tick_secs: 1,
        weekly: ilar_gateway::weekly::WeeklyConfig {
            // Every second, so the test sees it fire.
            cron: "* * * * * *".into(),
            ..Default::default()
        },
        ..GatewayConfig::default()
    };
    let (gateway, fake) = gateway_with(
        dir.path(),
        vec![
            says("hi"),
            messages("weekly: nothing needed changing", None),
            says("(the job's final text)"),
        ],
        settings,
    );
    // Nobody has written yet: the job is scheduled but skipped.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let jobs = ilar_gateway::cron::CronStore::open(dir.path().join("state/gateway/cron.json"))
        .unwrap()
        .list();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].id, "weekly");
    assert_eq!(jobs[0].target, "last");
    assert!(fake.sent().is_empty());
    // Once a chat has written, the job speaks to it through the tool.
    fake.inject("hello", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, Duration::from_secs(15)).await;
    let texts: Vec<&str> = sent.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(
        &texts[..2],
        ["hi", "weekly: nothing needed changing"],
        "{sent:?}"
    );
    assert!(gateway.tool_names("cron:weekly").is_some());
    gateway.cancel();
}

#[tokio::test]
async fn safe_mode_hides_the_unsafe_tools_from_the_chat_and_its_agents() {
    let dir = tempfile::tempdir().unwrap();
    let settings = GatewayConfig {
        tools: ilar_gateway::policy::ToolPolicy {
            safe_mode: true,
            deny: vec!["task".into()],
            ..Default::default()
        },
        ..GatewayConfig::default()
    };
    let (gateway, fake) = gateway_with(dir.path(), vec![says("hello")], settings);
    fake.inject("hi", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    let names = gateway.tool_names("fake:chat-1").expect("a seat");
    for gone in ["bash", "edit", "write", "task"] {
        assert!(!names.contains(&gone), "{gone} survived: {names:?}");
    }
    for kept in ["read", "grep", "message"] {
        assert!(names.contains(&kept), "{kept} missing: {names:?}");
    }
    // A subagent the chat spawns is under the same policy: the mutable
    // agent's definition was narrowed before the spawner was built.
    let agents = gateway.agent_tools("fake:chat-1").expect("a seat");
    let (_, build) = agents
        .iter()
        .find(|(name, _)| name == "build")
        .expect("the build agent");
    let build = build.as_ref().expect("narrowed to a list");
    assert!(!build.iter().any(|t| t == "bash"), "{build:?}");
    assert!(build.iter().any(|t| t == "read"), "{build:?}");
    gateway.cancel();
}

#[tokio::test]
async fn a_session_held_elsewhere_gets_the_busy_reply() {
    let dir = tempfile::tempdir().unwrap();
    let (gateway, fake) = gateway(dir.path(), vec![says("first"), says("never")]);

    fake.inject("hi", "chat-1", "alice").await;
    fake.wait_for_sent(1, WAIT).await;
    let routes = RouteStore::open(dir.path().join("state/gateway/routes.json"))
        .unwrap()
        .snapshot();
    let session_id = routes.session_for("fake:chat-1").unwrap().to_string();
    let store = ilar::runtime::session_store(&config(dir.path()));
    let _held = store
        .acquire_writer(&session_id)
        .expect("the lease, like a TUI would");

    fake.inject("again", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, WAIT).await;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1].text, BUSY_REPLY);
    gateway.cancel();
}

#[tokio::test]
async fn a_background_child_reports_back_through_the_chat() {
    let dir = tempfile::tempdir().unwrap();
    // The parent delegates and finishes; the child and the parent's
    // continuation both say "ok" (their order is a race the mock
    // cannot see); the child's completion then comes back as a
    // follow-up turn, whose answer reaches the chat.
    let (gateway, fake) = gateway(
        dir.path(),
        vec![
            delegates("look around"),
            says("ok"),
            says("ok"),
            says("follow-up delivered"),
        ],
    );

    fake.inject("survey the place", "chat-1", "alice").await;
    let sent = fake.wait_for_sent(2, WAIT).await;
    let texts: Vec<&str> = sent.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["ok", "follow-up delivered"], "{sent:?}");
    gateway.cancel();
}
