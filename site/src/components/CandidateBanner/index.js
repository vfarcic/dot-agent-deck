import Link from '@docusaurus/Link';
import {candidates} from '@site/src/data/landing-content';
import styles from './styles.module.css';

/**
 * Scaffolding strip shown at the top of every /style/* candidate (issue #1021,
 * Task 1). Deliberately ugly and deliberately identical across all four, so it
 * reads as a label on the experiment rather than as part of any candidate's
 * design. Deleted along with the losing routes once a direction is picked.
 */
export default function CandidateBanner({id}) {
  const candidate = candidates.find((c) => c.id === id);
  if (!candidate) {
    return null;
  }
  return (
    <aside className={styles.banner} aria-label="Style candidate scaffolding">
      <div className={styles.inner}>
        <span className={styles.tag}>Candidate {candidate.id.toUpperCase()}</span>
        <span className={styles.name}>{candidate.name}</span>
        <span className={styles.meta}>
          draws from {candidate.draws.join(', ')}
        </span>
        <span className={styles.meta}>optimises for {candidate.optimises}</span>
        <nav className={styles.nav} aria-label="Other candidates">
          {candidates.map((c) => (
            <Link
              key={c.id}
              to={c.route}
              className={c.id === id ? styles.navCurrent : styles.navLink}
              aria-current={c.id === id ? 'page' : undefined}>
              {c.id.toUpperCase()}
            </Link>
          ))}
          <Link to="/style/" className={styles.navLink}>
            index
          </Link>
        </nav>
        <span className={styles.warning}>
          temporary — delete with the losing routes (#1021)
        </span>
      </div>
    </aside>
  );
}
