# PRD #20 W1 RED test index

- `codex/hooks/001` (`tests/e2e_codex_hooks.rs`, L2 real-agent): requires native Codex prompt/tool/Stop events to reach the rendered dashboard while the wrapped process remains alive.
- `codex_hooks_001_native_payloads_emit_rich_codex_events` (`tests/codex_hook_ingestion.rs`, fast): sends the Claude-compatible Codex hook matrix through `hook --agent codex` and checks Codex identity plus rich event fields.
- `codex/spawn/006` (`tests/agent_detection.rs`, fast): requires explicit Codex identity to drive both wrapping and registry metadata for a non-inferable launcher.
- `wrap_io_001_noninteractive_streams_are_transparent` (`tests/wrap_io.rs`, fast): requires separate stdout/stderr, stdout-only pipe fidelity, and byte-exact binary/EOF stdin.
- `codex_detection_matrix_handles_common_launchers` (`tests/agent_detection.rs`, fast): covers nested command shells, `sudo -u`, `env -u`, and rejection of `--rcfile` as command mode.
