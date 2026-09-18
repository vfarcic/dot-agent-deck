/**
 * Landing-page content (issue #1021).
 *
 * Every user-visible string on `/` comes from here, so the copy can be
 * reviewed and corrected in one file rather than hunted through JSX. It began
 * as the shared module the four `/style/*` candidates imported; the candidates
 * are gone and direction C is now `/`, so the exports that only served the
 * losing directions went with them.
 *
 * The prose is lifted from the previous homepage, with seven corrections it
 * never had, every one checked against the repository rather than reasoned
 * about. The first four are the ones the page shipped with; the issue text was
 * wrong about two of them:
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
 * 3. Windows. The page linked #42, closed on 2026-07-15. The open work is #164,
 *    which is what `docs/installation.md` already points at.
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
    'A terminal dashboard for running multiple AI coding agents in parallel',
  shortDefinition:
    'A single Rust binary that runs your AI coding agents as embedded terminal panes, tracks what each one is doing in real time, and lets one agent delegate work to the others.',
  license: 'MIT',
  repo: 'https://github.com/vfarcic/dot-agent-deck',
  issues: 'https://github.com/vfarcic/dot-agent-deck/issues',
  releases: 'https://github.com/vfarcic/dot-agent-deck/releases/latest',
};

/** The "Why Agent Deck" block, verbatim from the previous homepage. */
export const why = {
  heading: 'Why Agent Deck',
  paragraphs: [
    "Running one AI agent at a time, you're still a software engineer who happens to use AI. Running five at once, you stop being one. You become a project manager supervising a team, a tech lead unblocking them, an architect designing the approach, a product manager deciding what to build.",
    "The agents write the code. Your job is everything around it — defining the work up front, supervising it in flight, and validating that the right thing got built. None of this is new. It's the same craft people have practiced for decades. The team just looks different.",
    'Agent Deck is the tool that lets you do that without losing your mind. One dashboard, every agent visible at a glance, keyboard-driven, in the terminal you already use, with the agent client you already know.',
  ],
};

export const principles = [
  {
    title: 'Runs in your terminal',
    description:
      'Ghostty, iTerm2, Alacritty, Kitty, WezTerm — whatever you already configured. Agent Deck is a guest, not a replacement.',
  },
  {
    title: 'Uses your agent client',
    description:
      'Claude Code, OpenCode, Pi, Codex or Devin — keep the shortcuts, skills, and configs you already dialed in. No new agent client to learn.',
  },
  {
    title: 'Focus-mode side panes',
    description:
      'Pair an agent with live test runs, log tails, or kubectl watches via per-project TOML config. Deep-diving on one agent doesn’t mean opening a dozen extra terminals.',
  },
  {
    title: 'No buttons',
    description:
      'Every action is one or two keystrokes away. Managing a team of agents has to fit in muscle memory — mouse-clicking breaks flow.',
  },
];

/**
 * Shipped agents, from `src/agent_registry.rs`. `integration` is that file's
 * own `IntegrationStrategy` for the agent -- four different mechanisms across
 * five agents, which is why the list is worth showing rather than asserting.
 */
export const agents = [
  {
    name: 'Claude Code',
    command: 'claude',
    integration: 'Native hooks',
    href: 'https://www.anthropic.com/claude-code',
  },
  {
    name: 'OpenCode',
    command: 'opencode',
    integration: 'Plugin',
    href: 'https://opencode.ai',
  },
  {
    name: 'Pi',
    command: 'pi',
    integration: 'Bundled extension',
    href: 'https://github.com/earendil-works/pi',
  },
  {
    name: 'Codex',
    command: 'codex',
    integration: 'Stdout wrapper',
    href: 'https://github.com/openai/codex',
  },
  {
    name: 'Devin',
    command: 'devin',
    integration: 'Native hooks',
    href: 'https://devin.ai',
  },
];

export const agentsNote =
  'Any other command still runs in a pane — it just gets no live status tracking. Adapters for Gemini CLI and Aider are designed and open, not shipped.';

/** Mirrors the Platform Support table in `docs/installation.md`. */
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
    status: 'Not yet — tracked in #164',
    supported: false,
    href: 'https://github.com/vfarcic/dot-agent-deck/issues/164',
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
 */
