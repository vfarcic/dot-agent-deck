
---

## BASELINE CAPTURE MODE (PRD #116 — overrides the interactive steps above)

You are being run **non-interactively** to capture a reproducible baseline. Apply these overrides to the instructions above:

- **Filesystem and shell tools are DISABLED.** Do NOT read files, do NOT run `which`, do NOT write any file. The project has already been discovered for you and is laid out under "PROJECT LAYOUT" below — treat it as the complete result of step 1's discovery. Assume every binary in `devbox.json`/the listed toolchain is installed and on PATH, so **skip step 2's `which` validation**.
- **Do NOT ask the user anything and do NOT wait for confirmation.** Skip step 5's negotiation/numbering and step 6's file write entirely.
- **Output format:** first a short (≤1 paragraph) rationale of the project-specific signals you used, then the COMPLETE proposed `.dot-agent-deck.toml` (modes, plus an orchestration if one applies) in a **single fenced ```toml code block**. Output nothing after the code block.

## PROJECT LAYOUT (result of discovery for /home/vfarcic/code/youtube-automation)

