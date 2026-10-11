import { CircleArrowUp, RotateCcw, X } from "lucide-react";

import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import type { SelfUpgradeCheck } from "../lib/selfUpgrade";

/**
 * Issue #1635 — the app's equivalent of the TUI's `Update available: vX
 * (current: vY)` badge, in the crate's own words (`UpgradePlan::headline`).
 *
 * Two places, both opening the same dialog. The rail button is the app's
 * chrome, on every screen, for as long as a copy is behind. The banner is at
 * the top of the dashboard, where the text can be read without hovering, and
 * can be dismissed; the rail button stays.
 *
 * Neither renders unless a newer release exists for one of the copies, or the
 * app itself was upgraded and still runs the old build: then the notice is the
 * relaunch prompt, and the dialog it opens offers Relaunch, until the app
 * restarts. The TUI's badge turns into its restart line the same way.
 */
export function noticeOf(check: SelfUpgradeCheck | undefined): string | undefined {
  return (check?.updateAvailable || check?.installed) && check.notice ? check.notice : undefined;
}

/** Whether the notice only asks for a relaunch: the app was upgraded and nothing else is behind. */
function relaunchOnly(check: SelfUpgradeCheck | undefined): boolean {
  return Boolean(check?.installed && !check.updateAvailable);
}

export function SelfUpgradeRailButton({ check, onOpen }: { check?: SelfUpgradeCheck; onOpen: () => void }) {
  const notice = noticeOf(check);
  if (!notice) return null;
  const text = displayText(notice, DISPLAY_LIMITS.title);
  const Icon = relaunchOnly(check) ? RotateCcw : CircleArrowUp;
  return (
    <button className="rail-update" data-testid="self-upgrade-rail" aria-label={text} title={text} onClick={onOpen}>
      <Icon size={18} />
    </button>
  );
}

export function SelfUpgradeBanner({ check, onOpen, onDismiss }: { check?: SelfUpgradeCheck; onOpen: () => void; onDismiss: () => void }) {
  const notice = noticeOf(check);
  if (!notice) return null;
  const relaunch = relaunchOnly(check);
  const Icon = relaunch ? RotateCcw : CircleArrowUp;
  return (
    <div className="overview-banner self-upgrade-banner" role="status" data-testid="self-upgrade-banner">
      <Icon size={13} />
      <span>{displayText(notice, DISPLAY_LIMITS.message)}</span>
      <button type="button" className="self-upgrade-banner-open" data-testid="self-upgrade-banner-open" onClick={onOpen}>{relaunch ? "Relaunch…" : "Upgrade…"}</button>
      <button type="button" aria-label="Dismiss" onClick={onDismiss}><X size={13} /></button>
    </div>
  );
}
