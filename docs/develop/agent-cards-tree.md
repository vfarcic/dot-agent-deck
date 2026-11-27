# The agent cards tree and checked references (issue #2418)

Issue #2418 is step 1 of PRD #2374's decision that the Cards view becomes a tree of root agents with compact, checked cards. The user page is [`docs/desktop/agent-cards.md`](../desktop/agent-cards.md); this page holds what a maintainer needs and the user page leaves out: how the daemon builds the tree, the roll-ups and the checked pull request and issue references, how that reaches clients, and why the decisions went the way they did.

Per CLAUDE.md rule 18 the **daemon** builds all three — parent links, roll-ups and checked references — and the desktop draws what it receives. The desktop derives no parent links, roll-ups or reference checks of its own: it hangs the cards it receives under the `parent` the daemon sent and words the results it was given. The TUI draws none of it (PRD #2374's documented gap); `dot-agent-deck card list` prints the fields.

## The wire: four optional fields and one capability

`AgentCardEntry` (`src/agent_cards.rs`) gained four fields, each `#[serde(default, skip_serializing_if = …)]`:

| field | what it holds |
| --- | --- |
| `parent` | the card key of the entry that started this one; absent for a root |
| `kind` | `"dispatcher"`, `"orchestrator"`, `"role"` or `"agent"` |
| `rollup` | on an entry with anything under it: `{ total, worst, counts, needs_you }` |
| `checked_refs` | per recognised field: `{ field, kind, claim, repo?, number?, url?, result }`, omitted when empty |

`result` (`CheckResult`) is internally tagged on `status`: `{"status":"pending"}`, `{"status":"checked","state":…,"ci":…,"review":…,"title":…,"checked_at":"<RFC 3339>","stale":false,"labels":[{"name":…,"color":…}]}` or `{"status":"unavailable","reason":…,"detail":…}`. The PR lifecycle, CI and review types are `src/pull_request_info.rs`'s, shared with the branch badge rather than duplicated. `kind` on a reference is the **resolved** kind (see [Recognition](#which-values-are-read-as-references)), not the field's type. `rollup.worst` and the `counts` keys are card status words (`waiting_for_input`, `blocked`, `error`, `waiting`, `working`, `thinking`, `compacting`, `idle`, `stopped`, `unknown`).

**An issue's labels (round 2, D13).** `CheckResult::Checked` carries `labels: Vec<IssueLabel>`, each `{ name, color? }`, `#[serde(default, skip_serializing_if = "Vec::is_empty")]` with `color` omitted when absent. It rides inside `checked_refs`, so the `agent-cards-tree` capability already gates it, and a client that does not know the field ignores it. The labels come from the same `gh issue view` call as the rest (`ISSUE_VIEW_FIELDS` is `number,state,title,url,labels`; no second call). They are repository text, so `labels_of` in `src/reference_check.rs` bounds them: at most `MAX_LABELS` = 20 per reference, in `gh`'s order; each name trimmed, control and bidi characters escaped (`escape_control_and_bidi`), then cut to `MAX_LABEL_NAME_CHARS` = 50 characters (GitHub's own limit); a label with a missing, non-string or blank name is skipped and does not count toward the cap; `color` is kept only when it is exactly six ASCII hex digits, otherwise dropped while the label stays; a `labels` value that is not an array reads as none rather than failing the lookup. Only an answer whose page is an issue keeps labels: an issue field whose number GitHub answers as a pull request carries none (pull request labels are a later step). The desktop bounds and checks them again (`issueLabels` in `desktop/src/lib/agentCardsTree.ts`; round 2, R2-A2), since it does not rely on a daemon it may not have built: it looks at the first 100 entries at most, skips one without a usable name, cuts a name to `MAX_ISSUE_LABEL_NAME_CHARS` = 50 characters, keeps at most `MAX_ISSUE_LABELS` = 20, and keeps a colour only when it is six hex digits, before anything builds a key, a tooltip or a chip from them; it draws the colour as a dot beside the name, so the name keeps the theme's text colour in both themes. On the card's line a chip is measured at its own width (no `max-width`), so a label wider than the line is hidden whole and counted by `+N` rather than cut.

**The daemon advertises `agent-cards-tree`** (`CAP_AGENT_CARDS_TREE` in `src/daemon_protocol.rs`) beside `agent-cards`. On the client side `daemon_client::tree_gate` reads it from the list connection's Hello and, against a daemon that does not advertise it, drops any `parent`, `kind`, `rollup` and `checked_refs` an entry carries before anything draws them: those names mean the tree only from a daemon that says it builds one. `AgentCardsAnswer.tree` carries the verdict to the desktop (`list_agent_cards_tree` in `desktop/src-tauri/src/lib.rs`), which then draws every card as a root with no roll-up or references and shows the one-line older-daemon note.

**Rule 12 answer: no `PROTOCOL_VERSION` bump and no `.breaking.md`.** The fields are additive and optional (round 2's `labels` included), the meaning of `group`, `status`, `needs_you` and `fields[]` is unchanged, and the tree is withheld behind a capability. One field's content moved while the branch was in review: `CheckResult::Unavailable.detail` became one fixed sentence per reason (see [What a client is told](#what-a-client-is-told-and-what-only-the-log-is-told)); it shipped that way, so no released build ever read the earlier content.

## Parent links (D1)

`starter_of` and `link_tree` in `src/agent_cards.rs` derive a parent from **exact identity**, never from display names:

- an orchestration's **role** hangs under its orchestrator's card, found by orchestration instance and seat. When several generations of the orchestrator have cards, a live generation wins over a stopped one, then the latest spawned. A role never hangs under the orchestration's dispatcher;
- a **dispatched unit** — including an orchestration's orchestrator when a dispatcher dispatched the orchestration, and a dispatcher another dispatcher started — hangs under its dispatcher's card, matched by the dispatcher's **pane and generation**. A reused dispatcher pane does not adopt the previous generation's units: they stay under the generation that dispatched them, or become roots when that card is gone;
- everything else is a root.

**Orphans become roots.** A starter whose card is gone (closed, pruned) is no parent, and neither is a card naming itself.

**Cycles are broken, not trusted away.** Exact identity should never form one, but `link_tree` does not rely on it: walking up from each card in spawn order ends at a root, a card already known to reach one, or a card already on the walk. In the last case the walk from that card on is a cycle, and only the earliest-spawned card **on** the cycle loses its parent, once per cycle; a tail that leads into the cycle keeps its starter. The result is a true forest. The desktop guards the same way independently (`cardTree`, `pathTo`, `TreeLevel`'s `above` set), because a client draws whatever snapshot it is given. Tests: `self_links_and_cycles_still_give_a_forest`, desktop `desktop/agent-cards/022`.

## Roll-ups and the severity order (D2)

Every entry with anything under it carries `rollup`:

- `total` — every descendant, at any depth, not the card itself;
- `counts` — those descendants by status word;
- `worst` — the most severe status among them;
- `needs_you` — how many of them the existing card `needs_you` predicate marks. The predicate's meaning was deliberately left alone.

The severity order, most severe first, is the one in the mockup the maintainer chose: `waiting_for_input`, `blocked`, `error`, `waiting`, `working`, `thinking`, `compacting`, `idle`, `stopped`, `unknown` (`status_severity`). The closed card's colour is the worst of the card's own status and `rollup.worst`; combining those two is drawing, so the desktop does it (`cardTone` in `desktop/src/lib/agentCardsTree.ts`, which also orders the counts bar by the same list). An open card takes its own status's colour.

Roll-ups ride the existing refresh and doorbell path: a grandchild's status change changes its root's projected entry, which bumps the root's revision and rings the doorbell (test `a_grandchild_change_bumps_the_roots_revision_and_doorbell`).

## Card `kind`

`project_entry` computes `kind` from the same identities: `orchestrator` by the orchestration seat (so an orchestrator that also dispatches reads as an orchestrator); else `dispatcher` when the dispatch records name this pane and generation as some unit's dispatcher, whether or not that unit's card is listed; else `role` by the seat; else `agent`. It is a tree field: sent only with `agent-cards-tree` and dropped by `tree_gate`. The desktop draws the daemon's `kind` and, for an older daemon or a kind this build does not know, falls back to inferring it from the relations among the cards it was given (`cardKinds`). That fallback cannot see a dispatcher whose units the dashboard filter hides, which is why the daemon sends the field.

## List order is the tree's preorder (D3)

`card list` and the desktop's snapshot are in **tree preorder**: roots in spawn order, each followed by its subtree, siblings in spawn order. The daemon no longer sorts by status or needs-you, so a root's position does not change when a status does.

**What an older desktop sees against this daemon.** Before #2418 the daemon sorted by group, then needs-you first, then spawn order, and the older desktop's `groupCards` groups *consecutive* entries with one `group.id`. Against a #2418 daemon such a desktop loses needs-you-first within a group, and a group can arrive as more than one run — each run is drawn as its own group heading. `groupCards` already handled repeated runs with unique keys, so nothing is misread; the daemon review recorded this as acceptable rather than a contract break. The TUI renders no cards and is unaffected.

## Which values are read as references (D4)

`src/github_refs.rs` does the reading, with no I/O:

- **Only fields whose type is `pull_request` or `issue`** are read: the catalog's `pull-request` and `issue`, a catalog field declared with either type, and a custom field the agent reports with either type (`checked_references` in `src/agent_cards.rs`).
- Accepted spellings are the ones report validation already accepts: `#123`, `owner/repo#123`, and `https://github.com/<owner>/<repo>/pull/<n>` or `…/issues/<n>` with at most a trailing `/`. No other host, port, userinfo, extra segment, query or fragment; owner and repository in GitHub's characters; the number positive decimal without a leading zero. Anything else is `unrecognised`: not looked up and not linked.
- **A URL decides its own kind.** For `#123` and `owner/repo#123` the kind is the field's; a URL names `/pull/` or `/issues/`, and that wins, so a pull request URL in an `issue` field is checked as a pull request.

**A bare `#123` in prose — the summary, a `text` field, a question — is not read. That is a decision.** Prose mentions numbers for many reasons ("unlike #12", "step #3", a colour such as `#2378`), and a reference the deck checks and links is a claim the card then shows as the deck's: reading prose would turn the agent's incidental words into deck-checked badges, links included. Requiring the typed field keeps the checked set to what the agent reported *as* its pull request or issue. The user page says so in one sentence.

**`#123` resolves against the agent's current folder.** `AgentFacts::reference_cwd` takes the live session's reported directory (`AppState::live_session_cwd_for`, matched by pane and generation with the same tie-break as `live_session_for`), else the folder the deck started the agent in, and the monitor resolves that folder's GitHub `origin` the way the branch badge does. An agent that changed into another repository after it started is read against that repository. A folder with no GitHub `origin` makes a bare `#N` `unrecognised` with the detail "no GitHub origin for this agent's folder".

## The lookups, and their budget

`src/reference_check.rs` splits the work so that **no network work runs inside the serialized projection turn**: the projection reads cached answers (`ReferenceChecks::view`) and records which references and folders it wanted (`set_wanted`), and a background monitor, like the branch badge's, does the `git` probes and the `gh` calls:

- a pull request: `gh pr view <n> --repo <owner/name> --json …`;
- an issue: `gh issue view <n> --repo <owner/name> --json …`. GitHub numbers issues and pull requests in one sequence and `gh issue view` answers a pull request's number too, with a `/pull/` URL. `parse_issue_reference` accepts that answer's URL and lifecycle state, but the reference stays an issue: `check_reference` keeps the issue kind and replaces only the URL and the result, and the issue lookup brings no CI checks or review, so the card shows an issue badge, which can read merged. A pull request URL in the original claim is different: `GithubReference::parse` takes the kind from the URL, so that reference is looked up and shown as a pull request.

It shares the branch badge's refresh policy (`fetch_due`, `DOT_AGENT_DECK_PR_REFRESH_SECS`, 60 s by default): an open one is re-read every interval, a merged or closed one is frozen unless its CI checks are still running (`checks_still_running`, shared with the badge), and then only for `TERMINAL_PENDING_CHECKS_WINDOW` (2 h) after it turned terminal, and a failure backs off for five intervals (`failure_backoff`). Each `gh` is bounded by `GH_TIMEOUT` (30 s).

Because an agent controls how many references its card names, admission is bounded independently of the refresh policy:

| bound | value | where |
| --- | --- | --- |
| token bucket | burst of `LOOKUP_BURST` = 16, then one lookup per `LOOKUP_REFILL_EVERY` = 3 s (at most about 1200 an hour, a quarter of GitHub's 5000) | `Budget` |
| daemon-wide pause after a `rate_limited` answer | `RATE_LIMIT_COOLDOWN` = 5 min, for every reference | `Cooldown` |
| started and unfinished lookups | `MAX_QUEUED_LOOKUPS` = 8, oldest-due first (entry's `fetched_at`, else first-wanted time), so new references cannot starve refreshes | `Scheduler::plan` |
| share of the `gh`/`git` job bound | a lane of `REFERENCE_LANE_PERMITS` = 2 of the shared 4, so the branch badge's monitor keeps the other 2 | `JobPool::with_lane` |
| entries for references no card names now | kept `retired_ttl` = max(30 min, the failure backoff), at most `MAX_RETIRED` = 512 | `Scheduler` |

The cooldown is checked twice: at admission, and again when a queued job first runs, after it holds its permits (`gated`). A job held by the cooldown releases its permits without running `gh`, refunds its token (`Scheduler::deferred`) and keeps its place for the first plan after the pause. A lookup already running when another answers `rate_limited` finishes.

The retired set is what keeps a failing or terminal reference from being looked up again by clearing and re-publishing it: naming a reference again restores its retry deadline, its terminal answer or its last value. Entries for which no lookup has finished are dropped rather than retired (a lookup still running when its entry is dropped records its answer when it finishes), and past `MAX_RETIRED` the entries that protect nothing (no answer, no failure inside its backoff) go first, then oldest. Repository names compare case-insensitively, so `O/R#1` and `o/r#1` share one cache entry.

## The answer has to name the page that was asked for

A `gh` answer's URL is what a client later opens, so it is validated, not trusted: `answered_page` requires a github.com page URL (`ResolvedReference::from_github_url`, the same strict parser as for claims, URL form only), the same number, a `/pull/` page for a pull request answer (`/pull/` or `/issues/` for an issue answer), and the same repository, case-insensitively. The canonical URL is what the entry carries.

**A renamed or transferred repository is not followed.** GitHub redirects the lookup and answers with the new repository's URL, which fails the same-repository check, so the reference reads `unavailable` with reason `error` and the log says "gh answered a page of another repository". Following the rename would need a further lookup to prove the new name is the same repository rather than an answer pointing somewhere else; the user page tells the user to have the agent report the current name instead.

The desktop repeats a narrower check before it opens anything: `openableReference` and `isIssueUrl`/`isPullRequestUrl` in TypeScript, `pr_browser::is_issue_url` in Rust (one rule in both: exactly `https://github.com/<owner>/<repo>/issues/<n>`, optional trailing `/`, no query or fragment), and the shell's `openCheckedReference` opens a reference only while the card's connected deck builds the tree and still lists that card with that checked reference.

## What a client is told, and what only the log is told (D5)

A reference that cannot be checked carries a reason: `no_github_access`, `not_found` (GitHub cannot tell missing from private, so it is labelled "not found or private"; a 403 "Resource not accessible by personal access token" or "… by integration" is `not_found` too, since `gh` is logged in but that login cannot see the repository), `rate_limited` (a 403 for a secondary rate limit or abuse detection included: `classify_gh_failure` checks the rate-limit phrases before the others, and a 403 matching none of the phrases is `error`), `unrecognised` or `error`; before the first lookup finishes it is `pending`; a last-known answer kept after a later failure is `checked` with `stale: true`.

The `detail` sent to clients is **one fixed sentence per reason** (`unavailable_detail`), never `gh`'s output:

| reason | `detail` |
| --- | --- |
| `no_github_access` | gh is missing or not logged in on the deck's host |
| `not_found` | not found, or private to the deck's GitHub login |
| `rate_limited` | GitHub rate limit reached on the deck's host |
| `error` | the deck's host could not check it; the deck's log says why |

`gh`'s stderr can hold tokens in URLs and home paths, and a card list reaches remote clients, so the full diagnostic stays on the daemon's host: the monitor logs it once per failure run, escaped. Test: `gh_output_never_reaches_the_cards_detail`. The desktop shows a short label on the card and a sentence of its own in the tooltip and the overlay, plus this `detail`.

## The desktop's half

`desktop/src/lib/agentCardsTree.ts` hangs received cards under `parent` (`cardTree`), computes the Needs-you path, the closed-card colour and the bar's segments, and words each checked reference; `AgentCardsGrid.tsx` lays out the rows (fixed 224 px columns, each opened root's tree in its own band under the row, `layoutRow` routing later roots' connector lines around earlier trees on the right to each tree's box); since issue #2533 nothing in Cards scrolls sideways: each opened root's tree is drawn in a box captioned with the root's name (`.agent-cards-box`, one neutral `--paper-sunken` tint and `--line` border for every box, since a card's colour means its status), and `layoutTreeLevel` in `desktop/src/lib/cardTreeLayout.ts` wraps the cards under each card into rows for the width the box has, at every level — pure arithmetic on the frame's width, so jsdom tests see the same rows as the browser. Each row hangs from its own bus, and the spine runs to the next row down a gap between two items (each item is a column holding its card and its open level, so a gap runs the row's full height); a row holding one item passes the spine in a 16 px lane kept free at its right, which is why a level that wraps lays its items out 16 px narrower. A card in a level deeper than the width allows is drawn narrower rather than spilling out of its box, and one layout pass memoises each open card's level by room and path, since a wrapped level measures its cards twice (PR #2550 review). The layout decides only the horizontal geometry; the lines' heights follow the rows in CSS. Every card of a level is a child of the level itself, keyed by the card, with a zero-height full-width break between rows and each row's bus and spine on its first card, so a card that moves to another row is not remounted and keeps the keyboard; `AgentCard.tsx` draws a compact card whose every line has a fixed height, so a status, reference, label or roll-up appearing never resizes a card or moves a root. Which trees are open is remembered in `localStorage` under `dot-agent-deck.cards.open` (see [desktop-gui.md](desktop-gui.md#the-localstorage-keys-and-the-one-that-is-gone)). The new controls dispatch through `VOICE_ACTIONS` — `openCardTree`, `closeCardTree`, `openCardPullRequest`, `openCardIssue`, and `showAgentCard` with its `references` section for **+N** — with `commands.toml` rows (CLAUDE.md rule 23).

**Round 2 (D11, D12, D14, D16).** The card carries no agent type (the Card tab shows it). PR #2491 decision 2026-11-25 replaced the card overlay with the agent pane's Card panel, and decision 2026-11-26 made it the pane's Card tab ([`agent-cards-tab.md`](agent-cards-tab.md)): the name and the body both dispatch `showAgentCard` with `opensOn: "terminal"`, which opens the pane on Terminal through `VOICE_ACTIONS.openAgent` with the target the overview's table row uses for that agent (`cardOpenTarget` in `AgentOverview.tsx`, falling back to the card's own deck, agent and `instance_id` for a card whose agent the fleet no longer lists), so for an agent the fleet lists the name opens exactly what its row opens. Its only action is a **Close** icon dispatching `runCardAction` with the card's `close` action id and token, the same call the review's Close confirmation sends, button, so the confirm step and disabled states are the overlay's. The `<article>`'s own `onClick` dispatches `showAgentCard`; each control sits in a `display: contents` wrapper (`.agent-card-control`) whose only handler stops the click, so it does not also open the overlay. The status line is a `<button>` with no handler: its click, a keyboard press included, bubbles to the article, which gives the keyboard a way to the overlay without nesting a control inside another. Its accessible name starts with the kind and status words it shows (`agent idle: show <name>'s card`), so a screen reader announces them. **Every press of a prompt the catalog did not write opens the review step** (`runCardAction` in `useAgentCardsView.ts`, decided by `sendableUnread`, the action's `prompt_source`; audit A4, round 2 R2-A1): a control's token names the definition it drew, not that its text was on screen, and the overlay draws a prompt under its button, where the end of a long one can lie below the overlay's visible part. The step is drawn at the overlay's top and scrolled into view with the focus on it. The label line's `+N` is text, not a control. The overlay leads with the report, the questions and the reply box, which is rendered whenever the entry offers `reply` (a `reply` step, from the Reply action or a spoken reply, only focuses it); the exact text an action sends is a `<blockquote>` styled as a quotation. Explain left the default catalog (`src/card_catalog.rs`, D15), and is listed in `RETIRED_DEFAULT_NAMES` so that a user's `remove = ["explain"]` is skipped silently rather than reported as naming no default; the `always` key stays, since a user's catalog can still set it, and no default sets it now. The card's name and Close reuse `openAgent` and `runCardAction`, which already have `commands.toml` rows (`open_agent`, `run_card_action`); labels are display only and need none.

## Tests

`tests/CATALOG.md` has the IDs. In short: projection and roll-up unit tests in `src/agent_cards.rs`; reference reading in `src/github_refs.rs`; scheduling, budget, cooldown and answer validation in `src/reference_check.rs`; `tree_gate` in `src/daemon_client.rs`; the L2 synthetic tree and reference scenarios in `tests/e2e_agent_cards.rs` and `tests/e2e_agent_cards_refs.rs` (with a stand-in `gh`); a lane-2 real-agent test in `tests/e2e_agent_cards_live.rs`; the catalog defaults in `src/card_catalog.rs` and `tests/agent_card_catalog.rs`; label bounding in `src/reference_check.rs` and the `labels` wire field in `src/agent_cards.rs`; desktop `desktop/agent-cards/003`–`026` in vitest and `desktop/e2e/agent-cards-tree.spec.ts` for the layout at five widths.
