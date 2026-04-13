## Troubleshooting Documentation

Added troubleshooting guide for Ghostty terminal users experiencing Shift+Enter not creating newlines when using Claude Code or other AI coding agents inside dot-agent-deck.

Documents the root cause (Ghostty intercepts Shift+Enter when mouse capture is enabled) and provides the configuration solution using CSI u format keybind: `keybind = shift+enter=csi:13;2u`

See the [Troubleshooting Guide](https://agent-deck.devopstoolkit.ai/troubleshooting) for complete instructions.
