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

/// A gateway on a fake channel and a scripted provider, running.
fn gateway(dir: &Path, turns: Vec<Vec<ProviderEvent>>) -> (Arc<Gateway>, Arc<FakeChannel>) {
    let config = config(dir);
    let settings = GatewayConfig {
        workspace: Some(dir.join("workspace")),
        ..GatewayConfig::default()
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
