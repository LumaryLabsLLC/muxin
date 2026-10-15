//! One-click things to ask a session's agent: commit, open a pull request,
//! review, fix the review's comments, ... Each is a prompt typed into the
//! agent. The user can change any of them in Settings (`[settings.prompts]`
//! in config.toml); `{branch}`, `{base}` and `{pull}` are filled in from the
//! session's git (see git.rs).

use std::sync::Arc;

use super::Control;
use super::git::GitStatus;
use crate::fleet::Engine;

pub struct SessionAction {
    pub id: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    /// Muxin's own prompt.
    pub prompt: &'static str,
    /// What it needs to make sense.
    pub needs: Needs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    Nothing,
    /// A git checkout.
    Git,
    /// A branch of its own, not the default one.
    Branch,
    /// An open pull request for the branch.
    Pull,
}

pub const ACTIONS: &[SessionAction] = &[
    SessionAction {
        id: "commit",
        label: "Commit",
        icon: "circle-dot",
        prompt: "Commit the changes you made, with a clear message that says what changed and why. Don't push yet.",
        needs: Needs::Git,
    },
    SessionAction {
        id: "commit_push",
        label: "Commit and push",
        icon: "arrow-up",
        prompt: "Commit the changes you made with a clear message, then push the branch.",
        needs: Needs::Git,
    },
    SessionAction {
        id: "pull_request",
        label: "Open a pull request",
        icon: "git-pull-request",
        prompt: "Push this branch ({branch}) and open a pull request for it with the GitHub CLI (gh pr create). Write a title and a description that say what changed and why. If it has a pull request already, update that one's description instead.",
        needs: Needs::Branch,
    },
    SessionAction {
        id: "review",
        label: "Review the changes",
        icon: "search",
        prompt: "Review the changes on this branch against {base} the way a careful reviewer would: bugs, edge cases, missing tests, anything unclear. List what you find, most important first. Don't change any code yet.",
        needs: Needs::Git,
    },
    SessionAction {
        id: "fix_comments",
        label: "Fix the review comments",
        icon: "message-square",
        prompt: "Read the review comments on this branch's pull request ({pull}) with the GitHub CLI. Fix what they ask for, reply to each one saying what you did, then push.",
        needs: Needs::Pull,
    },
    SessionAction {
        id: "conflicts",
        label: "Resolve conflicts",
        icon: "git-merge",
        prompt: "Bring this branch up to date with {base} and resolve any conflicts, keeping what both sides meant. Run the tests, then tell me what you changed.",
        needs: Needs::Branch,
    },
    SessionAction {
        id: "recap",
        label: "Write a recap",
        icon: "scroll-text",
        prompt: "Write a short recap of this session: what you changed, what's left to do, and anything I should check.",
        needs: Needs::Nothing,
    },
];

pub fn action(id: &str) -> Option<&'static SessionAction> {
    ACTIONS.iter().find(|a| a.id == id)
}

impl SessionAction {
    /// Whether it makes sense for a session working on `git` (None outside a checkout).
    pub fn fits(&self, git: Option<&GitStatus>) -> bool {
        match self.needs {
            Needs::Nothing => true,
            Needs::Git => git.is_some(),
            Needs::Branch => git.is_some_and(|g| !g.on_base()),
            Needs::Pull => git.and_then(|g| g.pull.as_ref()).is_some_and(|p| p.state == "open" || p.state == "draft"),
        }
    }

    /// "Update the pull request" once there is one.
    pub fn label_for(&self, git: Option<&GitStatus>) -> String {
        match (self.id, git.and_then(|g| g.pull.as_ref())) {
            ("pull_request", Some(p)) if p.state == "open" || p.state == "draft" => format!("Update pull request #{}", p.number),
            _ => self.label.to_string(),
        }
    }
}

