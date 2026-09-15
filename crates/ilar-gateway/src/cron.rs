//! Scheduled turns: a job store, its tool, and the schedule arithmetic.
//!
//! A job is a prompt run on its own session (`cron:<id>`) at a time,
//! addressed to a chat. It reaches that chat only through the message
//! tool: a scheduled turn's final text goes nowhere, so a job with
//! nothing to say says nothing.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use ilar::tools::{
    Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess, parse_input,
};
use serde::{Deserialize, Serialize};

use crate::routes::RouteStore;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// A cron expression: five fields as everyone writes them, or the
    /// six with seconds the `cron` crate speaks.
    Cron {
        expr: String,
    },
    Every {
        secs: u64,
    },
    /// Once.
    At {
        at: DateTime<Utc>,
    },
}

impl Schedule {
    /// The first firing strictly after `now`; `None` once a one-shot
    /// has passed.
    pub fn next_after(&self, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        Ok(match self {
            Self::Cron { expr } => {
                let expr = if expr.split_whitespace().count() == 5 {
                    format!("0 {expr}")
                } else {
                    expr.clone()
                };
                let schedule = cron::Schedule::from_str(&expr)
                    .with_context(|| format!("cron expression {expr:?}"))?;
                schedule.after(&now).next()
            }
            Self::Every { secs } => {
                if *secs == 0 {
                    bail!("every: secs must be at least 1");
                }
                Some(now + chrono::Duration::seconds(*secs as i64))
            }
            Self::At { at } => (*at > now).then_some(*at),
        })
    }
}

/// The target that means "whichever chat was last heard from", for
/// jobs the gateway owns; a job the model adds is always addressed.
pub const LAST_ACTIVE: &str = "last";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub schedule: Schedule,
    pub prompt: String,
    /// The chat the turn speaks to: a session key.
    pub target: String,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
    /// How often this job has been rescheduled after a failed run. A
    /// one-shot that never fired gets one more go, not a loop.
    #[serde(default)]
    pub retries: u32,
}

impl Job {
    pub fn session_key(&self) -> String {
        format!("cron:{}", self.id)
    }
}

/// The jobs on disk, rewritten whole through a rename.
pub struct CronStore {
    path: PathBuf,
    jobs: Mutex<Vec<Job>>,
}

impl CronStore {
    pub fn open(path: PathBuf) -> Result<Self> {
        let jobs = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing jobs {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error).context("reading jobs"),
        };
        Ok(Self {
            path,
            jobs: Mutex::new(jobs),
        })
    }

    pub fn list(&self) -> Vec<Job> {
        self.jobs.lock().unwrap().clone()
    }

    pub fn add(&self, mut job: Job, now: DateTime<Utc>) -> Result<Job> {
        job.next_run = job.schedule.next_after(now)?;
        if job.next_run.is_none() {
            bail!("the schedule never fires");
        }
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.iter().any(|existing| existing.id == job.id) {
            bail!("job id {} is taken", job.id);
        }
        jobs.push(job.clone());
        self.save(&jobs)?;
        Ok(job)
    }

    /// A job the gateway owns, kept in step with configuration: added
    /// when absent, updated in place when present, so a changed
    /// schedule or prompt takes effect at the next start.
    pub fn upsert(&self, mut job: Job, now: DateTime<Utc>) -> Result<Job> {
        job.next_run = job.schedule.next_after(now)?;
        if job.next_run.is_none() {
            bail!("the schedule never fires");
        }
        let mut jobs = self.jobs.lock().unwrap();
        match jobs.iter_mut().find(|existing| existing.id == job.id) {
            Some(existing) => {
                job.last_run = existing.last_run;
                *existing = job.clone();
            }
            None => jobs.push(job.clone()),
        }
        self.save(&jobs)?;
        Ok(job)
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        let mut jobs = self.jobs.lock().unwrap();
        let before = jobs.len();
        jobs.retain(|job| job.id != id);
        let removed = jobs.len() < before;
        if removed {
            self.save(&jobs)?;
        }
        Ok(removed)
    }

    /// Jobs whose time has come, each advanced to its next firing — or
    /// retired when there is none — before they are handed back, so a
    /// tick that dies mid-way does not fire them twice. A job past due
    /// at start fires once, not once per missed slot; `every` counts
    /// from the tick that fired it, so it drifts by tick granularity.
    pub fn take_due(&self, now: DateTime<Utc>) -> Result<Vec<Job>> {
        let mut jobs = self.jobs.lock().unwrap();
        let mut due = Vec::new();
        for job in jobs.iter_mut() {
            if job.next_run.is_some_and(|at| at <= now) {
                due.push(job.clone());
                job.last_run = Some(now);
                job.next_run = job.schedule.next_after(now)?;
            }
        }
        jobs.retain(|job| job.next_run.is_some());
        if !due.is_empty() {
            self.save(&jobs)?;
        }
        Ok(due)
    }

    fn save(&self, jobs: &[Job]) -> Result<()> {
        crate::routes::write_atomically(&self.path, &serde_json::to_vec_pretty(jobs)?)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Add,
    List,
    Remove,
}

