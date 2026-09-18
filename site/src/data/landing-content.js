/**
 * Landing-page content (issue #1021).
 *
 * Every user-visible string on `/` comes from here, so the copy can be
 * reviewed and corrected in one file rather than hunted through JSX. It began
 * as the shared module the four `/style/*` candidates imported; the candidates
 * are gone and direction C is now `/`, so the exports that only served the
 * losing directions went with them.
 *
 * The prose is lifted from the previous homepage, with four corrections it
 * never had. Each one was checked against the repository rather than against
 * the issue text, because the issue text was wrong about two of them:
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
 *    binary is published. What the assets DO carry is build provenance, from
 *    the credential-free `attest` job.
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
    'Every release also publishes a desktop build: a native window onto the same daemon the terminal deck talks to, and — unlike the TUI, which attaches to one — it can hold several decks at once.',
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
      body: 'There is no Developer ID certificate and no notarization, so the first launch hits a security dialog. The release notes carry the exact route past it.',
    },
    {
      title: 'No Windows bundle',
      body: 'The app carries the daemon as a sidecar, and no Windows daemon binary is published — so there is nothing to bundle yet.',
    },
  ],
  provenanceNote:
    'What every published asset does carry is build provenance: proof that this exact file came out of this repository’s release workflow, from a named commit.',
  provenanceCommand:
    'gh attestation verify <file> --repo vfarcic/dot-agent-deck',
};

/**
 * The screenshot catalogue. Paths are the existing ones on purpose -- the
 * image refresh keeps the filenames, so a refreshed image lands here for free.
 */
export const screenshots = {
  hero: {
    src: '/img/orchestration-coder.png',
    alt: 'Agent Deck orchestrating multiple agents in parallel — orchestrator and coder both working',
    caption:
      'A coder pane working on what the orchestrator just delegated to it.',
  },
  dashboard: {
    src: '/img/home-hero-dashboard.jpg',
    alt: 'Five agents running in parallel — cards switch to Compact density to fit them all without scrolling',
    caption:
      'Five agents in parallel. The cards drop to Compact density on their own so they all fit without scrolling.',
  },
  orchestration: {
    src: '/img/orchestration-start.png',
    alt: 'Orchestration tab on launch — five role cards in the sidebar, orchestrator pane active on the right',
    caption:
      'An orchestration on launch: five role panes — orchestrator, coder, reviewer, auditor, release.',
  },
  parallel: {
    src: '/img/orchestration-delegation-parallel.png',
    alt: 'Orchestrator delegating to reviewer and auditor in parallel — both cards light up simultaneously',
    caption:
      'One agent delegating to two others at once. Both cards light up together.',
  },
  modes: {
    src: '/img/modes.png',
    alt: 'A mode tab in action — agent pane on the left, with live Git status, kubectl pods, and kubectl events stacked on the right',
    caption:
      'A mode pairs one agent with the side panes you want next to it — here git status, pods and events.',
  },
  card: {
    src: '/img/session-management-card.jpg',
    alt: 'Single agent card showing directory, last activity, tool count, recent prompt, and recent tool calls',
    caption:
      'One card: directory, last activity, tool count, the last prompt and the recent tool calls.',
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
  sessions: '/docs/session-management',
  remote: '/docs/remote-environments',
};