/// An action's prompt (the user's, else Muxin's) with the session's branch,
/// base and pull request filled in.
pub fn prompt(action: &SessionAction, custom: Option<&str>, git: Option<&GitStatus>) -> String {
    let text = custom.map(str::trim).filter(|t| !t.is_empty()).unwrap_or(action.prompt);
    let branch = git.map(|g| g.branch.as_str()).filter(|b| !b.is_empty()).unwrap_or("this branch");
    let base = git.and_then(|g| g.base.as_deref()).unwrap_or("the default branch");
    let pull = git.and_then(|g| g.pull.as_ref()).map(|p| p.url.as_str()).unwrap_or("this branch's pull request");
    text.replace("{branch}", branch).replace("{base}", base).replace("{pull}", pull)
}

impl Engine {
    /// Asks a session's agent to do one of the actions: types its prompt in.
    /// Returns what was sent.
    pub async fn session_action(self: &Arc<Self>, id: &str, action_id: &str) -> Result<String, String> {
        let action = action(action_id).ok_or_else(|| format!("\"{action_id}\" isn't a session action. Use one of: {}.", ACTIONS.iter().map(|a| a.id).collect::<Vec<_>>().join(", ")))?;
        let find = || self.current().sessions.iter().find(|s| s.id == id).cloned().ok_or_else(|| format!("No session called {id}."));
        let mut session = find()?;
        // Its branch and pull request go into the prompt: look at them first unless that was just now.
        if session.git.is_none() && self.git_looked_at(&session.machine_id).is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(20)) {
            let _ = self.look_at_git(&session.machine_id).await;
            session = find()?;
        }
        if session.agent == "shell" || session.agent.is_empty() {
            return Err(format!("{} is a shell: actions are for agent sessions.", session.title));
        }
        if session.status != "running" {
            return Err(format!("{} has exited.", session.title));
        }
        let custom = self.settings().prompts.get(action.id).cloned();
        let text = prompt(action, custom.as_deref(), session.git.as_ref());
        self.control_session(id, Control::Send { text: &text, enter: true }).await?;
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::hub::BranchPull;

    fn git(branch: &str, pull: Option<&str>) -> GitStatus {
        GitStatus {
            branch: branch.into(),
            base: Some("origin/main".into()),
            pull: pull.map(|state| BranchPull { number: 12, title: "Fix".into(), state: state.into(), url: "https://github.com/acme/api/pull/12".into(), checks: Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn fills_in_the_branch_base_and_pull_request() {
        let a = action("pull_request").unwrap();
        assert!(prompt(a, None, Some(&git("fix-login", None))).starts_with("Push this branch (fix-login) and open"));
        let r = prompt(action("review").unwrap(), None, Some(&git("fix-login", None)));
        assert!(r.contains("against origin/main"), "{r}");
        let f = prompt(action("fix_comments").unwrap(), None, Some(&git("fix-login", Some("open"))));
        assert!(f.contains("(https://github.com/acme/api/pull/12)"), "{f}");
        // Outside a checkout, in words.
        assert!(prompt(action("conflicts").unwrap(), None, None).contains("with the default branch"));
    }

    #[test]
    fn the_users_own_prompt_wins_unless_its_empty() {
        let a = action("commit").unwrap();
        assert_eq!(prompt(a, Some("Commit on {branch}."), Some(&git("x", None))), "Commit on x.");
        assert_eq!(prompt(a, Some("  "), None), a.prompt);
    }

    #[test]
    fn offers_what_fits() {
        let fits = |id: &str, g: Option<&GitStatus>| action(id).unwrap().fits(g);
        assert!(fits("recap", None) && !fits("commit", None));
        assert!(fits("commit", Some(&git("main", None))) && !fits("pull_request", Some(&git("main", None))));
        assert!(fits("pull_request", Some(&git("fix", None))) && !fits("fix_comments", Some(&git("fix", None))));
        assert!(fits("fix_comments", Some(&git("fix", Some("open")))) && !fits("fix_comments", Some(&git("fix", Some("merged")))));
        assert_eq!(action("pull_request").unwrap().label_for(Some(&git("fix", Some("draft")))), "Update pull request #12");
        assert_eq!(ACTIONS.iter().map(|a| a.id).collect::<std::collections::HashSet<_>>().len(), ACTIONS.len());
    }
}
