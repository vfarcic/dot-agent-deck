/**
 * Landing-page content (issue #1021).
 *
 * Every user-visible string on `/` comes from here, so the copy can be
 * reviewed and corrected in one file rather than hunted through JSX. It began
 * as the shared module the four `/style/*` candidates imported; the candidates
 * are gone and direction C is now `/`, so the exports that only served the
 * losing directions went with them.
 *
 * ---------------------------------------------------------------------------
 * WHAT THIS PAGE CLAIMS THE PRODUCT IS
 * ---------------------------------------------------------------------------
 *
 * The page's first two passes inherited their feature framing from the old
 * homepage, which predates orchestration, dispatching and scheduling. This
 * pass re-derived the framing from the repository instead -- `docs/`, the
 * `Commands` enum in `src/main.rs`, and the Mode cycler in `src/ui.rs` -- and
 * the answer is that FOUR things are the product today:
 *
 * 1. Agents as panes, with live per-card status. The floor everything else
 *    stands on (`docs/session-management.md`).
 * 2. Orchestration -- one orchestrator delegating to workers in their own
 *    panes, with `work-done` back and idle-worker detection when one goes
 *    quiet (`docs/orchestration.md`, `delegate` / `work-done` / `pane`).
 * 3. Dispatching -- `dot-agent-deck dispatch` and dispatcher mode: a git
 *    worktree per unit, a single agent or a whole orchestration inside it,
 *    reporting back to the pane that asked (`docs/dispatcher-mode.md`).
 * 4. Scheduling -- cron-fired tabs, run by the daemon whether or not the deck
 *    is open (`docs/scheduled-tasks.md`, `schedule`).
 *
 * Remote environments and the desktop app are real but secondary -- they are
 * about WHERE the deck runs, not what it does -- and they keep the bands they
 * already had. Workspace modes are being REMOVED (issue #1199), so they are
 * gone from the marketing surface entirely.
 *
 * The four-step story arc was rebuilt on that basis. The old ending, "walk
 * away", was a PROPERTY of the product rather than a step in the arc, and its
 * BEFORE/AFTER frame never communicated; it is now principle 03, where a
 * sentence carries it better than a frame could. Dispatching takes the fourth
 * step, because it is the genuine escalation the arc was missing: one agent,
 * then many visible, then one running the others, then whole new lines of work
 * starting in their own copies of the repo.
 *
 * ---------------------------------------------------------------------------
 * IMPLEMENTATION DETAIL IS NOT A SELLING POINT
 * ---------------------------------------------------------------------------
 *
 * A visitor deciding whether to install this cares what it does for them, not
 * how it is wired. This pass swept the page for the second kind and removed
 * it: "written in Rust" from the hero eyebrow, "Rust" from the hero lede, the
 * four per-agent `IntegrationStrategy` labels under the agent strip (Native
 * hooks / Plugin / Bundled extension / Stdout wrapper), "embedded PTY",
 * "hooks installed for you", "per-project TOML", and "daemon"/"sidecar" from
 * the desktop band.
 *
 * Three survive, each because the implementation IS the reason a reader cares:
 * "a single binary" (nothing else to install), `Rust 1.85 or newer` on the
 * `cargo build` install route (you cannot take that route without it), and the
 * `gh attestation verify` command (it is an instruction, not a description).
 *
 * ---------------------------------------------------------------------------
 * CORRECTED FACTS
 * ---------------------------------------------------------------------------
 *
 * The prose is lifted from the previous homepage, with corrections it never
 * had, every one checked against the repository rather than reasoned about.
 * The first four are the ones the page shipped with; the issue text was wrong
 * about two of them:
 *
 * 1. The desktop GUI. Issue #1021 calls the artifacts "signed and notarized".
 *    They are neither. `.github/workflows/release.yml:523` introduces the
 *    bundle job as "an unsigned alpha artifact", the release body it writes is
 *    headed "Desktop GUI (alpha, unsigned)", and PRD #757 was closed having
 *    decided the free options only -- "the macOS warning stays, because there
 *    is no free way to remove it". The latest release (v0.41.0) carries exactly
 *    two desktop assets, both named `...-desktop-alpha-...`. There is also no
 *    Windows bundle: release.yml says "Windows is deliberately absent", because
 *    a Tauri bundle carries the daemon as a sidecar and no Windows daemon
 *    binary is published. What the binaries and packages DO carry is build
 *    provenance, from the credential-free `attest` job.
 * 2. The agent list. `src/event.rs`'s `AgentType` and `src/agent_registry.rs`
 *    ship five agents, not the two the page named and not the four the issue
 *    names: Claude Code, OpenCode, Pi, Codex and Devin. `docs/getting-started.md`
 *    already lists all five. Gemini (#211) and Aider (#212) are open PRDs and
 *    are not promised here.
 * 3. Windows. The page linked #42, closed on 2026-07-15 -- and that stale link
 *    is the whole argument against carrying ANY issue number on this page. A
 *    marketing page is the place least likely to be revisited, so a pointer
 *    parked here rots quietly: whoever retargets the work updates the issue
 *    tracker and never thinks about the landing page. The first fix was to
 *    point at the open work instead (#164); the maintainer's correction is
 *    that the number does not belong here at all -- "we might change the issue
 *    and forget to update the site". So the Windows-native row states the
 *    status and names the path that works today (WSL), and the "Full
 *    installation guide" link carries anyone who wants the tracking issue to
 *    `docs/installation.md`, which is now the SINGLE place it lives -- beside
 *    the rest of the platform detail, where whoever changes installation will
 *    see it.
 * 4. Linux install. The page hedged with "Homebrew (if available)". The
 *    generated formula in `Taskfile.yml` carries a full `on_linux` block for
 *    both amd64 and arm64, and `docs/installation.md` heads the section
 *    "Homebrew (macOS / Linux)". It is a supported path, so say so.
 *
 * A security audit of the published page then corrected three more, each
 * measured against `release.yml` and the live v0.41.0 release rather than
 * reasoned about:
 *
 * 5. "Every published asset carries build provenance" was false. A full
 *    release publishes EIGHT assets -- four CLI binaries, two desktop
 *    packages, and two checksum manifests -- and the subject collection at
 *    `release.yml:855-885` matches only `dot-agent-deck-*` and
 *    `dot-agent-deck-desktop-alpha-*`, so `checksums.txt` and
 *    `checksums-desktop-alpha.txt` are attested by nothing. Measured:
 *    `gh attestation verify` exits 0 on the `.dmg` and on a CLI binary, and
 *    returns an attestation API 404 on both checksum manifests. Widening the
 *    attestation subjects is the better long-term fix, but that is a
 *    release-workflow change and belongs in its own PR; the page narrows its
 *    claim instead.
 * 6. The provenance command was weaker than the sentence above it. `--repo`
 *    alone pins the repository, not the workflow that produced the file.
 *    `--signer-workflow vfarcic/dot-agent-deck/.github/workflows/release.yml`
 *    was verified against real v0.41.0 assets (exit 0 on both the `.dmg` and
 *    a CLI binary) and verified to REJECT a wrong workflow path -- `ci.yml`
 *    gives `Error: verifying with issuer "sigstore.dev"`, exit 1 -- so the
 *    page hands out the form that actually enforces what the prose claims.
 * 7. "Every release also publishes a desktop build" was a false absolute
 *    (CLAUDE.md rule 17). A manual dispatch can set `skip_desktop`
 *    (`release.yml:11`, gating both desktop jobs at `:546` and `:705`), and
 *    the bundle matrix is `fail-fast: false` with an explicit path where "the
 *    CLI release itself is unaffected and complete" when every leg fails.
 *
 * This pass adds an eighth, found while sweeping rather than reported:
 *
 * 8. The "No buttons" design principle was false. `docs/getting-started.md`
 *    says the dashboard "is also fully mouse-clickable: a button bar along the
 *    bottom exposes the main commands (each labelled with its keyboard
 *    shortcut), and cards, tab headers, dialogs, the directory picker, and
 *    forms all respond to clicks". The true claim is keyboard-FIRST, not
 *    keyboard-only, and the principle now says that and names the button bar.
 *
 * Ordering, not just wording, is corrected too: the unsigned-macOS caveat now
 * says to verify provenance BEFORE following the release notes past Gatekeeper.
 * The exact OS steps stay centralized in the release notes rather than being
 * duplicated here.
 */

