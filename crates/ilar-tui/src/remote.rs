//! `/remote`: control this session from a phone. The host (iroh, in
//! `ilar-remote`) lives in `run_app`; this module holds what the modal
//! shows and how host events change it.

use crate::app::App;
use crate::web::view;
use crate::{NoticeLevel, theme};
use futures::FutureExt;
use ilar::session::{
    LiveMessage, LiveTail, SessionEvent, SessionStore, SessionTail, TailUpdate, live_path,
};
use ilar_remote::EndpointId;
use ilar_remote::host::{HostEvent, Link, RemoteHost};
use ilar_remote::wire::{Choice, FromDevice, Image, ModelChoice, Prompt, ToDevice};
use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

/// What `/remote <args>` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteCommand {
    /// Start (when off) and show the QR code.
    Open,
    Stop,
}

pub(crate) const USAGE: &str = "usage: /remote (show the QR code) or /remote stop";

pub(crate) fn parse(args: &str) -> Option<RemoteCommand> {
    match args.trim() {
        "" => Some(RemoteCommand::Open),
        "stop" | "off" => Some(RemoteCommand::Stop),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteStatus {
    Starting,
    Waiting,
    Connected(String),
    /// Paired once; the device may come back without the QR code.
    Away(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteView {
    /// `None` until the host is up.
    pub(crate) link: Option<String>,
    pub(crate) status: RemoteStatus,
}

impl RemoteView {
    pub(crate) fn starting() -> Self {
        Self {
            link: None,
            status: RemoteStatus::Starting,
        }
    }

    pub(crate) fn ready(link: String) -> Self {
        Self {
            link: Some(link),
            status: RemoteStatus::Waiting,
        }
    }

    /// Applies a change; returns the notice worth showing, if any.
    pub(crate) fn apply(&mut self, change: Change) -> Option<String> {
        let (status, notice) = match change {
            Change::Paired(id) => (
                RemoteStatus::Connected(short(&id)),
                Some(format!("remote: device {} paired", short(&id))),
            ),
            Change::Back(id) => (RemoteStatus::Connected(short(&id)), None),
            Change::Away(id) => (
                RemoteStatus::Away(short(&id)),
                Some(format!("remote: device {} disconnected", short(&id))),
            ),
            Change::Refused(id) => {
                return Some(format!("remote: refused device {}", short(&id)));
            }
        };
        self.status = status;
        notice
    }

    fn status_line(&self) -> Line<'static> {
        let (text, color) = match &self.status {
            RemoteStatus::Starting => ("starting; reaching the iroh relay…".into(), theme::MUTED),
            RemoteStatus::Waiting => ("waiting for a device to scan".into(), theme::MUTED),
            RemoteStatus::Connected(id) => (format!("connected: device {id}"), theme::RUNNING),
            RemoteStatus::Away(id) => (
                format!("device {id} is away; it can reconnect without the code"),
                theme::MUTED,
            ),
            RemoteStatus::Failed(error) => (format!("failed: {error}"), theme::ERROR),
        };
        Line::styled(text, Style::default().fg(color))
    }
}

/// What happened to the device, as the modal tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    Paired(EndpointId),
    Back(EndpointId),
    Away(EndpointId),
    Refused(EndpointId),
}

/// The host and its plumbing, owned by `run_app`: dropping it (the
/// session ends or switches) stops the host and any start in flight.
#[derive(Default)]
pub(crate) struct RemoteControl {
    host: Option<RemoteHost>,
    starting: Option<JoinHandle<anyhow::Result<RemoteHost>>>,
    events: Option<UnboundedReceiver<HostEvent>>,
    /// The device's link while one is connected.
    link: Option<Link>,
    /// What the device has been told; reopened (with a snapshot) for
    /// every new link.
    feed: Option<Feed>,
}

impl Drop for RemoteControl {
    fn drop(&mut self) {
        self.stop();
    }
}

impl RemoteControl {
    /// Once per frame: takes the request, picks up a finished start and
    /// drains host events into the view.
    pub(crate) fn tick(&mut self, app: &mut App, store: &SessionStore, session_id: &str) {
        match app.remote_requested.take() {
            Some(RemoteCommand::Open) => {
                app.remote_visible = true;
                let failed = matches!(
                    app.remote.as_ref().map(|view| &view.status),
                    Some(RemoteStatus::Failed(_))
                );
                if app.remote.is_none() || failed {
                    self.start();
                    app.remote = Some(RemoteView::starting());
                }
            }
            Some(RemoteCommand::Stop) => {
                let was_on = app
                    .remote
                    .take()
                    .is_some_and(|view| !matches!(view.status, RemoteStatus::Failed(_)));
                app.remote_visible = false;
                self.stop();
                let text = if was_on {
                    "remote control stopped; the QR code is dead"
                } else {
                    "remote control is off"
                };
                app.set_notice(text, NoticeLevel::Info);
            }
            None => {}
        }
        // A finished handle can still say Pending (tokio's coop budget),
        // so it is kept and asked again next frame, never unwrapped.
        if let Some(mut handle) = self.starting.take() {
            match (&mut handle).now_or_never() {
                None => self.starting = Some(handle),
                Some(result) => {
                    let view = app.remote.get_or_insert_with(RemoteView::starting);
                    match result.map_err(anyhow::Error::from).and_then(|r| r) {
                        Ok(host) => {
                            let link = host.ticket().link(ilar_remote::DEFAULT_WEB_BASE);
                            *view = RemoteView::ready(link);
                            self.host = Some(host);
                        }
                        Err(error) => view.status = RemoteStatus::Failed(format!("{error:#}")),
                    }
                }
            }
        }
        while let Some(event) = self.events.as_mut().and_then(|rx| rx.try_recv().ok()) {
            let Some(change) = self.take_event(event) else {
                continue;
            };
            if let Some(view) = app.remote.as_mut()
                && let Some(notice) = view.apply(change)
            {
                app.set_notice(notice, NoticeLevel::Info);
            }
        }
        let Some(link) = &self.link else {
            return;
        };
        let followed = match &mut self.feed {
            Some(feed) => feed.pump(link, app),
            None => Feed::open(link, store, session_id, app).map(|feed| self.feed = Some(feed)),
        };
        // The log cannot be followed: say so and hang up, rather than
        // leave the device looking at a log that silently stopped.
        if let Err(error) = followed {
            let text = format!("cannot follow this session: {error}");
            let _ = link.to_device.send(ToDevice::Notice { text });
            (self.link, self.feed) = (None, None);
            if let Some(view) = app.remote.as_mut()
                && let RemoteStatus::Connected(id) = &view.status
            {
                view.status = RemoteStatus::Away(id.clone());
            }
        }
    }

    /// What the device sent since the last call; `run_app` does them,
    /// because the reply channels and the turn are its own. An image
    /// request is answered here, from the log the feed follows.
    pub(crate) fn commands(&mut self) -> Vec<FromDevice> {
        let Some(link) = self.link.as_mut() else {
            return Vec::new();
        };
        let mut commands = Vec::new();
        while let Ok(command) = link.from_device.try_recv() {
            match command {
                FromDevice::Image { event_id, n } => {
                    let answer = match &self.feed {
                        Some(feed) => feed.image(&event_id, n),
                        None => ToDevice::Image {
                            event_id,
                            n,
                            image: None,
                        },
                    };
                    let _ = link.to_device.send(answer);
                }
                command => commands.push(command),
            }
        }
        commands
    }

    /// Tells the device why its last command did nothing.
    pub(crate) fn notify(&self, text: String) {
        if let Some(link) = &self.link {
            let _ = link.to_device.send(ToDevice::Notice { text });
        }
    }

    /// Keeps the newest link; a disconnect of one already replaced is
    /// old news and changes nothing.
    fn take_event(&mut self, event: HostEvent) -> Option<Change> {
        match event {
            HostEvent::Paired { peer, link } => {
                (self.link, self.feed) = (Some(link), None);
                Some(Change::Paired(peer))
            }
            HostEvent::Welcome { peer, link } => {
                (self.link, self.feed) = (Some(link), None);
                Some(Change::Back(peer))
            }
            HostEvent::Disconnected { peer, serial } => {
                let current = self.link.as_ref().is_some_and(|l| l.serial == serial);
                current.then(|| {
                    (self.link, self.feed) = (None, None);
                    Change::Away(peer)
                })
            }
            HostEvent::Refused(peer) => Some(Change::Refused(peer)),
        }
    }

    fn start(&mut self) {
        self.stop();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.events = Some(rx);
        self.starting = Some(tokio::spawn(RemoteHost::start_public(tx)));
    }

    fn stop(&mut self) {
        if let Some(starting) = self.starting.take() {
            starting.abort();
        }
        self.events = None;
        (self.link, self.feed) = (None, None);
        if let Some(host) = self.host.take() {
            tokio::spawn(host.stop());
        }
    }
}

/// What a paired device is told: the session log's web projection (the
/// same one share and serve use), the running step's deltas from the
/// `.live` scratch, and the TUI's busy state and open prompt.
pub(crate) struct Feed {
    id: String,
    /// The listing row, as serve and share send it.
    session: Value,
    tail: SessionTail,
    live: LiveTail,
    /// Tool call inputs by id, so a result projects with its call.
    inputs: HashMap<String, Value>,
    busy: bool,
    prompt: Option<Prompt>,
    /// The model and thinking level the device was last told.
    model: (String, Option<String>),
}

impl Feed {
    /// Starts following the session and sends the snapshot.
    ///
    /// APPROXIMATION: the snapshot is the whole projected log in one
    /// message, folded on the TUI's thread, at every (re)connect. Serve
    /// pages 200 events at a time; a long session may want the same
    /// (the last N events and a "more" marker). Open question: how long
    /// do real sessions get before this is felt on a phone?
    pub(crate) fn open(
        link: &Link,
        store: &SessionStore,
        session_id: &str,
        app: &App,
    ) -> std::io::Result<Self> {
        let mut tail = SessionTail::open(store, session_id)?;
        // The first poll reads the whole log into the view; the snapshot
        // carries it, so the updates themselves are not sent.
        tail.poll()?;
        let head = store.head(session_id)?;
        let mut feed = Self {
            id: session_id.to_string(),
            session: view::session_summary(&head, true, "idle", Value::Null),
            tail,
            live: LiveTail::open(live_path(&store.session_path(session_id)?)),
            inputs: HashMap::new(),
            busy: app.busy,
            prompt: current_prompt(app),
            model: current_model(app),
        };
        feed.snapshot(link);
        let _ = link.to_device.send(model_menu(app));
        Ok(feed)
    }

    fn snapshot(&mut self, link: &Link) {
        self.live.poll();
        let events = self.tail.events();
        self.inputs.clear();
        view::harvest_call_inputs(events, &mut self.inputs);
        let page = view::page(
            &self.id,
            events,
            self.session.clone(),
            self.busy,
            self.tail.line(),
        );
        let _ = link.to_device.send(ToDevice::Snapshot {
            page,
            live: self
                .live
                .current()
                .iter()
                .map(view::project_live_delta)
                .collect(),
            busy: self.busy,
            prompt: self.prompt.clone(),
        });
    }

    /// Sends what changed since the last call: committed lines first,
    /// then the live step, then busy and the prompt, so the device never
    /// sees "idle" before the turn's last line (serve's order too). An
    /// error means the log can no longer be followed.
    pub(crate) fn pump(&mut self, link: &Link, app: &App) -> std::io::Result<()> {
        let send = |message| {
            let _ = link.to_device.send(message);
        };
        let updates = self.tail.poll()?;
        // A rewind or a rewritten file: resending is simpler than
        // explaining.
        if updates
            .iter()
            .any(|u| matches!(u, TailUpdate::Rewound { .. } | TailUpdate::Resync))
        {
            (self.busy, self.prompt) = (app.busy, current_prompt(app));
            self.snapshot(link);
            return Ok(());
        }
        for update in updates {
            if let TailUpdate::Appended { line, event } = update {
                view::harvest_call_inputs(std::slice::from_ref(&event), &mut self.inputs);
                let event = view::project_event_with(&event, Some(&self.inputs));
                send(ToDevice::Append { line, event });
            }
        }
        for message in self.live.poll() {
            let delta = match message {
                LiveMessage::Reset => view::live_reset(),
                LiveMessage::Delta(delta) => view::project_live_delta(&delta),
            };
            send(ToDevice::Live { delta });
        }
        if self.busy != app.busy {
            self.busy = app.busy;
            send(ToDevice::Busy { busy: self.busy });
        }
        let prompt = current_prompt(app);
        if prompt != self.prompt {
            self.prompt = prompt.clone();
            send(ToDevice::Prompt { prompt });
        }
        let model = current_model(app);
        if model != self.model {
            self.model = model;
            send(model_menu(app));
        }
        Ok(())
    }
}

impl Feed {
    /// Image `n` of event `event_id`, from the log the feed follows.
    pub(crate) fn image(&self, event_id: &str, n: usize) -> ToDevice {
        let image = self.tail.events().iter().find_map(|event| match event {
            SessionEvent::UserMessage { id, images, .. }
            | SessionEvent::ToolResult { id, images, .. }
                if id == event_id =>
            {
                images.get(n)
            }
            _ => None,
        });
        ToDevice::Image {
            event_id: event_id.to_string(),
            n,
            image: image.map(|image| Image {
                media_type: image.media_type.clone(),
                data: image.data.clone(),
            }),
        }
    }
}

fn current_model(app: &App) -> (String, Option<String>) {
    (app.current_model.clone(), app.current_variant.clone())
}

/// The TUI picker's choices: the configured models the catalog can
/// name, each with its thinking levels.
fn model_menu(app: &App) -> ToDevice {
    let models = app
        .available_models
        .iter()
        .filter_map(|id| ilar::model::find(id))
        .map(|model| ModelChoice {
            id: model.full_id(),
            name: model.name.to_string(),
            vision: model.supports_vision(),
            variants: model
                .variants()
                .iter()
                .map(|variant| Choice {
                    id: variant.id.to_string(),
                    name: variant.name.to_string(),
                })
                .collect(),
        })
        .collect();
    ToDevice::Models {
        current: app.current_model.clone(),
        variant: app.current_variant.clone(),
        models,
    }
}

/// What the session waits on, in the order the modals stack.
fn current_prompt(app: &App) -> Option<Prompt> {
    if let Some(modal) = &app.question_modal {
        Some(modal.remote_prompt())
    } else if let Some(modal) = &app.grant_modal {
        Some(modal.remote_prompt())
    } else {
        app.password_modal.as_ref().map(|m| m.remote_prompt())
    }
}

fn short(id: &ilar_remote::EndpointId) -> String {
    id.to_string().chars().take(8).collect()
}

/// The QR code as rows of half blocks, two modules per character cell,
/// black on white in any theme so a phone can read it.
pub(crate) fn qr_lines(text: &str) -> Vec<Line<'static>> {
    // Level L: a screen does not get scratched, and the smaller code
    // fits more terminals.
    let Ok(code) = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::L) else {
        return vec![Line::raw("(the link is too long for a QR code)")];
    };
    let width = code.width();
    let quiet = 2;
    let dark = |x: isize, y: isize| {
        (0..width as isize).contains(&x)
            && (0..width as isize).contains(&y)
            && code[(x as usize, y as usize)] == qrcode::Color::Dark
    };
    let side = (width + 2 * quiet) as isize;
    (0..side)
        .step_by(2)
        .map(|row| {
            let cells: String = (0..side)
                .map(|col| {
                    let (x, y) = (col - quiet as isize, row - quiet as isize);
                    match (dark(x, y), dark(x, y + 1)) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    }
                })
                .collect();
            Line::from(Span::styled(
                cells,
                Style::default().fg(Color::Black).bg(Color::White),
            ))
        })
        .collect()
}

