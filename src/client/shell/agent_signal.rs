//! drovr fork: what an agent reports through drovr-state-hook, and the inbox
//! item it makes (docs/design/inbox-pane.md, sections 2 and 8).
//!
//! The sidebar and the inbox build items from the same [`AgentSignal::item`],
//! so a glyph in the sidebar always matches a line in the inbox.

use crate::api::schema::AgentStatus;
use crate::protocol::ClientShellAgent;

/// Default for the stuck threshold. Build step 4 adds the per-workspace
/// setting (`[inbox] stuck_minutes` in `sidebar.toml`).
pub(super) const DEFAULT_STUCK_SECS: u64 = 10 * 60;

/// Share of the model's context window at which an agent shows as `limit`.
const LIMIT_PERCENT: u64 = 85;

/// Inbox item kinds, in inbox sort order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ItemKind {
    Permission,
    Question,
    Plan,
    Asks,
    Dialog,
    Stuck,
    Limit,
    Exited,
    Denied,
    Finished,
}

impl ItemKind {
    pub(super) fn glyph(self) -> &'static str {
        match self {
            Self::Permission => "!",
            Self::Question | Self::Asks => "?",
            Self::Plan => "▤",
            Self::Dialog => "◆",
            Self::Stuck => "⧗",
            Self::Limit => "◔",
            Self::Exited => "✗",
            Self::Denied => "⊘",
            Self::Finished => "✓",
        }
    }

    /// The agent is stopped on a prompt until someone answers it.
    pub(super) fn waiting(self) -> bool {
        matches!(
            self,
            Self::Permission | Self::Question | Self::Plan | Self::Dialog
        )
    }
}

/// `drovr_state` kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StateKind {
    Idle,
    Working,
    Asks,
    Finished,
    Limit,
    Denied,
    Exited,
}

/// The drovr-state tokens of one agent. Every field is optional: agents
/// without the hook have none, and the hook reports kinds only with
/// `DROVR_STATE_TEXT=0`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct AgentSignal {
    /// `drovr_state`: the kind and the unix time of its last change or
    /// reported PreToolUse.
    state: Option<(StateKind, u64)>,
    /// `drovr_doing`: the running tool ("Bash cargo test").
    pub(super) doing: Option<String>,
    /// Kind of the oldest pending request in `drovr_wait`.
    wait: Option<ItemKind>,
    /// `drovr_ctx`: context size in tokens (usage hook).
    ctx: Option<u64>,
}

