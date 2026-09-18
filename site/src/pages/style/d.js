import Head from '@docusaurus/Head';
import Link from '@docusaurus/Link';
import CandidateBanner from '@site/src/components/CandidateBanner';
import {
  agents,
  agentsNote,
  desktop,
  docLinks,
  features,
  installCommand,
  installRoutes,
  platforms,
  product,
  screenshots,
  why,
  workflow,
} from '@site/src/data/landing-content';
import styles from './d.module.css';

/**
 * Candidate D -- its own shell, no Docusaurus theme (issue #1021, Task 1).
 *
 * Draws from warp.dev, railway.com and supabase.com. Optimises for making /
 * a product site in its own right, and for making the handoff into /docs a
 * designed moment rather than an accident.
 *
 * There is deliberately no `@theme/Layout` here: own sticky header, own
 * footer, own type scale and colour tokens rather than Infima's.
 *
 * COLOR MODE. `useColorMode()` is NOT available on this page. Docusaurus
 * mounts `ColorModeProvider` inside `@theme/Layout/Provider`
 * (`@docusaurus/theme-classic/lib/theme/Layout/Provider/index.js`), not in
 * `Root`, so calling the hook outside a Layout throws. What IS available
 * everywhere is the attribute: theme-classic injects its pre-body inline
 * script through the plugin-level `injectHtmlTags`, so every page -- this one
 * included -- gets `data-theme` and `data-theme-choice` set on <html> before
 * paint, from `localStorage.theme` or the system preference.
 *
 * So this page keys its CSS off `[data-theme='dark']` and ships its own
 * toggle, which writes the same attributes and the same `theme` storage key
 * Docusaurus reads. The consequence is the one that matters: flip the theme
 * here, walk into /docs, and the docs come up in the theme you chose.
 *
 * The toggle holds no React state on purpose -- it reads the live attribute at
 * click time and CSS decides which glyph shows -- so there is nothing for the
 * server to render differently from the client.
 */

function toggleTheme() {
  const html = document.documentElement;
  const next = html.getAttribute('data-theme') === 'dark' ? 'light' : 'dark';
  html.setAttribute('data-theme', next);
  html.setAttribute('data-theme-choice', next);
  try {
    window.localStorage.setItem('theme', next);
  } catch (e) {
    // Private mode, blocked storage: the flip still applies to this page.
  }
}

function ThemeToggle() {
  return (
    <button
      type="button"
      className={styles.toggle}
      onClick={toggleTheme}
      aria-label="Toggle between light and dark">
      <span className={styles.iconLight} aria-hidden="true">
        ☀
      </span>
      <span className={styles.iconDark} aria-hidden="true">
        ☾
      </span>
    </button>
  );
}

function Mark() {
  return (
    <span className={styles.mark} aria-hidden="true">
      <span className={styles.markBar} />
      <span className={styles.markBar} />
      <span className={styles.markBar} />
    </span>
  );
}

const handoffDocs = [
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
    title: 'Modes',
    body: 'Pair an agent with the side panes you want beside it.',
  },
  {
    to: docLinks.keyboard,
    title: 'Keyboard shortcuts',
    body: 'Every action, and how to rebind the ones you disagree with.',
  },
  {
    to: docLinks.remote,
    title: 'Remote environments',
    body: 'One daemon per host, and how to attach to the ones you own.',
  },
  {
    to: docLinks.configuration,
    title: 'Configuration',
    body: 'The TOML that defines modes, orchestrations and defaults.',
  },
];

