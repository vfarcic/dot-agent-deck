import { CircleArrowUp, X } from "lucide-react";

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
 * Neither renders unless a newer release exists for one of the copies.
 */
export function noticeOf(check: SelfUpgradeCheck | undefined): string | undefined {
  return check?.updateAvailable && check.notice ? check.notice : undefined;
}

export function SelfUpgradeRailButton({ check, onOpen }: { check?: SelfUpgradeCheck; onOpen: () => void }) {
  const notice = noticeOf(check);
  if (!notice) return null;
  const text = displayText(notice, DISPLAY_LIMITS.title);
  return (
    <button className="rail-update" data-testid="self-upgrade-rail" aria-label={text} title={text} onClick={onOpen}>
      <CircleArrowUp size={18} />
    </button>
  );
}

export function SelfUpgradeBanner({ check, onOpen, onDismiss }: { check?: SelfUpgradeCheck; onOpen: () => void; onDismiss: () => void }) {
  const notice = noticeOf(check);
  if (!notice) return null;
  return (
    <div className="overview-banner self-upgrade-banner" role="status" data-testid="self-upgrade-banner">
      <CircleArrowUp size={13} />
      <span>{displayText(notice, DISPLAY_LIMITS.message)}</span>
      <button type="button" className="self-upgrade-banner-open" data-testid="self-upgrade-banner-open" onClick={onOpen}>Upgrade…</button>
      <button type="button" aria-label="Dismiss" onClick={onDismiss}><X size={13} /></button>
    </div>
  );
}