export const product = {
  name: 'Agent Deck',
  binary: 'dot-agent-deck',
  owner: 'DevOps Toolkit',
  tagline:
    'A terminal dashboard for running, orchestrating and dispatching AI coding agents in parallel',
  shortDefinition:
    'A single binary that runs your AI coding agents as panes in one terminal, tracks what each of them is doing in real time, and starts new work for you — a team under one orchestrator, an isolated unit in its own copy of the repo, or a task that fires on a schedule.',
  license: 'MIT',
  repo: 'https://github.com/vfarcic/dot-agent-deck',
  issues: 'https://github.com/vfarcic/dot-agent-deck/issues',
  releases: 'https://github.com/vfarcic/dot-agent-deck/releases/latest',
};

/**
 * The "Why Agent Deck" block. Carried over from the previous homepage, which
 * is where the page's strongest writing still is -- so this pass corrects only
 * what is false or dated in it and leaves the voice alone.
 *
 * Three corrections, all from the maintainer's review of the live page:
 *
 * - "Running five at once" is now "Running many at the same time". Five reads
 *   as a ceiling, and nothing in the product imposes one.
 * - "The agents write the code" was too narrow. They also run the tests, watch
 *   the pipelines and answer the review comments -- which is what an
 *   orchestration's reviewer, auditor and release roles do in this very repo.
 *   The sentence's job is to contrast their work with yours, so widening the
 *   first half without blurring the second is the whole trick: they execute,
 *   you decide and validate.
 * - "One dashboard, every agent visible at a glance" was written when a
 *   dashboard of single agents was all there was. There are now tabs,
 *   orchestrations and dispatched units, so the sentence says what it was
 *   always trying to say -- one place holds all of it -- in terms of what the
 *   product actually holds today. A REMOTE deck is deliberately not in that
 *   list, though it was in the first draft of this rewrite: the terminal deck
 *   attaches to one daemon at a time (`Endpoint` in `src/daemon_client.rs` is
 *   a single `Local`/`Remote` choice, not a map), so "one place holds ... a
 *   deck running on another machine" would be true only of the desktop app,
 *   which holds many at once and has its own band further down the page.
 */