export default function StyleD() {
  return (
    <>
      <Head>
        <title>{`${product.name} — run your coding agents in parallel`}</title>
        <meta name="description" content={product.tagline} />
      </Head>
      <CandidateBanner id="d" />
      <div className={styles.shell}>
        <header className={styles.header}>
          <div className={styles.headerInner}>
            <Link to="/style/d" className={styles.brand}>
              <Mark />
              <span className={styles.brandName}>{product.name}</span>
            </Link>
            <nav className={styles.nav} aria-label="Primary">
              <Link to={docLinks.gettingStarted}>Docs</Link>
              <a href="#desktop">Desktop</a>
              <Link to={docLinks.installation}>Install</Link>
              <Link href={product.repo}>GitHub</Link>
            </nav>
            <div className={styles.headerActions}>
              <ThemeToggle />
              <Link className={styles.headerCta} to={docLinks.gettingStarted}>
                Get started
              </Link>
            </div>
          </div>
        </header>

        <main>
          <section className={styles.hero}>
            <div className={styles.heroGrid}>
              <div className={styles.heroText}>
                <p className={styles.eyebrow}>Terminal-first · MIT · Rust</p>
                <h1 className={styles.heroTitle}>
                  Every agent you are running,
                  <span className={styles.heroTitleDim}> on one screen.</span>
                </h1>
                <p className={styles.heroLede}>{product.shortDefinition}</p>
                <div className={styles.command}>
                  <span aria-hidden="true">$</span>
                  <code>{installCommand}</code>
                </div>
                <div className={styles.heroActions}>
                  <Link className={styles.btnPrimary} to={docLinks.gettingStarted}>
                    Get started
                  </Link>
                  <Link className={styles.btnQuiet} href={product.repo}>
                    Read the source
                  </Link>
                </div>
              </div>
              <figure className={styles.heroShot}>
                <img
                  src={screenshots.hero.src}
                  alt={screenshots.hero.alt}
                />
                <figcaption>{screenshots.hero.caption}</figcaption>
              </figure>
            </div>

            <dl className={styles.stats}>
              <div>
                <dt>5</dt>
                <dd>agent clients tracked out of the box</dd>
              </div>
              <div>
                <dt>1</dt>
                <dd>binary — TUI and daemon in the same file</dd>
              </div>
              <div>
                <dt>0</dt>
                <dd>multiplexers required</dd>
              </div>
              <div>
                <dt>4</dt>
                <dd>ways to install it</dd>
              </div>
            </dl>
          </section>

          <section className={styles.bento} aria-label="What it does">
            <article className={`${styles.cell} ${styles.cellWide}`}>
              <h2>{why.heading}</h2>
              <p className={styles.lead}>{why.paragraphs[0]}</p>
              <p>{why.paragraphs[1]}</p>
              <p>{why.paragraphs[2]}</p>
            </article>

            {features.map((f) => (
              <article key={f.title} className={styles.cell}>
                <h3>{f.title}</h3>
                <p>{f.description}</p>
              </article>
            ))}

            <article className={`${styles.cell} ${styles.cellAgents}`}>
              <h3>The client you already use</h3>
              <ul className={styles.agentGrid}>
                {agents.map((a) => (
                  <li key={a.name}>
                    <Link href={a.href}>{a.name}</Link>
                    <code>{a.command}</code>
                    <span>{a.integration}</span>
                  </li>
                ))}
              </ul>
              <p className={styles.fine}>{agentsNote}</p>
            </article>
          </section>

          <section className={styles.flow}>
            <div className={styles.flowHead}>
              <h2>Four moves, and you are running a team</h2>
              <p>
                No new agent client, no new terminal, no configuration before the
                first pane opens.
              </p>
            </div>
            <ol className={styles.flowList}>
              {workflow.map((step) => (
                <li key={step.step}>
                  <span className={styles.flowStep}>{step.step}</span>
                  <h3>{step.title}</h3>
                  <p>{step.body}</p>
                </li>
              ))}
            </ol>
            <figure className={styles.flowShot}>
              <img
                src={screenshots.orchestration.src}
                alt={screenshots.orchestration.alt}
                loading="lazy"
              />
              <figcaption>{screenshots.orchestration.caption}</figcaption>
            </figure>
          </section>

          <section className={styles.desktop} id="desktop">
            <div className={styles.desktopInner}>
              <div className={styles.desktopHead}>
                <span className={styles.badge}>Alpha</span>
                <h2>{desktop.heading}</h2>
                <p>{desktop.intro}</p>
              </div>
              <div className={styles.desktopBody}>
                <ul className={styles.desktopCaveats}>
                  {desktop.caveats.map((c) => (
                    <li key={c.title}>
                      <h3>{c.title}</h3>
                      <p>{c.body}</p>
                    </li>
                  ))}
                </ul>
                <div className={styles.desktopFiles}>
                  <h3>Published on every release</h3>
                  <ul>
                    {desktop.artifacts.map((a) => (
                      <li key={a.file}>
                        <span>
                          {a.platform} · {a.arch}
                        </span>
                        <code>{a.file}</code>
                      </li>
                    ))}
                  </ul>
                  <p className={styles.fine}>{desktop.provenanceNote}</p>
                  <div className={styles.command}>
                    <span aria-hidden="true">$</span>
                    <code>{desktop.provenanceCommand}</code>
                  </div>
                  <p>
                    <Link className={styles.btnQuiet} href={product.releases}>
                      Latest release
                    </Link>
                  </p>
                </div>
              </div>
            </div>
          </section>

          <section className={styles.install}>
            <div className={styles.installGrid}>
              <div>
                <h2>Install it four ways</h2>
                <ul className={styles.routeList}>
                  {installRoutes.map((r) => (
                    <li key={r.name}>
                      <div>
                        <strong>{r.name}</strong>
                        <span>{r.detail}</span>
                      </div>
                      {r.code ? <code>{r.code}</code> : null}
                    </li>
                  ))}
                </ul>
              </div>
              <div>
                <h2>Where it runs</h2>
                <ul className={styles.platformList}>
                  {platforms.map((p) => (
                    <li
                      key={p.platform}
                      className={p.supported ? styles.ok : styles.pending}>
                      <strong>{p.platform}</strong>
                      <span>{p.detail}</span>
                      {p.href ? (
                        <Link href={p.href}>{p.status}</Link>
                      ) : (
                        <em>{p.status}</em>
                      )}
                    </li>
                  ))}
                </ul>
              </div>
            </div>
          </section>

          <section className={styles.handoff}>
            <div className={styles.handoffInner}>
              <p className={styles.handoffKicker}>The door into the docs</p>
              <div className={styles.handoffHead}>
                <h2>
                  From here on it is documentation, and it looks like
                  documentation.
                </h2>
                <p className={styles.handoffLede}>
                  The pages behind these links have a sidebar, a search box and
                  their own furniture. That is deliberate: this page is for
                  deciding, those pages are for doing. Pick where you are going.
                </p>
              </div>
              <div className={styles.handoffGrid}>
                {handoffDocs.map((d) => (
                  <Link key={d.to} to={d.to} className={styles.handoffCard}>
                    <span className={styles.handoffTitle}>{d.title}</span>
                    <span className={styles.handoffBody}>{d.body}</span>
                    <span className={styles.handoffArrow} aria-hidden="true">
                      →
                    </span>
                  </Link>
                ))}
              </div>
            </div>
          </section>
        </main>

        <footer className={styles.footer}>
          <div className={styles.footerInner}>
            <div className={styles.footerBrand}>
              <Mark />
              <div>
                <p className={styles.brandName}>{product.name}</p>
                <p className={styles.fine}>
                  {product.tagline}. Built by {product.owner}.
                </p>
              </div>
            </div>
            <nav className={styles.footerNav} aria-label="Footer">
              <div>
                <p className={styles.footerHeading}>Docs</p>
                <Link to={docLinks.gettingStarted}>Getting started</Link>
                <Link to={docLinks.installation}>Installation</Link>
                <Link to={docLinks.orchestration}>Orchestration</Link>
                <Link to={docLinks.keyboard}>Keyboard shortcuts</Link>
              </div>
              <div>
                <p className={styles.footerHeading}>Project</p>
                <Link href={product.repo}>GitHub</Link>
                <Link href={product.issues}>Issues</Link>
                <Link href={product.releases}>Releases</Link>
              </div>
            </nav>
          </div>
          <p className={styles.footerLegal}>
            © {new Date().getFullYear()} {product.owner}. {product.license}{' '}
            License.
          </p>
        </footer>
      </div>
    </>
  );
}
