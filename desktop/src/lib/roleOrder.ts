/**
 * The orchestration panel's saved role order, read back from `localStorage`.
 *
 * The stored value is whatever an earlier build — or anyone with access to the
 * webview's storage — last wrote, so it is parsed as untrusted data rather than
 * asserted into shape: a legacy `{"order":"abc"}` used to be copied verbatim
 * under the new key and crashed the orchestration editor on `order.map`, and
 * an enormous array drove needless scans of the profile list (issue #1045,
 * audit finding 1).
 */

/**
 * The most entries a stored order may hold. Far above any real set of agent
 * profiles; a value longer than this is refused outright rather than
 * truncated, because a truncated order is not the order anyone saved.
 */
export const MAX_STORED_ROLE_ORDER = 256;

/**
 * A stored order's normalized entries, or `undefined` for anything that is not
 * a plain object holding an `order` array of at most
 * {@link MAX_STORED_ROLE_ORDER} entries. Non-string entries and repeats are
 * dropped, keeping the first occurrence.
 */
export function parseStoredRoleOrder(raw: string | null): string[] | undefined {
  if (raw === null) return undefined;
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return undefined;
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
  const order = (value as { order?: unknown }).order;
  if (!Array.isArray(order) || order.length > MAX_STORED_ROLE_ORDER) return undefined;
  const seen = new Set<string>();
  for (const entry of order) {
    if (typeof entry === "string") seen.add(entry);
  }
  return [...seen];
}

/** What {@link planStoredRoleOrder} decided: the order to use, and the storage writes that settle it. */
export interface StoredRoleOrderPlan {
  /** The validated order, or `undefined` when nothing stored is usable. */
  order?: string[];
  /** The value to write under the current key — set only when migrating a legacy value that validated. */
  write?: string;
  /** Whether to remove the legacy key. */
  removeLegacy: boolean;
}

/**
 * How to read the saved role order from the current key's value `current`
 * and, when that is absent, the pre-#1045 key's value `legacy`. A legacy value
 * is written under the current key only once it has parsed, and in its
 * normalized form; the legacy key is removed either way, since a value that
 * did not parse will not parse next time.
 *
 * Pure rather than taking a `Storage`, so the caller keeps spelling each
 * access `localStorage.<op>(…)` — the form `xtask/linkage-check`'s
 * `desktop_settings_secrets` rule reads keys from.
 */
export function planStoredRoleOrder(current: string | null, legacy: string | null): StoredRoleOrderPlan {
  if (current !== null) return { order: parseStoredRoleOrder(current), removeLegacy: false };
  if (legacy === null) return { removeLegacy: false };
  const order = parseStoredRoleOrder(legacy);
  return order ? { order, write: JSON.stringify({ order }), removeLegacy: true } : { removeLegacy: true };
}

/**
 * The order to render: the stored entries that name a profile that exists, in
 * their stored order, so the result is never longer than `profileIds`. Falls
 * back to the profiles' own order when nothing stored survives.
 */
export function roleOrderFor(stored: string[] | undefined, profileIds: string[]): string[] {
  const known = new Set(profileIds);
  const order = (stored ?? []).filter((id) => known.has(id));
  return order.length ? order : [...profileIds];
}

/**
 * {@link roleOrderFor}, then every profile id it left out, appended in the
 * profiles' own order, so the result names each existing profile exactly
 * once. The orchestration editor reorders by position in this list, so a
 * profile missing from it — one a reset restored after the order was seeded —
 * renders but cannot move (PR #1342 review).
 */
export function reconcileRoleOrder(stored: string[] | undefined, profileIds: string[]): string[] {
  const order = [...new Set(roleOrderFor(stored, profileIds))];
  const placed = new Set(order);
  return [...order, ...profileIds.filter((id) => !placed.has(id))];
}
