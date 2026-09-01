{
  description = "pkg-config metadata for the Linux system libraries Tauri needs";

  # WHY THIS EXISTS (issue #771)
  #
  # `desktop/src-tauri` became a workspace member in daf94f0, and both gates
  # CLAUDE.md mandates carry `--workspace`, so `cargo clippy ... --features e2e`
  # and `cargo test-fast` now build `dot-agent-deck-desktop` — which needs
  # GTK 3, WebKitGTK and glib. `devbox.json` provided none of them, so both
  # gates failed inside a `devbox shell` on Linux.
  #
  # WHY A FLAKE RATHER THAN PLAIN devbox.json ENTRIES
  #
  # Two reasons, both measured rather than assumed:
  #
  #   1. TRANSITIVITY. A devbox package lands in the profile as a bare store
  #      path, so `gtk3` (even with its `dev` output selected) contributes
  #      `gtk+-3.0.pc` and nothing else. That file's `Requires:` names
  #      glib-2.0, cairo, pango, gdk-pixbuf-2.0, atk, harfbuzz, x11, wayland …
  #      and `pkg-config` cannot resolve any of them, because a Nix profile
  #      does not follow propagated build inputs. Listing the closure by hand
  #      in devbox.json means ~30 entries that rot on the next nixpkgs bump.
  #      A derivation gets the closure for free: put the libraries in
  #      `buildInputs` and pkg-config's own setup hook assembles the complete
  #      `PKG_CONFIG_PATH`, which this build then freezes into one output.
  #
  #   2. PLATFORM. `webkitgtk_4_1` and `libayatana-appindicator` exist on Linux
  #      only. Restricting a devbox package to Linux needs devbox's object form
  #      for `packages`, and converting the array to that form breaks
  #      `scripts/check-pin-lockstep.sh`, which reads the toolchain pins as
  #      `"rustc@1.97.1"` string entries. A flake can be Linux-only by itself
  #      (see `emptyDirectory` below) and leaves devbox.json's array intact.
  #
  # WHY NO SHARED LIBRARIES ARE SHIPPED HERE
  #
  # The output carries `.pc` files and nothing else. It does not need to carry
  # `libgdk-3.so.0`, because each `.pc` names absolute store paths, so the
  # linker is invoked with `-L/nix/store/…-gtk+3-3.24.52/lib` and Nix's
  # ld-wrapper turns every in-store `-L` into a matching `-rpath`. The test
  # binary therefore finds its libraries through its own RUNPATH. That is what
  # `apt-get install` could not do: a devbox shell runs under Nix glibc, whose
  # `ld.so.cache` does not exist (`ldconfig -p` returns zero entries), so
  # libraries under /usr/lib are invisible to the loader no matter what apt put
  # on disk. It is also why LD_LIBRARY_PATH stays UNSET — issue #771 measured
  # that pointing it at /usr/lib fixes the Rust gates and then breaks Nix's
  # node (`undefined symbol: uv_tcp_keepalive_ex`), and pointing it at a Nix
  # tree would take precedence over every DT_RUNPATH in the shell for the same
  # class of reason.

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      # NOT every default system. x86_64-darwin is omitted for the reason the
      # root flake.nix documents at length: nixpkgs 26.11 dropped it, and
      # importing nixpkgs for it `throw`s during evaluation rather than merely
      # failing to build ("Nixpkgs 26.11 has dropped support for x86_64-darwin",
      # verified against this pin) with no config opt-out.
      #
      # KNOW WHAT THIS COSTS, because it is more than the root flake's version of
      # the same decision: devbox.json now references this flake, so an INTEL MAC
      # can no longer enter `devbox shell` at all, where before it could (the
      # `gcloud` flake pins a 2024 nixpkgs that still has the platform). Apple
      # Silicon is unaffected — it resolves to `emptyDirectory` below.
      #
      # The remedy, if someone needs it, is a SECOND nixpkgs input pinned to
      # `github:NixOS/nixpkgs/nixpkgs-26.05-darwin` used only for that system's
      # empty output. It is not done pre-emptively because flake inputs are
      # fetched during evaluation regardless of platform, so every Linux
      # contributor would pay a second nixpkgs fetch for a platform this
      # repository has already declared unsupported.
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};

          # The same set ci.yml's `build` job installs with apt, translated to
          # nixpkgs attribute names, plus `glib` and `dbus` — which apt pulls in
          # as transitive dependencies of libwebkit2gtk-4.1-dev and which a Nix
          # profile would not.
          #
          #   libwebkit2gtk-4.1-dev        -> webkitgtk_4_1  (+ libsoup_3)
          #   libgtk-3-dev                 -> gtk3
          #   libayatana-appindicator3-dev -> libayatana-appindicator
          #   librsvg2-dev                 -> librsvg
          #   libxdo-dev                   -> xdotool        (ships libxdo)
          deps = with pkgs; [
            glib
            gtk3
            webkitgtk_4_1
            libsoup_3
            libayatana-appindicator
            librsvg
            xdotool
            dbus
          ];

          # Freeze the PKG_CONFIG_PATH that Nix's own pkg-config setup hook
          # computes for `deps` — transitive closure included — into a single
          # `lib/pkgconfig` directory. The copied `.pc` files still name their
          # own store paths, so Nix registers the libraries as references of
          # this output and `devbox install` fetches them.
          pkgconfigEnv = pkgs.runCommand "dot-agent-deck-tauri-deps"
            {
              nativeBuildInputs = [ pkgs.pkg-config ];
              buildInputs = deps;
            }
            ''
              mkdir -p "$out/lib/pkgconfig"
              IFS=: read -ra dirs <<< "$PKG_CONFIG_PATH"
              for d in "''${dirs[@]}"; do
                [ -d "$d" ] || continue
                for f in "$d"/*.pc; do
                  [ -e "$f" ] || continue
                  # First writer wins, matching pkg-config's own search order.
                  [ -e "$out/lib/pkgconfig/$(basename "$f")" ] || cp "$f" "$out/lib/pkgconfig/"
                done
              done
              count=$(find "$out/lib/pkgconfig" -name '*.pc' | wc -l)
              echo "collected $count .pc files"
              # A silent empty output would reproduce issue #771 with no symptom
              # until a contributor's build failed, so fail here instead.
              [ "$count" -gt 20 ] || { echo "too few .pc files -- setup hook did not run?" >&2; exit 1; }
            '';
        in
        rec {
          # macOS builds Tauri against the system WebKit, so there is nothing to
          # provide there — and webkitgtk_4_1 / libayatana-appindicator do not
          # exist for Darwin at all. An empty output keeps devbox.json's single
          # entry valid on every platform the repository supports.
          tauri-deps =
            if pkgs.stdenv.hostPlatform.isLinux then pkgconfigEnv else pkgs.emptyDirectory;

          default = tauri-deps;
        });
    };
}
