## Fix stuck "Needs Input" status

Removed the permission approval queue and blocking `PermissionRequest` hook that caused sessions to display "Needs Input" indefinitely. The deck previously registered both a `Notification` and a `PermissionRequest` hook for the same permission event — the blocking hook delayed every permission prompt and left stale entries in the queue when users approved in the terminal instead of the deck. The deck's permission UI (y/n approval) was already disabled, making the blocking hook purely harmful.

The "Needs Input" status indicator still works correctly via the fire-and-forget `Notification` hook and clears automatically when the agent resumes work.
