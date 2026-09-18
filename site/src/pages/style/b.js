import Layout from '@theme/Layout';
import Link from '@docusaurus/Link';
import CandidateBanner from '@site/src/components/CandidateBanner';
import {
  agents,
  agentsNote,
  agentsPlanned,
  contrasts,
  desktop,
  facts,
  installCommand,
  installRoutes,
  platforms,
  principles,
  product,
  screenshots,
  why,
} from '@site/src/data/landing-content';
import styles from './b.module.css';

/**
 * Candidate B -- dense technical credibility (issue #1021, Task 1).
 *
 * Draws from tailscale.com, fly.io and temporal.io. Optimises for a sceptical
 * engineer's three questions -- what is it, does it drive my agent, what does
 * it cost me to try -- all answered above the fold. Deliberately keeps the
 * Docusaurus chrome: this candidate's thesis is that / is the docs' front
 * porch, done properly, rather than a separate property.
 */

function SectionLabel({children}) {
  return <p className={styles.sectionLabel}>{children}</p>;
}

export default function StyleB() {
  return (
    <Layout title={`${product.name} — technical overview`} description={product.tagline}>
      <CandidateBanner id="b" />
      <div className={styles.page}>
        <div className={styles.wrap}>
          <section className={styles.fold}>
            <div className={styles.foldMain}>
              <h1 className={styles.title}>{product.name}</h1>
              <p className={styles.definition}>{product.shortDefinition}</p>
              <div className={styles.installBox}>
                <span className={styles.installLabel}>install</span>
                <code className={styles.installCode}>{installCommand}</code>
              </div>
              <p className={styles.foldLinks}>
                <Link className={styles.primaryLink} to="/docs/getting-started">
                  Getting started
                </Link>
                <Link className={styles.secondaryLink} to="/docs/installation">
                  All install options
                </Link>
                <Link className={styles.secondaryLink} href={product.repo}>
                  Source
                </Link>
              </p>
            </div>

            <div className={styles.foldAside}>
              <table className={styles.matrix}>
                <caption>Agents tracked out of the box</caption>
                <thead>
                  <tr>
                    <th scope="col">Agent</th>
                    <th scope="col">Command</th>
                    <th scope="col">Integration</th>
                  </tr>
                </thead>
                <tbody>
                  {agents.map((a) => (
                    <tr key={a.name}>
                      <th scope="row">
                        <Link href={a.href}>{a.name}</Link>
                      </th>
                      <td>
                        <code>{a.command}</code>
                      </td>
                      <td className={styles.dim}>{a.integration}</td>
                    </tr>
                  ))}
                  {agentsPlanned.map((a) => (
                    <tr key={a.name} className={styles.pending}>
                      <th scope="row">
                        <Link href={a.href}>{a.name}</Link>
                      </th>
                      <td colSpan={2} className={styles.dim}>
                        {a.label}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              <p className={styles.microNote}>{agentsNote}</p>

              <table className={styles.matrix}>
                <caption>Platform support</caption>
                <tbody>
                  {platforms.map((p) => (
                    <tr key={p.platform}>
                      <th scope="row">{p.platform}</th>
                      <td className={styles.dim}>{p.detail}</td>
                      <td
                        className={p.supported ? styles.statusOk : styles.statusNo}>
                        {p.href ? (
                          <Link href={p.href}>{p.status}</Link>
                        ) : (
                          p.status
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>

          <section className={styles.shots}>
            <SectionLabel>What you are looking at</SectionLabel>
            <div className={styles.shotGrid}>
              {[screenshots.dashboard, screenshots.orchestration, screenshots.modes].map(
                (shot) => (
                  <figure key={shot.src} className={styles.shot}>
                    <img src={shot.src} alt={shot.alt} loading="lazy" />
                    <figcaption>{shot.caption}</figcaption>
                  </figure>
                ),
              )}
            </div>
          </section>

          <div className={styles.twoUp}>
            <section>
              <SectionLabel>At a glance</SectionLabel>
              <table className={styles.facts}>
                <tbody>
                  {facts.map((f) => (
                    <tr key={f.label}>
                      <th scope="row">{f.label}</th>
                      <td>{f.value}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </section>

            <section>
              <SectionLabel>What it is, and is not</SectionLabel>
              <table className={styles.facts}>
                <thead>
                  <tr>
                    <th scope="col">It is</th>
                    <th scope="col">It is not</th>
                  </tr>
                </thead>
                <tbody>
                  {contrasts.map((c) => (
                    <tr key={c.is}>
                      <td>{c.is}</td>
                      <td className={styles.dim}>{c.isNot}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </section>
          </div>

          <section className={styles.why}>
            <SectionLabel>{why.heading}</SectionLabel>
            <div className={styles.whyCols}>
              {why.paragraphs.map((p, i) => (
                <p key={i}>{p}</p>
              ))}
            </div>
          </section>

          <section>
            <SectionLabel>Design principles</SectionLabel>
            <div className={styles.principleGrid}>
              {principles.map((p) => (
                <div key={p.title} className={styles.principle}>
                  <h3>{p.title}</h3>
                  <p>{p.description}</p>
                </div>
              ))}
            </div>
          </section>

          <section className={styles.desktop}>
            <SectionLabel>Desktop GUI</SectionLabel>
            <div className={styles.desktopGrid}>
              <div>
                <h2 className={styles.h2}>{desktop.heading}</h2>
                <p>{desktop.intro}</p>
                <table className={styles.matrix}>
                  <caption>Published every release</caption>
                  <tbody>
                    {desktop.artifacts.map((a) => (
                      <tr key={a.file}>
                        <th scope="row">
                          {a.platform} <span className={styles.dim}>{a.arch}</span>
                        </th>
                        <td>
                          <code className={styles.file}>{a.file}</code>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
                <p className={styles.microNote}>
                  {desktop.provenanceNote}{' '}
                  <code className={styles.file}>{desktop.provenanceCommand}</code>
                </p>
                <p className={styles.microNote}>
                  <Link href={product.releases}>Download from the latest release</Link>
                </p>
              </div>
              <ul className={styles.caveats}>
                {desktop.caveats.map((c) => (
                  <li key={c.title}>
                    <strong>{c.title}.</strong> {c.body}
                  </li>
                ))}
              </ul>
            </div>
          </section>

          <section>
            <SectionLabel>Four ways to install it</SectionLabel>
            <table className={styles.facts}>
              <tbody>
                {installRoutes.map((r) => (
                  <tr key={r.name}>
                    <th scope="row">{r.name}</th>
                    <td className={styles.dim}>{r.detail}</td>
                    <td>{r.code ? <code>{r.code}</code> : <span className={styles.dim}>—</span>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className={styles.microNote}>
              Every route, including the Nix overlay and the home-manager module, is in{' '}
              <Link to="/docs/installation">Installation</Link>.
            </p>
          </section>

          <section className={styles.handoff}>
            <p>
              <strong>Next:</strong>{' '}
              <Link to="/docs/getting-started">Getting started</Link> ·{' '}
              <Link to="/docs/orchestration">Orchestration</Link> ·{' '}
              <Link to="/docs/workspace-modes">Modes</Link> ·{' '}
              <Link to="/docs/keyboard-shortcuts">Keyboard shortcuts</Link> ·{' '}
              <Link to="/docs/remote-environments">Remote environments</Link>
            </p>
          </section>
        </div>
      </div>
    </Layout>
  );
}
