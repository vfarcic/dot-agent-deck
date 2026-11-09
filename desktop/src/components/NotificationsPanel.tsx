/**
 * The Notifications section (issue #2075): how long a question an agent asked
 * may wait unanswered before the app posts one operating-system notification
 * for it.
 *
 * It implements `SettingsPanelProps` and nothing else, like `ZoomPanel`: the
 * value is the `[questions] notify` key of the settings document, and writing
 * the document is all this panel does — `useQuestionNotifications` reads the
 * setting from the same document on its next check.
 *
 * A select, by `docs/develop/desktop-gui.md`'s cardinality rule: more than four
 * exclusive choices. A value the file holds that is not one of the presets (a
 * hand-edited `notify = 7`) is offered as its own option, so opening this panel
 * never silently rewrites it.
 */
import { AlertTriangle } from "lucide-react";
import { questionNotifyAfterMinutes } from "../lib/bridge";
import type { SettingsPanelProps } from "../lib/settingsContract";

/** The minutes offered, `0` being Off. */
export const QUESTION_NOTIFY_PRESETS = [0, 5, 10, 15, 30, 60] as const;

/** How a value reads in the select. */
export function formatNotifyAfter(minutes: number): string {
  if (minutes === 0) return "Off";
  if (minutes === 1) return "After 1 minute";
  return `After ${minutes} minutes`;
}

export function NotificationsPanel({ settings, onSave, saveError }: SettingsPanelProps) {
  const current = questionNotifyAfterMinutes(settings);
  const options: number[] = (QUESTION_NOTIFY_PRESETS as readonly number[]).includes(current)
    ? [...QUESTION_NOTIFY_PRESETS]
    : [...QUESTION_NOTIFY_PRESETS, current].sort((a, b) => a - b);

  return (
    <div className="settings-body">
      <div className="settings-row">
        <label htmlFor="question-notify">Notify when a question waits</label>
        <select
          id="question-notify"
          data-testid="question-notify"
          value={current}
          onChange={(event) => onSave({ ...settings, questions: { notify: Number(event.target.value) } })}
        >
          {options.map((minutes) => (
            <option key={minutes} value={minutes}>{formatNotifyAfter(minutes)}</option>
          ))}
        </select>
      </div>

      {saveError && (
        <p className="settings-error" role="alert">
          <AlertTriangle size={13} />
          <span>{saveError}</span>
        </p>
      )}
    </div>
  );
}
