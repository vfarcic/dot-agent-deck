//! Declared contract breaks: telling the intended outcome of a break from a
//! regression (issue #1596).
//!
//! CLAUDE.md rule 12 has a build declare a same-wire semantic break in
//! `CONTRACT_BREAKS` (`src/daemon_protocol.rs`), beside a
//! `changelog.d/<issue>.breaking.md` fragment, and `daemon hello` reports that
//! list. When the two builds of a run disagree on it, a tell can fail because
//! the break did exactly what it says it does: #318's daemon refuses a status
//! report from a deck-spawned pane that carries no hook capability token, and a
//! previous-release CLI sends `agent-event` without one, so tell-4's status
//! half fails in the reverse pairing. Reporting that as FAIL failed every PR
//! that ran the reverse pairing.
//!
//! A failed tell is explained only when ALL of these hold, and every failed
//! half of it is explained:
//!
//! * [`KNOWN_EFFECTS`] names the break, the tell and the half;
//! * the build the effect names as declaring it ([`Declarer`]) lists the break
//!   and the other build does not — so the break is declared between these two
//!   builds, in the direction the effect describes: usually the DAEMON's build,
//!   whose daemon refuses or reinterprets what an older client sends, and for
//!   PRD #1666 part 3's task-scoped `work-done` also the CLIENT's build, whose
//!   CLI asks an older daemon a question that daemon cannot read;
//! * its witness, in this run, carries every piece of the effect's signature
//!   and names a pane the failed half measured
//!   ([`crate::report::Tell::panes_of`]). The witness is one line of the
//!   sandbox `deck.log` — the daemon logged that pane's message the way the
//!   break says it will — except for a CLIENT-declared effect whose client
//!   refuses before it sends anything the daemon could log: such an effect
//!   may instead name a line of the client CLI's own output, as the tell's
//!   capture recorded it in that pane ([`Witness::ClientOutput`]). A
//!   daemon-declared effect always needs the daemon's log.
//!
//! A half the harness could not measure is not named like one that measured a
//! refusal: tell-4 calls its status half `status-query` when `daemon status`
//! could not be read, and no effect names that.
//!
//! Anything short of that stays a failure: a tell failing with no break
//! declared between the builds, a break declared but no refusal logged, or a
//! failed half no known effect names. A newly declared break that changes what
//! a tell sees fails the run until its effect is added below, which is the
//! point: the entry is the reviewed statement that this failure is that break.

use crate::report::{BreakExplanation, Evidence, Verdict};
use crate::sandbox::Direction;

/// A failure a declared break is known to cause, and the daemon's own record
/// of it.
pub struct KnownEffect {
    /// The `CONTRACT_BREAKS` entry.
    pub id: &'static str,
    /// The tell it fails.
    pub tell: &'static str,
    /// The half of that tell it fails ([`crate::report::Tell::failed_parts`]).
    pub part: &'static str,
    /// Which build must declare the break, the other one not.
    pub declared_by: Declarer,
    /// Where the run's record of the break is.
    pub witness: Witness,
    /// Substrings that must all appear on ONE line of the witness: the
    /// daemon's refusal in its own words, or the client's.
    pub log_signature: &'static [&'static str],
    /// How that line names the message's pane; the pane must be one the
    /// failed half measured ([`crate::report::Tell::panes_of`]).
    pub pane_field: PaneField,
    /// The one call that writes the line, as `(source file, texts)`: every
    /// text must appear inside the SAME `warn!(…);` call there — its message
    /// and each field the signature and pane field read — so renaming a field
    /// in that call fails `every_known_effect_is_pinned_to_the_source` even
    /// while another log call still spells it the old way; for a
    /// [`Witness::ClientOutput`], inside the one constant
    /// [`Self::client_text`] names. A message split across source lines is
    /// matched with its continuation escapes joined.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the source-pin guard test only")
    )]
    pub log_site: (&'static str, &'static [&'static str]),
    /// Pins outside that call — where a field's value comes from — as
    /// `(source file, text in it)`.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the source-pin guard test only")
    )]
    pub source_pins: &'static [(&'static str, &'static str)],
    /// For a [`Witness::ClientOutput`], the declaration that opens the
    /// constant the client prints (`pub const NAME: &str =`); empty otherwise.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the source-pin guard test only")
    )]
    pub client_text: &'static str,
    /// Why the break fails that half, for the evidence file.
    pub why: &'static str,
}

/// Where a run records what a break did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Witness {
    /// A line of the sandbox `deck.log`: the daemon's own record.
    DaemonLog,
    /// A line the client CLI printed in the pane the half measured, as the
    /// tell's capture recorded it. Only for a CLIENT-declared break whose
    /// client stops before sending the daemon anything to log.
    ClientOutput,
}

/// Which of the two builds declares the break an effect explains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Declarer {
    /// The daemon's build declares it and the client's does not.
    Daemon,
    /// The client's build declares it and the daemon's does not.
    Client,
}

impl Declarer {
    fn name(self) -> &'static str {
        match self {
            Declarer::Daemon => "daemon",
            Declarer::Client => "client",
        }
    }
}