/// `text` cut into rows of `width` characters. The link has no spaces
/// to wrap at, and a word wrapper would also reflow the QR rows.
fn rows(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width.max(1))
        .map(|row| Line::styled(row.iter().collect::<String>(), style))
        .collect()
}

/// Status first, then the code, then the link. The code is drawn whole
/// or not at all: a cut code does not scan, and the link still works.
pub(crate) fn render(frame: &mut Frame, view: &RemoteView) {
    let qr = view.link.as_deref().map(qr_lines).unwrap_or_default();
    let qr_width = qr.first().map_or(0, |line| line.width());
    let width = qr_width.max(52);
    let link_rows = view
        .link
        .as_ref()
        .map_or(0, |link| link.chars().count().div_ceil(width));
    let wanted = 2 + qr.len() + link_rows + 2;
    let area = crate::modals::centered_rect(frame.area(), width as u16 + 2, wanted as u16);
    let Some(inner) = crate::modals::modal_frame(
        frame,
        area,
        " remote ",
        theme::MARKUP,
        " Esc close · /remote stop ends it ",
    ) else {
        return;
    };
    let width = inner.width as usize;
    let status = view.status_line();
    let mut lines = rows(&status.to_string(), width, status.style);
    let link = view.link.as_deref().map_or_else(Vec::new, |link| {
        rows(link, width, Style::default().fg(theme::MUTED))
    });
    if !qr.is_empty() {
        lines.push(Line::raw(""));
        if qr_width <= width && lines.len() + qr.len() + link.len() <= inner.height as usize {
            lines.extend(qr);
        } else {
            lines.push(Line::styled(
                "enlarge the terminal to show the QR code",
                Style::default().fg(theme::ERROR),
            ));
        }
    }
    lines.extend(link);
    frame.render_widget(ratatui::widgets::Paragraph::new(lines).centered(), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> ilar_remote::EndpointId {
        ilar_remote::SecretKey::generate().public()
    }

    #[test]
    fn the_command_takes_nothing_or_stop() {
        assert_eq!(parse(""), Some(RemoteCommand::Open));
        assert_eq!(parse(" stop "), Some(RemoteCommand::Stop));
        assert_eq!(parse("now"), None);
    }

    #[test]
    fn pairing_connects_and_a_drop_leaves_the_device_away() {
        let mut view = RemoteView::ready("https://x/#t".into());
        let phone = device();
        assert!(view.apply(Change::Paired(phone)).is_some());
        assert_eq!(view.status, RemoteStatus::Connected(short(&phone)));
        view.apply(Change::Away(phone));
        assert_eq!(view.status, RemoteStatus::Away(short(&phone)));
        assert_eq!(view.apply(Change::Back(phone)), None);
        assert_eq!(view.status, RemoteStatus::Connected(short(&phone)));
    }

    #[test]
    fn a_refused_device_is_a_notice_not_a_status() {
        let mut view = RemoteView::ready("https://x/#t".into());
        assert!(view.apply(Change::Refused(device())).is_some());
        assert_eq!(view.status, RemoteStatus::Waiting);
    }

    fn link(serial: u64) -> Link {
        linked(serial).0
    }

    /// A link and the device's end of it.
    fn linked(serial: u64) -> (Link, UnboundedReceiver<ToDevice>) {
        let (to_device, device) = tokio::sync::mpsc::unbounded_channel();
        let (_, from_device) = tokio::sync::mpsc::unbounded_channel();
        let link = Link {
            serial,
            to_device,
            from_device,
        };
        (link, device)
    }

    fn user(text: &str) -> ilar::session::SessionEvent {
        ilar::session::SessionEvent::UserMessage {
            id: ilar::session::new_id(),
            text: text.into(),
            images: Vec::new(),
            ts: chrono::Utc::now(),
        }
    }

    fn drain(device: &mut UnboundedReceiver<ToDevice>) -> Vec<ToDevice> {
        std::iter::from_fn(|| device.try_recv().ok()).collect()
    }

    #[test]
    fn the_device_gets_a_snapshot_then_what_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = ilar::session::new_id();
        let mut session = store
            .create(ilar::session::SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session.append(user("first")).unwrap();
        let (link, mut device) = linked(0);
        let mut app = App::new();

        let mut feed = Feed::open(&link, &store, &id, &app).unwrap();
        let [
            ToDevice::Snapshot { page, busy, .. },
            ToDevice::Models { .. },
        ] = &drain(&mut device)[..]
        else {
            panic!("the snapshot first, then the model menu");
        };
        assert!(!busy);
        assert!(page["events"].to_string().contains("first"), "{page}");
        assert_eq!(page["line"], 2, "meta and one message: {page}");

        session.append(user("second")).unwrap();
        app.busy = true;
        feed.pump(&link, &app).unwrap();
        let sent = drain(&mut device);
        assert!(sent.contains(&ToDevice::Busy { busy: true }), "{sent:?}");
        assert!(
            sent.iter().any(
                |m| matches!(m, ToDevice::Append { line: 3, event } if event.to_string().contains("second"))
            ),
            "{sent:?}"
        );
        feed.pump(&link, &app).unwrap();
        assert!(drain(&mut device).is_empty(), "nothing new, nothing sent");
    }

    #[test]
    fn the_feed_answers_an_image_by_its_event_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = ilar::session::new_id();
        let mut session = store
            .create(ilar::session::SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        let event_id = ilar::session::new_id();
        session
            .append(ilar::session::SessionEvent::UserMessage {
                id: event_id.clone(),
                text: "look".into(),
                images: vec![ilar::session::ImageContent {
                    media_type: "image/png".into(),
                    data: "iVBORw0K".into(),
                }],
                ts: chrono::Utc::now(),
            })
            .unwrap();
        let (link, _device) = linked(0);
        let feed = Feed::open(&link, &store, &id, &App::new()).unwrap();

        let ToDevice::Image {
            image: Some(image), ..
        } = feed.image(&event_id, 0)
        else {
            panic!("the image is there");
        };
        assert_eq!(
            (image.media_type.as_str(), image.data.as_str()),
            ("image/png", "iVBORw0K")
        );
        assert!(matches!(
            feed.image(&event_id, 1),
            ToDevice::Image {
                image: None,
                n: 1,
                ..
            }
        ));
        assert!(matches!(
            feed.image("nowhere", 0),
            ToDevice::Image { image: None, .. }
        ));
    }

    #[test]
    fn the_model_menu_names_the_choices_and_follows_a_switch() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let id = ilar::session::new_id();
        store
            .create(ilar::session::SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        let (link, mut device) = linked(0);
        let mut app = App::new();
        let with_levels = ilar::model::catalog()
            .iter()
            .find(|model| !model.variants().is_empty())
            .expect("a catalog model with thinking levels");
        app.current_model = with_levels.full_id();
        app.available_models = vec![with_levels.full_id(), "nowhere/unknown".into()];

        let mut feed = Feed::open(&link, &store, &id, &app).unwrap();
        let sent = drain(&mut device);
        let Some(ToDevice::Models {
            current,
            variant,
            models,
        }) = sent.last()
        else {
            panic!("{sent:?}");
        };
        assert_eq!(current, &with_levels.full_id());
        assert_eq!(variant, &None);
        assert_eq!(
            models.len(),
            1,
            "a model the catalog cannot name is left out"
        );
        assert_eq!(models[0].variants.len(), with_levels.variants().len());

        let level = with_levels.variants()[0].id.to_string();
        app.current_variant = Some(level.clone());
        feed.pump(&link, &app).unwrap();
        assert!(
            matches!(&drain(&mut device)[..], [ToDevice::Models { variant: Some(v), .. }] if *v == level)
        );
        feed.pump(&link, &app).unwrap();
        assert!(drain(&mut device).is_empty(), "no change, no menu");
    }

    #[test]
    fn the_replaced_link_going_away_is_old_news() {
        let mut control = RemoteControl::default();
        let phone = device();
        let paired = HostEvent::Paired {
            peer: phone,
            link: link(0),
        };
        assert_eq!(control.take_event(paired), Some(Change::Paired(phone)));
        let back = HostEvent::Welcome {
            peer: phone,
            link: link(1),
        };
        assert_eq!(control.take_event(back), Some(Change::Back(phone)));
        let stale = HostEvent::Disconnected {
            peer: phone,
            serial: 0,
        };
        assert_eq!(control.take_event(stale), None);
        let current = HostEvent::Disconnected {
            peer: phone,
            serial: 1,
        };
        assert_eq!(control.take_event(current), Some(Change::Away(phone)));
        assert!(control.link.is_none());
    }

    fn screen(view: &RemoteView, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, view)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    fn real_link() -> String {
        let ticket = ilar_remote::Ticket {
            id: device(),
            relay: Some("https://euc1-1.relay.n0.iroh.link./".parse().unwrap()),
            secret: ilar_remote::Secret::generate(),
        };
        ticket.link(ilar_remote::DEFAULT_WEB_BASE)
    }

    /// The modal's text with its rows joined, so a link cut into rows
    /// reads whole again.
    fn joined(text: &str) -> String {
        text.lines()
            .map(|line| line.trim_matches(|c| c == ' ' || c == '║'))
            .collect()
    }

    #[test]
    fn a_large_terminal_shows_the_code_the_status_and_the_link() {
        let link = real_link();
        let text = screen(&RemoteView::ready(link.clone()), 132, 40);
        assert!(text.contains("waiting for a device"), "{text}");
        assert!(text.contains('█') && !text.contains("enlarge"), "{text}");
        assert!(joined(&text).contains(&link), "{text}");
    }

    #[test]
    fn a_small_terminal_asks_for_room_and_still_shows_the_link() {
        let link = real_link();
        let text = screen(&RemoteView::ready(link.clone()), 80, 24);
        assert!(text.contains("waiting for a device"), "{text}");
        assert!(text.contains("enlarge the terminal"), "{text}");
        assert!(!text.contains('█'), "a cut QR code: {text}");
        assert!(joined(&text).contains(&link), "{text}");
    }

    #[test]
    fn the_qr_code_is_square_with_a_quiet_zone() {
        let lines = qr_lines("https://lambadalambda.github.io/ilar/#AQIDBA");
        let width = lines[0].width();
        assert_eq!(lines.len(), width.div_ceil(2));
        let top: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(top.chars().all(|c| c == ' '), "{top:?}");
    }
}
