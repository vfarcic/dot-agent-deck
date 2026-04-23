## Docs Pod Readiness Probe Failure on Startup

The docs deployment's readiness and liveness probes now include a 5-second initial delay, preventing transient "connection refused" failures during pod startup. Previously, probes fired immediately before nginx had finished initializing and bound to port 8080, causing unhealthy pod events on every new pod creation.