export const why = {
  heading: 'Why Agent Deck',
  paragraphs: [
    "Running one AI agent at a time, you're still a software engineer who happens to use AI. Running many at the same time, you stop being one. You become a project manager supervising a team, a tech lead unblocking them, an architect designing the approach, a product manager deciding what to build.",
    "The agents do the work — writing the code, running the tests, watching the pipelines, answering the review comments. Your job is everything around it — defining the work up front, supervising it in flight, and validating that the right thing got built. None of this is new. It's the same craft people have practiced for decades. The team just looks different.",
    'Agent Deck is the tool that lets you do that without losing your mind. One place holds all of it — a lone agent, a team working under an orchestrator, a unit off in its own copy of the repo — and every one of them is a card or a tab you can open, watch and type into. Keyboard-driven, in the terminal you already use, with the agent client you already know.',
  ],
};

/**
 * The design decisions the page argues from. "Focus-mode side panes" was the
 * third of these and is GONE: workspace modes are being removed (issue #1199,
 * "they do not work well"), and a marketing page should not advertise a
 * feature on its way out.
 *
 * What took the slot is not filler to keep the grid at four -- it is the
 * "walk away" claim, displaced out of the story's fourth row. It belongs here:
 * the agents outliving your terminal session is a decision about how the thing
 * is built, and it is a claim a sentence can make and a screenshot cannot.
 */
export const principles = [
  {
    title: 'Runs in your terminal',
    description:
      'Ghostty, iTerm2, Alacritty, Kitty, WezTerm — whatever you already configured. Agent Deck is a guest in it, not a replacement, and there is no multiplexer to set up underneath.',
  },
  {
    title: 'Uses your agent client',
    description:
      'Claude Code, OpenCode, Pi, Codex or Devin — keep the shortcuts, skills, and configs you already dialed in. No new agent client to learn.',
  },
  {
    title: 'Closing the window does not stop the work',
    description:
      'Detach the deck and the agents carry on without you; open it again and you rejoin the same sessions, mid-run. The same holds over ssh — a deck running on another machine stays running when you disconnect.',
  },
  {
    title: 'Keyboard first',
    description:
      'Every action is one or two keystrokes away, because managing a team of agents has to fit in muscle memory. The mouse still works when you want it: a button bar along the bottom names each command and the key it answers to.',
  },
];

/**
 * Shipped agents, from `src/agent_registry.rs`.
 *
 * Each entry used to carry that file's own `IntegrationStrategy` for the agent
 * -- Native hooks / Plugin / Bundled extension / Stdout wrapper. All four are
 * gone. They are implementation detail in the strictest sense: a reader
 * deciding whether to install this wants to know THAT their client works, and
 * nothing about the four different mechanisms behind that answer changes what
 * they should do next.
 */
