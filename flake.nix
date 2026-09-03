{
  description = "A terminal dashboard for monitoring and controlling multiple AI coding agent sessions";

  # nixpkgs and flake-utils, plus crane for the build itself.
  #
  # No rust-overlay: the crate is edition 2024, which needs rustc 1.85 or newer,
  # and nixos-unstable already ships a stable rustc well past that. An extra
  # input would be one more thing for consumers to keep locked for no gain.
  #
  # crane is the one input that does earn its keep. It splits the build in two,
  # compiling the dependency closure as a derivation of its own, so a change to
  # this project's source reuses it instead of recompiling 222 crates and the
  # ~600 C files aws-lc-sys builds (ci.yml:149 documents that build script).
  # `rustPlatform.buildRustPackage` has one vendor-and-build step, so every
  # source change redid that C build. See `mkDotAgentDeck` below.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, crane }:
    let
      inherit (nixpkgs) lib;

      # NOT `flake-utils.lib.eachDefaultSystem`, whose fourth system is
      # x86_64-darwin. The pinned nixpkgs is 26.11 unstable, which dropped
      # x86_64-darwin outright, so `import nixpkgs { system =
      # "x86_64-darwin"; }` does not merely fail to build, it `throw`s during
      # evaluation: "Nixpkgs 26.11 has dropped support for x86_64-darwin".
      # Promising an output that cannot even evaluate is worse than not
      # promising it, and `nix flake check --all-systems` in ci.yml is what
      # makes that visible from a Linux runner. Intel Macs keep the release
      # binaries and the Homebrew tap; only the source-built flake is affected.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      # The released version, pinned in exactly one place.
      #
      # Cargo.toml says `version = "0.1.0"` and always will: release.yml calls it
      # "the 0.1.0 placeholder" and stamps the real, tag-derived version into the
      # binary instead. build.rs resolves the version as injected env -> `git
      # describe` -> CARGO_PKG_VERSION (build.rs:22-30, build.rs:47-51), and a
      # Nix build has neither an injected value by default nor a `.git` to
      # describe, so both non-injected paths land on `0.1.0`. That is precisely
      # the misreport `VersionSource::Placeholder` was added to name.
      #
      # So this flake uses the injection seam that issue #250 built for packagers
      # (the same one release.yml uses) and pins the version here. The
      # dot-ai-tag-release skill bumps this line as part of cutting a tag, and
      # release.yml refuses to release when it disagrees with the tag.
      version = "0.39.1";

      # `<version>-g<short-sha>`, the same shape build.rs composes out of git
      # metadata (build_version_resolve.rs:180-196).
      #
      # It has to be per-commit rather than per-release. The headline install is
      # `nix run github:vfarcic/dot-agent-deck`, an unpinned moving ref, so two
      # builds of two different commits both answer to "0.35.7". The TUI decides
      # "your daemon is stale, restart it" by comparing this exact string against
      # the running daemon's (src/build_version_handshake.rs), so a constant here
      # would make every such upgrade look like nothing had changed.
      #
      # `shortRev` covers a clean tree, including a `github:` ref (the GitHub
      # source carries its rev); `dirtyShortRev` is `<shortRev>-dirty` and covers
      # a local build over uncommitted work; `unknown` covers a source with no
      # git metadata at all, mirroring the `-unknown` sentinel `resolve_build_id`
      # degrades to. All three stay inside the bounded alphabet
      # `normalize_build_id` accepts (ASCII alphanumerics plus `.`, `-`, `+`, `_`).
      buildId = "${version}-g${self.shortRev or self.dirtyShortRev or "unknown"}";

      # Exactly what `cargo build -p dot-agent-deck` reads, and nothing else.
      # crane already keeps a doc edit from rebuilding the *dependencies*, but
      # without this filter it would still rebuild this crate: an unfiltered
      # `src = ./.` makes every file in the repo part of the source hash. The
      # two are complementary, not alternatives. The filter decides how often a
      # rebuild is triggered at all; crane decides how much a triggered rebuild
      # has to redo.
      #
      # The list deliberately errs on the side of too much, because a missing
      # file is a broken build while a spare one only costs an occasional
      # rebuild:
      #   * `Cargo.toml`, `Cargo.lock` and `.cargo/` are the workspace, its
      #     dependency graph and its aliases;
      #   * `build.rs` plus `build_version_resolve.rs`, which build.rs declares
      #     as an out-of-line `mod` (build.rs:11);
      #   * `src/`, and with it `assets/` and `pi-extension/src/`, which `src/`
      #     pulls in at compile time via `include_str!` (src/config_gen.rs:7,
      #     src/config_gen.rs:11, src/orchestrator_ext.rs:52-55);
      #   * `examples/`, because the three `[[example]]` targets at
      #     Cargo.toml:155-169 name explicit paths and cargo refuses to load a
      #     manifest whose declared target file is absent;
      #   * `xtask/`, whose three crates are workspace members and two of which
      #     are path dev-dependencies (Cargo.toml:96, Cargo.toml:100), so their
      #     manifests and lib targets must exist even though `-p dot-agent-deck`
      #     never compiles them. crane needs them for a second reason: it
      #     resolves the workspace once to build `cargoArtifacts`, over a dummy
      #     source it synthesises from these same manifests.
      #   * `desktop/src-tauri/` (PRD #176), for exactly the same reason as
      #     `xtask/`: it is a workspace member, so cargo cannot resolve the
      #     workspace without its manifest, and crane resolves the workspace
      #     before it builds anything. `-p dot-agent-deck` never compiles it and
      #     `nix flake check` does not build the GUI. Omitting it is what failed
      #     CI's `nix` job on PR #416 with `failed to load manifest for
      #     workspace member .../desktop/src-tauri`, before the crate had ever
      #     been compiled — a workspace member is a resolution-time dependency
      #     whether or not it is a build-time one.
      #
      # `tests/` is left out on purpose: `doCheck = false`, so nothing compiles
      # it.
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./.cargo
          ./build.rs
          ./build_version_resolve.rs
          ./src
          ./assets
          ./pi-extension/src
          ./examples
          ./xtask
          ./desktop/src-tauri
        ];
      };

      # Built once here and reused by packages, checks and the overlay, so the
      # overlay cannot drift from packages.default.
      mkDotAgentDeck = pkgs:
        let
          craneLib = crane.mkLib pkgs;

          # Shared by the dependency-only derivation and the real build, and
          # deliberately minimal. Every attribute in here is part of the cache
          # key for the dependency closure, so anything that moves per release
          # has to stay out. See the DAD_VERSION note on buildPackage.
          commonArgs = {
            inherit src;

            # Keep build-time and run-time dependencies apart rather than
            # letting native inputs leak into the target closure. crane
            # recommends it for anything that might ever cross-compile, and it
            # is cheaper to set now than to untangle later.
            strictDeps = true;

            # The workspace is the root package plus xtask/docs,
            # xtask/linkage-check and xtask/spec (Cargo.toml:1-7). Those are
            # repo-maintenance tools, not shipped artifacts, so restrict both
            # halves of the build to the root package rather than compiling
            # three binaries nobody installs.
            cargoExtraArgs = "-p dot-agent-deck";
          };

          # The reason crane is here. This derivation compiles nothing of ours:
          # crane replaces our sources with generated stubs and builds only the
          # dependency graph, aws-lc-sys and its ~600 C files included. It is
          # keyed on Cargo.lock and Cargo.toml, so it survives every change to
          # our own code and gets rebuilt only when the dependency set moves.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in
        craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;

          # pname and version are set HERE and not in commonArgs, for the same
          # reason DAD_VERSION is below: they name the derivation, so pinning
          # `version` into the shared args would give the dependency closure a
          # new store path on every release bump. Left out of commonArgs,
          # cargoArtifacts takes its name from Cargo.toml's permanent 0.1.0
          # placeholder and stays put.
          pname = "dot-agent-deck";
          inherit version;

          # Neither test tier can run in a Nix build. The fast tier is
          # `cargo nextest` (CLAUDE.md rule 5) and the e2e tier drives live
          # `claude` / `opencode` CLIs over the network (CONTRIBUTING.md), which a
          # hermetic builder has no access to. Correctness stays gated by the
          # upstream CI matrix; this derivation's job is to prove the tree builds
          # reproducibly from source.
          doCheck = false;

          # The crux of the whole derivation. build.rs reads both of these from
          # the build environment (build.rs:22-30) and bakes them in via
          # `cargo:rustc-env`, so this is what makes `dot-agent-deck --version`
          # report 0.35.7 instead of the 0.1.0 placeholder, and what gives the
          # daemon handshake a build id that actually moves between commits.
          #
          # ON buildPackage ONLY, never on commonArgs. Both values move on every
          # release, and DAD_BUILD_ID moves on every commit, so putting them in
          # the shared args would rebuild the dependency closure and its C every
          # time either changed, which is most of what moving to crane bought.
          # Nothing in the dependency graph reads them; only our own build.rs
          # does.
          env = {
            DAD_VERSION = version;
            DAD_BUILD_ID = buildId;
          };

          meta = {
            description = "A terminal dashboard for monitoring and controlling multiple AI coding agent sessions";
            homepage = "https://github.com/vfarcic/dot-agent-deck";
            license = pkgs.lib.licenses.mit;
            mainProgram = "dot-agent-deck";
          };
        });
    in
    flake-utils.lib.eachSystem systems (system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };
        dot-agent-deck = mkDotAgentDeck pkgs;
      in {
        packages = {
          default = dot-agent-deck;
          dot-agent-deck = dot-agent-deck;
        };

        # Makes `nix run github:vfarcic/dot-agent-deck` work with nothing
        # installed, which is the headline ask in issue #231.
        #
        # Written out rather than built with `flake-utils.lib.mkApp`, which emits
        # no `meta` and so makes every `nix flake show` warn that the app lacks
        # one. `lib.getExe` reads the `mainProgram` set above, so the binary name
        # is still stated in exactly one place.
        apps.default = {
          type = "app";
          program = lib.getExe dot-agent-deck;
          meta = {
            description = "Run the dot-agent-deck terminal dashboard";
          };
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            # `cargo test-fast` is an alias for `cargo nextest run`, so the shell
            # is useless for the per-task test gate without it.
            cargo-nextest
          ];
        };

        # `nix flake check` should compile something, not just evaluate. Pointing
        # it at the package means a nixpkgs bump that breaks the build fails here
        # rather than at the first user who types `nix run`.
        checks.default = dot-agent-deck;
      }
    ) // {
      # System-independent, so it lives outside eachSystem: an overlay is
      # a function of (final: prev), and `final` already carries the system.
      overlays.default = final: prev: {
        dot-agent-deck = mkDotAgentDeck final;
      };

      # Also system-independent, and for the same reason: a home-manager module
      # is a function of the evaluating configuration, which hands it the `pkgs`
      # for its own system.
      homeModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.dot-agent-deck;
          tomlFormat = pkgs.formats.toml { };
        in
        {
          options.programs.dot-agent-deck = {
            enable = lib.mkEnableOption "dot-agent-deck, a terminal dashboard for AI coding agent sessions";

            package = lib.mkOption {
              type = lib.types.package;
              default = pkgs.dot-agent-deck or (mkDotAgentDeck pkgs);
              defaultText = lib.literalMD
                "`pkgs.dot-agent-deck` when `overlays.default` is applied, otherwise this flake's package built against your own nixpkgs";
              description = ''
                The dot-agent-deck package to install.

                The default is built from this flake against the *evaluating*
                nixpkgs, which is exactly what `overlays.default` does, so a
                user who applies the overlay gets the same derivation rather
                than a second copy of the tool in their closure. Set this to
                `inputs.dot-agent-deck.packages.''${pkgs.system}.default` to
                build against the nixpkgs this flake pins instead.
              '';
            };

            settings = lib.mkOption {
              type = tomlFormat.type;
              default = { };
              example = lib.literalExpression ''
                {
                  default_command = "claude";
                  bell = { on_idle = true; };
                }
              '';
              description = ''
                Content of `~/.config/dot-agent-deck/config.toml`. Left empty,
                no file is written at all, so enabling the module does not
                clobber a config you already have.

                See <https://agent-deck.devopstoolkit.ai/docs/configuration>.
              '';
            };

            keybindings = lib.mkOption {
              type = tomlFormat.type;
              default = { };
              example = lib.literalExpression ''
                {
                  global = { toggle_layout = "Alt+Shift+l"; new_pane = ""; };
                  dashboard.help = "F1";
                }
              '';
              description = ''
                Content of `~/.config/dot-agent-deck/keybindings.toml`. Only the
                actions you list are remapped; everything else keeps its
                default. Left empty, no file is written.

                See <https://agent-deck.devopstoolkit.ai/docs/keyboard-shortcuts>.
              '';
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];

            # `home.file`, NEVER `xdg.configFile`. This is deliberate and it is
            # load-bearing.
            #
            # The deck resolves its config root with `config_dir()`, which is
            # HOME-anchored and deliberately ignores $XDG_CONFIG_HOME
            # (src/platform/paths.rs:370-382). The test
            # `config_dir_is_home_anchored_and_ignores_xdg_config_home`
            # (src/platform/paths.rs:521-531) asserts exactly that: with
            # XDG_CONFIG_HOME pointed elsewhere, `config_dir()` is still
            # `home_dir()/.config/dot-agent-deck`.
            #
            # `xdg.configFile` follows `xdg.configHome`, so for any user who has
            # moved it these files would be written somewhere the deck never
            # reads. Nothing would error: the deck would simply run on its
            # defaults, and the user would be left wondering why their config
            # had no effect. Do not "modernise" this to `xdg.configFile`.
            #
            # Only written when there is something to write, so `enable = true`
            # on its own installs the package and touches no config.
            home.file =
              lib.optionalAttrs (cfg.settings != { }) {
                ".config/dot-agent-deck/config.toml".source =
                  tomlFormat.generate "dot-agent-deck-config.toml" cfg.settings;
              }
              // lib.optionalAttrs (cfg.keybindings != { }) {
                ".config/dot-agent-deck/keybindings.toml".source =
                  tomlFormat.generate "dot-agent-deck-keybindings.toml" cfg.keybindings;
              };

            # Three neighbours in the same directory are deliberately NOT
            # managed here:
            #
            #   * `session.toml` is runtime state the deck writes itself (the
            #     saved workspace). home-manager symlinks its files read-only
            #     out of the store, so managing it would make the deck unable
            #     to save, and the module would be fighting the application on
            #     every launch.
            #   * `remotes.toml` is written imperatively by `dot-agent-deck
            #     remote add`, same problem.
            #   * `schedules.toml` is the odd one out on paths: it is the one
            #     file that DOES honour $XDG_CONFIG_HOME (`schedules_path`, via
            #     `xdg_config_home()` in src/platform/paths.rs:390-402). Because
            #     its location follows a different rule from its neighbours,
            #     managing it correctly needs `xdg.configHome` handling that the
            #     other two files must not get, so it is left for a follow-up
            #     rather than quietly written to the wrong place.
            #
            # `dot-agent-deck hooks install` is likewise not run from an
            # activation script. It edits OTHER tools' configuration (Claude,
            # OpenCode, Codex and friends), which is outside home-manager's
            # ownership and cannot be rolled back with a generation switch. It
            # stays a one-off imperative step the user runs knowingly.
          };
        };
    };
}