export const desktop = {
  heading: 'There is a desktop app too. It is an alpha.',
  intro:
    'Where the terminal deck attaches to one daemon at a time, the desktop app is a native window that holds several at once — the agents on your laptop and the ones on a remote box, side by side in the same window. It rides along with a release rather than gating it, so check the assets on the release you open: the CLI ships even when a desktop bundle does not.',
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
      body: 'The app carries the daemon as a sidecar, and no Windows daemon binary is published — so there is nothing to bundle yet.',
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
 * The screenshot catalogue. Every entry here is rendered: `orchestration`
 * (orchestration-start.png) once sat here unreferenced and is gone, and
 * `dashboard` / `modes` left for the same reason in this pass.
 *
 * Each story row carries the frame its own copy describes, and each caption
 * describes what is IN the frame rather than what the row argues. Two pairings
 * changed once the refreshed captures landed:
 *
 * - Row 01 ("Open a pane. Ctrl+n, pick a directory, name it, and give it the
 *   command that launches your agent") now carries `orchestration-new-deck.png`
 *   -- that sentence word for word. The reviewer's earlier suggestion, re-pair
 *   `orchestration-start.png` onto this row, was declined and stays declined:
 *   it is an ORCHESTRATION frame, tab bar and all, two rows before
 *   orchestration is introduced. The row's real fix was always a capture of the
 *   form `Ctrl+n` opens, and that capture now exists. The recapture is of the
 *   CURRENT form, which the old one predated: it always shows the Mode chips
 *   and the Agent selector now, and the old `Tip:` row is gone. The caption
 *   accounts for both; the row body does not, because the row is about opening
 *   a pane and the dialog merely offers them.
 * - Row 04 ("Walk away ... detach the deck, come back later, reattach") now
 *   carries `reattach.png` and no longer carries `modes.png`, which showed git
 *   status and kubectl panes and illustrated nothing the row claims.
 *
 * `home-hero-dashboard.jpg` and `modes.png` are both freed by those two moves,
 * and both LEAVE the landing page rather than being re-homed. Nothing on the
 * page wants either one without a slot being invented for it: every story row
 * already carries the frame its own copy describes, `why` is the page's one
 * text-only breather between two image-heavy bands, and `showcase` is a single
 * device frame under the hero. Neither is orphaned -- the dashboard shot is
 * `docs/session-management.md`'s Compact-density illustration, which is the
 * exact claim its caption made here, and the modes shot is used by
 * `docs/getting-started.md` and `docs/workspace-modes.md`. Nor does the page
 * lose "several agents at once": the hero shows a three-card sidebar with live
 * per-card status, and row 03 a five-card one.
 *
 * `orchestration-config.png` was deleted from `site/static/img/` in the same
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
    alt: 'The New Agent form — Dir /tmp/storefront, a Mode row offering No mode, schedule and dispatcher, Agent on auto, Name storefront, Command claude, and Submit and Cancel buttons',
    caption:
      'The form Ctrl+n opens once the directory is picked: name the pane, give it a command. Mode and Agent are optional — left on auto, the client is read off the command.',
  },
  card: {
    src: '/img/session-management-card.jpg',
    alt: 'One agent card — storefront-checkout, Working — with its directory, its last prompt, three recent tool calls, and Last 3s / Tools 3 on the bottom border',
    caption:
      'One card: status, directory, the last prompt, the recent tool calls, and the last-activity and tool counters on the border.',
  },
  parallel: {
    src: '/img/orchestration-delegation-parallel.png',
    alt: 'Orchestrator delegating to reviewer and auditor in parallel — both cards light up simultaneously',
    caption:
      'One agent delegating to two others at once. Both cards light up together.',
  },
  reattach: {
    src: '/img/reattach.png',
    alt: 'Two labelled frames of one pane, storefront-release-verification — BEFORE DETACH ending at 22:23:56 with 27 integration tests passed, and AFTER REATTACH opening on that same line and running on to 22:24:12',
    caption:
      'Two frames, not one view: the same pane before detaching and after reattaching. The 22:23:56 line that closes the top one opens the bottom — one session, not a restart.',
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

export const workflow = [
  {
    step: '01',
    title: 'Open a pane',
    body: 'Ctrl+n, pick a directory, name it, and give it the command that launches your agent. It runs in an embedded PTY — no multiplexer involved.',
  },
  {
    step: '02',
    title: 'Watch every one of them',
    body: 'Each pane gets a card: status, the tool it is running right now, its working directory, its last prompt. Updated live, from hooks installed for you.',
  },
  {
    step: '03',
    title: 'Let one agent run the others',
    body: 'Define roles in per-project TOML — orchestrator, coder, reviewer. The orchestrator delegates, and you watch the work land in the other panes.',
  },
  {
    step: '04',
    title: 'Walk away',
    body: 'The agents belong to a daemon, not to your terminal. Detach the deck, come back later, reattach — they kept working.',
  },
];

export const docLinks = {
  gettingStarted: '/docs/getting-started',
  installation: '/docs/installation',
  orchestration: '/docs/orchestration',
  configuration: '/docs/configuration',
  keyboard: '/docs/keyboard-shortcuts',
  modes: '/docs/workspace-modes',
  remote: '/docs/remote-environments',
};