export const agents = [
  {
    name: 'Claude Code',
    command: 'claude',
    href: 'https://www.anthropic.com/claude-code',
  },
  {
    name: 'OpenCode',
    command: 'opencode',
    href: 'https://opencode.ai',
  },
  {
    name: 'Pi',
    command: 'pi',
    href: 'https://github.com/earendil-works/pi',
  },
  {
    name: 'Codex',
    command: 'codex',
    href: 'https://github.com/openai/codex',
  },
  {
    name: 'Devin',
    command: 'devin',
    href: 'https://devin.ai',
  },
];

export const agentsNote =
  'Any other command still runs in a pane — it just gets no live status tracking. Adapters for Gemini CLI and Aider are designed and open, not shipped.';

/**
 * Mirrors the Platform Support table in `docs/installation.md`, with one
 * deliberate divergence: that table links the Windows-native tracking issue
 * and this one does not. See correction 3 at the top of this file before
 * "restoring parity" by adding the link back.
 */
export const platforms = [
  {
    platform: 'macOS',
    detail: 'Intel & Apple Silicon',
    status: 'Supported',
    supported: true,
  },
  {
    platform: 'Linux',
    detail: 'amd64 & arm64',
    status: 'Supported',
    supported: true,
  },
  {
    platform: 'Windows via WSL',
    detail: 'runs as Linux',
    status: 'Supported',
    supported: true,
  },
  {
    platform: 'Windows native',
    detail: 'no .exe in the release artifacts yet',
    status: 'Not yet — use WSL today',
    supported: false,
  },
];

export const installCommand = 'brew tap vfarcic/tap && brew install dot-agent-deck';

export const installRoutes = [
  {
    name: 'Homebrew',
    detail: 'macOS and Linux, amd64 and arm64',
    code: 'brew tap vfarcic/tap && brew install dot-agent-deck',
  },
  {
    name: 'Nix',
    detail: 'flake, overlay and home-manager module',
    code: 'nix run github:vfarcic/dot-agent-deck',
  },
  {
    name: 'Prebuilt binary',
    detail: 'darwin/linux, amd64 and arm64, from the releases page',
    code: null,
  },
  {
    name: 'Source',
    detail: 'Rust 1.85 or newer, edition 2024',
    code: 'cargo build --release',
  },
];

/**
 * The desktop GUI. Every claim here is checked -- see the note at the top of
 * this file. "Alpha" and "unsigned" are both load-bearing and neither is
 * softened, because a visitor who downloads the .dmg meets a macOS dialog
 * within the minute.
 *
 * `intro` and the Windows caveat used to explain themselves in daemon and
 * sidecar terms. Both now say the same thing in what the reader can see: the
 * terminal deck shows one machine's agents at a time and the app shows
 * several, and there is no Windows build of the deck for an app to carry.
 */
export const desktop = {
  heading: 'There is a desktop app too. It is an alpha.',
  intro:
    'The terminal deck shows you one machine’s agents at a time. The desktop app is a native window that holds several at once — the agents on your laptop and the ones on a remote box, side by side. It rides along with a release rather than gating it, so check the assets on the release you open: the CLI ships even when a desktop bundle does not.',
  artifacts: [
    {
      platform: 'macOS',
      arch: 'Apple Silicon',
      file: 'dot-agent-deck-desktop-alpha-macos-arm64.dmg',
    },
    {
      platform: 'Linux',
      arch: 'x86_64',
      file: 'dot-agent-deck-desktop-alpha-linux-amd64.deb',
    },
  ],
  caveats: [
    {
      title: 'Alpha, and labelled that way',
      body: 'It ships outside the support expectations of the CLI. The terminal deck is the product; this is an early preview of a second way in.',
    },
    {
      title: 'Unsigned, so macOS will stop you',
      body: 'There is no Developer ID certificate and no notarization, so the first launch hits a security dialog. Verify the download with the provenance command below first, then follow the release notes for the exact route past the dialog — in that order, because getting past the warning is the step you want to take only once you know what you have.',
    },
    {
      title: 'No Windows bundle',
      body: 'There is no native Windows build of Agent Deck itself yet, so there is nothing for a desktop bundle to carry. Windows via WSL runs the terminal deck today.',
    },
  ],
  provenanceNote:
    'The binaries and the desktop packages carry build provenance: proof that this exact file came out of this repository’s release workflow, and a record of the commit it was built from. Run it on what you downloaded before you open it.',
  provenanceCommand:
    'gh attestation verify <file> --repo vfarcic/dot-agent-deck --signer-workflow vfarcic/dot-agent-deck/.github/workflows/release.yml',
  provenanceScope:
    'It does not cover the two checksums manifests published beside them: those carry no attestation, so running the command on one returns a 404 rather than a verdict.',
};

