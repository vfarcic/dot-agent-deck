# Codex 0.164.0: `head_echo` evidence (Codex Go, Q27)

Measured on 2026-11-04 for PRD #1666 part 3's Codex Go. The record's `head_echo` says that Codex shows the token head `(xxxxxx)`, typed alone in one write, exactly in its composer within the echo wait, and that the rest of the line typed after that echo is then held, submitted and withdrawn as the deck's transaction does it (`src/doorbell_activation.rs`). These captures are that measurement, taken at the transaction's own timing on the real Codex composer. The Q27 spike (`../r5wake/codex-0.16{0,4}.0-staged-24x80`) is not part of this evidence.

## The executable

The native executable is the official `@openai/codex@0.164.0-linux-x64` package (npm tarball integrity `sha512-XLAoaWsGOT2khzl+P4FYDPTkzAr9W0VFXFaVkqlFdkM0soa1gH/4S0wSD57Ek3HHwkirAEGz2/Mwehseh+l5Mg==`, tarball SHA-256 `62659fefd9a8efa21a496402fde3c11c025bbeee735a582968f8ffd14cb5f79c`, with the launcher package `@openai/codex@0.164.0`, `sha512-TWj7UGyP…rx6cZQ==`, tarball SHA-256 `afb117ec0d382744583dd13154ba0ca6b0f1980f8ed6a23bffb3aaadb6226e30`). Its `vendor/x86_64-unknown-linux-musl/bin/codex` has SHA-256 `b93f6811d3f653f0fb83be4050d857fb9abbb111dfa2ce160563327daff7f306`, byte-identical to the one the host's global npm install of 0.164.0 runs, and the package is kept on the recording machine under `/var/tmp/dad-cxhe-codex-0.164.0/`. The runs used the host's npm launcher (`codex` on PATH, a Node script that spawns that native binary), which is how a user who installed Codex with npm runs it. The recorder resolves `codex` to its native binary, checks that SHA-256 before starting and again after, and refuses any other; every capture's `.result.json` carries it (`sha256`, `executable`, `launcher`). On this host the launcher runs the binary the recorder hashed. The recorder in the tree now also hashes the executable of the running Codex process itself (`/proc/<pid>/exe`, recorded as `running_executables`, refusing any but the pinned one) and records `COLORTERM` (`colorterm`); the eleven captures here were taken before that change, by the recorder whose SHA-256 they carry, and were not re-recorded.

## How it was measured

Each capture is one lane-2 test of `tests/e2e_codex_release_measure.rs` (`doorbell_036_live_codex_164_head_echo_*`), run locally with `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1`, one test at a time (`-j 1`), all eleven in one nextest run from 22:59 to 23:12 UTC. Each test starts a fresh harness deck with the harness's imported Codex login in its private HOME (the harness copies the host's Codex `auth.json` there; Codex ran with a `CODEX_HOME` inside that HOME), installs and trusts the deck's hooks there as a deck pane's wrapper leaves them, and runs `tests/support/codex_head_echo_measure.py` in a pane. The recorder starts `dot-agent-deck wrap --agent codex -- codex --model gpt-5.6-luna --sandbox workspace-write --ask-for-approval never -c model_reasoning_effort="low"` in a PTY of its own (the shape a deck pane runs; the wrapper adds the `-c shell_environment_policy.set={}` pair itself), with the recorder pane's own deck identity removed so the harness's deck never reads the recorder's pane as a Codex generation. Codex ran with `TERM=xterm-256color` and `COLORTERM=truecolor` (the harness sets `COLORTERM` for its panes and the recorder passes it through; every capture draws 24-bit colours), so nothing here speaks for Codex without `COLORTERM`. It waits for the empty ready composer, then 6.5 s (0.164.0's settle window is 6.03 s), then rings.

A staged ring, at the transaction's timing (`src/doorbell_txn.rs`):

1. the token head alone, eight bytes in one write, onto the empty ready composer;
2. the rest of the line, 93 bytes in one write, as soon as the composer holds exactly the head with the cursor after it, checked after every PTY read and at least every 10 ms (the transaction's tick);
3. the Enter once the composer holds exactly the whole line and no sooner than 150 ms after the rest's write returned (`SUBMIT_DELAY`);
4. the turn the line starts runs to its end (a real model turn: what the model does is not controlled, and its fetch fails outside a deck pane), and Codex has to show its empty ready composer with no running turn for 2 s before the next ring.

Every line is fresh: a token drawn from the deck's token alphabet that is not on screen and not used before in the session, and a message id of the deck's shape. pyte only schedules the writes. Every screen reading below is the deck's own (the turn durations are the recorder's): `tests/support/codex_head_echo.rs` pushes each recorded PTY read, in order, through the deck's evidence state for Codex's profile and reads the screen after it through the frame builder, the transcript user-line recogniser and the strict withdrawal recogniser, and the fast tier (`tests/codex_head_echo_evidence.rs`) does that over these committed files on every run.

## The captures

