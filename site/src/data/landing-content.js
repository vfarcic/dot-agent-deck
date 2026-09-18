/**
 * Shared landing-page content for the /style/* candidates (issue #1021, Task 1).
 *
 * Every candidate route imports from here, so the four directions are compared
 * on layout and personality rather than on who wrote nicer placeholder copy.
 *
 * The prose is lifted from `site/src/pages/index.js`, with four corrections the
 * live page has not had yet. Each one was checked against the repository rather
 * than against the issue text, because the issue text was wrong about two of
 * them:
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
 *    ship five agents, not the two the page names and not the four the issue
 *    names: Claude Code, OpenCode, Pi, Codex and Devin. `docs/getting-started.md`
 *    already lists all five. Gemini (#211) and Aider (#212) are open PRDs and
 *    are not promised here.
 * 3. Windows. The page links #42, closed on 2026-07-15. The open work is #164,
 *    which is what `docs/installation.md` already points at.
 * 4. Linux install. The page hedges with "Homebrew (if available)". The
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
  subTagline:
    'One dashboard, every agent visible at a glance, keyboard-driven, in the terminal you already use.',
  shortDefinition:
    'A single Rust binary that runs your AI coding agents as embedded terminal panes, tracks what each one is doing in real time, and lets one agent delegate work to the others.',
  license: 'MIT',
  repo: 'https://github.com/vfarcic/dot-agent-deck',
  issues: 'https://github.com/vfarcic/dot-agent-deck/issues',
  releases: 'https://github.com/vfarcic/dot-agent-deck/releases/latest',
};

/** The "Why Agent Deck" block, verbatim from the current homepage. */
export const why = {
  heading: 'Why Agent Deck',
  paragraphs: [
    "Running one AI agent at a time, you're still a software engineer who happens to use AI. Running five at once, you stop being one. You become a project manager supervising a team, a tech lead unblocking them, an architect designing the approach, a product manager deciding what to build.",
    "The agents write the code. Your job is everything around it — defining the work up front, supervising it in flight, and validating that the right thing got built. None of this is new. It's the same craft people have practiced for decades. The team just looks different.",
    'Agent Deck is the tool that lets you do that without losing your mind. One dashboard, every agent visible at a glance, keyboard-driven, in the terminal you already use, with the agent client you already know.',
  ],
};

export const features = [
  {
    title: 'Real-time monitoring',
    description:
      'See status, active tool, working directory, and last prompt for every agent session — updated in real time.',
  },
  {
    title: 'Keyboard-driven',
    description:
      'Vim-style navigation with single-key actions. Create, focus, close, and rename panes without leaving the dashboard.',
  },
  {
    title: 'Five agents, no configuration',
    description:
      'Claude Code, OpenCode, Pi, Codex and Devin are tracked out of the box. On launch the deck sets up whatever each one needs for live status — hooks, a plugin, an extension or a wrapper.',
  },
  {
    title: 'Single binary',
    description:
      'No external terminal multiplexer needed. dot-agent-deck is one binary with native embedded terminal panes — and it is the daemon too.',
  },
];

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

export const agentsPlanned = [
  {
    name: 'Gemini CLI',
    href: 'https://github.com/vfarcic/dot-agent-deck/issues/211',
    label: 'designed (#211)',
  },
  {
    name: 'Aider',
    href: 'https://github.com/vfarcic/dot-agent-deck/issues/212',
    label: 'designed (#212)',
  },
];

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

export const installTabs = [
  {
    value: 'macos',
    label: 'macOS',
    code: `# 1. Install via Homebrew
brew tap vfarcic/tap && brew install dot-agent-deck

# 2. Launch the dashboard
dot-agent-deck`,
    note: 'Apple Silicon and Intel. A Nix flake, a prebuilt binary and a source build all work too.',
    links: [{text: 'All install options', to: '/docs/installation'}],
  },
  {
    value: 'linux',
    label: 'Linux',
    code: `# 1. Install via Homebrew
brew tap vfarcic/tap && brew install dot-agent-deck

# 2. Launch the dashboard
dot-agent-deck`,
    note: 'Homebrew is a supported path on Linux, amd64 and arm64 — not a maybe. If you would rather not use it, there is a Nix flake (with an overlay and a home-manager module), prebuilt binaries on the releases page, and a source build.',
    links: [{text: 'All install options', to: '/docs/installation'}],
  },
  {
    value: 'windows',
    label: 'Windows',
    code: null,
    note: 'Native Windows is not there yet: the daemon still reports Unsupported and no .exe ships in the release artifacts. WSL is a supported path today — install it and follow the Linux instructions inside your WSL shell, where Agent Deck runs as Linux.',
    links: [
      {
        text: 'Install WSL',
        href: 'https://learn.microsoft.com/en-us/windows/wsl/install',
      },
      {
        text: 'Follow native Windows support (#164)',
        href: 'https://github.com/vfarcic/dot-agent-deck/issues/164',
      },
    ],
  },
];

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

