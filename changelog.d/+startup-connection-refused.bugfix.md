## No More "Connection refused" When the Deck Starts on a Busy Machine

On a heavily loaded machine, starting `dot-agent-deck` with no daemon running could occasionally exit straight away with `build-version handshake probe failed: Connection refused (os error 111)`. The deck now waits until the daemon it just started is ready to accept connections, so the dashboard opens normally, even on a busy machine.
