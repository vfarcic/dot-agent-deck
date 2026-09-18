import Layout from '@theme/Layout';
import Link from '@docusaurus/Link';
import CandidateBanner from '@site/src/components/CandidateBanner';
import {
  agents,
  agentsNote,
  desktop,
  features,
  installTabs,
  platforms,
  principles,
  product,
  screenshots,
  why,
} from '@site/src/data/landing-content';
import styles from './a.module.css';

/**
 * Candidate A -- terminal-native minimalism (issue #1021, Task 1).
 *
 * Draws from charm.sh, ghostty.org and zed.dev. Optimises for looking like the
 * tool it sells: monospace throughout, near-black on paper, hairline rules and
 * whitespace doing the work that cards do elsewhere. No gradients, no shadows,
 * no rounded pill buttons, no three-across icon grid.
 */

function Rule({label}) {
  return (
    <div className={styles.rule} role="presentation">
      <span className={styles.ruleLabel}>{label}</span>
    </div>
  );
}

export default function StyleA() {
  return (
    <Layout title={`${product.name} — terminal-native`} description={product.tagline}>
      <CandidateBanner id="a" />
      <div className={styles.page}>
        <div className={styles.column}>
          <header className={styles.hero}>
            <p className={styles.prompt}>
              <span className={styles.promptSigil}>~ $</span> {product.binary}
            </p>
            <h1 className={styles.title}>{product.name}</h1>
            <p className={styles.tagline}>{product.tagline}</p>
            <p className={styles.sub}>{product.subTagline}</p>
            <p className={styles.actions}>
              <Link className={styles.action} to="/docs/getting-started">
                [ get started ]
              </Link>
              <Link className={styles.action} href={product.repo}>
                [ source ]
              </Link>
              <Link className={styles.action} to="/docs/installation">
                [ install ]
              </Link>
            </p>
          </header>

          <figure className={styles.frame}>
            <div className={styles.frameBar} aria-hidden="true">
              <span>{product.binary}</span>
              <span className={styles.frameBarKeys}>— ☐ ✕</span>
            </div>
            <img
              className={styles.frameImage}
              src={screenshots.hero.src}
              alt={screenshots.hero.alt}
            />
            <figcaption className={styles.caption}>
              {screenshots.hero.caption}
            </figcaption>
          </figure>

          <Rule label="why" />
          <section className={styles.prose}>
            <h2 className={styles.h2}>{why.heading}</h2>
            {why.paragraphs.map((p, i) => (
              <p key={i}>{p}</p>
            ))}
          </section>

          <Rule label="agents" />
          <section>
            <h2 className={styles.h2}>Five clients, no configuration</h2>
            <ul className={styles.agentList}>
              {agents.map((a) => (
                <li key={a.name} className={styles.agentRow}>
                  <Link className={styles.agentName} href={a.href}>
                    {a.name}
                  </Link>
                  <span className={styles.dots} aria-hidden="true" />
                  <code className={styles.agentCommand}>{a.command}</code>
                  <span className={styles.agentStrategy}>{a.integration}</span>
                </li>
              ))}
            </ul>
            <p className={styles.note}>{agentsNote}</p>
          </section>

          <Rule label="what it does" />
          <section>
            <dl className={styles.defs}>
              {features.map((f) => (
                <div key={f.title} className={styles.defRow}>
                  <dt className={styles.defTerm}>{f.title}</dt>
                  <dd className={styles.defBody}>{f.description}</dd>
                </div>
              ))}
            </dl>
          </section>

          <Rule label="principles" />
          <section>
            <dl className={styles.defs}>
              {principles.map((p) => (
                <div key={p.title} className={styles.defRow}>
                  <dt className={styles.defTerm}>{p.title}</dt>
                  <dd className={styles.defBody}>{p.description}</dd>
                </div>
              ))}
            </dl>
          </section>

          <Rule label="install" />
          <section>
            {installTabs.map((tab) => (
              <div key={tab.value} className={styles.installBlock}>
                <h3 className={styles.h3}>{tab.label}</h3>
                {tab.code ? (
                  <pre className={styles.pre}>
                    <code>{tab.code}</code>
                  </pre>
                ) : null}
                <p className={styles.note}>{tab.note}</p>
                <p className={styles.linkRow}>
                  {tab.links.map((l) => (
                    <Link
                      key={l.text}
                      className={styles.inlineLink}
                      to={l.to}
                      href={l.href}>
                      {l.text}
                    </Link>
                  ))}
                </p>
              </div>
            ))}
            <table className={styles.table}>
              <caption className={styles.tableCaption}>platform support</caption>
              <tbody>
                {platforms.map((p) => (
                  <tr key={p.platform}>
                    <th scope="row">{p.platform}</th>
                    <td className={styles.tableDim}>{p.detail}</td>
                    <td className={p.supported ? styles.yes : styles.no}>
                      {p.supported ? 'yes' : p.status}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </section>

          <Rule label="desktop" />
          <section>
            <h2 className={styles.h2}>{desktop.heading}</h2>
            <p className={styles.prose}>{desktop.intro}</p>
            <ul className={styles.fileList}>
              {desktop.artifacts.map((a) => (
                <li key={a.file}>
                  <code>{a.file}</code>
                  <span className={styles.agentStrategy}>
                    {a.platform}, {a.arch}
                  </span>
                </li>
              ))}
            </ul>
            <dl className={styles.defs}>
              {desktop.caveats.map((c) => (
                <div key={c.title} className={styles.defRow}>
                  <dt className={styles.defTermWarn}>! {c.title}</dt>
                  <dd className={styles.defBody}>{c.body}</dd>
                </div>
              ))}
            </dl>
            <p className={styles.note}>{desktop.provenanceNote}</p>
            <pre className={styles.pre}>
              <code>{desktop.provenanceCommand}</code>
            </pre>
            <p className={styles.linkRow}>
              <Link className={styles.inlineLink} href={product.releases}>
                releases
              </Link>
            </p>
          </section>

          <Rule label="" />
          <footer className={styles.foot}>
            <p>
              {product.owner} · {product.license} ·{' '}
              <Link className={styles.inlineLink} href={product.repo}>
                github
              </Link>{' '}
              ·{' '}
              <Link className={styles.inlineLink} to="/docs/getting-started">
                docs
              </Link>
            </p>
          </footer>
        </div>
      </div>
    </Layout>
  );
}
