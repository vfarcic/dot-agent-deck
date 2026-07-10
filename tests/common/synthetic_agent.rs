//! Agent-agnostic **synthetic-agent harness** (PRD #201 M1.3).
//!
//! A thin, scripted stand-in for a managed agent that emits the three
//! orchestration/status frames a real agent's extension would put on the
//! daemon socket — `delegate`, `work-done`, and `agent-event` — but built
//! deterministically in-process so the fast tier can assert the daemon's
//! routing / role-guard / status wiring without a PTY, a socket, or an LLM.
//!
//! ## Parameterized by agent identity from line one
//!
//! Every [`SyntheticAgent`] carries an [`AgentType`] identity. This PRD
//! instantiates only the `Pi` row (`SyntheticAgent::new(AgentType::Pi, …)`),
//! but the harness hard-codes nothing about Pi: the companion PRD adds
//! `{claude, opencode}` rows by constructing the same struct with a
//! different identity — **without rewriting the harness**. The identity is
//! stamped onto every `agent-event` frame the agent emits (the `agent_type`
//! field of the [`AgentEvent`]); the `delegate` / `work-done` signals carry
//! no agent type on the wire (routing is keyed on pane role, not agent
//! type — which is exactly why the same routing holds for a Pi identity).
//!
//! ## Where the lifecycle-state → EventType mapping lives
//!
//! The `dot-agent-deck agent-event --type <state>` subcommand maps a
//! lifecycle **state** string (`running` / `waiting` / `finished`) to an
//! [`EventType`] via a production seam
//! (`dot_agent_deck::event::agent_event_type_from_state`, added by the
//! coder in M1.2). The harness deliberately takes the already-resolved
//! [`EventType`] in [`SyntheticAgent::agent_event`] so the harness itself
//! stays seam-free and reusable; the test that pins the state→type mapping
//! calls the production seam directly and feeds the result here.

#![allow(dead_code)]

use dot_agent_deck::event::{AgentEvent, AgentType, DelegateSignal, EventType, WorkDoneSignal};
use dot_agent_deck::state::AppState;

/// A scripted stand-in agent, identified by its [`AgentType`]. Instantiate
/// with [`SyntheticAgent::new`] and drive it with [`agent_event`](Self::agent_event),
/// [`delegate`](Self::delegate), and [`work_done`](Self::work_done).
#[derive(Debug, Clone)]
pub struct SyntheticAgent {
    /// The agent identity stamped onto every emitted `agent-event` frame.
    /// Pi for this PRD; the companion PRD passes ClaudeCode / OpenCode.
    pub identity: AgentType,
    /// The `DOT_AGENT_DECK_PANE_ID` this agent runs under — the pane the
    /// daemon injects at spawn time and the key every routed frame carries.
    pub pane_id: String,
    /// The optional `DOT_AGENT_DECK_AGENT_ID` — the daemon-side registry id.
    /// `None` models a pane spawned before agent-id tagging.
    pub agent_id: Option<String>,
    /// A stable session id so repeated `agent-event`s update the SAME session
    /// card (the daemon keys sessions by this id). Derived from the pane id.
    pub session_id: String,
}

impl SyntheticAgent {
    /// A new synthetic agent of `identity` running under `pane_id`.
    pub fn new(identity: AgentType, pane_id: impl Into<String>) -> Self {
        let pane_id = pane_id.into();
        let session_id = format!("{pane_id}-session");
        Self {
            identity,
            pane_id,
            agent_id: None,
            session_id,
        }
    }

    /// Tag this agent with a daemon-side registry id (the value the daemon
    /// injects as `DOT_AGENT_DECK_AGENT_ID`).
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// The stable session id the daemon will key this agent's card on.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Build the [`AgentEvent`] this agent's extension would put on the
    /// daemon socket for `event_type` — the exact frame the
    /// `dot-agent-deck agent-event --type <state>` subcommand produces after
    /// resolving `<state>` through the production seam. It rides the existing
    /// raw-`AgentEvent` socket path (no `message_type` envelope), carries the
    /// pane/agent ids the daemon injected, and stamps this agent's identity.
    pub fn agent_event(&self, event_type: EventType) -> AgentEvent {
        AgentEvent {
            session_id: self.session_id.clone(),
            agent_type: self.identity.clone(),
            event_type,
            tool_name: None,
            tool_detail: None,
            cwd: None,
            timestamp: chrono::Utc::now(),
            user_prompt: None,
            metadata: Default::default(),
            pane_id: Some(self.pane_id.clone()),
            agent_id: self.agent_id.clone(),
        }
    }

    /// Build the [`DelegateSignal`] this agent (as an orchestrator) would send
    /// via `dot-agent-deck delegate --to <role> --task <text>`.
    pub fn delegate(&self, task: impl Into<String>, to: &[&str]) -> DelegateSignal {
        DelegateSignal {
            pane_id: self.pane_id.clone(),
            task: task.into(),
            to: to.iter().map(|r| r.to_string()).collect(),
            timestamp: chrono::Utc::now(),
        }
    }

    /// Build the [`WorkDoneSignal`] this agent (as a worker, or as the
    /// orchestrator with `done`) would send via `dot-agent-deck work-done`.
    pub fn work_done(&self, task: impl Into<String>, done: bool) -> WorkDoneSignal {
        WorkDoneSignal {
            pane_id: self.pane_id.clone(),
            task: task.into(),
            done,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Register this agent into `state` as an orchestration role pane,
    /// mirroring what the StartAgent path records for a live orchestration
    /// tab: the managed-pane set, the pane→role map, the orchestrator set
    /// (when `is_orchestrator`), the pane→orchestration map, and the pane cwd.
    /// This is the setup `handle_delegate` / `handle_work_done` read.
    pub fn register_role(
        &self,
        state: &mut AppState,
        role: &str,
        is_orchestrator: bool,
        orchestration: (String, String),
        cwd: &str,
    ) {
        state.register_pane(self.pane_id.clone());
        state
            .pane_role_map
            .insert(self.pane_id.clone(), role.to_string());
        if is_orchestrator {
            state.orchestrator_pane_ids.insert(self.pane_id.clone());
        }
        state
            .pane_orchestration_map
            .insert(self.pane_id.clone(), orchestration);
        state
            .pane_cwd_map
            .insert(self.pane_id.clone(), cwd.to_string());
    }
}
