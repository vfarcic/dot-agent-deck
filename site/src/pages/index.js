import Layout from '@theme/Layout';
import Link from '@docusaurus/Link';
import {
  agents,
  agentsNote,
  audience,
  desktop,
  docLinks,
  installCommand,
  installRoutes,
  platforms,
  principles,
  product,
  screenshots,
  why,
  workflow,
} from '@site/src/data/landing-content';
import styles from './index.module.css';

/**
 * The landing page (issue #1021).
 *
 * Direction C -- bold product marketing, drawing on linear.app, raycast.com
 * and warp.dev -- promoted from `/style/c` after the four candidates were
 * compared. It optimises for a first-time visitor who has never heard of this:
 * the pitch lands before the specification does.
 *
 * The `/` <-> `/docs` relationship is SHARED CHROME, chosen rather than
 * inherited. The page renders inside `@theme/Layout`, so the navbar and the
 * footer are the docs' own and nothing moves when a visitor clicks through.
 * Everything between them is this page's: full-bleed bands, its own colour and
 * type tokens, its own background. The last band ("The door into the docs",
 * carried over from candidate D) says the handoff out loud and hands the
 * visitor six specific pages instead of a bare "Docs" link.
 *
 * All copy comes from `src/data/landing-content.js` so the corrected facts
 * live in one place. Screenshot paths are the existing ones on purpose: the
 * image refresh keeps the filenames, so it lands here for free.
 */

/** One screenshot per workflow step, in step order. */
const storyShots = [
  screenshots.dashboard,
  screenshots.card,
  screenshots.parallel,
  screenshots.modes,
];

/** The pages the closing panel hands the visitor, in the order to read them. */
const doorPages = [
  {
    to: docLinks.gettingStarted,
    title: 'Getting started',
    body: 'Install it, open your first pane, and read the card it gives you.',
  },
  {
    to: docLinks.orchestration,
    title: 'Orchestration',
    body: 'Roles, delegation, and letting one agent run the others.',
  },
  {
    to: docLinks.modes,
    title: 'Workspace modes',
    body: 'Pair an agent with the side panes you want beside it.',
  },
  {
    to: docLinks.keyboard,
    title: 'Keyboard shortcuts',
    body: 'The full key map, and the TOML that rebinds most of it.',
  },
  {
    to: docLinks.remote,
    title: 'Remote environments',
    body: 'Run the deck on another machine over ssh, and leave the agents there.',
  },
  {
    to: docLinks.configuration,
    title: 'Configuration',
    body: 'Environment variables, defaults, and the .dot-agent-deck.toml a project carries.',
  },
];

function InstallPill({command}) {
  return (
    <div className={styles.installPill}>
      <span className={styles.installSigil} aria-hidden="true">
        $
      </span>
      <code>{command}</code>
    </div>
  );
}

