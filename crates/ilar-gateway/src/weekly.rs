//! The weekly review: a scheduled turn that reads the daily notes,
//! promotes what recurs into the core memory, retires what is stale
//! and consolidates overlapping skills — plus the part that needs no
//! model at all, the sweep that moves unused skills aside.
//!
//! Hermes's Curator counts uses and moves skills active → stale →
//! archived without a model, then optionally runs a consolidation
//! pass; OpenClaw's "dreaming" promotes daily notes into the core.
//! Here both are one cron job with a fixed prompt, and the sweep runs
//! right before it so the prompt can name what went stale.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use crate::skills::SkillLibrary;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WeeklyConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When the job runs; five-field cron, UTC. Mondays at four.
    #[serde(default = "default_cron")]
    pub cron: String,
    /// A skill unused this long is stale: listed last, named to the
    /// review.
    #[serde(default = "default_stale_days")]
    pub stale_after_days: i64,
    /// Unused this long, it is moved to `skills/.archive/`.
    #[serde(default = "default_archive_days")]
    pub archive_after_days: i64,
}

impl Default for WeeklyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cron: default_cron(),
            stale_after_days: default_stale_days(),
            archive_after_days: default_archive_days(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_cron() -> String {
    "0 4 * * 1".into()
}

fn default_stale_days() -> i64 {
    30
}

fn default_archive_days() -> i64 {
    90
}

/// The cron job's id and name: one per gateway, kept in step with the
/// configuration at every start.
pub const JOB_ID: &str = "weekly";

/// What the scheduled turn is asked. The sweep's findings are appended.
pub const PROMPT: &str = "Weekly review, as yourself. Read this week's daily notes under memory/daily \
in your home with the read tool, and search the archive with memory_search for anything that \
recurs. Then: promote what recurs or still matters into the core memory with the memory tool \
(add, or replace an entry that is about the same thing; the files are small, so consolidate); \
remove core entries that are no longer true; file as notes what is worth finding later but \
not worth the core; amend notes the week corrected rather than filing second ones about the \
same thing, and forget the ones it disproved; rewrite a note that says \"yesterday\" or \"last \
week\" to name the date, since it will be read months from now; and where two of your skills \
overlap, merge them with skill_manage — patch the one that stays, delete the other. Skills named stale below have not been used in a while: \
delete the ones you would not reach for again. Send one short message with what you changed, \
or that nothing needed changing; do nothing else.";

/// What the sweep did and found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sweep {
    pub archived: Vec<String>,
    pub stale: Vec<String>,
}

impl Sweep {
    /// The lines appended to the prompt.
    pub fn report(&self) -> String {
        let mut lines = Vec::new();
        if !self.stale.is_empty() {
            lines.push(format!("Stale skills: {}.", self.stale.join(", ")));
        }
        if !self.archived.is_empty() {
            lines.push(format!(
                "Archived for disuse (under skills/.archive): {}.",
                self.archived.join(", ")
            ));
        }
        lines.join(" ")
    }
}

/// Move skills unused for `archive_after_days` to `.archive/`, and name
/// the ones unused for `stale_after_days`. "Used" is the latest of
/// created, viewed and patched; a skill the ledger has never seen is
/// left alone, since nothing is known about it.
pub fn sweep(
    library: &SkillLibrary,
    now: DateTime<Utc>,
    config: &WeeklyConfig,
) -> anyhow::Result<Sweep> {
    let ledger = library.ledger()?;
    let mut sweep = Sweep::default();
    for name in library.names()? {
        let Some(usage) = ledger.get(&name) else {
            continue;
        };
        let last = [
            usage.created_at,
            usage.last_viewed_at,
            usage.last_patched_at,
        ]
        .into_iter()
        .flatten()
        .max();
        let Some(last) = last else {
            continue;
        };
        let idle = now - last;
        if idle >= Duration::days(config.archive_after_days) {
            library.archive(&name)?;
            sweep.archived.push(name);
        } else if idle >= Duration::days(config.stale_after_days) {
            sweep.stale.push(name);
        }
    }
    Ok(sweep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_names_the_stale_and_archives_the_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let library = SkillLibrary::new(dir.path().join("skills"));
        for name in ["fresh", "stale", "forgotten", "unknown"] {
            library.create(name, "a skill", &[], "body").unwrap();
        }
        let now = Utc::now();
        // Backdate through the ledger file itself.
        let mut ledger = library.ledger().unwrap();
        ledger.get_mut("stale").unwrap().created_at = Some(now - Duration::days(40));
        ledger.get_mut("forgotten").unwrap().created_at = Some(now - Duration::days(100));
        ledger.remove("unknown");
        crate::routes::write_atomically(
            &dir.path().join("skills/.usage.json"),
            &serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();

        let first = sweep(&library, now, &WeeklyConfig::default()).unwrap();
        assert_eq!(first.stale, ["stale"]);
        assert_eq!(first.archived, ["forgotten"]);
        assert_eq!(library.names().unwrap(), ["fresh", "stale", "unknown"]);
        assert!(
            dir.path()
                .join("skills/.archive/forgotten/SKILL.md")
                .is_file()
        );
        assert!(!library.ledger().unwrap().contains_key("forgotten"));
        assert!(first.report().contains("Stale skills: stale."));
        assert!(first.report().contains("Archived for disuse"));
        assert_eq!(
            sweep(&library, now, &WeeklyConfig::default()).unwrap(),
            Sweep {
                archived: vec![],
                stale: vec!["stale".into()]
            }
        );
    }
}