/// Unknown fields are refused: a `channel`/`chat` pair meant for the
/// message tool would otherwise schedule for the home chat unnoticed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    action: Action,
    name: Option<String>,
    prompt: Option<String>,
    /// One of the three.
    cron: Option<String>,
    every_secs: Option<u64>,
    at: Option<DateTime<Utc>>,
    /// Another chat than this one, as `channel:chat`; a bare id means
    /// this channel.
    target: Option<String>,
    id: Option<String>,
}

/// The least interval: the scheduler ticks every thirty seconds, so
/// anything shorter would fire on every tick.
const MIN_EVERY_SECS: u64 = 60;
/// A year: beyond it an interval is a mistake, not a plan.
const MAX_EVERY_SECS: u64 = 366 * 24 * 3600;

/// The chat a job speaks to: this one unless named; a bare id is on
/// this chat's channel.
fn resolve_target(target: Option<&str>, home: &str) -> String {
    match target.map(str::trim).filter(|t| !t.is_empty()) {
        None => home.to_string(),
        Some(target) if target.contains(':') => target.to_string(),
        Some(id) => {
            let channel = home.split_once(':').map(|(c, _)| c).unwrap_or(home);
            format!("{channel}:{id}")
        }
    }
}

/// How a schedule reads in a listing.
fn describe(schedule: &Schedule) -> String {
    match schedule {
        Schedule::Cron { expr } => format!("cron {expr}"),
        Schedule::Every { secs } => format!("every {secs}s"),
        Schedule::At { at } => format!("at {}", at.to_rfc3339()),
    }
}

/// The `cron` tool: the model schedules its own reminders and checks.
pub struct CronTool {
    store: Arc<CronStore>,
    routes: Arc<RouteStore>,
    home: String,
}

impl CronTool {
    pub fn new(store: Arc<CronStore>, routes: Arc<RouteStore>, home: &str) -> Arc<Self> {
        Arc::new(Self {
            store,
            routes,
            home: home.to_string(),
        })
    }
}