export default function Home() {
  return (
    <Layout
      title="Run your coding agents in parallel"
      description={product.tagline}>
      <div className={styles.page}>
        <header className={styles.hero}>
          <div className={styles.spotlight} aria-hidden="true" />
          <div className={styles.heroInner}>
            <p className={styles.eyebrow}>
              <span className={styles.dot} aria-hidden="true" />
              Open source · MIT · written in Rust
            </p>
            <h1 className={styles.heroTitle}>
              Stop watching one agent.
              <br />
              <span className={styles.heroTitleAccent}>Run the whole team.</span>
            </h1>
            <p className={styles.heroLede}>{product.shortDefinition}</p>
            <div className={styles.ctaRow}>
              <Link className={styles.ctaPrimary} to={docLinks.gettingStarted}>
                Get started
              </Link>
              <Link className={styles.ctaGhost} href={product.repo}>
                Star on GitHub
              </Link>
            </div>
            <InstallPill command={installCommand} />
          </div>
        </header>

        <main>
          <section className={styles.showcase}>
            <figure className={styles.device}>
              <div className={styles.deviceBar} aria-hidden="true">
                <span className={styles.light} />
                <span className={styles.light} />
                <span className={styles.light} />
                <span className={styles.deviceTitle}>{product.binary}</span>
              </div>
              <img
                className={styles.deviceImage}
                src={screenshots.hero.src}
                alt={screenshots.hero.alt}
              />
            </figure>
            <p className={styles.showcaseCaption}>{screenshots.hero.caption}</p>
          </section>

          <section className={styles.agentStrip}>
            <p className={styles.agentStripLabel}>
              Drives the client you already use
            </p>
            <ul className={styles.agentStripList}>
              {agents.map((a) => (
                <li key={a.name}>
                  <Link href={a.href}>{a.name}</Link>
                  <span className={styles.agentHow}>{a.integration}</span>
                </li>
              ))}
            </ul>
            <p className={styles.agentStripNote}>{agentsNote}</p>
          </section>

          <section className={styles.why}>
            <h2 className={styles.sectionTitle}>{why.heading}</h2>
            <p className={styles.pullQuote}>{why.paragraphs[0]}</p>
            <div className={styles.whyRest}>
              {why.paragraphs.slice(1).map((p, i) => (
                <p key={i}>{p}</p>
              ))}
            </div>
          </section>

          <section className={styles.story} aria-labelledby="story-title">
            {/*
              * The four steps are <h3>s, so the section needs an <h2> above
              * them or the document outline jumps a level. The design has no
              * room for a visible one between the pull quote and the first
              * row, so it is visually hidden rather than dropped.
              */}
            <h2 id="story-title" className={styles.visuallyHidden}>
              How it works
            </h2>
            {workflow.map((step, i) => (
              <div
                key={step.step}
                className={i % 2 === 0 ? styles.storyRow : styles.storyRowFlip}>
                <div className={styles.storyText}>
                  <span className={styles.storyStep}>{step.step}</span>
                  <h3>{step.title}</h3>
                  <p>{step.body}</p>
                </div>
                <figure className={styles.storyFigure}>
                  <img src={storyShots[i].src} alt={storyShots[i].alt} loading="lazy" />
                  <figcaption>{storyShots[i].caption}</figcaption>
                </figure>
              </div>
            ))}
          </section>

          <section className={styles.audience}>
            <div className={styles.audienceInner}>
              <h2 className={styles.sectionTitle}>{audience.heading}</h2>
              <ul className={styles.audienceList}>
                {audience.forYou.map((line) => (
                  <li key={line}>
                    <span className={styles.check} aria-hidden="true">
                      ✓
                    </span>
                    {line}
                  </li>
                ))}
              </ul>
              <p className={styles.audienceNot}>{audience.notYou}</p>
            </div>
          </section>

          <section className={styles.principles}>
            <h2 className={styles.sectionTitle}>Four decisions that shaped it</h2>
            <div className={styles.principleGrid}>
              {principles.map((p, i) => (
                <article key={p.title} className={styles.principleCard}>
                  <span className={styles.principleNum}>{`0${i + 1}`}</span>
                  <h3>{p.title}</h3>
                  <p>{p.description}</p>
                </article>
              ))}
            </div>
          </section>

          <section className={styles.desktop}>
            <div className={styles.desktopInner}>
              <span className={styles.alphaBadge}>Alpha</span>
              <h2 className={styles.sectionTitle}>{desktop.heading}</h2>
              <p className={styles.desktopLede}>{desktop.intro}</p>
              <div className={styles.desktopGrid}>
                {desktop.caveats.map((c) => (
                  <div key={c.title} className={styles.desktopCaveat}>
                    <h3>{c.title}</h3>
                    <p>{c.body}</p>
                  </div>
                ))}
              </div>
              <p className={styles.desktopFiles}>
                {desktop.artifacts.map((a) => (
                  <code key={a.file}>{a.file}</code>
                ))}
              </p>
              <p className={styles.desktopProvenance}>{desktop.provenanceNote}</p>
              <InstallPill command={desktop.provenanceCommand} />
              <p className={styles.desktopProvenanceScope}>
                {desktop.provenanceScope}
              </p>
              <p className={styles.desktopLink}>
                <Link href={product.releases}>Get it from the latest release →</Link>
              </p>
            </div>
          </section>

          <section className={styles.install}>
            <div className={styles.installInner}>
              <h2 className={styles.sectionTitle}>Runs where you work</h2>
              <div className={styles.installCols}>
                <div className={styles.installCol}>
                  <h3 className={styles.installHeading}>Platforms</h3>
                  <ul className={styles.platformList}>
                    {platforms.map((p) => (
                      <li key={p.platform}>
                        <span
                          className={p.supported ? styles.markOk : styles.markNot}
                          aria-hidden="true">
                          {p.supported ? '✓' : '·'}
                        </span>
                        <span className={styles.platformName}>
                          {p.platform}
                          <span className={styles.platformDetail}>{p.detail}</span>
                        </span>
                        <span className={styles.platformStatus}>
                          {p.href ? (
                            <Link href={p.href}>{p.status}</Link>
                          ) : (
                            p.status
                          )}
                        </span>
                      </li>
                    ))}
                  </ul>
                </div>
                <div className={styles.installCol}>
                  <h3 className={styles.installHeading}>Ways in</h3>
                  <ul className={styles.routeList}>
                    {installRoutes.map((r) => (
                      <li key={r.name}>
                        <span className={styles.routeName}>{r.name}</span>
                        <span className={styles.routeDetail}>{r.detail}</span>
                        {r.code ? (
                          <code className={styles.routeCode}>{r.code}</code>
                        ) : null}
                      </li>
                    ))}
                  </ul>
                  <p className={styles.installMore}>
                    <Link to={docLinks.installation}>
                      Full installation guide →
                    </Link>
                  </p>
                </div>
              </div>
            </div>
          </section>

          <section className={styles.close}>
            <div className={styles.closeInner}>
              <h2 className={styles.closeTitle}>
                One command to install. One dashboard for every agent you run.
              </h2>
              <InstallPill command={installCommand} />
              <div className={styles.ctaRow}>
                <Link className={styles.ctaPrimary} to={docLinks.gettingStarted}>
                  Read the guide
                </Link>
                <Link className={styles.ctaGhost} href={product.repo}>
                  Browse the source
                </Link>
              </div>
            </div>
          </section>

          <section className={styles.door} aria-labelledby="door-title">
            <div className={styles.doorInner}>
              <p className={styles.doorKicker}>The door into the docs</p>
              <div className={styles.doorHead}>
                <h2 id="door-title" className={styles.doorTitle}>
                  The furniture stays. The job changes.
                </h2>
                <p className={styles.doorLede}>
                  These links keep the navbar and the footer you are looking at
                  now — you are not leaving the site, and nothing moves under
                  you. What changes is the page between them: a sidebar, a table
                  of contents, and prose written for someone who has already
                  decided. This page was for deciding. Those are for doing.
                </p>
              </div>
              <div className={styles.doorGrid}>
                {doorPages.map((d) => (
                  <Link key={d.to} to={d.to} className={styles.doorCard}>
                    <span className={styles.doorCardTitle}>{d.title}</span>
                    <span className={styles.doorCardBody}>{d.body}</span>
                    <span className={styles.doorArrow} aria-hidden="true">
                      →
                    </span>
                  </Link>
                ))}
              </div>
            </div>
          </section>
        </main>
      </div>
    </Layout>
  );
}
