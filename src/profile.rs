//! Profile lookup and execution.

use crate::config::{Action, Config, Profile};

/// Finds a profile by GUID. Comparison ignores case and surrounding braces
/// so `--profile {GUID}` and `--profile GUID` address the same profile. The
/// normalization lives in `config::guid_key` so lookup and config
/// validation cannot disagree about which strings name the same profile.
pub fn find_profile<'a>(config: &'a Config, id: &str) -> Option<&'a Profile> {
    let key = crate::config::guid_key(id);
    config
        .profiles
        .iter()
        .find(|p| crate::config::guid_key(&p.id) == key)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ActionStatus {
    Succeeded,
    Failed(String),
}

#[derive(Debug)]
pub struct ActionOutcome {
    pub action_id: String,
    pub status: ActionStatus,
}

#[derive(Debug)]
pub struct ProfileResult {
    pub outcomes: Vec<ActionOutcome>,
    pub skipped_disabled: usize,
}

impl ProfileResult {
    /// True when every enabled action succeeded. Skipped disabled actions do
    /// not count as failures.
    pub fn is_success(&self) -> bool {
        self.outcomes
            .iter()
            .all(|o| o.status == ActionStatus::Succeeded)
    }

    pub fn failure_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, ActionStatus::Failed(_)))
            .count()
    }
}

/// Runs the profile's enabled actions in config order.
///
/// A failing action never stops later actions; every result is recorded so
/// the caller can report partial failure. The launcher is injected so this
/// ordering/skipping/aggregation logic is testable without launching real
/// processes — no framework, just a closure.
pub fn execute_profile(
    profile: &Profile,
    launcher: &mut dyn FnMut(&Action) -> Result<(), String>,
) -> ProfileResult {
    let mut result = ProfileResult {
        outcomes: Vec::with_capacity(profile.actions.len()),
        skipped_disabled: 0,
    };
    for action in &profile.actions {
        if action.disabled {
            result.skipped_disabled += 1;
            continue;
        }
        let status = match launcher(action) {
            Ok(()) => ActionStatus::Succeeded,
            Err(e) => ActionStatus::Failed(e),
        };
        result.outcomes.push(ActionOutcome {
            action_id: action.id.clone(),
            status,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ActionKind, CommandShell};
    use std::collections::HashSet;

    fn action(id: &str, disabled: bool, kind: ActionKind) -> Action {
        Action {
            id: id.to_string(),
            disabled,
            kind,
        }
    }

    fn profile(actions: Vec<Action>) -> Profile {
        Profile {
            id: "{11111111-2222-3333-4444-555555555555}".to_string(),
            name: "Test".to_string(),
            actions,
        }
    }

    fn wait(ms: u64) -> ActionKind {
        ActionKind::Wait { milliseconds: ms }
    }

    #[test]
    fn finds_profile_regardless_of_case_and_braces() {
        let config = Config {
            version: 1,
            profiles: vec![profile(vec![])],
        };
        assert!(find_profile(&config, "{11111111-2222-3333-4444-555555555555}").is_some());
        assert!(find_profile(&config, "11111111-2222-3333-4444-555555555555").is_some());
        assert!(find_profile(&config, "aaaaaaaa-2222-3333-4444-555555555555").is_none());
    }

    #[test]
    fn runs_actions_in_order_and_skips_disabled() {
        let p = profile(vec![
            action("a", false, wait(0)),
            action("b", true, wait(0)),
            action("c", false, wait(0)),
        ]);
        let mut calls: Vec<String> = Vec::new();
        let result = execute_profile(&p, &mut |a: &Action| {
            calls.push(a.id.clone());
            Ok(())
        });
        assert_eq!(calls, ["a", "c"]);
        assert_eq!(result.skipped_disabled, 1);
        assert!(result.is_success());
    }

    #[test]
    fn continues_after_failure_and_reports_partial_failure() {
        let p = profile(vec![
            action("a", false, wait(0)),
            action("boom", false, wait(0)),
            action("c", false, wait(0)),
        ]);
        let mut calls: Vec<String> = Vec::new();
        let result = execute_profile(&p, &mut |a: &Action| {
            calls.push(a.id.clone());
            if a.id == "boom" {
                Err("launch failed".to_string())
            } else {
                Ok(())
            }
        });
        assert_eq!(calls, ["a", "boom", "c"]);
        assert_eq!(result.failure_count(), 1);
        assert!(!result.is_success());
        assert_eq!(
            result.outcomes[1].status,
            ActionStatus::Failed("launch failed".to_string())
        );
    }

    #[test]
    fn all_disabled_is_a_noop_success() {
        let p = profile(vec![action("a", true, wait(0))]);
        let mut ran = HashSet::new();
        let result = execute_profile(&p, &mut |a: &Action| {
            ran.insert(a.id.clone());
            Ok(())
        });
        assert!(ran.is_empty());
        assert!(result.is_success());
    }

    #[test]
    fn launcher_sees_the_action_kind() {
        let p = profile(vec![action(
            "cmd1",
            false,
            ActionKind::Command {
                shell: CommandShell::Cmd,
                command: "echo hi".to_string(),
            },
        )]);
        let mut seen_command = false;
        execute_profile(&p, &mut |a: &Action| {
            if let ActionKind::Command { command, .. } = &a.kind {
                seen_command = command == "echo hi";
            }
            Ok(())
        });
        assert!(seen_command);
    }
}