impl Tool for CronTool {
    fn name(&self) -> &'static str {
        "cron"
    }

    fn description(&self) -> &'static str {
        "Schedule a prompt to run later on its own session, addressed to this chat (or a \
         known one): action add with name, prompt and one of cron (five-field expression), \
         every_secs or at (RFC 3339); action list; action remove with id, or a unique name. A \
         scheduled turn reaches the chat only through the message tool."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Barrier
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "remove"]},
                "name": {"type": "string", "description": "A short label for the job"},
                "prompt": {"type": "string", "description": "What the scheduled turn is asked"},
                "cron": {"type": "string", "description": "Five-field cron expression, in UTC"},
                "every_secs": {"type": "integer", "description": "Seconds between runs, at least 60"},
                "at": {"type": "string", "description": "Once, at an RFC 3339 time such as 2026-09-10T15:00:00Z; UTC unless an offset is given"},
                "target": {"type": "string", "description": "channel:chat of another chat that has written (default: this chat); a bare id means this channel"},
                "id": {"type": "string", "description": "For remove: the id from add or list; a unique name also works"}
            },
            "required": ["action"]
        })
    }

    fn run(&self, input: serde_json::Value, _ctx: ToolContext) -> ToolFuture {
        let store = self.store.clone();
        let routes = self.routes.clone();
        let home = self.home.clone();
        Box::pin(async move {
            let input: Input = match parse_input(input, "cron") {
                Ok(input) => input,
                Err(error) => return error,
            };
            match input.action {
                Action::List => {
                    let jobs = store.list();
                    if jobs.is_empty() {
                        return ToolOutput::text("(no jobs)");
                    }
                    ToolOutput::text(
                        jobs.iter()
                            .map(|job| {
                                format!(
                                    "{} {} ({}) → {} next {}: {}",
                                    job.id,
                                    job.name,
                                    describe(&job.schedule),
                                    job.target,
                                    job.next_run
                                        .map(|at| at.to_rfc3339())
                                        .unwrap_or_else(|| "never".into()),
                                    job.prompt
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                }
                Action::Remove => {
                    let Some(wanted) = input.id.or(input.name) else {
                        return ToolOutput::error("cron: remove needs id (or a unique name)");
                    };
                    let jobs = store.list();
                    let by_name: Vec<&Job> = jobs.iter().filter(|job| job.name == wanted).collect();
                    let id = match jobs.iter().find(|job| job.id == wanted) {
                        Some(job) => job.id.clone(),
                        None if by_name.len() == 1 => by_name[0].id.clone(),
                        None if by_name.len() > 1 => {
                            return ToolOutput::error(format!(
                                "cron: {} jobs are named {wanted:?}; remove by id: {}",
                                by_name.len(),
                                by_name
                                    .iter()
                                    .map(|job| job.id.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ));
                        }
                        None => {
                            let listed = jobs
                                .iter()
                                .map(|job| format!("{} {}", job.id, job.name))
                                .collect::<Vec<_>>()
                                .join(", ");
                            return ToolOutput::error(format!(
                                "cron: no job {wanted:?}; jobs: {}",
                                if listed.is_empty() {
                                    "(none)".to_string()
                                } else {
                                    listed
                                }
                            ));
                        }
                    };
                    match store.remove(&id) {
                        Ok(_) => ToolOutput::text(format!("removed {id}")),
                        Err(error) => ToolOutput::error(format!("cron: {error:#}")),
                    }
                }
                Action::Add => {
                    let now = Utc::now();
                    let schedule = match (input.cron, input.every_secs, input.at) {
                        (Some(expr), None, None) => Schedule::Cron { expr },
                        (None, Some(secs), None) if secs < MIN_EVERY_SECS => {
                            return ToolOutput::error(format!(
                                "cron: every_secs is at least {MIN_EVERY_SECS}; for something \
                                 once and soon, use at"
                            ));
                        }
                        (None, Some(secs), None) if secs > MAX_EVERY_SECS => {
                            return ToolOutput::error(format!(
                                "cron: every_secs {secs} is over a year; the most is {MAX_EVERY_SECS}"
                            ));
                        }
                        (None, Some(secs), None) => Schedule::Every { secs },
                        (None, None, Some(at)) if at <= now => {
                            return ToolOutput::error(format!(
                                "cron: at {} is in the past; now is {}",
                                at.to_rfc3339(),
                                now.to_rfc3339()
                            ));
                        }
                        (None, None, Some(at)) => Schedule::At { at },
                        _ => {
                            return ToolOutput::error(
                                "cron: add needs exactly one of cron, every_secs, at",
                            );
                        }
                    };
                    let (Some(name), Some(prompt)) = (
                        input.name.filter(|n| !n.trim().is_empty()),
                        input.prompt.filter(|p| !p.trim().is_empty()),
                    ) else {
                        return ToolOutput::error(
                            "cron: add needs a name and a prompt, neither empty",
                        );
                    };
                    let target = resolve_target(input.target.as_deref(), &home);
                    let routes = routes.snapshot();
                    if routes.session_for(&target).is_none() {
                        return ToolOutput::error(format!(
                            "cron: no chat {target}; target is channel:chat, and only chats that \
                             have written can be targeted. Known: {}",
                            routes.known_chats()
                        ));
                    }
                    let job = Job {
                        id: ilar::session::new_id()[..8].to_string(),
                        name,
                        schedule,
                        prompt,
                        target,
                        next_run: None,
                        last_run: None,
                        retries: 0,
                    };
                    match store.add(job, now) {
                        Ok(job) => ToolOutput::text(format!(
                            "scheduled {} ({}), next {}",
                            job.name,
                            job.id,
                            job.next_run.map(|at| at.to_rfc3339()).unwrap_or_default()
                        )),
                        Err(error) => ToolOutput::error(format!("cron: {error:#}")),
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ilar::tools::Tool;

    #[test]
    fn a_target_is_a_key_or_a_bare_id_on_this_channel() {
        assert_eq!(resolve_target(None, "fake:12"), "fake:12");
        assert_eq!(resolve_target(Some(" "), "fake:12"), "fake:12");
        assert_eq!(resolve_target(Some("15"), "fake:12"), "fake:15");
        assert_eq!(resolve_target(Some("other:3"), "fake:12"), "other:3");
    }

    #[tokio::test]
    async fn the_tool_refuses_what_cannot_be_meant_and_says_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(CronStore::open(dir.path().join("cron.json")).unwrap());
        let routes = Arc::new(RouteStore::open(dir.path().join("routes.json")).unwrap());
        routes
            .update(|routes| {
                routes.bind("fake:12", "s1");
                routes.bind("fake:15", "s2");
            })
            .unwrap();
        let tool = CronTool::new(store.clone(), routes, "fake:12");
        let ctx = || ToolContext::root(dir.path().to_path_buf());
        let run = |input: serde_json::Value| tool.run(input, ctx());

        let out = run(serde_json::json!({"action": "add", "name": "n", "prompt": "p", "every_secs": 60, "channel": "fake", "chat": "15"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown field"), "{}", out.content);

        let out =
            run(serde_json::json!({"action": "add", "name": "n", "prompt": "p", "every_secs": 5}))
                .await;
        assert!(out.content.contains("at least 60"), "{}", out.content);

        let out = run(serde_json::json!({"action": "add", "name": "n", "prompt": "p", "at": "2020-01-01T00:00:00Z"})).await;
        assert!(
            out.content.contains("in the past; now is"),
            "{}",
            out.content
        );

        let out =
            run(serde_json::json!({"action": "add", "name": " ", "prompt": "p", "every_secs": 60}))
                .await;
        assert!(out.content.contains("neither empty"), "{}", out.content);

        let out = run(serde_json::json!({"action": "add", "name": "n", "prompt": "p", "every_secs": 60, "target": "99"})).await;
        assert!(
            out.content.contains("no chat fake:99")
                && out.content.contains("Known: fake:12, fake:15"),
            "{}",
            out.content
        );

        let out = run(serde_json::json!({"action": "add", "name": "hourly", "prompt": "p", "every_secs": 3600, "target": "15"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(store.list()[0].target, "fake:15");

        let out = run(serde_json::json!({"action": "list"})).await;
        assert!(
            out.content.contains("hourly (every 3600s) → fake:15"),
            "{}",
            out.content
        );

        let out = run(serde_json::json!({"action": "remove", "id": "nope"})).await;
        assert!(
            out.content.contains("jobs: ") && out.content.contains("hourly"),
            "{}",
            out.content
        );
        let out = run(serde_json::json!({"action": "remove", "name": "hourly"})).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(store.list().is_empty());
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn schedules_find_their_next_firing() {
        let now = at("2026-09-08T12:00:00Z");
        let daily = Schedule::Cron {
            expr: "30 9 * * *".into(),
        };
        assert_eq!(
            daily.next_after(now).unwrap(),
            Some(at("2026-09-09T09:30:00Z"))
        );
        let every = Schedule::Every { secs: 90 };
        assert_eq!(
            every.next_after(now).unwrap(),
            Some(at("2026-09-08T12:01:30Z"))
        );
        let once = Schedule::At {
            at: at("2026-09-08T12:00:01Z"),
        };
        assert_eq!(
            once.next_after(now).unwrap(),
            Some(at("2026-09-08T12:00:01Z"))
        );
        assert_eq!(once.next_after(at("2026-09-08T12:00:01Z")).unwrap(), None);
        assert!(
            Schedule::Cron {
                expr: "nope".into()
            }
            .next_after(now)
            .is_err()
        );
    }

    #[test]
    fn due_jobs_are_taken_once_and_one_shots_retire() {
        let dir = tempfile::tempdir().unwrap();
        let store = CronStore::open(dir.path().join("cron.json")).unwrap();
        let now = at("2026-09-08T12:00:00Z");
        let job = |id: &str, schedule| Job {
            id: id.into(),
            name: id.into(),
            schedule,
            prompt: "p".into(),
            target: "fake:1".into(),
            next_run: None,
            last_run: None,
            retries: 0,
        };
        store
            .add(
                job(
                    "once",
                    Schedule::At {
                        at: at("2026-09-08T12:00:05Z"),
                    },
                ),
                now,
            )
            .unwrap();
        store
            .add(job("often", Schedule::Every { secs: 10 }), now)
            .unwrap();
        assert!(
            store
                .take_due(at("2026-09-08T12:00:04Z"))
                .unwrap()
                .is_empty()
        );
        let due = store.take_due(at("2026-09-08T12:00:10Z")).unwrap();
        let ids: Vec<&str> = due.iter().map(|j| j.id.as_str()).collect();
        assert_eq!(ids, ["once", "often"]);
        // Taken means advanced: nothing is due again at the same instant,
        // and the one-shot is gone.
        assert!(
            store
                .take_due(at("2026-09-08T12:00:10Z"))
                .unwrap()
                .is_empty()
        );
        let left = CronStore::open(dir.path().join("cron.json"))
            .unwrap()
            .list();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "often");
        assert_eq!(left[0].next_run, Some(at("2026-09-08T12:00:20Z")));
    }
}
