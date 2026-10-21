## The TUI no longer reattaches to a replacement daemon on another attach protocol

If the daemon was replaced while the TUI was open, the TUI reconnected to the new one without checking that the two speak the same attach protocol, so it could show a normal dashboard while it could not read what that daemon sent. It now checks on every reconnect. When the protocols differ it does not reattach: a footer row says `⚠ Not attaching: daemon speaks attach protocol vN, but this binary speaks vM`, with the same two ways out as the startup refusal, until a daemon on its own protocol answers again. In the desktop app, an agent's terminal reopened after its stream ended checks the daemon again too, and refuses one on another protocol naming both versions.

See [Troubleshooting](https://agent-deck.devopstoolkit.ai/docs/troubleshooting.md#daemon-speaks-attach-protocol-vn-but-this-binary-speaks-vm).