/**
 * The screenshot catalogue. Every entry here is rendered -- an unreferenced
 * entry is removed rather than left to rot, which is how `orchestration`,
 * `dashboard`, `modes` and now `card` and `reattach` left in turn.
 *
 * Each story row carries the frame its own copy describes, and each caption
 * describes what is IN the frame rather than what the row argues, so a
 * recapture that changes what a frame shows is a one-file correction.
 *
 * `tall` and `wide` are OPTIONAL layout flags on an entry, read by `index.js`.
 * Both say the same thing about a frame -- "this one is far enough from the
 * others' shape that the row falls out of step with its neighbours" -- and the
 * measure for both is the same: how tall the frame stands, in multiples of the
 * figure column's own width, when the story is at its 1120px maximum. Rows 01
 * and 03 set the house shape at 0.58 and 0.56.
 *
 * `tall` means "much closer to 4:3 than the 16:9 the other rows carry".
 * `busy-deck-real.webp` is 2554x1936, so at the column's full width it stands
 * 0.76 -- about a third taller than rows 01 and 03. The flag caps its width
 * above the two-column breakpoint instead of cropping it: a crop would cut off
 * the two edges it is here for, the sidebar's cards on one side and the
 * footer's counts on the other.
 *
 * `wide` is the same problem in the other direction, and needs the opposite
 * treatment because no width cap can fix a frame for being too SHORT.
 * `dispatch.webp` is 1946x482 -- a 4.04 band -- so in the ordinary figure
 * column it stands 0.25, further below the house shape than the uncapped
 * `busy-deck-real.webp` stood above it. The flag stacks that row instead: the
 * text over the frame, and the frame across the whole story measure, which is
 * 1.81 column-widths and so 0.45 tall.
 *
 * What changed, frame by frame:
 *
 * - Row 02 ("Watch every one of them") now carries `busy-deck-real.webp`
 *   instead of `session-management-card.jpg`. The row argues both "every pane
 *   gets a card" and "the cards tighten up rather than make you scroll", and a
 *   frame of ONE card could only ever show the first half of that.
 *
 *   The frame is the MAINTAINER'S OWN capture of a real deck at work, chosen
 *   by them over a synthetic nine-card alternative in a side-by-side
 *   comparison. It was declined once, for three reasons -- it shows unreleased
 *   work in its directory names, a quoted security-audit finding in the
 *   focused pane, and `experimental: on` in the footer -- and the maintainer
 *   overruled all three: "It's a screenshot of the deck doing real work. This
 *   project is public so nothing is a secret." The synthetic frame it replaced
 *   (`busy-deck.webp`, nine cards, five tabs) is deleted rather than left in
 *   the tree unused.
 *
 *   Write the copy to THIS frame's strength, which is not the synthetic one's.
 *   Its four tab titles all truncate to near-identical `mixed ·
 *   dot-agent-deck-dis…` text, so it is weak evidence for "many tabs". What it
 *   shows instead is FOUR DIFFERENT AGENT CLIENTS running at once, which is
 *   the page's own claim made visible and which none of the page's other three
 *   frames carries: the hero and row 03 are `ClaudeCode` on every card, and
 *   row 01 is a form with no cards at all. So the caption leans there, and
 *   does not describe the focused pane's contents. The row's density half is
 *   now carried by the body copy alone.
 *
 *   `session-management-card.jpg` is not orphaned by the move: it is
 *   `docs/session-management.md`'s card-anatomy illustration, which is the
 *   claim it was really making here. `home-hero-dashboard.jpg` briefly held
 *   this slot while the new capture was being taken and has gone back to being
 *   `docs/session-management.md`'s Compact-density illustration.
 * - Row 04 (dispatching) now carries `dispatch.webp`, commissioned for this
 *   row because nothing in the repository illustrated a dispatched unit --
 *   `docs/dispatcher-mode.md` carries no images at all -- and shot to the
 *   specification the row asked for: a dispatcher pane mid-conversation, the
 *   plain-English ask visible, the reply naming the unit and its sibling
 *   directory, and the new unit's card already on the deck beside it. The row
 *   rendered as a single centred column while the filename did not exist,
 *   rather than shipping a broken image; adding `shot:` was the whole change.
 *
 *   Two things the caption is written AROUND, both read off the frame rather
 *   than off the row's argument. Both cards show `Idle` and `Tools: 0`: the
 *   unit has been created and has not begun working, so a caption claiming
 *   visible work would be describing the copy and not the picture. And the
 *   dispatched unit's own card carries no directory (`Dir: --`) -- the sibling
 *   path `../storefront-dispatch-login-timeout` is in the reply text beside
 *   it, not on the card.
 *
 *   It carries `wide`. The frame shipped at 1946x1187 and was recropped to
 *   1946x482 to drop a large empty region below the cards -- content
 *   unchanged, so the alt and the caption survived it, but the shape did not:
 *   1.64 became 4.04. See the flag's own note above for the arithmetic. What
 *   the crop removed is worth knowing before writing to this frame again: the
 *   deck footer's counts, the `TYPING` indicator and the `Command Mode Ctrl+D`
 *   hint are all gone from it, so neither the alt nor the caption may reach
 *   for them.
 * - `reattach.png` leaves the page with the "walk away" row it illustrated.
 *   The maintainer's verdict on that frame was "I'm not sure I understand" it,
 *   and the diagnosis is that the idea has no moment to photograph: detaching
 *   and reattaching is the absence of an event, and the labelled BEFORE/AFTER
 *   pair needed a paragraph of caption to parse, which is the frame failing to
 *   carry it. The claim moves to principle 03, where a sentence makes it
 *   cleanly.
 *
 * `detach.webp` -- the Quit dialog, supplied and approved by the maintainer
 * for the old "walk away" row -- has no home on this page either, and for a
 * reason that now outranks the two it was declined for (it shows the CHOICE
 * rather than the outcome, and it spells "daemon" on screen in a pass that
 * removed that vocabulary from the marketing surface): row 04 is dispatching
 * now, so the row it was meant for does not exist. It is placed in
 * `docs/session-management.md` instead, beside the detach/resume cycle it
 * actually illustrates, where "daemon" is ordinary vocabulary.
 *
 * `orchestration-config.png` was deleted from `site/static/img/` in an earlier
 * round -- it published a maintainer's home path and an unrelated project's
 * orchestration config. Confirmed after the deletion rather than before it:
 * `grep -rn 'orchestration-config' docs/ site/` returns nothing.
 */
