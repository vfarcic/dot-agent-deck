---
sidebar_position: 8
title: Troubleshooting
---

# Troubleshooting

## Shift+Enter Not Working in Ghostty Terminal

When running Claude Code or other AI coding agents inside dot-agent-deck with the **Ghostty terminal emulator**, Shift+Enter may not create newlines in chat inputs as expected.

### Why This Happens

Ghostty intercepts Shift+Enter for its own terminal features when applications enable mouse capture mode. This prevents the keystroke from reaching the embedded application.

### Solution

Add the following line to your Ghostty configuration file:

**Location:** `~/Library/Application Support/com.mitchellh.ghostty/config`

```
keybind = shift+enter=csi:13;2u
```

This uses the CSI u format (modern keyboard protocol) to send Shift+Enter with the SHIFT modifier preserved.

### How to Apply

1. Edit the Ghostty config file:
   ```bash
   nano ~/Library/Application\ Support/com.mitchellh.ghostty/config
   ```

2. Add the keybind line (you can add it anywhere in the file)

3. Restart Ghostty or reload its configuration

4. Test in dot-agent-deck: Shift+Enter should now create newlines in chat applications

### Verification

After applying the fix:
- Regular **Enter** should submit messages
- **Shift+Enter** should create newlines without submitting

### Note

This configuration only affects Ghostty. Other terminal emulators (iTerm2, Alacritty, Warp, etc.) typically work without additional configuration.
