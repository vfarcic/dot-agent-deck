{
  description = "PRD #176 desktop GUI (Tauri v2) Linux build dependencies: WebKitGTK 4.1 + companions, exposed as a single pkg-config environment (dev .pc files + headers + runtime libs, transitively closed). macOS uses the system WKWebView and needs none of this, so the package degrades to an empty stub on darwin. Referenced from the repo-root devbox.json as `path:gui/nix#tauri-build-deps`; see gui/README.md for why this can't be a plain list of devbox packages (devbox fetches only each package's runtime output, never the `-dev` output that holds the .pc files pkg-config and the Tauri build need).";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        lib = pkgs.lib;

        # Top-level libraries the Tauri v2 Linux build resolves through
        # pkg-config. webkitgtk_4_1 provides both webkit2gtk-4.1.pc and
        # javascriptcoregtk-4.1.pc; the rest are its companions. Transitive
        # pkg-config requirements (glib, cairo, pango, gdk-pixbuf, atk, ...)
        # come in automatically via the dev outputs' propagated inputs — see
        # the capture step below — so they are NOT enumerated here.
        webviewLibs = with pkgs; [
          webkitgtk_4_1
          gtk3
          libsoup_3
          librsvg
          openssl
        ];

        # Build a single store path whose lib/pkgconfig holds EVERY .pc in the
        # transitive closure of the libraries above. We let stdenv's pkg-config
        # setup hook compute the fully-resolved PKG_CONFIG_PATH from the dev
        # outputs (lib.getDev) and then re-export each .pc as a symlink. The
        # symlinks keep the referenced -dev outputs (headers) and -out outputs
        # (shared libs) in this derivation's runtime closure, so a consumer that
        # adds this path as a buildInput gets working --cflags AND --libs.
        tauriBuildDeps = pkgs.runCommandLocal "dad-gui-build-deps"
          {
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = map lib.getDev webviewLibs;
          }
          ''
            mkdir -p "$out/lib/pkgconfig"
            IFS=':' read -ra dirs <<< "$PKG_CONFIG_PATH"
            for d in "''${dirs[@]}"; do
              [ -d "$d" ] || continue
              for pc in "$d"/*.pc; do
                [ -e "$pc" ] || continue
                ln -sf "$pc" "$out/lib/pkgconfig/$(basename "$pc")"
              done
            done
            # Correctness guard: fail the build if the webview .pc files don't
            # resolve fully (cflags + libs) using ONLY the captured directory.
            # This is what makes "transitive closure is complete" a build-time
            # invariant instead of a runtime surprise during `cargo build`.
            export PKG_CONFIG_PATH="$out/lib/pkgconfig"
            pkg-config --exists --print-errors webkit2gtk-4.1
            pkg-config --exists --print-errors javascriptcoregtk-4.1
            pkg-config --exists --print-errors gtk+-3.0
            pkg-config --exists --print-errors libsoup-3.0
            pkg-config --cflags --libs webkit2gtk-4.1 > /dev/null
          '';

        # On macOS Tauri uses the system WKWebView; there is nothing to
        # provision. An empty stub keeps the flake evaluable/buildable on darwin
        # so devbox can resolve the reference there even though the repo-root
        # devbox.json also excludes it on the darwin platforms.
        stub = pkgs.runCommandLocal "dad-gui-build-deps-noop" { } "mkdir -p \"$out\"";

        chosen = if pkgs.stdenv.hostPlatform.isLinux then tauriBuildDeps else stub;
      in
      {
        packages.tauri-build-deps = chosen;
        packages.default = chosen;
      });
}