/** Compact fact table for the dense candidate. */
export const facts = [
  {label: 'What it is', value: 'A terminal dashboard for parallel AI coding agents'},
  {label: 'Written in', value: 'Rust — one binary, which is also the daemon'},
  {label: 'Multiplexer', value: 'None. Terminal panes are embedded, not tmux'},
  {
    label: 'Agents tracked',
    value: 'Claude Code, OpenCode, Pi, Codex, Devin',
  },
  {label: 'Platforms', value: 'macOS, Linux, Windows via WSL'},
  {label: 'Install', value: 'Homebrew, Nix, prebuilt binary, source'},
  {
    label: 'Session lifetime',
    value: 'Agents outlive the TUI — detach, reattach, they are still there',
  },
  {label: 'Remote', value: 'dot-agent-deck connect, one daemon per host'},
  {label: 'Desktop GUI', value: 'Alpha, unsigned — macOS and Linux'},
  {label: 'License', value: 'MIT'},
];

/** What it is / what it is not, for the candidates that want the contrast. */
export const contrasts = [
  {
    is: 'A dashboard over the agent clients you already run',
    isNot: 'A new agent client, or a new model',
  },
  {
    is: 'Embedded terminal panes in one binary',
    isNot: 'A tmux configuration, or a terminal emulator',
  },
  {
    is: 'A daemon your agents outlive the TUI on',
    isNot: 'A hosted service — the daemon is a process you run',
  },
  {
    is: 'Keyboard-first, every action one or two keys',
    isNot: 'A mouse-driven IDE panel',
  },
];

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

/**
 * The four candidate directions (issue #1021, Task 1). Used by the scaffolding
 * banner on each route and by the /style index. Delete with the routes once a
 * direction is picked.
 */
export const candidates = [
  {
    id: 'a',
    route: '/style/a',
    name: 'Terminal-native minimalism',
    draws: ['charm.sh', 'ghostty.org', 'zed.dev'],
    optimises:
      'looking like the tool it sells — a developer should recognise what this is before reading a word',
    chrome: 'Keeps the Docusaurus navbar and footer.',
    summary:
      'Monospace throughout, near-black on paper, hairline rules and whitespace instead of cards. No gradients, no shadows, nothing that reads as a SaaS grid. The page is laid out as a document with a terminal’s manners.',
  },
  {
    id: 'b',
    route: '/style/b',
    name: 'Dense technical credibility',
    draws: ['tailscale.com', 'fly.io', 'temporal.io'],
    optimises:
      'answering a sceptical engineer’s three questions — what is it, does it drive my agent, what does it cost me to try — above the fold',
    chrome:
      'Deliberately keeps the Docusaurus chrome. Its thesis is that / is the docs’ front porch, done properly.',
    summary:
      'Information-dense: a one-sentence definition, the install command, the agent matrix and the platform table before you scroll. Then a facts table, an is/is-not contrast, and screenshots inline with captions.',
  },
  {
    id: 'c',
    route: '/style/c',
    name: 'Bold product marketing',
    draws: ['linear.app', 'raycast.com', 'warp.dev'],
    optimises:
      'a first-time visitor who has never heard of this — the pitch lands before the specification does',
    chrome: 'Keeps the Docusaurus navbar and footer.',
    summary:
      'Spotlight hero, oversized framed screenshot, a scroll story of alternating sections, a real colour system, an explicit who-this-is-for, and CTAs you cannot miss.',
  },
  {
    id: 'd',
    route: '/style/d',
    name: 'Own shell, no Docusaurus theme',
    draws: ['warp.dev', 'railway.com', 'supabase.com'],
    optimises:
      'making / a product site in its own right, and making the handoff into /docs a designed moment rather than an accident',
    chrome:
      'No @theme/Layout at all. Its own sticky header, its own footer, its own theme toggle, its own type and colour tokens.',
    summary:
      'A modular editorial layout — left-aligned hero with a stat strip, a bento grid of capabilities, a dedicated desktop block, and a closing panel that hands you to the docs on purpose.',
  },
];

/**
 * These pages were built from what is known of the reference sites rather than
 * from a live fetch. Stated on the index so the attribution is honest.
 */
export const attributionNote =
  'The reference sites were not fetched while building these — the directions are drawn from what is known of them. The point of naming them is to make the choice discussable, not to copy a layout.';