export const screenshots = {
  hero: {
    src: '/img/orchestration-coder.png',
    alt: 'Agent Deck’s split view — a sidebar of orchestrator, coder and reviewer cards with only the coder marked Working, beside the coder’s own pane running a grep, an edit to src/email/order_confirmation.rs, and cargo test order_confirmation',
    caption:
      'A coder pane working on what the orchestrator just delegated to it.',
  },
  newPane: {
    src: '/img/orchestration-new-deck.png',
    alt: 'The New Agent form — Dir /tmp/storefront, a Mode row offering No mode, Orch: review-team, schedule and dispatcher, Agent on auto, Name storefront, Command claude, and Submit and Cancel buttons',
    caption:
      'The form Ctrl+n opens once the directory is picked. The Mode row is where the choice above is made — a plain agent, a scheduled task, a dispatcher, and review-team, which is not a built-in option but the orchestration this directory defines.',
  },
  deck: {
    src: '/img/busy-deck-real.webp',
    tall: true,
    alt: 'A deck with the Dashboard and four orchestration tabs along the top and six agent cards down the sidebar — ClaudeCode, Pi, OpenCode and Codex filling the orchestrator, coder, reviewer, auditor, tester and release roles, two marked Working and the rest Idle — the cards all laid out the same way, with the directory, the last prompt, the command last run, the time since the last activity and a tool count, over a footer reading 23 active, 2 working, 1 thinking, 20 idle',
    caption:
      'Four different agent clients — Claude Code, Pi, OpenCode and Codex — running side by side under one orchestrator, and the deck reads all of them the same way: the same card, the same live status, whichever client is behind it. The header counts the six sessions in view out of the deck’s 25; the footer counts every agent it is holding: 23 active, 2 working, 1 thinking, 20 idle.',
  },
  parallel: {
    src: '/img/orchestration-delegation-parallel.png',
    alt: 'Orchestrator delegating to reviewer and auditor in parallel — both cards light up simultaneously',
    caption:
      'One agent delegating to two others at once. Both cards light up together.',
  },
  dispatch: {
    src: '/img/dispatch.webp',
    wide: true,
    alt: 'A dispatcher pane mid-conversation — the ask “Start work on the login timeout bug.”, and a reply naming a single-agent unit login-timeout and the sibling directory ../storefront-dispatch-login-timeout — beside a sidebar where a second card, dispatch-login-timeout, has already appeared under the dispatcher’s own; both cards read Idle with a tool count of 0',
    caption:
      'One sentence to a dispatcher pane. The reply names the unit — login-timeout — and the sibling directory it gets instead of this checkout, ../storefront-dispatch-login-timeout, and the unit’s card is already on the deck, directly under the card of the pane that asked for it. Both cards still read Idle with no tools run: this is the unit arriving, a moment before it starts.',
  },
};

