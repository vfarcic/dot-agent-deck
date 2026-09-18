import Layout from '@theme/Layout';
import Link from '@docusaurus/Link';
import CandidateBanner from '@site/src/components/CandidateBanner';
import {
  agents,
  audience,
  desktop,
  installCommand,
  principles,
  product,
  screenshots,
  why,
  workflow,
} from '@site/src/data/landing-content';
import styles from './c.module.css';

/**
 * Candidate C -- bold product marketing (issue #1021, Task 1).
 *
 * Draws from linear.app, raycast.com and warp.dev. Optimises for a first-time
 * visitor who has never heard of this: the pitch lands before the
 * specification does. Spotlight hero, oversized framed screenshot, a scroll
 * story of alternating sections, a real colour system, an explicit
 * who-this-is-for, and CTAs you cannot miss.
 */

const storyShots = [
  screenshots.dashboard,
  screenshots.card,
  screenshots.parallel,
  screenshots.modes,
];

export default function StyleC() {
  return (
    <Layout title={`${product.name} — run your agents in parallel`} description={product.tagline}>
      <CandidateBanner id="c" />
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
              <Link className={styles.ctaPrimary} to="/docs/getting-started">
                Get started
              </Link>
              <Link className={styles.ctaGhost} href={product.repo}>
                Star on GitHub
              </Link>
            </div>
            <div className={styles.installPill}>
              <span className={styles.installSigil} aria-hidden="true">
                $
              </span>
              <code>{installCommand}</code>
            </div>
          </div>
        </header>

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
          <p className={styles.agentStripLabel}>Drives the client you already use</p>
          <ul className={styles.agentStripList}>
            {agents.map((a) => (
              <li key={a.name}>
                <Link href={a.href}>{a.name}</Link>
              </li>
            ))}
          </ul>
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

        <section className={styles.story}>
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
                <img
                  src={storyShots[i].src}
                  alt={storyShots[i].alt}
                  loading="lazy"
                />
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
            <p className={styles.desktopLink}>
              <Link href={product.releases}>Get it from the latest release →</Link>
            </p>
          </div>
        </section>

        <section className={styles.close}>
          <div className={styles.closeInner}>
            <h2 className={styles.closeTitle}>One command, then five agents.</h2>
            <div className={styles.installPill}>
              <span className={styles.installSigil} aria-hidden="true">
                $
              </span>
              <code>{installCommand}</code>
            </div>
            <div className={styles.ctaRow}>
              <Link className={styles.ctaPrimary} to="/docs/getting-started">
                Read the guide
              </Link>
              <Link className={styles.ctaGhost} to="/docs/installation">
                Other install routes
              </Link>
            </div>
          </div>
        </section>
      </div>
    </Layout>
  );
}
