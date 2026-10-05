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
//! * the build serving the DAEMON lists the break and the build acting as the
//!   CLIENT does not — so the break is declared between these two builds, in
//!   the direction that refuses the client;
//! * one line of the sandbox `deck.log` carries every piece of the effect's
//!   log signature and names the pane the failed half measured
//!   ([`crate::report::Tell::subject_pane`]): the daemon refused that pane's
//!   message, for the reason the break gives, in this run.
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
    /// Substrings that must all appear on ONE line of the sandbox `deck.log`:
    /// the daemon's refusal, in its own words.
    pub log_signature: &'static [&'static str],
    /// The field that line names the refused message's pane in, as
    /// `<field><pane>`; the pane must be the tell's
    /// [`subject_pane`](crate::report::Tell::subject_pane).
    pub pane_field: &'static str,
    /// The one log call that writes the refusal, as `(source file, texts)`:
    /// every text must appear inside the SAME `warn!(…);` call there — its
    /// message and each field the signature and pane field read — so renaming
    /// a field in that call fails `every_known_effect_is_pinned_to_the_source`
    /// even while another log call still spells it the old way. A message split
    /// across source lines is matched with its continuation escapes joined.
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
    /// Why the break fails that half, for the evidence file.
    pub why: &'static str,
}

/// Every break known to fail a tell, and how. Keep each `log_signature` pinned
/// to the daemon's source by the test below.
pub const KNOWN_EFFECTS: &[KnownEffect] = &[KnownEffect {
    id: "318-hook-event-capability-token",
    tell: "tell-4",
    part: "status",
    log_signature: &[
        "refused a status event whose hook capability token",
        "verb=\"agent_event\"",
        "reason=\"missing_token\"",
    ],
    pane_field: "claimed_pane=",
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
    why: "a daemon declaring it refuses a status report from a deck-spawned pane that carries \
          no hook capability token, and a CLI from a build that does not declare it sends \
          `agent-event` without one",
}];

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
    let declared: Vec<&String> = daemon.iter().filter(|b| !client.contains(b)).collect();
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
            match explain_part(
                &tell.id,
                part,
                tell.subject_pane.as_deref(),
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

/// Whether a declared break explains one failed half of one tell: `Ok` with
/// the explanation's summary and detail, or `Err` with why not.
fn explain_part(
    tell: &str,
    part: &str,
    subject_pane: Option<&str>,
    declared: &[&String],
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
        if !declared.iter().any(|b| b.as_str() == effect.id) {
            why_not.push(format!(
                "`{}` is not declared by the daemon's build without the client's",
                effect.id
            ));
            continue;
        }
        let Some(pane) = subject_pane else {
            why_not.push(format!(
                "`{}` is declared, but the run did not record which pane the failed half \
                 measured, so no refusal can be tied to it",
                effect.id
            ));
            continue;
        };
        let pane_token = format!("{}{pane}", effect.pane_field);
        let Some(line) = deck_log.lines().find(|l| {
            effect.log_signature.iter().all(|sig| l.contains(sig))
                && l.split_whitespace().any(|w| w == pane_token)
        }) else {
            why_not.push(format!(
                "`{}` is declared, but the daemon logged no refusal matching {:?} for \
                 `{pane_token}`",
                effect.id, effect.log_signature
            ));
            continue;
        };
        return Ok((
            format!("`{part}` half: `{}`", effect.id),
            format!(
                "`{part}` half: `{}` — {}. The daemon's build declares it and the client's does \
                 not; the daemon logged: `{}`",
                effect.id,
                effect.why,
                line.trim()
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
                "src/hook_provenance.rs" => include_str!("../../../src/hook_provenance.rs"),
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
            assert!(
                site.len() > effect.log_signature.len(),
                "`{}` pins fewer texts in its log call than it has signature pieces and a \
                 pane field",
                effect.id
            );
            assert!(
                one_call_contains(&source(site_file), "warn!(", site),
                "`{}`: no single `warn!(…);` call in {site_file} contains all of {site:?}",
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
        // The pins really are what the #1595 run's daemon wrote.
        let status = &KNOWN_EFFECTS[0];
        assert!(
            status
                .log_signature
                .iter()
                .all(|sig| REFUSAL_LINE.contains(sig))
                && REFUSAL_LINE.contains(&format!("{}3 ", status.pane_field))
        );
    }
}
