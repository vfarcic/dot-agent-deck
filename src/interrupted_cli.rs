//! PRD #1790 stage 2: the `dot-agent-deck interrupted` command group —
//! `list`, `show`, `reclaim` and `dismiss` — over the work a daemon restart
//! left behind (and reports left for a caller that is gone, decision D3).
//!
//! The daemon derives the list and does every reclaim and dismissal
//! ([`crate::interrupted_work_daemon`]); this module turns the command line
//! and the environment into a request through [`DaemonClient`], which
//! withholds each one from a daemon that does not advertise
//! [`crate::daemon_protocol::CAP_INTERRUPTED_WORK`], and turns the answer into
//! text and an exit code. Every sentence it prints is one of the shared
//! wording constants of [`crate::interrupted_work`] (decision D2), so the CLI,
//! the TUI and the desktop say the same thing.
//!
//! Reclaim and dismiss carry the caller's pane claim when the shell has a deck
//! pane's identity, as `close` does ([`crate::close_cli::ambient_identity`]);
//! the daemon refuses an agent (decision D4).
//!
//! Exit codes: 0 done; 1 refused, kept, not listed or failed; 2 the running
//! daemon does not support interrupted work; 3 no deck daemon is running.

use crate::close_cli::{
    AmbientIdentity, EXIT_DAEMON_TOO_OLD, EXIT_OK, EXIT_REFUSED, EXIT_UNREACHABLE,
};
use crate::daemon_client::{ClientError, DaemonClient, GatedQuery, PinnedQuery};
use crate::interrupted_work::{
    self as words, DismissOutcome, InterruptedItem, InterruptedList, InterruptedShow, ItemKind,
    ReclaimOutcome, ReclaimVerdict, RefusalReason,
};
use crate::roles_cli::CliOutput;

/// `value` made safe to print on one terminal line: control and bidi
/// characters escaped, cut to [`words::MAX_SHOWN_CHARS`]
/// ([`words::display_value`], which the TUI shows values through too).
fn shown(value: &str) -> String {
    words::display_value(value)
}

fn ok(stdout: String) -> CliOutput {
    CliOutput {
        stdout,
        stderr: String::new(),
        code: EXIT_OK,
    }
}

fn failed(message: impl std::fmt::Display, code: u8) -> CliOutput {
    CliOutput {
        stdout: String::new(),
        stderr: format!("{message}\n"),
        code,
    }
}

/// Whether a client error means nothing answered at the attach socket.
fn is_unreachable(e: &ClientError) -> bool {
    match e {
        ClientError::SocketMissing(_) => true,
        ClientError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::AddrNotAvailable
        ),
        _ => false,
    }
}

fn client_error(e: ClientError) -> CliOutput {
    if is_unreachable(&e) {
        return failed(words::NO_DAEMON, EXIT_UNREACHABLE);
    }
    match e {
        // PR #2431 round 5 (Qodo): a running daemon on another attach
        // protocol cannot serve interrupted work, which is exit 2. Only a
        // client that checks the protocol meets this: the one-shot CLI's
        // does not (`run_interrupted_cli` in `main.rs`), so it is answered
        // by the daemon's capabilities alone.
        ClientError::ProtocolMismatch { .. } => unsupported(),
        // The daemon's own sentence (an id that matches nothing, say).
        ClientError::Server(message) => failed(shown(&message), EXIT_REFUSED),
        other => failed(
            format!("{} {}", words::DAEMON_FAILED, shown(&other.to_string())),
            EXIT_REFUSED,
        ),
    }
}