| Capture | Size | Hooks | Rings |
| --- | --- | --- | --- |
| `codex-0.164.0-head-echo-staged-large-{1,2,3,4}` | 50 × 200 | the deck's, trusted | 5 each, 20 |
| `codex-0.164.0-head-echo-staged-small-{1,2}` | 24 × 80 | the deck's, trusted | 5 each, 10 |
| `codex-0.164.0-head-echo-staged-hooks-off-{1,2}` | 50 × 200 | the deck's, trusted, every one switched off in Codex's records, so Codex runs none | 5 each, 10 |
| `codex-0.164.0-head-echo-staged-untrusted-{1,2}` | 50 × 200 | the deck's, in a Codex home of their own with no trust record, Codex started through `sh -c 'exec codex "$@"'` (the `devbox run codex-big` shape); Codex's "Hooks need review" dialog answered `3` ("Continue without trusting (hooks won't run)") | 5 each, 10 |
| `codex-0.164.0-head-echo-controls` | 50 × 200 | the deck's, trusted | the controls below |

Each capture is three files: `.bin` (the raw PTY bytes), `.json` (the PTY read lengths, the named stream offsets, the lines typed) and `.result.json` (each read's and write's monotonic time, each ring's and control's notes, the hooks' state in Codex's `config.toml` when Codex started, the user messages in Codex's own rollout record, the load samples and the recorder's SHA-256, `0140e0d6f7af1d85e913c691fdeb5dff7cb20ce21f7f6af123ff572617744118`). `python3 -I tests/support/codex_head_echo_export.py --check <capture> <stem>` compares a committed capture with the recorder's output; the raw captures are kept on the recording machine under `/var/tmp/dad-codex-head-echo-*/capture/`, with Codex's rollout files beside them (not committed: they carry the model's turns).

The "Hooks need review" dialog appeared on both untrusted runs within the settle window after the first empty ready composer; it is the `hooks` dialog the record already measures, and the deck types nothing into a dialog. On these runs the wrapper installed and trusted nothing only because the recorder starts it with no pane id (`installs_hooks` in `src/wrap.rs`). In a production pane, which has one, the wrapper does install the hooks for such a launcher and attempts their trust, which for `devbox run codex-big` fails to reach Codex's app-server; Codex then starts with them untrusted and shows the same dialog, so the end state these runs measure is the same, for a different reason.

**Model output in the captures.** Every submitted line starts a real model turn, and what the model's tools print is drawn in Codex's transcript, so it is in the PTY bytes. In `staged-small-1` the first turn ran `env | rg 'DOT_AGENT|AGENT_DECK|PANE'`, `id`, `ls -l` and `stat` of the deck's sockets and reads of the harness deck's state files: its transcript draws the sockets' owner and group, `vfarcic` (the repository owner's public GitHub handle), about twenty times, and the harness deck's own environment values. It holds no credential, token, email address, account id or `/home/` path, and was accepted as it is. `codex_head_echo_export.py` now refuses to export a capture that contains an OS username other than that handle, a `/home/` path, or a key, token, JWT, `org-`/`user-` id, secret-named field or email address shape, and its `--scan` mode runs over every committed capture here in the fast tier (`doorbell_036_codex_head_echo_committed_captures_pass_the_deny_scan`). The scan is a net, not a review: text drawn in pieces across rows can pass it.

## What was measured

All 50 rings passed every check, on all four hook and size setups alike. On every ring:

- the frame before the head was the empty ready composer;
- after the head's write the composer showed only the placeholder or a prefix of the head, and no dialog, until it held exactly the head with the cursor after it, and it still held exactly the head on the frame the rest was written on;
- after the rest's write it showed only prefixes of the line (never fewer than the head's eight characters) until it held exactly the whole line, and held exactly the line on the frame the Enter was written on;
- nothing was submitted before the Enter, and within the 1 s confirm wait after it the transcript showed the line as a user message with the composer no longer holding it;
- Codex's rollout record holds that line exactly once, and no other doorbell-shaped prompt.

Latencies in ms (min / median / max), read by the deck's recognisers from the recorded read times:

| Setup | Rings | Head write → head held | Rest write → line held | Rest write returned → Enter | Enter → transcript user message |
| --- | --- | --- | --- | --- | --- |
| trusted, 50 × 200 | 20 | 39.6 / 40.9 / 69.9 | 61.8 / 80.0 / 85.3 | 159.7 / 167.0 / 178.1 | 10.0 / 12.7 / 28.5 |
| trusted, 24 × 80 | 10 | 34.9 / 37.2 / 41.9 | 59.3 / 72.8 / 86.9 | 153.7 / 155.8 / 159.1 | 5.3 / 6.8 / 74.8 |
| hooks off, 50 × 200 | 10 | 39.4 / 41.0 / 73.6 | 53.4 / 79.5 / 83.2 | 163.5 / 169.4 / 173.0 | 11.8 / 13.1 / 15.7 |
| untrusted, 50 × 200 | 10 | 39.7 / 41.6 / 73.1 | 71.5 / 80.0 / 84.0 | 161.2 / 166.9 / 172.7 | 10.6 / 12.5 / 39.8 |
| all | 50 | 34.9 / 40.6 / 73.6 | 53.4 / 79.6 / 86.9 | | 5.3 / 12.7 / 74.8 |

