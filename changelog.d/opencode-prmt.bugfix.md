## OpenCode Prompts Render Again

The bundled OpenCode plugin now emits `session.prompt` events as soon as `message.created` fires, so OpenCode decks once again show the `Prmt:` label after opencode.ai’s recent API change. Reinstall the plugin (`dot-agent-deck hooks install --agent opencode`) to pick up the fix.
