### Fixed

- **Docs site port leak in directory redirects**
  Clicking links like `/docs/installation` (no trailing slash) on the public docs site no longer bounces users to a non-routable `http://agent-deck.devopstoolkit.ai:8080/...` URL. The docs container now ships a custom nginx config that disables `absolute_redirect` and `port_in_redirect`, so directory 301 redirects emit relative `Location` headers and the upstream Gateway's host, scheme, and port are preserved.
