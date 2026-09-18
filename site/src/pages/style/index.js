import Layout from '@theme/Layout';
import Link from '@docusaurus/Link';
import {attributionNote, candidates} from '@site/src/data/landing-content';
import styles from './index.module.css';

/**
 * Index of the landing-page style candidates (issue #1021, Task 1).
 *
 * Scaffolding. This route and the four it lists are deleted once a direction
 * is picked; nothing here is linked from the navbar or the sidebar.
 */
export default function StyleIndex() {
  return (
    <Layout
      title="Landing page style candidates"
      description="Four candidate directions for the Agent Deck landing page (issue #1021)">
      <div className={styles.page}>
        <div className={styles.wrap}>
          <header className={styles.head}>
            <p className={styles.kicker}>Scaffolding · issue #1021 · Task 1</p>
            <h1 className={styles.title}>Four directions for the landing page</h1>
            <p className={styles.lede}>
              Same product, same copy, four different answers to what{' '}
              <code>/</code> should be. Open them side by side, in both themes,
              and at phone width. The real decision hiding underneath is whether{' '}
              <code>/</code> keeps the Docusaurus chrome — A, B and C say yes in
              three different tones of voice, D says no.
            </p>
            <p className={styles.note}>{attributionNote}</p>
            <p className={styles.note}>
              Every candidate renders the real copy, including four corrections
              the live homepage has not had yet: the desktop GUI exists (as an{' '}
              <strong>unsigned alpha</strong>, macOS and Linux only — not
              &ldquo;signed and notarized&rdquo;, and no Windows bundle), five
              agent clients are tracked rather than two, Windows points at the
              open issue instead of a closed one, and Homebrew on Linux is
              stated as supported rather than hedged.
            </p>
          </header>

          <ol className={styles.list}>
            {candidates.map((c) => (
              <li key={c.id} className={styles.item}>
                <div className={styles.itemHead}>
                  <span className={styles.letter}>{c.id.toUpperCase()}</span>
                  <div>
                    <h2 className={styles.itemTitle}>
                      <Link to={c.route}>{c.name}</Link>
                    </h2>
                    <p className={styles.route}>
                      <code>{c.route}</code>
                    </p>
                  </div>
                </div>
                <dl className={styles.meta}>
                  <div>
                    <dt>Draws from</dt>
                    <dd>{c.draws.join(', ')}</dd>
                  </div>
                  <div>
                    <dt>Optimises for</dt>
                    <dd>{c.optimises}</dd>
                  </div>
                  <div>
                    <dt>Chrome</dt>
                    <dd>{c.chrome}</dd>
                  </div>
                </dl>
                <p className={styles.summary}>{c.summary}</p>
                <p>
                  <Link className={styles.open} to={c.route}>
                    Open candidate {c.id.toUpperCase()} →
                  </Link>
                </p>
              </li>
            ))}
          </ol>

          <section className={styles.footNote}>
            <h2>What to look at</h2>
            <ul>
              <li>
                <strong>Does it survive the theme toggle?</strong> The site
                defaults to dark and respects the OS preference, so half the
                visitors see the other one.
              </li>
              <li>
                <strong>Does it survive 360px?</strong> Narrow the window until
                it is phone-width; nothing should scroll sideways.
              </li>
              <li>
                <strong>What does the walk into <code>/docs</code> feel like?</strong>{' '}
                Follow a docs link from each candidate. On A, B and C the
                furniture never changes. On D it changes completely, which is
                either the point or the objection.
              </li>
              <li>
                <strong>Would it still work with a real logo?</strong> Issue
                #746 is open and none of these assume a finished mark.
              </li>
            </ul>
          </section>
        </div>
      </div>
    </Layout>
  );
}
