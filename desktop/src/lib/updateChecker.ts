/**
 * Release update checker, ported from the old CLI release notifier.
 *
 * Asks GitHub for the latest Agent Deck release and reports whether it is newer
 * than the version the desktop app is running.
 */
import axios from "axios";

const LATEST_RELEASE_URL =
  "https://api.github.com/repos/vfarcic/dot-agent-deck/releases/latest";

export interface ReleaseInfo {
  version: string;
  url: string;
}

export async function latestRelease(): Promise<ReleaseInfo> {
  const { data } = await axios.get(LATEST_RELEASE_URL, {
    headers: { Accept: "application/vnd.github+json" },
  });
  return {
    version: String(data.tag_name).replace(/^v/, ""),
    url: String(data.html_url),
  };
}

export function isNewer(latest: string, current: string): boolean {
  const a = latest.split(".").map(Number);
  const b = current.split(".").map(Number);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const diff = (a[i] ?? 0) - (b[i] ?? 0);
    if (diff !== 0) return diff > 0;
  }
  return false;
}

export async function checkForUpdate(
  current: string,
): Promise<ReleaseInfo | null> {
  const latest = await latestRelease();
  return isNewer(latest.version, current) ? latest : null;
}