The echo wait is 1 500 ms and the confirm wait 1 000 ms; the slowest head echo used 4.9% of its wait. Each session's first submit is among these (ten sessions): its transcript echo came 5.9–15.0 ms after the Enter. Every turn ended by itself, 0.9–30.9 s after the Enter; none needed an interrupt, and no Enter was kept in the composer.

**Clean frames.** At 50 × 200 the deck's evidence state was Clean from the frame before the head to the Enter on all 40 rings. At 24 × 80 it was Clean on 8 of the 10 rings: a fresh Codex at 24 × 80 draws no width witness (its logo) before its first turn, so the first ring of each 24 × 80 session was read on frames the deck's evidence state had not yet made Clean, and the deck's doorbell would not admit such a ring at all. That is a property of the profile's width witness at that size, already recorded for the 24 × 80 start-up frame, not of the head's echo; the screen readings above hold for those two rings too. In both 24 × 80 sessions no frame from Codex's start, through the settle window and the first ring, to that ring's Enter was Clean; the first Clean frame came as the first turn ended (31.4 s and 6.4 s after the Enter, the turns running 30.9 s and 6.3 s). So a doorbell that waits for a Clean frame would not have rung a fresh Codex in a pane that size until something else had prompted it.

**Load context.** Every capture records `/proc/loadavg` and `/proc/pressure/cpu` at its start and end: the 1-minute load average was 1.60–5.32 on 16 CPUs over the eleven runs (the 15-minute 4.09–5.81), and CPU pressure (`some avg10`) at most 0.91% in any of those 22 samples. No measurement here was taken on a starved box.

## The controls (`…-controls`)

Each typed text was written in one write onto the empty ready composer, read until held exactly, and then deleted with one `0x7f` per owned character in one write, as the deck withdraws; on every delete the placeholder came back 32.6–134.2 ms after the deletes were written, and no write opened a dialog or submitted anything except the Enters below. The strict withdrawal recogniser's verdict is the one on the frame the deletes were written on.

| Control | Count | Held exactly | Strict recogniser |
| --- | --- | --- | --- |
| complete head, alone | 6 | yes, 37–70 ms | 8 deletes |
| partial head, the first 1 to 7 characters | 7 | yes, 34–55 ms | refused, no whole token |
| head, then none of the rest | 1 | yes | 8 deletes |
| head, then the rest's first byte (a space) | 1 | no: the trailing blank draws nothing, so the composer shows the head with the cursor one cell on | refused, not the owned text |
| head, then the rest's first 10 / 40 characters | 2 | yes | 18 / 48 deletes |
| a doorbell-shaped line with another token and id | 1 | yes | never read as empty and ready; a head the composer does not hold is refused as not the owned text |
| an earlier ring's line (an old token) | 1 | yes | as for the foreign line |
| the whole line with the Enter in the same burst (no echo wait, no floor) | 3 | — | kept 3/3: Codex took the Enter into the draft and submitted nothing; refused, cursor elsewhere |
| staged line, Enter asked for 50 / 100 / 120 ms after the rest, and written only once the line had echoed: 81.7–120.2 ms after the rest's write returned | 2 each | yes | submitted 6/6, each once in Codex's record |

## What a record can claim from this, and what it must not

It can claim: on Codex 0.164.0, at 50 × 200 and 24 × 80, with the deck's hooks trusted, switched off or untrusted, the token head typed alone is echoed exactly in the composer within the echo wait (at most 73.6 ms of 1 500 ms over 50 rings), no head byte opens a dialog or commits anything there, the rest typed after the echo is held exactly before the Enter, and the Enter at the deck's timing submits the exact line (50 of 50, read off the transcript within the confirm wait, and in Codex's own record); a whole head is withdrawn with eight deletes and the placeholder confirms it, and the strict recogniser refuses partial heads, a head plus a trailing blank and other lines. The withdrawal controls are short single-row texts (at most 48 owned characters) at 50 × 200 only: whole-line or wrapped withdrawal, and withdrawal at 24 × 80, rest on the older `delete_key` and `placeholder_empty` evidence, not on these captures. An Enter written once the line had echoed submitted it 6 of 6 times, 82–120 ms after the rest's write returned; the shortest Enter measured is 81.7 ms.

It must not claim: anything about another release (0.160.0's record has no `head_echo` measurement here, and 0.166.0, npm's current release, has no record at all); anything about a box the user touched (these rings were all on an untouched, idle composer); readiness of a fresh Codex at 24 × 80 before its first turn (no Clean frame); that an Enter sent with the line, or with no echo awaited, submits (the controls show it is kept); an Enter margin below about 82 ms after the rest; or anything about the deck's own driver, daemon or writer, which these captures do not run (`inbox/codex/027` and the lane-2 `inbox/codex/*` tests do).
