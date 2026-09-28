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
 * Remote environments are real but secondary -- they are about WHERE the deck
 * runs, not what it does -- and keep the band they already had. The desktop
 * app is not secondary any more: since PRD #1321 the page presents both
 * clients from the top, and its install detail sits with the CLI's. Workspace modes are being REMOVED (issue #1199), so they are
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
 * 1. The desktop GUI. Issue #1021 called the artifacts "signed and notarized"
 *    when they were neither, and this page said "Unsigned, so macOS will stop
 *    you" until issue #1324. That stopped being true at v0.42.0: PRD #757's
 *    `desktop-sign` job published its `.dmg` signed with the project's
 *    Developer ID and notarized by Apple, the ticket stapled to both the app
 *    and the disk image, and the maintainer verified it on a Mac (PRD #757's
 *    M6). The `.deb` is still unsigned (PRD #757 Decision 7), and signing did
 *    not graduate the GUI out of alpha (Decision 9). The copy below claims
 *    what v0.42.0 carries and points at each release's notes rather than
 *    promising every release is signed, because that is not a property the
 *    workflow guarantees: `desktop-sign` still has a deliberate unsigned mode
 *    for a run with none of the Apple secrets registered, a failed signing
 *    run publishes no `.dmg` at all, and the desktop section of each release
 *    note is composed from what that run actually signed (Decision 10).
 *    v0.42.0 carries exactly two desktop assets, both named
 *    `...-desktop-alpha-...`. There is also no
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
 *    the `attest` job in `release.yml` matched only `dot-agent-deck-*` and
 *    `dot-agent-deck-desktop-alpha-*`, so `checksums.txt` and
 *    `checksums-desktop-alpha.txt` were attested by nothing. Measured on
 *    v0.41.0: `gh attestation verify` exits 0 on the `.dmg` and on a CLI
 *    binary, and returns an attestation API 404 on both checksum manifests.
 *    The page narrowed its claim to the six assets that genuinely carried
 *    provenance, and noted that widening the subjects was the better fix.
 *    Issue #1152 then did the widening: each manifest now travels to the
 *    `attest` job as an artifact of its own, published bytes and all, so all
 *    eight are subjects. The claim here is widened back to match -- with the
 *    ONE exclusion that survives, which is not an oversight: GitHub's
 *    auto-generated "Source code" archives are synthesized from the tag
 *    rather than uploaded by the workflow, so no job ever holds their bytes
 *    to attest. Read the wording as "every asset the workflow uploads", which
 *    is what it says, rather than as "everything on the release page".
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
 * Ordering, not just wording, is corrected too: the macOS caveat says to
 * verify provenance BEFORE opening the app. That ordering outlived the caveat
 * it was written for -- it used to come before following the release notes
 * past Gatekeeper, and since v0.42.0's signed `.dmg` (item 1) it comes before
 * the one confirmation macOS still asks for. Any exact OS steps stay
 * centralized in the release notes rather than being duplicated here.
 */

export const product = {
  name: 'Agent Deck',
  binary: 'dot-agent-deck',
  owner: 'DevOps Toolkit',
  tagline:
    'A dashboard for running, orchestrating and dispatching AI coding agents in parallel — in your terminal, or in a desktop app',
  shortDefinition:
    'Runs your AI coding agents under one background daemon, shows what each of them is doing in real time — in a terminal UI or in a desktop app, both watching the same agents — and starts new work for you: a team under one orchestrator, an isolated unit in its own copy of the repo, or a task that fires on a schedule.',
  license: 'MIT',
  repo: 'https://github.com/vfarcic/dot-agent-deck',
  issues: 'https://github.com/vfarcic/dot-agent-deck/issues',
  releases: 'https://github.com/vfarcic/dot-agent-deck/releases/latest',
};

/**
 * The two clients (PRD #1321 M5). Both are clients of the same daemon, which
 * is what the section says first, because it is the fact a reader needs to
 * choose: nothing is lost by picking one, since an agent started in either
 * shows up in the other.
 *
 * Every claim is checked against the code. The TUI "attaches to one daemon at
 * a time" (`Endpoint`, `src/daemon_client.rs`); the desktop app holds several
 * (`desktop/src-tauri/src/daemon_bridge.rs`, one link per deck) and has voice
 * control, which the TUI does not. "Starts no daemon of its own" is the
 * flag-off app: `connect()` bootstraps with `startIfMissing: false`
 * (`desktop/src/lib/bridge.ts`), and the controls that start one are on the
 * experimental deck screen. The screenshots are `cargo docs-screenshots`
 * output (the `dashboard` scenario), one per client, of the same scene.
 */
export const clients = {
  heading: 'Two clients, one daemon',
  intro:
    'The agents run under a small background daemon, not inside a window. The terminal UI and the desktop app are two ways of looking at it, so an agent you start in one shows up in the other, and closing either leaves the agents running.',
  items: [
    {
      title: 'Terminal UI',
      body: 'The dot-agent-deck binary, in the terminal you already use: agent cards beside their live panes, keyboard first, with workspace modes, the Schedules manager and remote hosts over ssh. It attaches to one daemon at a time.',
      shot: {
        src: '/img/dashboard-tui.png',
        alt: 'The terminal UI with four agent cards on the left, each showing its status, directory and last prompt, and the focused agent’s terminal pane on the right',
      },
      link: {label: 'Get started with the terminal UI →', to: '/docs/getting-started'},
    },
    {
      title: 'Desktop app (alpha)',
      body: 'A native window for macOS on Apple Silicon and Linux amd64: one dashboard over several daemons at once — this machine and remote ones — with each agent’s terminal a click away, and voice control. It connects to a running daemon and starts none of its own.',
      shot: {
        src: '/img/dashboard-desktop.png',
        alt: 'The desktop app’s dashboard with four agents in one daemon section, each row showing its status, name and uptime',
      },
      link: {label: 'Read about the desktop app →', to: '/docs/desktop'},
    },
  ],
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
 *   which holds many at once, as the two-clients section says.
 */
export const why = {
  heading: 'Why Agent Deck',
  paragraphs: [
    "Running one AI agent at a time, you're still a software engineer who happens to use AI. Running many at the same time, you stop being one. You become a project manager supervising a team, a tech lead unblocking them, an architect designing the approach, a product manager deciding what to build.",
    "The agents do the work — writing the code, running the tests, watching the pipelines, answering the review comments. Your job is everything around it — defining the work up front, supervising it in flight, and validating that the right thing got built. None of this is new. It's the same craft people have practiced for decades. The team just looks different.",
    'Agent Deck is the tool that lets you do that without losing your mind. One place holds all of it — a lone agent, a team working under an orchestrator, a unit off in its own copy of the repo — and every one of them is something you can open, watch and type into, in the terminal you already use or in a desktop window beside it, with the agent client you already know.',
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
    title: 'Your terminal, or a window',
    description:
      'The terminal UI runs in Ghostty, iTerm2, Alacritty, Kitty, WezTerm — whatever you already configured — as a guest, not a replacement, with no multiplexer to set up underneath. The desktop app is a native window over the same agents, so you can use either, or both at once.',
  },
  {
    title: 'Uses your agent client',
    description:
      'Claude Code, OpenCode, Pi, Codex or Devin — keep the shortcuts, skills, and configs you already dialed in. No new agent client to learn.',
  },
  {
    title: 'Closing the window does not stop the work',
    description:
      'Detach the terminal UI or quit the desktop app, and the agents carry on without you; open either again and you rejoin the same agents, mid-run. The same holds over ssh — agents running on another machine stay running when you disconnect.',
  },
  {
    title: 'Keyboard, pointer or voice',
    description:
      'In the terminal UI every action is one or two keystrokes away, because managing a team of agents has to fit in muscle memory, and a button bar along the bottom names each command and the key it answers to. The desktop app is built for the pointer, with a few keys of its own, and it can be driven by voice: open screens, start a new agent, and type into one.',
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
 * Mirrors the Platform Support table in `docs/installation.md`, including its
 * Desktop app row (the first three rows are the terminal UI and the daemon,
 * which the CLI carries on every platform), with one deliberate divergence: that table links the Windows-native tracking issue
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
    platform: 'Desktop app',
    detail: 'macOS on Apple Silicon and Linux amd64',
    status: 'Alpha',
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
  {
    name: 'Desktop app (alpha)',
    detail: 'Outside the CLI’s support expectations, from the releases page. A release can ship without one or both desktop packages, so check the assets on the release you open; the CLI ships even when they do not. The app connects to a daemon that is already running and starts none of its own: on Linux the .deb installs the dot-agent-deck CLI alongside the app, and on macOS install the CLI too, or start the daemon from the copy inside the app bundle, as the install steps describe. Each release’s notes say whether its .dmg is signed and notarized; the .deb is unsigned, so check it as below before installing it.',
    code: null,
    files: [
      'dot-agent-deck-desktop-alpha-macos-arm64.dmg',
      'dot-agent-deck-desktop-alpha-linux-amd64.deb',
    ],
    link: {label: 'How to install the desktop app →', to: '/docs/installation#desktop-app'},
  },
];

/**
 * The download check, for everything the install section offers (PRD #1321).
 * It used to live in a separate band that introduced the desktop app late in
 * the page ("There is a desktop app too. It is an alpha."), which stopped
 * making sense once the page presented both clients from the top. What that
 * band carried was install detail -- the two desktop assets, signing, the
 * missing Windows bundle, and this check -- so the assets and the signing
 * line moved into the desktop app's install route, the missing Windows build
 * is the platform list's last row, and the check sits under both columns,
 * because it covers every asset the release publishes, the CLI's included.
 * The signing claims follow the note at the top of this file (item 1): each
 * release's notes say whether its own .dmg is signed.
 */
export const verify = {
  heading: 'Check what you downloaded',
  note: 'Every asset the release workflow uploads — the binaries, the desktop packages, and both checksums manifests — carries build provenance: proof that this exact file came out of this repository’s release workflow, and a record of the commit it was built from. Run it on what you downloaded before you open it.',
  command:
    'gh attestation verify <file> --repo vfarcic/dot-agent-deck --signer-workflow vfarcic/dot-agent-deck/.github/workflows/release.yml',
  scope:
    'The manifests matter most here: the list of hashes you would check everything else against is exactly the file worth swapping, so it is vouched for by the same proof rather than trusted on its own. The one thing not covered is GitHub’s own “Source code” archives — GitHub synthesizes those from the tag rather than the workflow uploading them.',
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
 * There WERE two layout flags on these entries, `tall` and `wide`, and both
 * left when the story stopped alternating text and one frame side by side
 * (PRD #1321). They existed to keep a two-column row in step with its
 * neighbours: `wide` stacked a row whose frame was a 4.04 band, and `tall`
 * capped the width of the two near-4:3 frames, `busy-deck-real.webp` and
 * `dispatch.webp`, so they stood no taller than the 16:9 rows. Each step now
 * puts its text over the terminal UI's and the desktop app's frames side by
 * side, each frame at half the story's width and the top edges aligned, so a
 * taller frame no longer pushes a row out of step with anything, and neither
 * flag has a job left.
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
 * - Row 04 (dispatching) now carries the MAINTAINER'S OWN capture, at the same
 *   `dispatch.webp` filename, so nothing else in this file moved to take it.
 *   It replaces a frame commissioned for the row, which showed one
 *   single-agent unit reading `Idle` with `Tools: 0` -- the moment after it
 *   was created. That was the strongest claim that frame could support, and
 *   its caption had to say so out loud ("a moment before it starts"), because
 *   a caption claiming visible work would have been describing the copy and
 *   not the picture.
 *
 *   The new frame supports the row's WHOLE claim rather than half of it. The
 *   row says "an agent, or a whole orchestration" and "start as many as you
 *   like". The frame shows two units up at once -- `voice-control` and
 *   `product-website`, each a 6-role mixed orchestration, each in its own
 *   sibling checkout, both cut from `main` at the same commit -- and then an
 *   ask for three running at a time up to twenty in total, with the reply
 *   working out that eighteen are left. Both halves are visible now instead of
 *   asserted, which is why the old caption had to go rather than be trimmed.
 *
 *   Three things the copy is written to deliberately.
 *
 *   The maintainer drew a YELLOW ARROW on the frame, pointing down at that
 *   ask. It is content, not a blemish, so the alt names it and says what it
 *   points at -- a screen-reader user should not be the one person who cannot
 *   tell an annotation is there -- and the caption follows where it points
 *   instead of talking around it.
 *
 *   The two directory paths are the maintainer's real ones, under
 *   `/home/vfarcic/code/`. That is settled, twice over: the project is public
 *   and the capture is the deck doing real work, the same verdict that carried
 *   row 02's frame. Do not scrub them, do not hedge around them, and do not
 *   re-raise it.
 *
 *   The sidebar's single card (`Prmt: Why is 450 red?`) and the pane's token
 *   counter are both incidental to what the row argues, so neither the alt nor
 *   the caption reaches for them.
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
  /*
   * The hero is a PAIR since PRD #1321, one frame per client, shown side by
   * side at the same height. `aspect` is each frame's width over its height;
   * the page sizes the two columns in that ratio, which is what makes their
   * heights match without cropping either. Both frames are the maintainer's
   * own captures of the SAME orchestration, dot-agent-deck-dispatch-issue-544,
   * taken for this slot at close to the same shape (1.31 and 1.21), so each
   * window gets about half the width and stays readable. They replaced a 3:1
   * terminal UI band (`orchestration-coder.png`) that, beside any desktop
   * frame at equal height, left the desktop window too narrow to read.
   */
  hero: {
    src: '/img/orchestration-tui-home.png',
    aspect: 2536 / 1942,
    alt: 'Agent Deck’s terminal UI on the dot-agent-deck-dispatch-issue-544 orchestration tab — six role cards down the left, orchestrator, coder, reviewer, auditor, tester and release, run by Claude Code, Pi, OpenCode and Codex, with tester Working and the rest Idle, beside the orchestrator’s pane showing its report on the pull request it prepared, over a footer reading 15 active, 4 working, 1 thinking, 10 idle',
  },
  heroDesktop: {
    src: '/img/orchestration-desktop-home.png',
    aspect: 2482 / 2044,
    alt: 'The desktop app’s Agent dashboard across all daemons — 43 agents over 3 daemons — with a remote daemon’s standalone agents above two six-role orchestrations: the first lists 01 orchestrator, marked ORCHESTRATOR, then coder, reviewer, auditor, tester and release, one running and five waiting, each with its uptime',
  },
  heroCaption:
    'The same six-role orchestration in both clients. In the terminal UI its roles are cards beside the orchestrator’s pane, Claude Code, Pi, OpenCode and Codex side by side; in the desktop app they are a group on a dashboard that spans every daemon, with the orchestrator marked.',
  newPane: {
    src: '/img/orchestration-new-deck.png',
    alt: 'The New Agent form — Dir /tmp/storefront, a Mode row offering No mode, Orch: review-team, schedule and dispatcher, Agent on auto, Name storefront, Command claude, and Submit and Cancel buttons',
    caption:
      'The form Ctrl+n opens once the directory is picked. The Mode row is where the choice above is made — a plain agent, a scheduled task, a dispatcher, and review-team, which is not a built-in option but the orchestration this directory defines.',
  },
  deck: {
    src: '/img/busy-deck-real.webp',
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
    alt: 'A deck with the Dashboard and three mixed · dot-agent-deck… orchestration tabs along the top, and a dispatcher pane reading “Confirmed — both units are now up:” over voice-control → /home/vfarcic/code/dot-agent-deck-dispatch-voice-control (#802) and product-website → /home/vfarcic/code/dot-agent-deck-dispatch-product-website (#1021), then “Both cut from main at d7bbbd39, each a 6-role mixed orchestration.” A yellow arrow drawn onto the screenshot points down at the request below that, which asks for three dispatched agents or teams at a time and a stop at twenty in total; the reply reads it back as a standing loop with eighteen still to dispatch. The footer counts 17 active agents, 1 working and 1 thinking',
    caption:
      'Two units up from one dispatcher pane, and neither of them is a single agent — each is a whole 6-role orchestration, in its own copy of the repository, both cut from main at the same commit. The yellow arrow marks the ask that follows: keep three running at a time, start a fresh one whenever a slot frees, stop at twenty — which the pane reads back as eighteen still to go.',
  },
  /*
   * The desktop app's frames for the story rows. Unlike the terminal UI's
   * frames above, which are captures of real decks at work, these come from
   * `cargo docs-screenshots` (docs/develop/docs-screenshots.md) and show the
   * app's fixture data, so a recapture after a UI change is one command. The
   * alt text and captions are written against the committed frames.
   */
  newAgentDesktop: {
    src: '/img/new-agent-desktop.png',
    alt: 'The desktop app’s New agent dialog over the Agent dashboard — daemon Local daemon, directory /home/dev/demo-project, a Mode row offering No mode, Orch: demo-loop, schedule and dispatcher, Name demo-project, an empty Command field, and Discard and Create agent buttons',
    caption:
      'The desktop app’s New agent dialog makes the same choice, after asking which daemon to start the agent on. demo-loop is not built in: it is the orchestration this directory defines.',
  },
  deckDesktop: {
    src: '/img/dashboard-fleet-desktop.png',
    alt: 'The desktop app’s Agent dashboard showing All daemons: a Local daemon section with four standalone agents and a dev@build-box section with two, each row showing its status, running or waiting, its name and its uptime, under counters reading 6 agents, 3 running, 3 waiting and daemons 2/2',
    caption:
      'Two daemons on one dashboard, this machine and a remote build box, each in its own section with its own New agent button.',
  },
  parallelDesktop: {
    src: '/img/orchestration-desktop.png',
    alt: 'The desktop app’s Agent dashboard with a demo-loop orchestration group below the standalone agents: 01 planner, marked ORCHESTRATOR, and 02 builder, both running, with a Close button on the group’s header',
    caption:
      'In the desktop app an orchestration is a group: its roles in order, the one you message marked ORCHESTRATOR, and one Close for all of them.',
  },
};

export const audience = {
  heading: 'Who this is for',
  forYou: [
    'You already run more than one coding agent at a time, and you are losing track of what each one is doing.',
    'You would rather keep your own tools than move into someone else’s IDE to get a dashboard: the terminal you have spent years configuring, or a plain desktop window beside it.',
    'You want one agent to plan the work and hand pieces of it to others, with somewhere to watch that happen.',
  ],
  notYou:
    'If you run one agent, in one window, and that is working fine — this is not for you yet. Come back when the second one starts drifting.',
};

/**
 * The four story rows. Each step names the `screenshots` entries it carries,
 * so the page joins copy to frame BY NAME -- `step.shots.tui` and
 * `step.shots.desktop` -- rather than by lining two arrays up positionally.
 * Reordering the steps or inserting one therefore carries each row's images,
 * alt text and captions with it; the positional form mispaired them silently,
 * which is what `bc6f0abe` had to repair by hand. `screenshots` is declared
 * above, so a typo here is `undefined` and the first property read fails the
 * build rather than rendering the wrong frame.
 *
 * The copy is written for BOTH clients (PRD #1321): each capability here lives
 * in the daemon, so the terminal UI and the desktop app both have it, and where
 * the two present it differently -- a card or a row, a tab or a group -- the
 * body names both. Both clients' frames are shown at once, side by side under
 * the text, each labelled with its client: the home page deliberately does not
 * make the reader pick one, which is what the docs' TUI | Desktop tabs do.
 *
 * `shots` is OPTIONAL, and so is either half of it. A step with no frame at
 * all is text alone. A step with one client's frame shows it alone, centred at
 * the width a frame has in the other rows; row 04 is that case, because the
 * dispatcher's work is a conversation in an agent's pane and the desktop
 * fixture has no dispatcher transcript to capture, so its `desktopNote` is
 * added to the terminal UI frame's caption to say what the desktop app does
 * there instead.
 *
 * The arc is an escalation, and each rung is a shipped feature rather than a
 * restatement of the one before it: one agent, then many of them visible at
 * once, then one agent running the others, then whole new lines of work
 * starting in their own copies of the repo without you setting any of it up.
 */
export const workflow = [
  {
    step: '01',
    title: 'Start an agent',
    body: 'Pick a directory and choose what starts there: Ctrl+n in the terminal UI, New agent in the desktop app. A single agent on the command you give it. A full multi-agent orchestration, every role at once. A dispatcher you can ask for isolated work. Or a scheduled task, written here and fired later by the deck itself.',
    shots: {tui: screenshots.newPane, desktop: screenshots.newAgentDesktop},
  },
  {
    step: '02',
    title: 'Watch every one of them',
    body: 'Every agent shows up live, with what it is doing right now, the tool it is running, its directory and its last prompt, and nothing for you to wire up. In the terminal UI each one gets a card, and the cards get tighter the more agents you run, because the deck would rather shrink them than make you go looking for one. In the desktop app each one gets a row with the columns you choose, and one dashboard can hold the agents of several daemons, on this machine and on remote ones.',
    shots: {tui: screenshots.deck, desktop: screenshots.deckDesktop},
  },
  {
    step: '03',
    title: 'Let one agent run the others',
    body: 'Define the roles your project needs — orchestrator, coder, reviewer, release — and one agent hands each piece of work to the right one. Every worker starts fresh, with only the context it was given, and you watch the hand-offs land: across the orchestration’s tab in the terminal UI, down its group in the desktop app. If a worker goes quiet, the deck tells the orchestrator, so a stalled run does not sit there unnoticed.',
    shots: {tui: screenshots.parallel, desktop: screenshots.parallelDesktop},
  },
  {
    step: '04',
    title: 'Send work off on its own',
    body: 'Ask a dispatcher for something — “work on the search bug” — and it makes a separate copy of your repository and puts an agent, or a whole orchestration, to work inside it. It works in that copy rather than in your working tree, so start as many as you like and carry on with what you were doing. Each unit arrives on the deck, as a card or a tab in the terminal UI and as a row or a group in the desktop app, and reports back when it is done.',
    shots: {tui: screenshots.dispatch},
    desktopNote:
      'In the desktop app a dispatcher is an agent like any other: you talk to it in its pane, and the units it starts appear on the dashboard.',
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
  desktop: '/docs/desktop',
};
