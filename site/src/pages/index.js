import clsx from 'clsx';
import Link from '@docusaurus/Link';
import useDocusaurusContext from '@docusaurus/useDocusaurusContext';
import Layout from '@theme/Layout';
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
    <header className={clsx('hero hero--primary', styles.heroBanner)}>
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

function HomepageQuickStart() {
  return (
    <section className={styles.quickStart}>
      <div className="container">
        <h2 className="text--center">Quick Start</h2>
        <div className={styles.codeBlock}>
          <pre>
            <code>
              {`# 1. Install dot-agent-deck
brew tap vfarcic/tap && brew install dot-agent-deck

# 2. Register agent hooks
dot-agent-deck hooks install                    # Claude Code
dot-agent-deck hooks install --agent opencode   # OpenCode

# 3. Launch the dashboard
dot-agent-deck`}
            </code>
          </pre>
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
        <HomepageFeatures />
        <HomepageQuickStart />
      </main>
    </Layout>
  );
}