export const audience = {
  heading: 'Who this is for',
  forYou: [
    'You already run more than one coding agent at a time, and you are losing track of what each one is doing.',
    'You have a terminal you have spent years configuring, and you are not moving into someone else’s app to get a dashboard.',
    'You want one agent to plan the work and hand pieces of it to others, with somewhere to watch that happen.',
  ],
  notYou:
    'If you run one agent, in one window, and that is working fine — this is not for you yet. Come back when the second one starts drifting.',
};

/**
 * The four story rows. Each step names the `screenshots` entry it carries, so
 * the page joins copy to frame BY NAME -- `step.shot` -- rather than by lining
 * two arrays up positionally. Reordering the steps or inserting one therefore
 * carries each row's image, alt text and caption with it; the positional form
 * mispaired them silently, which is what `bc6f0abe` had to repair by hand.
 * `screenshots` is declared above, so a typo here is `undefined` and the first
 * property read fails the build rather than rendering the wrong frame.
 *
 * `shot` is OPTIONAL. A step without one renders as a single centred column
 * rather than as half a two-column row with the picture missing. Every step
 * carries one today; the branch is kept for the next row written before its
 * frame is taken, which is how row 04 shipped while `dispatch.webp` was being
 * captured.
 *
 * The arc is an escalation, and each rung is a shipped feature rather than a
 * restatement of the one before it: one pane, then many of them visible at
 * once, then one agent running the others, then whole new lines of work
 * starting in their own copies of the repo without you setting any of it up.
 */
export const workflow = [
  {
    step: '01',
    title: 'Open a pane',
    body: 'Ctrl+n, pick a directory, and choose what starts there. A single agent on the command you give it. A full multi-agent orchestration, in its own tab. A dispatcher you can ask for isolated work. Or a scheduled task, written here and fired later by the deck itself.',
    shot: screenshots.newPane,
  },
  {
    step: '02',
    title: 'Watch every one of them',
    body: 'Every pane gets a card: what it is doing right now, the tool it is running, its directory, its last prompt. Live, with nothing for you to wire up. The more agents you run, the tighter the cards get — the deck would rather shrink them than make you go looking for one.',
    shot: screenshots.deck,
  },
  {
    step: '03',
    title: 'Let one agent run the others',
    body: 'Define the roles your project needs — orchestrator, coder, reviewer, release — and one agent hands each piece of work to the right one. Every worker starts fresh, with only the context it was given, and you watch the hand-offs land in the other panes. If a worker goes quiet, the deck tells the orchestrator, so a stalled run does not sit there unnoticed.',
    shot: screenshots.parallel,
  },
  {
    step: '04',
    title: 'Send work off on its own',
    body: 'Ask a dispatcher pane for something — “work on the search bug” — and it makes a separate copy of your repository and puts an agent, or a whole orchestration, to work inside it. It works in that copy rather than in your working tree, so start as many as you like and carry on with what you were doing. Each unit arrives on the deck as a card or a tab, and reports back when it is done.',
    shot: screenshots.dispatch,
  },
];

export const docLinks = {
  gettingStarted: '/docs/getting-started',
  installation: '/docs/installation',
  orchestration: '/docs/orchestration',
  dispatcher: '/docs/dispatcher-mode',
  configuration: '/docs/configuration',
  keyboard: '/docs/keyboard-shortcuts',
  remote: '/docs/remote-environments',
};
