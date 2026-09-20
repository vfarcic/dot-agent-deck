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
 * The `/` <-> `/docs` relationship is SHARED CHROME AND ONE PALETTE. The page
 * renders inside `@theme/Layout`, so the navbar and the footer are the docs'
 * own and nothing moves when a visitor clicks through; and since the colour
 * tokens moved to `:root` in `src/css/custom.css`, both halves now draw from
 * the same set, so nothing changes colour underfoot either. That is why the
 * closing band no longer explains the relationship: it used to open with "The
 * furniture stays. The job changes." and a paragraph about which parts of the
 * site persist, which is the SITE'S ARCHITECTURE explained to someone who only
 * wants to know where to click. With the two halves sharing a palette there is
 * nothing left to explain, so the band is now a plain heading over the six
 * pages, in the order to read them.
 *
 * All copy comes from `src/data/landing-content.js` so the corrected facts
 * live in one place, and so do the screenshot paths, the alt text and the
 * captions -- which are written against the frames themselves, so a recapture
 * that changes what a frame shows is a one-file correction. That file also
 * carries the feature analysis this page's story arc is built on.
 */

/**
 * The pages the closing panel hands the visitor, in the order to read them.
 *
 * "Workspace modes" was the third of these and is now "Dispatcher mode".
 * Modes are being removed (issue #1199) and the page dropped its modes
 * principle in the same pass, so leaving a door onto the modes reference would
 * have pointed the one visitor who followed the page's argument at the one
 * feature it deliberately stopped making. Dispatching, meanwhile, is the
 * story's fourth step and had no door at all.
 */
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
    to: docLinks.dispatcher,
    title: 'Dispatcher mode',
    body: 'Start isolated work in its own copy of the repo, just by asking for it.',
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

/**
 * Spelled-out counts for the principles heading, which names how many cards
 * are under it. The page has already shipped one stale count ("five at once",
 * corrected in this pass), so the heading reads its number off the array
 * rather than asserting one that a later edit can falsify.
 */
const COUNT_WORDS = ['No', 'One', 'Two', 'Three', 'Four', 'Five', 'Six'];

/**
 * Which layout a story row takes. A step with no frame stands alone;
 * everything else alternates sides down the page.
 *
 * A `wide` flag used to take a third branch here, stacking the text over a
 * full-measure frame for the old row 04 band. That frame is gone and the flag
 * went with it, so no row leaves the alternation any more. The surviving
 * `tall` flag is read below, on the <figure> rather than on the row: it caps
 * how wide a frame runs inside its own column and does not change which
 * layout the row takes. `landing-content.js` carries the arithmetic.
 */
function storyRowClass(step, index) {
  if (!step.shot) {
    return styles.storyRowSolo;
  }
  return index % 2 === 0 ? styles.storyRow : styles.storyRowFlip;
}

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
              Open source · MIT
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
              <div key={step.step} className={storyRowClass(step, i)}>
                <div className={styles.storyText}>
                  <span className={styles.storyStep}>{step.step}</span>
                  <h3>{step.title}</h3>
                  <p>{step.body}</p>
                </div>
                {step.shot ? (
                  <figure
                    className={
                      step.shot.tall
                        ? `${styles.storyFigure} ${styles.storyFigureTall}`
                        : styles.storyFigure
                    }>
                    <img
                      src={step.shot.src}
                      alt={step.shot.alt}
                      loading="lazy"
                    />
                    <figcaption>{step.shot.caption}</figcaption>
                  </figure>
                ) : null}
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
            <h2 className={styles.sectionTitle}>
              {`${
                COUNT_WORDS[principles.length] ?? principles.length
              } decisions that shaped it`}
            </h2>
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
                One command to install. One place for everything you have
                running.
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
              <h2 id="door-title" className={styles.doorTitle}>
                Where to start in the docs
              </h2>
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
