import type { DesktopSettingsDto } from "./bridge";

/**
 * Issue #1350's review — the one `desktop_set_settings` rejection that is not a
 * bare string.
 *
 * A desktop save writes two files: the deck edits to the shared `remotes.toml`,
 * then everything else to `desktop.toml`. They cannot be replaced atomically
 * together, so a save can fail AFTER its deck edits landed. The crate then
 * rejects with `{ message, written }` (`DesktopSettingsSaveError::Partial` in
 * `src-tauri/src/dto.rs`): `message` says which half was saved, and `written`
 * is the settings re-read from disk, so the window can show what is actually
 * there rather than either the edit it asked for or the document it had.
 *
 * A save refused because the deck list changed outside the app — a deck it
 * updates or removes is now a different deck, e.g. a CLI `remote remove` and
 * `remote add` under the same name — wrote nothing and rejects the same way,
 * since the window's list is just as stale.
 */
export class PartialSettingsSaveError extends Error {
  /** The settings as both files hold them after the failure, normalised. */
  readonly written: DesktopSettingsDto;

  constructor(message: string, written: DesktopSettingsDto) {
    super(message);
    this.name = "PartialSettingsSaveError";
    this.written = written;
  }
}

/**
 * The parts of a partial-save rejection, or `undefined` for every other
 * rejection — which the caller rethrows untouched, so nothing that read a
 * string before sees a different value now. `written` is still raw: the
 * bridge normalises it.
 */
export function partialSettingsSave(cause: unknown): { message: string; written: unknown } | undefined {
  if (typeof cause !== "object" || cause === null || cause instanceof Error) return undefined;
  const record = cause as Record<string, unknown>;
  if (typeof record.message !== "string" || typeof record.written !== "object" || record.written === null) return undefined;
  return { message: record.message, written: record.written };
}
