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
        jobs.push(job.clone());
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
    /// tick that dies mid-way does not fire them twice.
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
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(jobs)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
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

#[derive(Deserialize)]
struct Input {
    action: Action,
    name: Option<String>,
    prompt: Option<String>,
    /// One of the three.
    cron: Option<String>,
    every_secs: Option<u64>,
    at: Option<DateTime<Utc>>,
    /// Another chat than this one, as `channel:chat`.
    target: Option<String>,
    id: Option<String>,
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
         every_secs or at (RFC 3339); action list; action remove with id. A scheduled turn \
         reaches the chat only through the message tool."
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
                "name": {"type": "string"},
                "prompt": {"type": "string", "description": "What the scheduled turn is asked"},
                "cron": {"type": "string", "description": "Five-field cron expression, UTC"},
                "every_secs": {"type": "integer"},
                "at": {"type": "string", "description": "RFC 3339 timestamp, once"},
                "target": {"type": "string", "description": "channel:chat (default: this chat)"},
                "id": {"type": "string", "description": "For remove"}
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
                                    "{} {} → {} next {}: {}",
                                    job.id,
                                    job.name,
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
                    let Some(id) = input.id else {
                        return ToolOutput::error("cron: remove needs id");
                    };
                    match store.remove(&id) {
                        Ok(true) => ToolOutput::text(format!("removed {id}")),
                        Ok(false) => ToolOutput::error(format!("cron: no job {id}")),
                        Err(error) => ToolOutput::error(format!("cron: {error:#}")),
                    }
                }
                Action::Add => {
                    let schedule = match (input.cron, input.every_secs, input.at) {
                        (Some(expr), None, None) => Schedule::Cron { expr },
                        (None, Some(secs), None) => Schedule::Every { secs },
                        (None, None, Some(at)) => Schedule::At { at },
                        _ => {
                            return ToolOutput::error(
                                "cron: add needs exactly one of cron, every_secs, at",
                            );
                        }
                    };
                    let (Some(name), Some(prompt)) = (input.name, input.prompt) else {
                        return ToolOutput::error("cron: add needs name and prompt");
                    };
                    let target = input.target.unwrap_or(home);
                    if routes.snapshot().session_for(&target).is_none() {
                        return ToolOutput::error(format!(
                            "cron: no chat {target}; only chats that have written can be targeted"
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
                    };
                    match store.add(job, Utc::now()) {
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
