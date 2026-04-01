## Version Update Notification

The upgrade notification in the dashboard status bar now reliably detects newer releases. Previously, a 24-hour version check cache could retain stale data, causing the app to incorrectly conclude no update was available. The cache has been removed — each launch now fetches the latest release directly from GitHub (in the background, with a 10-second timeout).
