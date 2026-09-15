//! What the model may run on someone's behalf.
//!
//! ilar's own stance is that the sandbox is the permission system,
//! which is right for a terminal and wrong for a box anyone can
//! message. A policy is applied by building registries without the
//! tools it excludes — the model never sees them — and it reaches the
//! subagents a chat spawns, not only the chat's own turn.

use serde::Deserialize;

/// Tools that change the machine or reach out from it. Safe mode is
/// this list, denied.
pub const UNSAFE_TOOLS: &[&str] = &["bash", "write", "edit", "service", "image_gen", "sudo"];

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    /// Only these tools, when set.
    pub allow: Option<Vec<String>>,
    /// Never these.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Deny [`UNSAFE_TOOLS`] on top of the lists.
    #[serde(default)]
    pub safe_mode: bool,
}

impl ToolPolicy {
    pub fn is_empty(&self) -> bool {
        self.allow.is_none() && self.deny.is_empty() && !self.safe_mode
    }

    /// The names that survive the policy, in the order given.
    pub fn admit<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        names
            .into_iter()
            .filter(|name| self.admits(name))
            .map(str::to_string)
            .collect()
    }

    pub fn admits(&self, name: &str) -> bool {
        if self.deny.iter().any(|denied| denied == name) {
            return false;
        }
        if self.safe_mode && UNSAFE_TOOLS.contains(&name) {
            return false;
        }
        match &self.allow {
            Some(allowed) => allowed.iter().any(|allowed| allowed == name),
            None => true,
        }
    }

    /// An agent definition's own restriction, narrowed by this policy:
    /// what a subagent of a policed chat gets. An unrestricted agent
    /// under any policy becomes an explicit list — `all` is every name
    /// a definition may use — since a spawner builds an unrestricted
    /// agent's registry from nothing but its own defaults.
    pub fn narrow<'a>(
        &self,
        agent_tools: Option<&[String]>,
        all: impl IntoIterator<Item = &'a str>,
    ) -> Option<Vec<String>> {
        match agent_tools {
            None if self.is_empty() => None,
            None => Some(self.admit(all)),
            Some(tools) => Some(self.admit(tools.iter().map(String::as_str))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<&'static str> {
        vec!["read", "grep", "bash", "edit", "write", "task", "message"]
    }

    #[test]
    fn an_empty_policy_admits_everything() {
        let policy = ToolPolicy::default();
        assert!(policy.is_empty());
        assert_eq!(policy.admit(all()), all());
        assert_eq!(policy.narrow(None, all()), None);
    }

    #[test]
    fn safe_mode_drops_the_unsafe_tools_and_deny_beats_allow() {
        let policy = ToolPolicy {
            allow: Some(vec!["read".into(), "bash".into(), "task".into()]),
            deny: vec!["task".into()],
            safe_mode: true,
        };
        assert_eq!(policy.admit(all()), ["read"]);
        assert_eq!(
            policy.narrow(Some(&["read".to_string(), "edit".to_string()]), all()),
            Some(vec!["read".to_string()])
        );
        assert_eq!(policy.narrow(None, all()), Some(vec!["read".to_string()]));
    }

    #[test]
    fn a_deny_turns_an_unrestricted_agent_into_an_explicit_list() {
        let policy = ToolPolicy {
            allow: None,
            deny: vec!["bash".into()],
            safe_mode: false,
        };
        let expected: Vec<String> = ["read", "grep", "edit", "write", "task", "message"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(policy.narrow(None, all()), Some(expected.clone()));
        assert_eq!(policy.admit(all()), expected);
    }
}