/// How a log line names a pane.
#[derive(Clone, Copy, Debug)]
pub enum PaneField {
    /// The client's own output names no pane: the pane is the one the
    /// capture recorded running it ([`Witness::ClientOutput`] only).
    Capture,
    /// A `tracing` field: `<field><pane>` is one whitespace-separated word of
    /// the line.
    Word(&'static str),
    /// A JSON member of a line the daemon logged: the line contains
    /// `<member><pane>"`, the closing quote keeping pane `3` from matching
    /// `33`.
    Json(&'static str),
}

impl PaneField {
    /// The text that names `pane`, as it appears in the line.
    fn token(self, pane: &str) -> String {
        match self {
            PaneField::Capture => format!("pane={pane}"),
            PaneField::Word(field) => format!("{field}{pane}"),
            PaneField::Json(member) => format!("{member}{pane}\""),
        }
    }

    fn names(self, line: &str, pane: &str) -> bool {
        let token = self.token(pane);
        match self {
            // Checked against the capture's own pane, not the line.
            PaneField::Capture => false,
            PaneField::Word(_) => line.split_whitespace().any(|w| w == token),
            PaneField::Json(_) => line.contains(&token),
        }
    }
}

/// The break of PRD #1666 part 3, round 5: a worker's `work-done` completes the
/// task it names, and is sent only to a daemon that confirms it serves that.
const TASK_SCOPED_COMPLETION: &str = "1666-task-scoped-completion-and-recovery";

/// PRD #1666 part 3 (H5): the daemon refuses the legacy `inbox fetch` an
/// older CLI sends, and takes nothing.
const LEGACY_FETCH_WITHDRAWN: &str = "1666-legacy-inbox-delivery-withdrawal";

/// PRD #1666 part 3 (H4): `inbox fetch` takes messages only from a daemon
/// that confirms it serves this command on receipts.
const ONE_SHOT_RECEIPTS: &str = "1666-remaining-inbox-one-shot-receipts";

/// The H5 refusal, the same for every tell it fails: the branch daemon logs
/// refusing the older CLI's fetch, naming the fetching pane.
const H5_SIGNATURE: &[&str] = &[
    "refused a fetch the way only an older dot-agent-deck asks; nothing was taken",
    "consumer=Cli",
];
const H5_LOG_SITE: (&str, &[&str]) = (
    "src/daemon.rs",
    &[
        "inbox: refused a fetch the way only an older dot-agent-deck asks; nothing was taken",
        "pane_id = %escape_id_for_log(&req.pane_id),",
        "consumer = ?req.consumer,",
    ],
);
const H5_SOURCE_PINS: &[(&str, &str)] = &[(
    "src/event.rs",
    "pub enum InboxFetchConsumer {\n    /// `dot-agent-deck inbox fetch`.\n    Cli,",
)];
const H5_WHY: &str = "a daemon declaring it refuses the `inbox fetch` that only a CLI from a build \
     that does not declare it sends, and takes nothing, so that pane's message is never handed \
     over";

/// Every break known to fail a tell, and how. Keep each `log_signature` pinned
/// to the source by the test below.
pub const KNOWN_EFFECTS: &[KnownEffect] = &[
    KnownEffect {
        id: "318-hook-event-capability-token",
        tell: "tell-4",
        part: "status",
        declared_by: Declarer::Daemon,
        witness: Witness::DaemonLog,
        log_signature: &[
            "refused a status event whose hook capability token",
            "verb=\"agent_event\"",
            "reason=\"missing_token\"",
        ],
        pane_field: PaneField::Word("claimed_pane="),
        log_site: (
            "src/daemon.rs",
            &[
                "hook socket: refused a status event whose hook capability token",
                "verb = \"agent_event\",",
                "claimed_pane = %escape_id_for_log(",
                "reason = refusal.code(),",
            ],
        ),
        source_pins: &[(
            "src/hook_provenance.rs",
            "Refusal::Missing => \"missing_token\",",
        )],
        client_text: "",
        why: "a daemon declaring it refuses a status report from a deck-spawned pane that carries \
          no hook capability token, and a CLI from a build that does not declare it sends \
          `agent-event` without one",
    },
    // Forward: the branch CLI asks the old daemon `hook_capabilities` first,
    // the old daemon cannot decode that and logs it as a malformed event, and
    // the CLI sends no report. The logging is the previous release's own
    // fallback for a line it cannot decode (`Malformed event:`, the token
    // redacted since #318); it is pinned to the branch's copy of that code,
    // which is the same call. The reporting pane is the reviewer's.
    KnownEffect {
        id: TASK_SCOPED_COMPLETION,
        tell: "tell-4",
        part: "work-done",
        declared_by: Declarer::Client,
        witness: Witness::DaemonLog,
        log_signature: &["Malformed event:", "\"message_type\":\"hook_capabilities\""],
        pane_field: PaneField::Json("\"pane_id\":\""),
        log_site: (
            "src/daemon.rs",
            &[
                "line_bytes = line.len(),",
                "\"Malformed event: {}\",",
                "hook_line_for_log(&line)",
            ],
        ),
        source_pins: &[
            ("src/event.rs", "#[serde(tag = \"message_type\")]"),
            ("src/event.rs", "#[serde(rename = \"hook_capabilities\")]"),
            (
                "src/event.rs",
                "pub struct HookCapabilitiesRequest {\n    pub pane_id: String,",
            ),
        ],
        client_text: "",
        why: "a CLI declaring it sends `work-done` only after the daemon confirms task-scoped \
              completion on the same connection, and a daemon from a build that does not \
              declare it cannot read that question, so the CLI reports nothing",
    },
    // Reverse, tell 3: the old CLI's `inbox fetch` in the coder's pane is the
    // legacy fetch, which the branch daemon refuses.
    KnownEffect {
        id: LEGACY_FETCH_WITHDRAWN,
        tell: "tell-3",
        part: "fetch",
        declared_by: Declarer::Daemon,
        witness: Witness::DaemonLog,
        log_signature: H5_SIGNATURE,
        pane_field: PaneField::Word("pane_id="),
        log_site: H5_LOG_SITE,
        source_pins: H5_SOURCE_PINS,
        client_text: "",
        why: H5_WHY,
    },
    // Reverse, tell 4: the old CLI's report is delivered as a note, which
    // the orchestrator's old `inbox fetch` then cannot take, for the same
    // reason. The note being delivered is not a failure, so it is not an
    // effect; the refused fetch, in the orchestrator's pane, is.
    KnownEffect {
        id: LEGACY_FETCH_WITHDRAWN,
        tell: "tell-4",
        part: "work-done",
        declared_by: Declarer::Daemon,
        witness: Witness::DaemonLog,
        log_signature: H5_SIGNATURE,
        pane_field: PaneField::Word("pane_id="),
        log_site: H5_LOG_SITE,
        source_pins: H5_SOURCE_PINS,
        client_text: "",
        why: H5_WHY,
    },
    // Forward, tell 3: the branch CLI's `inbox fetch` asks the old daemon
    // whether it serves receipts, reads no, and takes nothing. The old daemon
    // logs nothing for that, so the only record is what the CLI printed in the
    // coder's pane.
    KnownEffect {
        id: ONE_SHOT_RECEIPTS,
        tell: "tell-3",
        part: "fetch",
        declared_by: Declarer::Client,
        witness: Witness::ClientOutput,
        log_signature: &[
            "Could not fetch messages: the deck daemon did not confirm that it can \
             hand them to this `dot-agent-deck inbox fetch`, so no message was taken",
        ],
        pane_field: PaneField::Capture,
        log_site: (
            "src/inbox_cli.rs",
            &[
                "Could not fetch messages: the deck daemon did not confirm that it can hand them \
               to this `dot-agent-deck inbox fetch`, so no message was taken",
            ],
        ),
        source_pins: &[(
            "src/inbox_cli.rs",
            "Opened::Unsupported => return say(err, CLI_RECEIPTS_UNSUPPORTED),",
        )],
        client_text: "pub const CLI_RECEIPTS_UNSUPPORTED: &str =",
        why: "a CLI declaring it takes messages only from a daemon that confirms it serves \
              `inbox fetch` on receipts, and a daemon from a build that does not declare it does \
              not confirm that, so the CLI takes nothing and says so",
    },
];

/// The `contract_breaks` list of a `daemon hello` document, or `None` when the
/// document does not parse or carries no such list.
fn contract_breaks(hello: &str) -> Option<Vec<String>> {
    let doc: serde_json::Value = serde_json::from_str(hello.trim()).ok()?;
    Some(
        doc.get("contract_breaks")?
            .as_array()?
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
    )
}

/// Mark every failed tell a declared break explains, and note in the run log
/// why each failed tell is or is not explained. `direction` decides which
/// build served the daemon: the previous release forward, the branch in
/// reverse.
pub fn explain(ev: &mut Evidence, direction: Direction, deck_log: &str) {
    let (daemon_hello, client_hello) = match direction {
        Direction::Forward => (&ev.old_hello, &ev.new_hello),
        Direction::Reverse => (&ev.new_hello, &ev.old_hello),
    };
    let (Some(daemon), Some(client)) =
        (contract_breaks(daemon_hello), contract_breaks(client_hello))
    else {
        if ev.tells.iter().any(|t| t.verdict == Verdict::Fail) {
            ev.step(
                "declared breaks: a `daemon hello` carried no readable `contract_breaks`, so no \
                 failed tell is attributed to a declared break",
            );
        }
        return;
    };
    let declared = Declared {
        by_daemon: daemon.iter().filter(|b| !client.contains(b)).collect(),
        by_client: client.iter().filter(|b| !daemon.contains(b)).collect(),
    };
    let mut notes = Vec::new();
    for tell in ev.tells.iter_mut().filter(|t| t.verdict == Verdict::Fail) {
        if tell.failed_parts.is_empty() {
            notes.push(format!(
                "declared breaks: `{}` failed as a whole, and no declared break is known to fail \
                 a whole tell, so it stays a failure",
                tell.id
            ));
            continue;
        }
        let mut explained = Vec::new();
        let mut unexplained = Vec::new();
        for part in &tell.failed_parts {
            let outputs: Vec<(&str, &str)> = tell
                .client_outputs
                .iter()
                .filter(|(half, _, _)| half == part)
                .map(|(_, pane, out)| (pane.as_str(), out.as_str()))
                .collect();
            match explain_part(
                &tell.id,
                part,
                &tell.panes_of(part),
                &outputs,
                &declared,
                deck_log,
            ) {
                Ok(why) => explained.push(why),
                Err(why) => unexplained.push(format!("`{part}`: {why}")),
            }
        }
        if unexplained.is_empty() {
            let (summary, detail): (Vec<String>, Vec<String>) = explained.into_iter().unzip();
            tell.declared_break = Some(BreakExplanation {
                summary: summary.join("; "),
                detail: detail.join("\n"),
            });
        } else {
            notes.push(format!(
                "declared breaks: `{}` stays a failure — {}",
                tell.id,
                unexplained.join("; ")
            ));
        }
    }
    for note in notes {
        ev.step(note);
    }
}

/// The breaks one build of the run declares and the other does not.
struct Declared<'a> {
    by_daemon: Vec<&'a String>,
    by_client: Vec<&'a String>,
}

/// Whether a declared break explains one failed half of one tell: `Ok` with
/// the explanation's summary and detail, or `Err` with why not. `panes` are
/// the panes the half measured, and `outputs` what the client printed in
/// them, as `(pane, output)`.
fn explain_part(
    tell: &str,
    part: &str,
    panes: &[&str],
    outputs: &[(&str, &str)],
    declared: &Declared<'_>,
    deck_log: &str,
) -> Result<(String, String), String> {
    let candidates: Vec<&KnownEffect> = KNOWN_EFFECTS
        .iter()
        .filter(|e| e.tell == tell && e.part == part)
        .collect();
    if candidates.is_empty() {
        return Err("no declared break is known to fail it".to_string());
    }
    let mut why_not = Vec::new();
    for effect in candidates {
        let (by, other) = match effect.declared_by {
            Declarer::Daemon => (&declared.by_daemon, Declarer::Client),
            Declarer::Client => (&declared.by_client, Declarer::Daemon),
        };
        if !by.iter().any(|b| b.as_str() == effect.id) {
            why_not.push(format!(
                "`{}` is not declared by the {}'s build without the {}'s",
                effect.id,
                effect.declared_by.name(),
                other.name()
            ));
            continue;
        }
        if panes.is_empty() {
            why_not.push(format!(
                "`{}` is declared, but the run did not record which pane the failed half \
                 measured, so no refusal can be tied to it",
                effect.id
            ));
            continue;
        }
        let witnessed = match effect.witness {
            Witness::DaemonLog => deck_log
                .lines()
                .find(|l| {
                    effect.log_signature.iter().all(|sig| l.contains(sig))
                        && panes.iter().any(|pane| effect.pane_field.names(l, pane))
                })
                .map(|line| format!("the daemon logged: `{}`", line.trim())),
            // The client's own words stand in for the daemon's log only for a
            // break the CLIENT declares: a daemon's refusal is the daemon's to
            // record.
            Witness::ClientOutput if effect.declared_by != Declarer::Client => {
                why_not.push(format!(
                    "`{}` names the client's output as its witness, which only a \
                     client-declared break may",
                    effect.id
                ));
                continue;
            }
            Witness::ClientOutput => outputs
                .iter()
                .filter(|(pane, _)| panes.contains(pane))
                .find_map(|(pane, out)| {
                    out.lines()
                        .find(|l| effect.log_signature.iter().all(|sig| l.contains(sig)))
                        .map(|line| {
                            format!("the client printed, in pane {pane}: `{}`", line.trim())
                        })
                }),
        };
        let Some(witnessed) = witnessed else {
            let tokens: Vec<String> = panes.iter().map(|p| effect.pane_field.token(p)).collect();
            why_not.push(format!(
                "`{}` is declared, but {} matching {:?} for {}",
                effect.id,
                match effect.witness {
                    Witness::DaemonLog => "the daemon logged no refusal",
                    Witness::ClientOutput => "the client printed no line",
                },
                effect.log_signature,
                tokens
                    .iter()
                    .map(|t| format!("`{t}`"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ));
            continue;
        };
        return Ok((
            format!("`{part}` half: `{}`", effect.id),
            format!(
                "`{part}` half: `{}` — {}. The {}'s build declares it and the {}'s does not; \
                 {witnessed}",
                effect.id,
                effect.why,
                effect.declared_by.name(),
                other.name(),
            ),
        ));
    }
    Err(why_not.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{CONTRACT_TELLS, ROLE_SET_TELL, RunVerdict, Verdict};

    /// `daemon hello` of v0.45.1, as the PR #1595 run recorded it.
    const V0451_HELLO: &str = r#"{"ok":true,"server_version":10,"build_version":"0.45.1-g4a332587","daemon_version":"0.45.1","contract_breaks":["617-pane-write-agent-binding","1049-stop-daemon-verb","1077-hook-capability-token","1105-focused-client-terminal-size","580-delegate-refuses-busy-worker","555-orchestration-title-uniqueness","708-worker-failure-reports-submitted","1233-prepare-refuses-ambiguous-orchestration","1337-respawn-failure-report-submitted","505-unsolicited-work-done-label-reworded"]}"#;

    /// `daemon hello` of that run's branch build, which declares #318's break
    /// (and #1396's two) on top of v0.45.1's list.
    const BRANCH_HELLO: &str = r#"{"ok":true,"server_version":10,"build_version":"0.45.1-g6f38d2a5","daemon_version":"0.45.1","contract_breaks":["617-pane-write-agent-binding","1049-stop-daemon-verb","1077-hook-capability-token","1105-focused-client-terminal-size","580-delegate-refuses-busy-worker","555-orchestration-title-uniqueness","708-worker-failure-reports-submitted","1233-prepare-refuses-ambiguous-orchestration","1337-respawn-failure-report-submitted","505-unsolicited-work-done-label-reworded","1396-start-refuses-non-directory-cwd","1396-dispatch-refuses-ambiguous-orchestration","318-hook-event-capability-token"]}"#;

    /// The line the branch daemon wrote into that run's sandbox `deck.log` when
    /// it refused the v0.45.1 CLI's `agent-event --type running`.
    const REFUSAL_LINE: &str = r#"2026-10-05T17:08:35.602037Z  WARN dot_agent_deck::daemon: hook socket: refused a status event whose hook capability token does not attest the pane it names; see docs/develop/hook-provenance.md verb="agent_event" event_type=Thinking claimed_pane=3 reason="missing_token""#;

    /// The branch daemon's line for a v0.45.1 CLI's `work-done --task`, which
    /// names no task (PRD #1666 part 3, round 5), on the reviewer's pane `3`.
    const NOTE_LINE: &str = r#"2026-10-23T09:12:01.000000Z  WARN dot_agent_deck::daemon: hook socket: delivered a work-done that names no task as a note that completes no task; a dot-agent-deck CLI older than this daemon sends that, and `work-done --task-id` completes a task verb="work_done" claimed_pane=3 reason="no_task""#;

    /// What a previous-release daemon logs for the branch CLI's capability
    /// question on pane `3`: its fallback for a line it cannot decode, the
    /// token taken out.
    const QUESTION_LINE: &str = r#"2026-10-23T09:12:01.000000Z  WARN dot_agent_deck::daemon: Malformed event: {"message_type":"hook_capabilities","pane_id":"3"} line_bytes=94"#;

    /// The branch's `daemon hello`, declaring the task-scoped break on top of
    /// [`BRANCH_HELLO`]'s list.
    fn task_scoped_hello() -> String {
        BRANCH_HELLO.replace(
            r#""318-hook-event-capability-token"]"#,
            r#""318-hook-event-capability-token","1666-task-scoped-completion-and-recovery"]"#,
        )
    }

    /// A run of `direction` in which tell-4's `work-done` half failed and its
    /// status half held, the reviewer's pane `pane`, the branch declaring the
    /// task-scoped break when `declared`.
    fn work_done_run(direction: Direction, declared: bool, pane: &str) -> Evidence {
        let branch = if declared {
            task_scoped_hello()
        } else {
            BRANCH_HELLO.to_string()
        };
        let mut ev = Evidence {
            branch: "6f38d2a559ba4bca111f2419d9c71e0e80d8c77e".into(),
            direction,
            old_hello: V0451_HELLO.into(),
            new_hello: branch,
            ..Default::default()
        };
        for id in ["tell-1", "tell-2", "tell-3", ROLE_SET_TELL] {
            ev.tell(id, "t", Verdict::Pass, "measured");
        }
        ev.tell_in_parts(
            "tell-4",
            "hooks (work-done, status) still arrived",
            &[("work-done", false), ("status", true)],
            "work-done: the feedback line did NOT appear",
        );
        ev.tells.last_mut().unwrap().subject_pane = Some(pane.to_string());
        ev
    }

    fn log_with(line: &str) -> String {
        format!("{}{line}\n", deck_log(false))
    }

    /// Reverse: the old CLI's report reached the branch daemon, which logged
    /// delivering it as a note, but the work-done half failed for another
    /// reason the log does not show (no H5 refusal). The note line explains
    /// nothing: it is not an effect, so the run FAILS.
    #[test]
    fn a_note_logged_while_delivery_failed_for_another_reason_is_not_explained() {
        let mut ev = work_done_run(Direction::Reverse, true, "3");
        explain(&mut ev, Direction::Reverse, &log_with(NOTE_LINE));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        assert!(ev.tells.iter().all(|t| t.declared_break.is_none()));
    }

    /// Forward: the branch CLI declares the break, and the old daemon logged
    /// the CLI's question on the reviewer's pane as a line it cannot decode. A
    /// DECLARED BREAK.
    #[test]
    fn an_unread_question_is_the_task_scoped_break_forward() {
        let mut ev = work_done_run(Direction::Forward, true, "3");
        explain(&mut ev, Direction::Forward, &log_with(QUESTION_LINE));
        let v = ev.verdict();
        assert!(is_declared_break(&v), "{}", v.label());
        let tell = ev.tells.iter().find(|t| t.id == "tell-4").unwrap();
        let detail = &tell.declared_break.as_ref().unwrap().detail;
        assert!(
            detail.contains("The client's build declares it"),
            "{detail}"
        );
    }

    /// Controls: each direction's witness explains only its own direction, the
    /// break must be declared, and the line must name the subject pane.
    #[test]
    fn the_task_scoped_break_needs_its_own_witness_declaration_and_pane() {
        // The forward witness in the reverse direction.
        let mut ev = work_done_run(Direction::Reverse, true, "3");
        explain(&mut ev, Direction::Reverse, &log_with(QUESTION_LINE));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        // Not declared.
        let mut ev = work_done_run(Direction::Forward, false, "3");
        explain(&mut ev, Direction::Forward, &log_with(QUESTION_LINE));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        // Another pane, one whose id extends the subject's.
        let mut ev = work_done_run(Direction::Forward, true, "33");
        explain(&mut ev, Direction::Forward, &log_with(QUESTION_LINE));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        let thirty_three = QUESTION_LINE.replace(r#""pane_id":"3""#, r#""pane_id":"33""#);
        let mut ev = work_done_run(Direction::Forward, true, "3");
        explain(&mut ev, Direction::Forward, &log_with(&thirty_three));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
    }

    /// The branch daemon's H5 refusal of an older CLI's `inbox fetch` in pane
    /// `pane`.
    fn h5_line(pane: &str) -> String {
        format!(
            "2026-10-24T09:12:01.000000Z  WARN dot_agent_deck::daemon: inbox: refused a fetch \
             the way only an older dot-agent-deck asks; nothing was taken pane_id={pane} \
             consumer=Cli ack=false"
        )
    }

    /// What the branch CLI's `inbox fetch` captured in pane `pane` against a
    /// daemon that does not serve receipts: the capture's pane line, the
    /// CLI's stderr, its exit status.
    fn h4_capture(pane: &str) -> String {
        format!(
            "pane={pane}\nError: {}\nexit=1\n",
            "Could not fetch messages: the deck daemon did not confirm that it can hand them \
             to this `dot-agent-deck inbox fetch`, so no message was taken and they stay \
             waiting."
        )
    }

    /// The branch's `daemon hello`, declaring the round's other two breaks
    /// too.
    fn inbox_hello() -> String {
        task_scoped_hello().replace(
            r#""1666-task-scoped-completion-and-recovery"]"#,
            r#""1666-task-scoped-completion-and-recovery","1666-remaining-inbox-one-shot-receipts","1666-legacy-inbox-delivery-withdrawal"]"#,
        )
    }

    /// A run of `direction` in which `tell`'s halves held or failed as
    /// `parts`, each half measuring the panes `part_panes` names, with the
    /// client's `outputs` per half, the branch declaring the round's breaks
    /// when `declared`. Every other contract tell passed.
    fn run_of(
        direction: Direction,
        declared: bool,
        tell: &str,
        parts: &[(&str, bool)],
        part_panes: &[(&str, &str)],
        outputs: &[(&str, &str, String)],
    ) -> Evidence {
        let mut ev = Evidence {
            branch: "6f38d2a559ba4bca111f2419d9c71e0e80d8c77e".into(),
            direction,
            old_hello: V0451_HELLO.into(),
            new_hello: if declared {
                inbox_hello()
            } else {
                BRANCH_HELLO.to_string()
            },
            ..Default::default()
        };
        for id in ["tell-1", "tell-2", "tell-3", "tell-4", ROLE_SET_TELL] {
            if id == tell {
                ev.tell_in_parts(id, "t", parts, "measured");
                let t = ev.tells.last_mut().unwrap();
                t.part_panes = part_panes
                    .iter()
                    .map(|(h, p)| (h.to_string(), p.to_string()))
                    .collect();
                t.client_outputs = outputs
                    .iter()
                    .map(|(h, p, o)| (h.to_string(), p.to_string(), o.clone()))
                    .collect();
            } else {
                ev.tell(id, "t", Verdict::Pass, "measured");
            }
        }
        ev
    }

    /// Reverse, tell 3: the old CLI's fetch in the coder's pane `2` is the
    /// legacy fetch, which the branch daemon refused and logged. A DECLARED
    /// BREAK; not without that line, not for another pane, not undeclared.
    #[test]
    fn a_refused_legacy_fetch_is_h5_in_reverse_on_tell_3() {
        let run = |declared: bool| {
            run_of(
                Direction::Reverse,
                declared,
                "tell-3",
                &[("fetch", false)],
                &[("fetch", "2")],
                &[],
            )
        };
        let mut ev = run(true);
        explain(&mut ev, Direction::Reverse, &log_with(&h5_line("2")));
        assert!(is_declared_break(&ev.verdict()), "{}", ev.verdict().label());
        assert!(ev.verdict().label().contains(LEGACY_FETCH_WITHDRAWN));
        for (declared, log) in [
            (true, deck_log(false)),
            (true, log_with(&h5_line("22"))),
            (true, log_with(&h5_line("5"))),
            (false, log_with(&h5_line("2"))),
        ] {
            let mut ev = run(declared);
            explain(&mut ev, Direction::Reverse, &log);
            assert_eq!(ev.verdict(), RunVerdict::Fail, "{declared} {log}");
        }
    }

    /// Reverse, tell 4: the old CLI's report reached the orchestrator's inbox
    /// as a note, and the orchestrator's old fetch in pane `1` was refused.
    /// The work-done half measured the reviewer's pane `3` and the
    /// orchestrator's `1`; the refusal names `1`. A DECLARED BREAK; the note
    /// alone is not, nor a refusal for the reviewer's own fetch on `3` when
    /// the half recorded only the reviewer's pane.
    #[test]
    fn a_refused_legacy_fetch_is_h5_in_reverse_on_tell_4() {
        let run = |panes: &[(&str, &str)]| {
            run_of(
                Direction::Reverse,
                true,
                "tell-4",
                &[("work-done", false), ("status", true)],
                panes,
                &[],
            )
        };
        let both = [("work-done", "3"), ("work-done", "1")];
        let mut ev = run(&both);
        explain(
            &mut ev,
            Direction::Reverse,
            &log_with(&format!("{NOTE_LINE}\n{}", h5_line("1"))),
        );
        assert!(is_declared_break(&ev.verdict()), "{}", ev.verdict().label());
        let mut ev = run(&both);
        explain(&mut ev, Direction::Reverse, &log_with(NOTE_LINE));
        assert_eq!(ev.verdict(), RunVerdict::Fail, "the note alone");
        let mut ev = run(&[("work-done", "3")]);
        explain(&mut ev, Direction::Reverse, &log_with(&h5_line("1")));
        assert_eq!(
            ev.verdict(),
            RunVerdict::Fail,
            "a pane the half did not measure"
        );
    }

    /// Where, in the run's order, a line of the daemon's log was written
    /// relative to tell-4's `work-done` half.
    #[derive(Clone, Copy, Debug)]
    enum At {
        /// Before the reviewer reported: its own fetch of the delegate.
        BeforeReport,
        /// After the reviewer typed `work-done`, before the orchestrator's
        /// fetch.
        Report,
        /// During the orchestrator's fetch of the report.
        Fetch,
        /// During the orchestrator's listing.
        Listing,
    }

    /// Tell-4 as production records it: the reviewer (pane `3`) reports, the
    /// orchestrator (pane `1`) fetches and lists; the `work-done` half failed
    /// and the status half held. `fetched` when the orchestrator's fetch handed
    /// the report over, so only its listing failed. Each line of `lines` is
    /// written to the log at its place in the run. Returns the run and its log.
    fn tell4_run(
        direction: Direction,
        fetched: bool,
        lines: &[(At, String)],
    ) -> (Evidence, String) {
        let _ = fetched;
        let mut log = deck_log(false);
        for (at, line) in lines {
            let _ = at;
            log.push_str(line);
            log.push('\n');
        }
        let ev = run_of(
            direction,
            true,
            "tell-4",
            &[("work-done", false), ("status", true)],
            &[("work-done", "3"), ("work-done", "1")],
            &[],
        );
        (ev, log)
    }

    fn explained(direction: Direction, fetched: bool, lines: &[(At, String)]) -> RunVerdict {
        let (mut ev, log) = tell4_run(direction, fetched, lines);
        explain(&mut ev, direction, &log);
        ev.verdict()
    }

    /// Reverse, tell 4 (review R31 / U2-2): H5 explains the orchestrator's
    /// failed fetch of the report only with a refusal of THAT fetch. The
    /// reviewer's own earlier fetch of its delegate, refused on pane `3`, is a
    /// different operation and explains nothing — nor does a refusal on the
    /// orchestrator's pane from before the report was sent, nor any refusal
    /// once the fetch handed the report over and only the listing failed.
    #[test]
    fn h5_explains_tell_4_only_with_the_orchestrators_own_fetch() {
        let r = Direction::Reverse;
        let v = explained(r, false, &[(At::Fetch, h5_line("1"))]);
        assert!(is_declared_break(&v), "{}", v.label());
        let v = explained(
            r,
            false,
            &[(At::BeforeReport, h5_line("3")), (At::Fetch, h5_line("1"))],
        );
        assert!(is_declared_break(&v), "{}", v.label());
        for (fetched, lines, why) in [
            (
                false,
                vec![(At::BeforeReport, h5_line("3"))],
                "only the reviewer's earlier refusal",
            ),
            (
                false,
                vec![(At::Fetch, h5_line("3"))],
                "a refusal on the reporter's pane during the fetch",
            ),
            (
                false,
                vec![(At::BeforeReport, h5_line("1"))],
                "an earlier refusal on the orchestrator's pane",
            ),
            (
                false,
                vec![(At::Listing, h5_line("1"))],
                "a refusal after the fetch ended",
            ),
            (
                true,
                vec![(At::Fetch, h5_line("1"))],
                "the fetch handed the report over; only the listing failed",
            ),
        ] {
            assert_eq!(explained(r, fetched, &lines), RunVerdict::Fail, "{why}");
        }
    }

    /// Forward, tell 4: the task-scoped break explains the half only with the
    /// REPORTER's unread capability question, sent after it typed `work-done`
    /// — not one naming the orchestrator's pane, not one from before the
    /// report, and not when the report was fetched and only the listing failed.
    #[test]
    fn the_task_scoped_break_explains_tell_4_only_with_the_reporters_question() {
        let f = Direction::Forward;
        let v = explained(f, false, &[(At::Report, QUESTION_LINE.to_string())]);
        assert!(is_declared_break(&v), "{}", v.label());
        let on_orchestrator = QUESTION_LINE.replace(r#""pane_id":"3""#, r#""pane_id":"1""#);
        for (fetched, lines, why) in [
            (
                false,
                vec![(At::Report, on_orchestrator)],
                "the question on the orchestrator's pane",
            ),
            (
                false,
                vec![(At::BeforeReport, QUESTION_LINE.to_string())],
                "a question from before the report",
            ),
            (
                true,
                vec![(At::Report, QUESTION_LINE.to_string())],
                "the report was fetched; only the listing failed",
            ),
        ] {
            assert_eq!(explained(f, fetched, &lines), RunVerdict::Fail, "{why}");
        }
    }

    /// Forward, tell 3: the branch CLI's fetch in the coder's pane `2` read
    /// that the old daemon does not serve receipts, took nothing and said so;
    /// the old daemon logged nothing. The CLIENT declares the break, so its own
    /// output is the witness: a DECLARED BREAK. Not for output captured in
    /// another pane, not without the line, not when the line is only in the
    /// daemon's log, and not undeclared.
    #[test]
    fn an_unconfirmed_receipt_fetch_is_h4_forward_on_tell_3() {
        let run = |declared: bool, outputs: &[(&str, &str, String)]| {
            run_of(
                Direction::Forward,
                declared,
                "tell-3",
                &[("fetch", false)],
                &[("fetch", "2")],
                outputs,
            )
        };
        let mut ev = run(true, &[("fetch", "2", h4_capture("2"))]);
        explain(&mut ev, Direction::Forward, &deck_log(false));
        assert!(is_declared_break(&ev.verdict()), "{}", ev.verdict().label());
        let detail = &ev.tells[2].declared_break.as_ref().unwrap().detail;
        assert!(detail.contains("the client printed, in pane 2"), "{detail}");
        assert!(
            detail.contains("The client's build declares it"),
            "{detail}"
        );
        for (declared, outputs, log) in [
            (true, vec![("fetch", "5", h4_capture("5"))], deck_log(false)),
            (
                true,
                vec![("fetch", "2", "pane=2\nexit=0\n".to_string())],
                deck_log(false),
            ),
            (true, vec![], log_with(&h4_capture("2"))),
            (
                false,
                vec![("fetch", "2", h4_capture("2"))],
                deck_log(false),
            ),
        ] {
            let mut ev = run(declared, &outputs);
            explain(&mut ev, Direction::Forward, &log);
            assert_eq!(ev.verdict(), RunVerdict::Fail, "{declared} {outputs:?}");
        }
    }

    /// Only a CLIENT-declared break may name the client's output as its
    /// witness: a daemon's refusal is the daemon's to log.
    #[test]
    fn only_client_declared_effects_take_the_clients_output_as_witness() {
        for effect in KNOWN_EFFECTS {
            if effect.witness == Witness::ClientOutput {
                assert_eq!(effect.declared_by, Declarer::Client, "{}", effect.id);
                assert!(
                    matches!(effect.pane_field, PaneField::Capture),
                    "{}",
                    effect.id
                );
                assert!(!effect.client_text.is_empty(), "{}", effect.id);
            } else {
                assert!(
                    !matches!(effect.pane_field, PaneField::Capture),
                    "{}",
                    effect.id
                );
            }
        }
    }

    fn deck_log(with_refusal: bool) -> String {
        let mut log = String::from(
            "2026-10-05T17:07:52.100000Z  INFO dot_agent_deck::daemon: Attach protocol listening\n",
        );
        if with_refusal {
            log.push_str(REFUSAL_LINE);
            log.push('\n');
        }
        log
    }

    /// The PR #1595 reverse run, reduced to what decides its verdict: every
    /// expected tell measured, tell-4's `work-done` half held and its `status`
    /// half did not, with the daemon answering `daemon status` and naming the
    /// reviewer's pane `3`.
    fn reverse_run(new_hello: &str, work_done: bool) -> Evidence {
        reverse_run_with(new_hello, work_done, "status", Some("3"))
    }

    fn reverse_run_with(
        new_hello: &str,
        work_done: bool,
        status_part: &str,
        subject_pane: Option<&str>,
    ) -> Evidence {
        let mut ev = Evidence {
            branch: "6f38d2a559ba4bca111f2419d9c71e0e80d8c77e".into(),
            direction: Direction::Reverse,
            old_hello: V0451_HELLO.into(),
            new_hello: new_hello.into(),
            ..Default::default()
        };
        for id in ["tell-1", "tell-2", "tell-3", ROLE_SET_TELL] {
            ev.tell(id, "t", Verdict::Pass, "measured");
        }
        ev.tell_in_parts(
            "tell-4",
            "hooks (work-done, status) still arrived",
            &[("work-done", work_done), (status_part, false)],
            "status: the status never changed",
        );
        ev.tells.last_mut().unwrap().subject_pane = subject_pane.map(str::to_string);
        assert!(
            CONTRACT_TELLS
                .iter()
                .all(|id| ev.tells.iter().any(|t| t.id == *id))
        );
        ev
    }

    fn is_declared_break(v: &RunVerdict) -> bool {
        v.label().starts_with("DECLARED BREAK")
    }

    /// Issue #1596's reproduction: the reverse pairing of PR #1595's run. The
    /// branch daemon declares `318-hook-event-capability-token`, v0.45.1 does
    /// not, and the daemon logged refusing the old CLI's token-less status
    /// event. That is the outcome the declared break says will happen, so the
    /// run is a DECLARED BREAK, not a FAIL.
    #[test]
    fn a_failed_tell_explained_by_a_declared_break_is_reported_as_one() {
        let mut ev = reverse_run(BRANCH_HELLO, true);
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        let v = ev.verdict();
        assert!(
            is_declared_break(&v),
            "expected DECLARED BREAK, got {}",
            v.label()
        );
        assert!(
            v.label().contains("318-hook-event-capability-token"),
            "{}",
            v.label()
        );
    }

    /// Control: the same failure with no break declared between the two builds
    /// is a regression, and still fails the run.
    #[test]
    fn the_same_failure_with_no_declared_break_still_fails() {
        let undeclared = BRANCH_HELLO.replace(r#","318-hook-event-capability-token""#, "");
        let mut ev = reverse_run(&undeclared, true);
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
    }

    /// Control: the break is declared, but the daemon logged no refusal for
    /// it, so nothing shows that the break is what failed the tell.
    #[test]
    fn a_declared_break_without_its_refusal_in_the_log_still_fails() {
        let mut ev = reverse_run(BRANCH_HELLO, true);
        explain(&mut ev, Direction::Reverse, &deck_log(false));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        assert!(
            ev.steps.iter().any(|s| s.contains("logged no refusal")),
            "{:?}",
            ev.steps
        );
    }

    /// Control: #318 explains the status half only. With the work-done half
    /// failing too, part of the tell is unexplained, so the run fails.
    #[test]
    fn a_failed_half_no_declared_break_explains_still_fails() {
        let mut ev = reverse_run(BRANCH_HELLO, false);
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        assert!(ev.tells.iter().all(|t| t.declared_break.is_none()));
    }

    /// Control: a second, unrelated failing tell is not covered by the break
    /// that explains tell-4.
    #[test]
    fn another_failed_tell_beside_an_explained_one_still_fails() {
        let mut ev = reverse_run(BRANCH_HELLO, true);
        ev.tells
            .iter_mut()
            .find(|t| t.id == "tell-3")
            .unwrap()
            .verdict = Verdict::Fail;
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        assert!(
            ev.tells
                .iter()
                .find(|t| t.id == "tell-4")
                .unwrap()
                .declared_break
                .is_some(),
            "tell-4 is still explained; tell-3 is what fails the run"
        );
    }

    /// Control (review of #1597): `daemon status` could not be read at all, so
    /// the status half measured nothing. A refusal in the log does not make
    /// that a declared break.
    #[test]
    fn a_status_query_that_could_not_be_read_is_not_a_declared_break() {
        let mut ev = reverse_run_with(BRANCH_HELLO, true, "status-query", None);
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        let mut ev = reverse_run_with(BRANCH_HELLO, true, "status-query", Some("3"));
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
    }

    /// Control: the refusal names a pane other than the one the failed half
    /// measured, so it is some other message's refusal.
    #[test]
    fn a_refusal_for_another_pane_does_not_explain_the_failure() {
        let mut ev = reverse_run_with(BRANCH_HELLO, true, "status", Some("2"));
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        assert!(
            ev.steps.iter().any(|s| s.contains("claimed_pane=2")),
            "{:?}",
            ev.steps
        );
        // `claimed_pane=3` must not satisfy a pane `3x`, nor `3` a `33`.
        let mut ev = reverse_run_with(BRANCH_HELLO, true, "status", Some("33"));
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
    }

    /// Control: a run that did not record the pane cannot tie a refusal to it.
    #[test]
    fn a_failed_half_with_no_recorded_pane_is_not_explained() {
        let mut ev = reverse_run_with(BRANCH_HELLO, true, "status", None);
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
    }

    /// Control: forward, the daemon is the previous release, which does not
    /// declare #318, so the break cannot be what refused the client.
    #[test]
    fn a_break_declared_only_by_the_client_side_does_not_explain_a_failure() {
        let mut ev = reverse_run(BRANCH_HELLO, true);
        ev.direction = Direction::Forward;
        explain(&mut ev, Direction::Forward, &deck_log(true));
        assert_eq!(ev.verdict(), RunVerdict::Fail);
    }

    /// A declared break is a verdict about a COMPLETE run: an unmeasured tell
    /// still makes it INCOMPLETE, and an isolation failure still dominates.
    #[test]
    fn a_declared_break_ranks_below_incomplete_and_isolation() {
        let mut ev = reverse_run(BRANCH_HELLO, true);
        ev.tells
            .iter_mut()
            .find(|t| t.id == "tell-2")
            .unwrap()
            .verdict = Verdict::NotChecked;
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        assert!(
            matches!(ev.verdict(), RunVerdict::Incomplete(_)),
            "{:?}",
            ev.verdict()
        );

        let mut ev = reverse_run(BRANCH_HELLO, true);
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        ev.isolation_failed("host endpoint changed");
        assert!(
            matches!(ev.verdict(), RunVerdict::Incomplete(_)),
            "{:?}",
            ev.verdict()
        );
    }

    /// The explanation survives the inner half's JSON hand-over and reaches the
    /// evidence file, beside the failed half and the refusal line.
    #[test]
    fn the_explanation_round_trips_and_is_rendered() {
        let mut ev = reverse_run(BRANCH_HELLO, true);
        explain(&mut ev, Direction::Reverse, &deck_log(true));
        let back: Evidence = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert!(matches!(back.verdict(), RunVerdict::DeclaredBreak(_)));
        let md = back.render();
        assert!(md.contains("**Verdict: DECLARED BREAK"), "{md}");
        assert!(
            md.contains("explained by a declared contract break"),
            "{md}"
        );
        assert!(md.contains("> failed: status"), "{md}");
        assert!(md.contains("reason=\"missing_token\""), "{md}");
    }

    /// Strip Rust's string-continuation escapes (`\` + newline + indentation)
    /// so a message split across source lines reads as the daemon logs it.
    fn joined(src: &str) -> String {
        let mut out = String::new();
        let mut rest = src;
        while let Some(i) = rest.find("\\\n") {
            out.push_str(&rest[..i]);
            rest = rest[i + 2..].trim_start();
        }
        out.push_str(rest);
        out
    }

    /// Whether one `<open>…);` call in `src` contains every text — so a field
    /// that some OTHER call also spells does not satisfy the pin.
    fn one_call_contains(src: &str, open: &str, texts: &[&str]) -> bool {
        src.match_indices(open).any(|(i, _)| {
            let call = &src[i..];
            let call = &call[..call.find(");").unwrap_or(call.len())];
            texts.iter().all(|t| call.contains(t))
        })
    }

    #[test]
    fn a_pin_spread_across_two_log_calls_does_not_hold() {
        let src = "warn!(verb = \"agent_event\", \"refused a status event\");\n\
                   warn!(claimed_pane = %escape_id_for_log(p), reason = refusal.code(), \"other\");";
        let site = [
            "refused a status event",
            "verb = \"agent_event\",",
            "claimed_pane = %escape_id_for_log(",
        ];
        assert!(!one_call_contains(src, "warn!(", &site));
        let joined_call = "warn!(verb = \"agent_event\", claimed_pane = %escape_id_for_log(p), \
                           \"refused a status event\");";
        assert!(one_call_contains(joined_call, "warn!(", &site));
    }

    /// Every known effect names an entry `CONTRACT_BREAKS` really carries and
    /// a tell the scenario records, and every one of its source pins is text
    /// the daemon's source really contains, so a renamed break or reworded
    /// refusal turns this red rather than quietly turning a declared break back
    /// into a FAIL on every PR. Each effect needs at least as many pins as
    /// signature pieces plus its pane field.
    #[test]
    fn every_known_effect_is_pinned_to_the_source() {
        let protocol = include_str!("../../../src/daemon_protocol.rs");
        let list_start = protocol
            .find("pub const CONTRACT_BREAKS: &[&str] = &[")
            .expect("CONTRACT_BREAKS in src/daemon_protocol.rs");
        let list = &protocol[list_start..];
        let list = &list[..list.find("];").expect("the list's end")];
        let source = |file: &str| -> String {
            joined(match file {
                "src/daemon.rs" => include_str!("../../../src/daemon.rs"),
                "src/event.rs" => include_str!("../../../src/event.rs"),
                "src/hook_provenance.rs" => include_str!("../../../src/hook_provenance.rs"),
                "src/inbox_cli.rs" => include_str!("../../../src/inbox_cli.rs"),
                other => panic!("a source pin names {other}, which this test does not read"),
            })
        };
        assert!(!KNOWN_EFFECTS.is_empty());
        for effect in KNOWN_EFFECTS {
            assert!(
                list.contains(&format!("\"{}\"", effect.id)),
                "`{}` is not in CONTRACT_BREAKS",
                effect.id
            );
            assert!(
                CONTRACT_TELLS.contains(&effect.tell),
                "`{}` names an unknown tell",
                effect.id
            );
            let (site_file, site) = effect.log_site;
            // A client's line names no pane, so it pins no pane field.
            let pane_pins = usize::from(effect.witness == Witness::DaemonLog);
            assert!(
                site.len() >= effect.log_signature.len() + pane_pins,
                "`{}` pins fewer texts in its log call than it has signature pieces and a \
                 pane field",
                effect.id
            );
            let open = match effect.witness {
                Witness::DaemonLog => "warn!(",
                Witness::ClientOutput => effect.client_text,
            };
            assert!(
                one_call_contains(&source(site_file), open, site),
                "`{}`: no single `{open}…` in {site_file} contains all of {site:?}",
                effect.id
            );
            for (file, text) in effect.source_pins {
                assert!(
                    source(file).contains(text),
                    "`{}`: {file} no longer contains {text:?}",
                    effect.id
                );
            }
        }
        // The pins really are what the #1595 run's daemon wrote, and what
        // the task-scoped break's two lines say.
        let h5 = h5_line("3");
        let h4 = h4_capture("3");
        for (effect, line) in KNOWN_EFFECTS.iter().zip([
            REFUSAL_LINE,
            QUESTION_LINE,
            h5.as_str(),
            h5.as_str(),
            h4.lines().nth(1).unwrap(),
        ]) {
            assert!(
                effect.log_signature.iter().all(|sig| line.contains(sig))
                    && (effect.witness == Witness::ClientOutput
                        || effect.pane_field.names(line, "3")),
                "`{}` does not match {line}",
                effect.id
            );
        }
        // The client's line is the constant the CLI prints, whole.
        assert!(h4.contains(&format!("{} ", KNOWN_EFFECTS[4].log_signature[0])));
        assert_eq!(KNOWN_EFFECTS.len(), 5);
    }
}
