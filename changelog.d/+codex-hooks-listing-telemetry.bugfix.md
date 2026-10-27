## Checking Codex's hooks no longer sends telemetry under `codex-app-server`

With telemetry export on and Codex installed, the deck's own check of Codex's hooks, which runs `codex app-server` briefly, passed your `OTEL_*` variables to it, so your backend received traces from a `codex-app-server` service you never ran. That check now runs without them. Codex sessions you or your agents start are unchanged.
