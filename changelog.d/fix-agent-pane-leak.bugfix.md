### Fixed

- **Mode tab agent pane leak on close**
  Closing a mode tab (Ctrl+W) now properly closes the agent pane's embedded PTY. Previously, `close_tab()` only closed persistent and reactive panes via `deactivate_mode()`, leaving the agent pane orphaned in the embedded pane controller. These orphaned panes accumulated on the dashboard's right-side terminal pane list each time a mode tab was closed and reopened.