fn unsupported() -> CliOutput {
    failed(words::UNSUPPORTED, EXIT_DAEMON_TOO_OLD)
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// `interrupted list`: the list as the daemon answered it with `json`,
/// otherwise as text. `git` asks for each item's Git and pull request facts.
pub async fn run_list(client: &DaemonClient, json: bool, git: bool) -> CliOutput {
    match client.list_interrupted_work(git, None).await {
        Ok(GatedQuery::Answered(list)) => {
            if json {
                match serde_json::to_string_pretty(&list) {
                    Ok(text) => ok(format!("{text}\n")),
                    Err(e) => failed(format!("{} {e}", words::DAEMON_FAILED), EXIT_REFUSED),
                }
            } else {
                ok(render_list(&list))
            }
        }
        Ok(GatedQuery::Unsupported) => unsupported(),
        Err(e) => client_error(e),
    }
}

/// The list as text: the title, the units, orchestrations and issue
/// dispatches, then plain agents under their own heading (decision D1), and
/// the footer.
pub fn render_list(list: &InterruptedList) -> String {
    let mut out = format!("{}\n", words::INTERRUPTED_WORK_TITLE);
    if list.incomplete {
        out.push_str(&format!("{}\n", words::LIST_INCOMPLETE));
    }
    if list.items.is_empty() {
        out.push_str(&format!("\n{}\n", words::NOTHING_INTERRUPTED));
        return out;
    }
    let (agents, work): (Vec<&InterruptedItem>, Vec<&InterruptedItem>) = list
        .items
        .iter()
        .partition(|item| item.kind == ItemKind::Agent);
    for item in &work {
        out.push('\n');
        out.push_str(&render_item(item));
    }
    if !agents.is_empty() {
        out.push_str(&format!("\n{}\n", words::OTHER_AGENTS_GROUP));
        for item in &agents {
            out.push('\n');
            out.push_str(&render_item(item));
        }
    }
    out.push('\n');
    out.push_str(&words::interrupted_footer(list.items.len()));
    out.push('\n');
    if list.omitted > 0 {
        out.push_str(&words::omitted_sentence(list.omitted));
        out.push('\n');
    }
    out
}

/// One item: its id, kind and name, then one indented line per fact. As a
/// list carries it: its first report entries, the next page starting after
/// them.
pub fn render_item(item: &InterruptedItem) -> String {
    render_item_paged(item, None)
}

/// [`render_item`] for an item a show carries, whose report entries are the
/// page `(reports_from, next_reports_from)` of its kept reports.
fn render_item_paged(item: &InterruptedItem, page: Option<(usize, Option<usize>)>) -> String {
    let mut out = format!("{}  {}", item.id, words::kind_label(item.kind));
    if let Some(name) = &item.name {
        out.push_str(&format!("  {}", shown(name)));
    }
    out.push('\n');
    let mut line = |label: &str, value: String| out.push_str(&format!("    {label}: {value}\n"));
    if let Some(issue) = item.issue {
        let repo = item.repo.as_deref().map(shown).unwrap_or_default();
        line(words::LABEL_ISSUE, format!("{repo}#{issue}"));
    }
    if let Some(branch) = &item.branch {
        line(words::LABEL_BRANCH, shown(branch));
    }
    if let Some(worktree) = &item.worktree {
        let label = if item.kind == ItemKind::Agent {
            words::LABEL_FOLDER
        } else {
            words::LABEL_WORKTREE
        };
        line(label, shown(worktree));
    }
    if let Some(launcher) = &item.launcher {
        line(words::LABEL_STARTED_BY, shown(launcher));
    }
    line(
        words::LABEL_LAST_ENTRY,
        format!(
            "{}, {}",
            shown(&item.last_entry.kind),
            words::format_time(item.last_entry.ts_ms)
        ),
    );
    line(
        words::LABEL_PROGRESS,
        shown(&words::progress_line(&item.progress)),
    );
    if let Some(git) = &item.git {
        line(words::LABEL_GIT, words::git_facts_line(git));
    }
    for report in &item.reports {
        line(
            words::LABEL_REPORT,
            format!(
                "{}, {}",
                shown(&report.report_id),
                words::kept_until(report.kept_until_ms)
            ),
        );
    }
    let id = item.id.as_str();
    match page {
        None if item.reports_omitted > 0 => {
            out.push_str(&format!(
                "    {} {}\n",
                words::more_reports_sentence(item.reports_omitted, id),
                words::next_reports_sentence(id, item.reports.len())
            ));
        }
        None => {}
        Some((from, next)) => {
            let total = item.reports.len() + item.reports_omitted;
            if (from > 0 || next.is_some()) && !item.reports.is_empty() {
                out.push_str(&format!(
                    "    {}\n",
                    words::report_range_sentence(from + 1, from + item.reports.len(), total)
                ));
            }
            // The reports after this page.
            let after = item.reports_omitted.saturating_sub(from);
            if after > 0 {
                let mut line = words::more_reports_sentence(after, id);
                if let Some(next) = next {
                    line.push(' ');
                    line.push_str(&words::next_reports_sentence(id, next));
                }
                out.push_str(&format!("    {line}\n"));
            }
        }
    }
    if item.cut {
        out.push_str(&format!("    {}\n", words::ITEM_CUT));
    }
    out
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

/// `interrupted show <id> [--report <report id>] [--reports-from <n>]`: the
/// item and the body of its first kept reports, or of the one `report`
/// names, as the daemon framed it (untrusted text written by an agent); with
/// `reports_from`, the page of its kept reports from the `reports_from`-th
/// on, and their bodies.
pub async fn run_show(
    client: &DaemonClient,
    id: &str,
    report: Option<&str>,
    reports_from: Option<usize>,
) -> CliOutput {
    match client
        .show_interrupted_page(id, report, reports_from, None)
        .await
    {
        Ok(GatedQuery::Answered(shown)) => ok(render_show(&shown)),
        Ok(GatedQuery::Unsupported) => unsupported(),
        Err(e) => client_error(e),
    }
}

/// [`run_show`]'s text.
pub fn render_show(show: &InterruptedShow) -> String {
    let mut out = String::new();
    if show.incomplete {
        out.push_str(&format!("{}\n", words::LIST_INCOMPLETE));
    }
    out.push_str(&render_item_paged(
        &show.item,
        Some((show.reports_from, show.next_reports_from)),
    ));
    if show.bodies_omitted > 0 {
        out.push_str(&format!(
            "{}\n",
            words::bodies_omitted_sentence(show.bodies_omitted, show.item.id.as_str())
        ));
    }
    // The item's listed reports past the bodies shown (the first ones, or
    // the one asked for): each can be read by its id. Reports the item left
    // out of its list are counted by `render_item` already.
    let unshown = show
        .item
        .reports
        .len()
        .saturating_sub(show.reports.len() + show.bodies_omitted);
    if unshown > 0 {
        out.push_str(&format!(
            "{}\n",
            words::more_reports_sentence(unshown, show.item.id.as_str())
        ));
    }
    for report in &show.reports {
        out.push_str(&format!(
            "\n{} {}\n",
            words::LABEL_REPORT,
            shown(&report.report_id)
        ));
        match &report.body {
            Some(body) => {
                if report.cut {
                    out.push_str(&format!("{}\n", words::REPORT_CUT));
                }
                // The daemon framed it as untrusted text; it is printed as
                // framed, ending on its own closing marker, its lines and
                // tabs kept and any other control or bidi character escaped
                // (a malformed reply's included).
                let body = words::display_block(body);
                out.push_str(&body);
                if !body.ends_with('\n') {
                    out.push('\n');
                }
            }
            None => out.push_str(&format!(
                "{} {}\n",
                words::REPORT_UNREADABLE,
                shown(report.error.as_deref().unwrap_or_default())
            )),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// reclaim
// ---------------------------------------------------------------------------

/// `interrupted reclaim <id> [--dry-run]`: remove the item's worktree and keep
/// its branch, or say what that would do.
pub async fn run_reclaim(
    client: &DaemonClient,
    id: &str,
    dry_run: bool,
    identity: &AmbientIdentity,
) -> CliOutput {
    match client
        .reclaim_interrupted(id, dry_run, identity.claim(), None)
        .await
    {
        Ok(GatedQuery::Answered(outcome)) => render_reclaim(&outcome),
        Ok(GatedQuery::Unsupported) => unsupported(),
        Err(e) => client_error(e),
    }
}

/// The sentence for a reclaim's outcome, from the shared wording. The TUI
/// says the same ([`crate::ui`]'s interrupted work list).
pub fn reclaim_message(outcome: &ReclaimOutcome) -> String {
    match (&outcome.refusal, outcome.verdict) {
        (Some(refusal), _) if refusal.reason != RefusalReason::Unknown => {
            words::refusal_sentence(refusal.reason).to_string()
        }
        (Some(refusal), _) => shown(&refusal.message),
        (None, ReclaimVerdict::Removed) if !outcome.recorded => {
            words::RECLAIM_NOT_RECORDED.to_string()
        }
        (None, ReclaimVerdict::Unknown) => shown(&outcome.message),
        (None, verdict) => words::reclaim_sentence(verdict).to_string(),
    }
}

/// [`run_reclaim`]'s text and exit code: 0 for a removal the journal
/// recorded or a dry run that would remove, 1 otherwise.
pub fn render_reclaim(outcome: &ReclaimOutcome) -> CliOutput {
    let succeeded = match outcome.verdict {
        ReclaimVerdict::Removed => outcome.recorded,
        ReclaimVerdict::WouldRemove => true,
        _ => false,
    } && outcome.refusal.is_none();
    let mut text = format!("{}\n", reclaim_message(outcome));
    if let Some(worktree) = &outcome.worktree {
        text.push_str(&format!(
            "    {}: {}\n",
            words::LABEL_WORKTREE,
            shown(worktree)
        ));
    }
    if let Some(branch) = &outcome.branch {
        text.push_str(&format!("    {}: {}\n", words::LABEL_BRANCH, shown(branch)));
    }
    for nested in &outcome.nested_worktrees {
        text.push_str(&format!(
            "    {}: {}\n",
            words::LABEL_NESTED_WORKTREE,
            shown(nested)
        ));
    }
    if let Some(sentence) = outcome.unpushed_commits.and_then(words::unpushed_sentence) {
        text.push_str(&format!("{sentence}\n"));
    }
    if succeeded {
        ok(text)
    } else {
        CliOutput {
            stdout: String::new(),
            stderr: text,
            code: EXIT_REFUSED,
        }
    }
}

// ---------------------------------------------------------------------------
// dismiss
// ---------------------------------------------------------------------------

/// `interrupted dismiss`'s arguments, as clap parsed them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DismissArgs {
    pub ids: Vec<String>,
    pub all: bool,
    pub yes: bool,
}

/// How `dismiss --all` without `--yes` asks the person.
pub enum Confirm<'a> {
    /// There is no terminal to ask on: refuse.
    NoTerminal,
    /// Ask this question and answer whether the person agreed.
    Ask(&'a mut dyn FnMut(&str) -> bool),
}

/// `interrupted dismiss <id>... | --all [--yes]`. Without `--yes`, `--all`
/// lists the items first and asks; it then dismisses exactly the items it
/// listed, so one that appears in between is not dismissed unseen, and only
/// on the daemon process that listed them (verification finding A).
pub async fn run_dismiss(
    client: &DaemonClient,
    args: &DismissArgs,
    identity: &AmbientIdentity,
    confirm: Confirm<'_>,
) -> CliOutput {
    if args.all == !args.ids.is_empty() {
        return failed(words::REFUSED_BAD_REQUEST, EXIT_REFUSED);
    }
    let claim = identity.claim();
    let (ids, all, pin) = if args.all && !args.yes {
        let Confirm::Ask(ask) = confirm else {
            return failed(words::DISMISS_ALL_NEEDS_YES, EXIT_REFUSED);
        };
        // An agent is refused whatever it confirms: ask the daemon before
        // asking the person anything.
        if claim.is_some() {
            return send_dismiss(client, Vec::new(), true, claim, None).await;
        }
        let (list, listed_by) = match client.list_interrupted_work_pinned(false, None).await {
            Ok(PinnedQuery::Answered { value, instance }) => (value, instance),
            Ok(PinnedQuery::Unsupported | PinnedQuery::OtherInstance) => return unsupported(),
            Err(e) => return client_error(e),
        };
        if list.items.is_empty() {
            return ok(format!("{}\n", words::NOTHING_INTERRUPTED));
        }
        if !ask(&words::dismiss_all_question(list.items.len())) {
            return failed(words::NOTHING_DISMISSED, EXIT_REFUSED);
        }
        (
            list.items.iter().map(|item| item.id.to_string()).collect(),
            false,
            listed_by,
        )
    } else {
        (args.ids.clone(), args.all, None)
    };
    send_dismiss(client, ids, all, claim, pin).await
}

/// Send the dismissal, `ids` in the batches the daemon admits
/// ([`words::dismiss_batches`]: its count and byte limits), one after
/// another, and render the outcomes together. Every batch is pinned to one
/// daemon process: `pin`, the process the confirmation's list came from, or
/// else the process that answered the first batch. A batch that process does
/// not answer — another process is listening, or the request was refused,
/// failed or is not supported — stops the rest, and the answer says so
/// ([`DismissOutcome::stopped`]). A pinned batch is checked against the
/// process on the connection that carries it, so a replacement is sent
/// nothing. A running daemon always names its process (`daemon serve` does
/// not start without an identity), so a pin is always there to hold.
async fn send_dismiss(
    client: &DaemonClient,
    ids: Vec<String>,
    all: bool,
    claim: Option<crate::daemon_protocol::CallerClaim>,
    mut pin: Option<String>,
) -> CliOutput {
    let batches: Vec<Vec<String>> = if all {
        vec![Vec::new()]
    } else {
        words::dismiss_batches(&ids)
    };
    let mut merged: Option<DismissOutcome> = None;
    for (n, batch) in batches.iter().enumerate() {
        let answer = client
            .dismiss_interrupted_pinned(batch.clone(), all, claim.clone(), pin.as_deref())
            .await;
        let stop = match (answer, merged.as_mut()) {
            (Ok(PinnedQuery::Answered { value, instance }), done) if value.refusal.is_none() => {
                if pin.is_none() {
                    pin = instance;
                }
                match done {
                    Some(done) => done.absorb(value),
                    None => merged = Some(value),
                }
                continue;
            }
            // The first batch: as a single dismissal reads.
            (Ok(PinnedQuery::Answered { value, .. }), None) => return render_dismiss(&value),
            (Ok(PinnedQuery::Unsupported), None) => return unsupported(),
            (Err(e), None) => return client_error(e),
            // The process the confirmation listed was replaced before the
            // first batch: nothing was sent.
            (Ok(PinnedQuery::OtherInstance), None) => {
                merged = Some(DismissOutcome::default());
                (words::DismissStopReason::Replaced, None)
            }
            (Ok(PinnedQuery::Answered { value, .. }), Some(_)) => (
                words::DismissStopReason::Refused,
                value.refusal.map(|r| r.reason),
            ),
            (Ok(PinnedQuery::Unsupported | PinnedQuery::OtherInstance), Some(_)) => {
                (words::DismissStopReason::Replaced, None)
            }
            (Err(_), Some(_)) => (words::DismissStopReason::Failed, None),
        };
        let done = merged.as_mut().expect("a later batch");
        done.stopped = Some(words::DismissStopped {
            reason: stop.0,
            refusal: stop.1,
            not_sent: batches[n..].iter().map(Vec::len).sum(),
        });
        break;
    }
    render_dismiss(&merged.unwrap_or_default())
}

/// [`run_dismiss`]'s text and exit code: 0 when nothing was refused.
pub fn render_dismiss(outcome: &DismissOutcome) -> CliOutput {
    let refusal_text = |refusal: &words::Refusal| {
        if refusal.reason == RefusalReason::Unknown {
            shown(&refusal.message)
        } else {
            words::refusal_sentence(refusal.reason).to_string()
        }
    };
    if let Some(refusal) = &outcome.refusal {
        return failed(refusal_text(refusal), EXIT_REFUSED);
    }
    let mut stdout = String::new();
    for item in &outcome.dismissed {
        stdout.push_str(&format!("{} {}", words::DISMISSED, shown(&item.id)));
        if let Some(name) = &item.name {
            stdout.push_str(&format!("  {}", shown(name)));
        }
        stdout.push('\n');
    }
    if outcome.dismissed_omitted > 0 {
        stdout.push_str(&format!(
            "{}\n",
            words::more_dismissed_sentence(outcome.dismissed_omitted)
        ));
    }
    let nothing = outcome.dismissed.is_empty() && outcome.dismissed_omitted == 0;
    if nothing && outcome.refused.is_empty() && outcome.stopped.is_none() {
        stdout.push_str(&format!("{}\n", words::NOTHING_INTERRUPTED));
    }
    let mut stderr = String::new();
    if nothing && outcome.stopped.is_some() {
        stderr.push_str(&format!("{}\n", words::NOTHING_DISMISSED));
    }
    if outcome.incomplete {
        stderr.push_str(&format!("{}\n", words::LIST_INCOMPLETE));
    }
    for refused in &outcome.refused {
        stderr.push_str(&format!(
            "{}: {}\n",
            shown(&refused.id),
            refusal_text(&refused.refusal)
        ));
    }
    if outcome.refused_omitted > 0 {
        stderr.push_str(&format!(
            "{}\n",
            words::more_refused_sentence(outcome.refused_omitted)
        ));
    }
    if let Some(stopped) = &outcome.stopped {
        stderr.push_str(&format!("{}\n", words::dismiss_stopped_sentence(stopped)));
    }
    CliOutput {
        stdout,
        stderr,
        code: if outcome.refused.is_empty()
            && outcome.refused_omitted == 0
            && outcome.stopped.is_none()
        {
            EXIT_OK
        } else {
            EXIT_REFUSED
        },
    }
}

/// The output for an identity [`crate::close_cli::ambient_identity`] refused.
pub fn identity_error_output(message: String) -> CliOutput {
    failed(shown(&message), EXIT_REFUSED)
}

#[cfg(test)]
#[path = "interrupted_cli_tests.rs"]
mod tests;
