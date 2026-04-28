import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
import Tabs from '@theme/Tabs';
import TabItem from '@theme/TabItem';
import styles from './index.module.css';

const features = [
  {
    title: 'Real-time Monitoring',
    description:
      'See status, active tool, working directory, and last prompt for every agent session — updated in real time.',
  },
  {
    title: 'Keyboard-Driven',
    description:
      'Vim-style navigation with single-key actions. Create, focus, close, and rename panes without leaving the dashboard.',
  },
  {
    title: 'Multi-Agent Support',
    description:
      'Works with Claude Code and OpenCode out of the box. Auto-installed hooks get you running in one command.',
  },
  {
    title: 'Single Binary',
    description:
      'No external terminal multiplexer needed. dot-agent-deck is a single binary with native embedded terminal panes.',
  },
];

function HomepageHero() {
  const { siteConfig } = useDocusaurusContext();
  return (
    <header className={clsx('hero', styles.heroBanner)}>
      <div className="container">
        <p className={styles.brandName}>DevOps Toolkit</p>
        <h1 className="hero__title">{siteConfig.title}</h1>
        <p className="hero__subtitle">{siteConfig.tagline}</p>
        <div className={styles.buttons}>
          <Link
            className="button button--secondary button--lg"
            to="/docs/getting-started"
          >
            Get Started
          </Link>
          <Link
            className="button button--secondary button--outline button--lg"
            href="https://github.com/vfarcic/dot-agent-deck"
          >
            GitHub
          </Link>
        </div>
      </div>
    </header>
  );
}

function Feature({ title, description }) {
  return (
    <div className={clsx('col col--3')}>
      <div className="text--center padding-horiz--md padding-vert--lg">
        <h3>{title}</h3>
        <p>{description}</p>
      </div>
    </div>
  );
}

function HomepageFeatures() {
  return (
    <section className={styles.features}>
      <div className="container">
        <div className="row">
          {features.map((props, idx) => (
            <Feature key={idx} {...props} />
          ))}
        </div>
      </div>
    </section>
  );
}

function HomepageScreenshot() {
  return (
    <section className={styles.screenshot}>
      <div className="container">
        <img
          src="/img/home-hero-dashboard.jpg"
          alt="Agent Deck dashboard with multiple agent sessions running in parallel"
          className={styles.screenshotImage}
        />
      </div>
    </section>
  );
}

function HomepageWhy() {
  return (
    <section className={styles.why}>
      <div className="container">
        <div className={styles.whyContent}>
          <h2 className="text--center">Why Agent Deck</h2>
          <p>
            Running one AI agent at a time, you're still a software engineer who
            happens to use AI. Running five at once, you stop being one. You
            become a project manager supervising a team, a tech lead unblocking
            them, an architect designing the approach, a product manager
            deciding what to build.
          </p>
          <p>
            The agents write the code. Your job is everything around it —
            defining the work up front, supervising it in flight, and
            validating that the right thing got built. None of this is new.
            It's the same craft people have practiced for decades. The team
            just looks different.
          </p>
          <p>
            Agent Deck is the tool that lets you do that without losing your
            mind. One dashboard, every agent visible at a glance,
            keyboard-driven, in the terminal you already use, with the agent
            client you already know.
          </p>
        </div>
      </div>
    </section>
  );
}

const principles = [
  {
    title: 'Runs in your terminal',
    description:
      'Ghostty, iTerm2, Alacritty, Kitty, WezTerm — whatever you already configured. Agent Deck is a guest, not a replacement.',
  },
  {
    title: 'Uses your agent client',
    description:
      'Claude Code or OpenCode — keep the shortcuts, skills, and configs you already dialed in. No new agent client to learn.',
  },
  {
    title: 'Focus-mode side panes',
    description:
      'Pair an agent with live test runs, log tails, or kubectl watches via per-project TOML config. Deep-diving on one agent doesn\u2019t mean opening a dozen extra terminals.',
  },
  {
    title: 'No buttons',
    description:
      'Every action is one or two keystrokes away. Managing a team of agents has to fit in muscle memory \u2014 mouse-clicking breaks flow.',
  },
];

function HomepagePrinciples() {
  return (
    <section className={styles.principles}>
      <div className="container">
        <h2 className="text--center">Design Principles</h2>
        <div className="row">
          {principles.map((p, idx) => (
            <div key={idx} className="col col--3">
              <div className="padding-horiz--md padding-vert--md">
                <h3>{p.title}</h3>
                <p>{p.description}</p>
              </div>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}

function HomepageQuickStart() {
  return (
    <section className={styles.quickStart}>
      <div className="container">
        <h2 className="text--center">Quick Start</h2>
        <div className={styles.tabsWrapper}>
          <Tabs groupId="os" defaultValue="macos">
            <TabItem value="macos" label="macOS">
              <pre>
                <code>
                  {`# 1. Install via Homebrew
brew tap vfarcic/tap && brew install dot-agent-deck

# 2. Launch the dashboard
dot-agent-deck`}
                </code>
              </pre>
            </TabItem>
            <TabItem value="linux" label="Linux">
              <pre>
                <code>
                  {`# 1. Install via Homebrew (if available)
brew tap vfarcic/tap && brew install dot-agent-deck

# 2. Launch the dashboard
dot-agent-deck`}
                </code>
              </pre>
            </TabItem>
            <TabItem value="windows" label="Windows">
              <p className={styles.tabNote}>
                Native Windows is{' '}
                <Link href="https://github.com/vfarcic/dot-agent-deck/issues/42">coming soon</Link>.
                For now, install{' '}
                <Link href="https://learn.microsoft.com/en-us/windows/wsl/install">WSL</Link>{' '}
                and follow the Linux instructions inside your WSL shell.
              </p>
            </TabItem>
          </Tabs>
        </div>
        <div className={styles.installCallout}>
          <strong>Prebuilt binaries and source builds</strong> are also
          available for macOS and Linux.{' '}
          <Link to="/docs/installation">See all install options &rarr;</Link>
        </div>
      </div>
    </section>
  );
}

export default function Home() {
  const { siteConfig } = useDocusaurusContext();
  return (
    <Layout
      title={siteConfig.title}
      description={siteConfig.tagline}
    >
      <HomepageHero />
      <main>
        <HomepageScreenshot />
        <HomepageWhy />
        <HomepageFeatures />
        <HomepagePrinciples />
        <HomepageQuickStart />
      </main>
    </Layout>
  );
}
