# Brand assets: the logo, the app icon and the favicon

The Agent Deck mark is one symbol — a stack of three terminal windows fanning out from the bottom-left — and every place the project shows itself uses a copy of the same master file. This page says where that master is, which copies are derived from it, and how to regenerate them. It was set up by issue #746, which replaced three unrelated placeholder marks (a teal terminal outline on the docs site, a cream "AD" letterform as the app icon, and an "AD" text badge in the desktop rail).

## What lives where

| file | what it is | how it is produced |
| --- | --- | --- |
| `assets/brand/logo.svg` | **the master symbol**, 512×512, transparent background | `scripts/brand-logo.py` |
| `assets/brand/lockup-light.svg`, `lockup-dark.svg` | symbol plus the "Agent Deck" wordmark, for light and dark backgrounds; used at the top of `README.md` | `scripts/brand-lockups.py` |
| `assets/brand/reference/gpt-original.png` | the GPT-generated image the symbol was traced from | kept for reference only; nothing reads it |
| `site/static/img/logo.svg` | docs navbar logo (`site/docusaurus.config.js`) | copy of the master, `scripts/brand-icons.sh` |
| `site/static/img/favicon.ico` | docs favicon, frames at 16/24/32/48/64/256px | copy of the Tauri `icon.ico`, `scripts/brand-icons.sh` |
| `desktop/src/assets/logo.svg` | the desktop rail badge (`desktop/src/components/NavigationRail.tsx`) | copy of the master, `scripts/brand-icons.sh` |
| `desktop/src-tauri/icons/*` | the desktop app icon set: `icon.png`, `32x32.png`, `64x64.png`, `128x128.png`, `128x128@2x.png`, `icon.icns` (macOS), `icon.ico` (Windows) | `tauri icon`, via `scripts/brand-icons.sh` |

Every copy is a copy: **edit the master, never a derived file**, then regenerate. A derived file edited in place is silently overwritten by the next regeneration.

The icon set is exactly what `bundle.icon` in `desktop/src-tauri/tauri.conf.json` lists. `icon.png` is listed first on purpose: `tauri-codegen` (2.6.3 when this was written, `src/context.rs`'s `find_icon`) takes the first `.png` in that list as the window icon on Linux and macOS (and the first `.ico` on Windows), so putting a 32px file first would give the running app a blurry window icon.

## Regenerating after a change to the symbol

Three steps. A change to the shape needs all three; a change to one derived file's pipeline needs only the step that produces it.

```bash
python3 scripts/brand-logo.py      # rewrites assets/brand/logo.svg (standard library only)
./scripts/brand-icons.sh           # rewrites the navbar logo, the rail badge, the Tauri icon set and the favicon
# ...then rebuild the two README lockups, which embed the symbol -- see "Regenerating the lockups" below
```

The first two steps do **not** touch `assets/brand/lockup-light.svg` and `lockup-dark.svg`: each lockup carries its own copy of the symbol, so skipping the third step leaves the README showing the old mark while everything else shows the new one. It is a separate step only because it needs a font toolchain the other two do not.

`scripts/brand-icons.sh` needs the desktop's node dependencies (`pnpm --dir desktop install`), because it runs the `tauri` CLI from there. It calls `pnpm exec tauri icon` rather than `pnpm tauri icon` because the latter runs the `pretauri` hook, which builds and syncs the daemon binary — irrelevant to icons and slow. `tauri icon` also writes Android and iOS sets and the Windows Store tiles (`Square*Logo.png`, `StoreLogo.png`); the script deletes them, because this app ships none of those packages.

`scripts/brand-logo.py` holds the geometry as parameters — each window's four corners, corner radius, title-bar height and dot positions — which is much easier to adjust than the SVG's path data. Hand-editing `logo.svg` directly also works, as long as the next person knows the script no longer reproduces it; if you do that, delete the script rather than leave it describing a different shape.

## Regenerating the lockups

Needed whenever the symbol or the wordmark changes. The wordmark is Inter Bold converted to outlines, so the lockups render the same everywhere without the font installed. Inter is licensed under the SIL Open Font License 1.1, whose FAQ treats a logo made with a font as artwork rather than as Font Software, so the outlined wordmark carries no OFL obligation and no entry in `THIRD_PARTY_NOTICES.md`; no part of the font file itself is committed. The script needs `fontTools` and `uharfbuzz` and the Inter variable font. On a machine with Nix, run this from the repository root and one build gets all three, from the `nixpkgs` revision pinned in this repository's `flake.lock` and for the system you are on, so two maintainers get the same font and shaping versions. That covers the systems `flake.nix` declares — `x86_64-linux`, `aarch64-linux` and `aarch64-darwin` — and **not an Intel Mac**: the pinned `nixpkgs` has dropped `x86_64-darwin` (the comment above `systems` in `flake.nix` has the detail), so on one the expression fails during evaluation; use the non-Nix route below there.

```bash
env=$(nix --extra-experimental-features 'nix-command flakes' build --impure --no-link --print-out-paths --expr \
  'let p = (builtins.getFlake (toString ./.)).inputs.nixpkgs.legacyPackages.${builtins.currentSystem}; in p.buildEnv { name = "brandtools"; paths = [ (p.python3.withPackages (ps: [ ps.fonttools ps.uharfbuzz ])) p.inter ]; }')
"$env/bin/python3" scripts/brand-lockups.py "$env/share/fonts/truetype/InterVariable.ttf"
```

The first run can take several minutes when that `nixpkgs` revision is not in your store yet.

Without Nix — or on an Intel Mac — `pip install fonttools uharfbuzz` plus a downloaded `InterVariable.ttf` should do the same, though the versions are then whatever pip resolves rather than the pinned ones; only the Nix route above has been run, and only on `x86_64-linux`.

## Checking a change before committing it

The design constraints the mark was chosen against, and what to look at if you change it:

- **Legible at 16px.** The favicon and the smallest icon frames are 16px. Render the master at 16px and zoom it with nearest-neighbour scaling rather than trusting a smooth preview; detail that is two pixels wide is gone.
- **Works on light and dark.** Check it on the docs site's two backgrounds (`#ffffff` and `#0a0e12`) and on the desktop rail (`--shell`, `#222a27` light / `#1b2220` dark). This is why the front window's body is `#18232c` rather than the ink `#0e1418` of the reference: ink on the near-black dark theme made the window's bottom edge vanish.
- **No tile.** The app icon is the symbol on a transparent background, like the favicon and the navbar logo, so there is one mark rather than a mark and a tile-mounted variant.

`resvg` renders an SVG to PNG at any size (`resvg -w 16 assets/brand/logo.svg out.png`), and `nix --extra-experimental-features 'nix-command flakes' shell nixpkgs#resvg nixpkgs#imagemagick` provides it along with ImageMagick for compositing onto test backgrounds.

## Where the change becomes visible

- **Desktop app:** the icon set is compiled in by `tauri-build` (the window icon) and read by the bundler (`.icns`, `.ico`, PNGs) at packaging time, so a rebuild picks it up. The rail badge is a normal Vite asset.
- **Docs site:** `site/static/img/` ships with the site, which goes live on a release or a `/publish-docs` run, not on merge.
- **README:** immediately, on GitHub.