impl AgentSignal {
    /// Reads the tokens of `agent`. Pane tokens outlive the agent, so they
    /// only count while the pane runs an agent that has the hook.
    pub(super) fn parse(agent: &ClientShellAgent) -> Self {
        if !matches!(agent.agent.as_deref(), Some("claude" | "codex")) {
            return Self::default();
        }
        let token = |name: &str| {
            super::projects::agent_token(agent, name)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        let state = token("drovr_state").and_then(|value| {
            let (kind, at) = value.split_once('|')?;
            let kind = match kind {
                "idle" => StateKind::Idle,
                "working" => StateKind::Working,
                "asks" => StateKind::Asks,
                "finished" => StateKind::Finished,
                "limit" => StateKind::Limit,
                "denied" => StateKind::Denied,
                "exited" => StateKind::Exited,
                _ => return None,
            };
            Some((kind, at.parse().ok()?))
        });
        let wait = token("drovr_wait").and_then(|value| match value.split('|').next()? {
            "permission" => Some(ItemKind::Permission),
            "question" => Some(ItemKind::Question),
            "plan" => Some(ItemKind::Plan),
            _ => None,
        });
        Self {
            state,
            doing: token("drovr_doing").map(str::to_owned),
            wait,
            ctx: token("drovr_ctx").and_then(|value| value.parse().ok()),
        }
    }

    /// The inbox item this agent makes, if any. `now` is the unix time in
    /// seconds; `stuck_secs` is the workspace's stuck threshold.
    pub(super) fn item(&self, status: AgentStatus, now: u64, stuck_secs: u64) -> Option<ItemKind> {
        let kind = self.state.map(|(kind, _)| kind);
        match status {
            // herdr sees a prompt; the hook says which, else it is a dialog
            // drovr cannot answer (Codex without hook, multi-question, MCP
            // elicitation, trust prompt).
            AgentStatus::Blocked => Some(self.wait.unwrap_or(ItemKind::Dialog)),
            AgentStatus::Working => match self.state {
                Some((StateKind::Working, at)) if now.saturating_sub(at) >= stuck_secs => {
                    Some(ItemKind::Stuck)
                }
                _ => self.near_limit().then_some(ItemKind::Limit),
            },
            // Idle, Done (finished, not yet seen) and Unknown.
            _ => match kind {
                Some(StateKind::Asks) => Some(ItemKind::Asks),
                Some(StateKind::Limit) => Some(ItemKind::Limit),
                Some(StateKind::Exited) => Some(ItemKind::Exited),
                Some(StateKind::Denied) => Some(ItemKind::Denied),
                _ if self.near_limit() => Some(ItemKind::Limit),
                _ if status == AgentStatus::Done => Some(ItemKind::Finished),
                _ => None,
            },
        }
    }

    /// Seconds since the running tool started, while the hook says working.
    pub(super) fn doing_secs(&self, now: u64) -> Option<u64> {
        match self.state {
            Some((StateKind::Working, at)) if self.doing.is_some() => Some(now.saturating_sub(at)),
            _ => None,
        }
    }

    /// Context at or above [`LIMIT_PERCENT`] of the model window. The tokens
    /// do not name the model, so the window is 200k, or 1M once the context
    /// is past 200k. Ceiling: a 1M-window session between 170k and 200k shows
    /// as limit; a `drovr_model` token from the usage hook would fix it.
    fn near_limit(&self) -> bool {
        self.ctx.is_some_and(|ctx| {
            let window = if ctx > 200_000 { 1_000_000 } else { 200_000 };
            ctx * 100 >= window * LIMIT_PERCENT
        })
    }
}

/// Elapsed time for a working agent's row: "30s", "4m", "2h".
pub(super) fn format_elapsed(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// Seconds since the Unix epoch.
pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// What a sidebar click on an item glyph or a section count opens the inbox
/// on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum InboxFilter {
    /// One workspace on one machine.
    Workspace {
        endpoint_id: super::ClientEndpointId,
        workspace_id: String,
    },
    /// A sidebar section: a project name, or [`super::projects::OTHER`].
    Project(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(vendor: &str, tokens: &[(&str, &str)]) -> ClientShellAgent {
        ClientShellAgent {
            pane_id: "p1".into(),
            workspace_id: "w1".into(),
            tab_id: "t1".into(),
            name: None,
            display_agent: None,
            agent: Some(vendor.into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: AgentStatus::Idle,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: tokens
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            focused: false,
        }
    }

    const NOW: u64 = 1_800_000_000;

    fn item(vendor: &str, tokens: &[(&str, &str)], status: AgentStatus) -> Option<ItemKind> {
        AgentSignal::parse(&agent(vendor, tokens)).item(status, NOW, DEFAULT_STUCK_SECS)
    }

    #[test]
    fn parses_the_hook_tokens() {
        let signal = AgentSignal::parse(&agent(
            "claude",
            &[
                ("drovr_state", "working|1799999970"),
                ("$drovr_doing", "Bash cargo test"),
                ("drovr_wait", "plan|ab12cd34||Inbox pane"),
                ("drovr_ctx", "1200"),
            ],
        ));
        assert_eq!(
            signal,
            AgentSignal {
                state: Some((StateKind::Working, 1_799_999_970)),
                doing: Some("Bash cargo test".into()),
                wait: Some(ItemKind::Plan),
                ctx: Some(1200),
            }
        );
        assert_eq!(signal.doing_secs(NOW), Some(30));
        // Malformed values and other agents' leftover tokens are ignored.
        let bad = [
            ("drovr_state", "working"),
            ("drovr_wait", "elicit|x||"),
            ("drovr_ctx", "lots"),
            ("drovr_doing", "  "),
        ];
        assert_eq!(
            AgentSignal::parse(&agent("claude", &bad)),
            AgentSignal::default()
        );
        let leftover = [("drovr_state", "asks|1"), ("drovr_doing", "Edit x")];
        assert_eq!(
            AgentSignal::parse(&agent("pi", &leftover)),
            AgentSignal::default()
        );
        assert!(AgentSignal::parse(&agent("codex", &leftover))
            .doing
            .is_some());
    }

    #[test]
    fn blocked_agents_wait_on_the_hook_request_or_a_dialog() {
        use AgentStatus::Blocked;
        let wait = |kind: &str| [("drovr_wait", format!("{kind}|ab12cd34||x"))];
        for (kind, expected) in [
            ("permission", ItemKind::Permission),
            ("question", ItemKind::Question),
            ("plan", ItemKind::Plan),
        ] {
            let tokens = wait(kind);
            let tokens = [(tokens[0].0, tokens[0].1.as_str())];
            assert_eq!(item("claude", &tokens, Blocked), Some(expected));
        }
        assert_eq!(item("codex", &[], Blocked), Some(ItemKind::Dialog));
        assert_eq!(item("pi", &[], Blocked), Some(ItemKind::Dialog));
        // A request token alone is not an item: herdr must see the prompt.
        let tokens = [("drovr_wait", "permission|ab12cd34||git push")];
        assert_eq!(item("claude", &tokens, AgentStatus::Idle), None);
    }

    #[test]
    fn working_agents_are_items_only_when_stuck_or_near_the_limit() {
        use AgentStatus::Working;
        let at = |secs_ago: u64| format!("working|{}", NOW - secs_ago);
        let recent = at(DEFAULT_STUCK_SECS - 1);
        assert_eq!(item("claude", &[("drovr_state", &recent)], Working), None);
        let quiet = at(DEFAULT_STUCK_SECS);
        assert_eq!(
            item("claude", &[("drovr_state", &quiet)], Working),
            Some(ItemKind::Stuck)
        );
        // Without the hook there is no event time, so never stuck.
        assert_eq!(item("pi", &[], Working), None);
        assert_eq!(
            item("claude", &[("drovr_ctx", "170000")], Working),
            Some(ItemKind::Limit)
        );
        assert_eq!(item("claude", &[("drovr_ctx", "169999")], Working), None);
        assert_eq!(item("claude", &[("drovr_ctx", "300000")], Working), None);
        assert_eq!(
            item("claude", &[("drovr_ctx", "850000")], Working),
            Some(ItemKind::Limit)
        );
    }

    #[test]
    fn stopped_agents_take_the_hook_kind_then_herdr_done() {
        use AgentStatus::{Done, Idle};
        let state = |kind: &str| [("drovr_state", format!("{kind}|{NOW}"))];
        let check = |kind: &str, status| {
            let tokens = state(kind);
            item("claude", &[(tokens[0].0, tokens[0].1.as_str())], status)
        };
        assert_eq!(check("asks", Idle), Some(ItemKind::Asks));
        assert_eq!(check("asks", Done), Some(ItemKind::Asks));
        assert_eq!(check("limit", Idle), Some(ItemKind::Limit));
        assert_eq!(check("exited", Idle), Some(ItemKind::Exited));
        assert_eq!(check("denied", Idle), Some(ItemKind::Denied));
        assert_eq!(check("finished", Done), Some(ItemKind::Finished));
        // Seen: herdr moved Done to Idle.
        assert_eq!(check("finished", Idle), None);
        assert_eq!(check("idle", Idle), None);
        assert_eq!(item("pi", &[], Done), Some(ItemKind::Finished));
        assert_eq!(item("pi", &[], Idle), None);
    }

    #[test]
    fn kinds_sort_in_inbox_order_with_their_glyphs() {
        let kinds = [
            ItemKind::Permission,
            ItemKind::Question,
            ItemKind::Plan,
            ItemKind::Asks,
            ItemKind::Dialog,
            ItemKind::Stuck,
            ItemKind::Limit,
            ItemKind::Exited,
            ItemKind::Denied,
            ItemKind::Finished,
        ];
        assert!(kinds.windows(2).all(|pair| pair[0] < pair[1]));
        let glyphs = kinds.map(ItemKind::glyph).concat();
        assert_eq!(glyphs, "!?▤?◆⧗◔✗⊘✓");
        assert_eq!(kinds.iter().filter(|kind| kind.waiting()).count(), 4);
        assert_eq!(format_elapsed(30), "30s");
        assert_eq!(format_elapsed(240), "4m");
        assert_eq!(format_elapsed(7200), "2h");
    }
}
