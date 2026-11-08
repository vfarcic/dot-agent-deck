# E-XT — pinned desktop xterm.js colour reply terminator

Task: E-XT evidence gate for Q35-16, amendment 2 §5/§10; message `m-5rjoaneb-d2adb53aeddf8683`. Source inspection on 2026-10-30 against deck commit `216adaebaf7198de717877c04a4fa1e9c3dfb180`. This is static evidence, not an executed terminal experiment. No xterm package code was run.

**Conclusion:** the deck's automatic colour-query responses do not produce R-OB: the pinned desktop emits ST (`ESC \`, bytes `1b 5c`), and the TUI does not answer the agent's colour queries; retain `061`'s forced BEL/ST fixtures as protocol/adversarial coverage, and carry R-OB into Q35-19 as a foreign-client or injected/typed-input residual rather than a desktop-generated reply premise.

## Package identity and reproducibility

The inspected installed copy is `/home/vfarcic/code/dot-agent-deck-dispatch-prd-1666/desktop/node_modules/@xterm/xterm/`, resolving to `desktop/node_modules/.pnpm/@xterm+xterm@6.0.0/node_modules/@xterm/xterm/` in that checkout. Its `package.json:4` says `"version": "6.0.0"`; `package.json:5-6` selects `lib/xterm.js` for `main` and `lib/xterm.mjs` for `module`. The package's recorded upstream commit is `f447274f430fd22513f6adbf9862d19524471c04`.

`desktop/pnpm-lock.yaml:23-25` resolves the desktop importer to version `6.0.0`. Its package entry at lines 715-716 records `sha512-TQwDdQGtwwDt+2cgKDLn0IRaSxYu1tSUjgKarSDkUM0ZNiSRXFpjxEsvc/Zgc5kq5omJ+V0a8/kIM2WD3sMOYg==`. The installed copy was present and matched the required version, so the npm-pack fallback was unnecessary. This inspection records the lock's tarball integrity; it does not claim to have recomputed that tarball digest from an installed directory.

SHA-256 of the inspected files:

| File, relative to installed package | SHA-256 |
| --- | --- |
| `package.json` | `f5a0dde40a146d6c7a86b22daf6e5740517e5d708ec1d162197e0445431d8f4f` |
| `src/browser/CoreBrowserTerminal.ts` | `15c1149d38db18e9aa2c4a896e6c69c5cc40fb73a3fa01bb0602828cae3210a1` |
| `src/common/InputHandler.ts` | `b03afd5d38dd708440699e81135f1735626d55ffdf24621fbd54cfbc41320925` |
| `lib/xterm.js` | `14903579ff54664cd72f8e8699e6961a6272c21863ec1c3b118cdc8af5d4a972` |
| `lib/xterm.mjs` | `b336ec65a086c056d4804b3d4c2347da5663d3f23c3f25be866467bd8857ad59` |

## Reply construction: OSC 4, 10, 11 and 12

Paths in this section are relative to the installed package above.

`src/common/InputHandler.ts:299,306,308,310` registers OSC 4/10/11/12 handlers. OSC 4 parses valid index/spec pairs and fires a REPORT for `spec === '?'` at lines 2965-2988. OSC 10/11/12 invoke `_setOrReportSpecialColor(data, offset)` with offsets 0/1/2 at lines 3096-3115. That shared routine maps the foreground/background/cursor indices at line 3051 and fires REPORT events for `?` at lines 3058-3071. OSC 10 can therefore query three stacked special-colour slots, OSC 11 two, and OSC 12 one. The prose note at lines 3093-3094 saying OSC 12 is unsupported is stale relative to these registered executable handlers; it is not the evidence for this conclusion.

`src/browser/CoreBrowserTerminal.ts:168` wires `onColor` to `_handleColorEvent`. Lines 195-210 select identifiers `10`, `11`, `12`, or `4;<index>`. The REPORT branch at lines 213-218 constructs the response at line 217:

```ts
this.coreService.triggerDataEvent(`${C0.ESC}]${ident};${toRgbString(colorRgb)}${C1_ESCAPED.ST}`);
```

`src/common/data/EscapeSequences.ts:151-152` defines `C1_ESCAPED.ST` as ESC followed by backslash. `src/common/input/XParseColor.ts:77-79` formats the payload as `rgb:<r>/<g>/<b>` (16-bit depth by default). Thus the response forms are `ESC ] 10 ; rgb:... ESC \`, `ESC ] 11 ; rgb:... ESC \`, `ESC ] 12 ; rgb:... ESC \`, and `ESC ] 4 ; <index> ; rgb:... ESC \`.

**The query terminator is not mirrored.** `src/common/parser/EscapeSequenceParser.ts:129` accepts BEL, ESC and C1 ST as OSC-ending bytes; lines 776-777 call the OSC parser with a success boolean rather than a terminator argument. `src/common/parser/OscParser.ts:218-223` invokes its string handler with the payload alone. The colour events above contain request type/index (and a colour for SET), and the response formatter unconditionally appends `C1_ESCAPED.ST`. A valid BEL-ended query therefore receives an ST-ended response, as does an ST-ended query.

The shipped distributions agree with the source: `lib/xterm.js:1`, column 13967, contains `triggerDataEvent` followed by the same template ending in `${E.C1_ESCAPED.ST}`; its ST definition is on line 1, column 228165. `lib/xterm.mjs:51`, column 15156, contains `triggerDataEvent` followed by the equivalent template ending in `${fs.ST}`; its ST definition is on line 47, column 690. Columns are 1-based character offsets within each line. The shipped `lib/xterm.mjs.map` embeds `../src/browser/CoreBrowserTerminal.ts` and `../src/common/data/EscapeSequences.ts`; their `sourcesContent` strings were compared to the installed source files and match exactly.

## Desktop wiring

At the inspected deck commit, `desktop/src/components/TerminalViewport.tsx:242-283` constructs the browser `Terminal`. Its theme supplies the reported colour values (lines 260-281); those values do not alter the formatter above. Fit and WebGL addons are loaded at lines 284-285 and 298-311. A search of production `desktop/src` found this terminal construction and OSC registrations at lines 356 and 402: OSC 52 is a clipboard handler; the other registrations are the colour replay guards.

The colour guard enumerates `[4, 10, 11, 12]` at lines 67-68 and claims a colour query only while `replaying` at lines 391-403. Replay is bracketed by ordered write callbacks at lines 343-353. A live colour query falls through to xterm's own handler, so its reply uses the ST formatter above; a historical query that the guard claims has its reply suppressed. `disableStdin` follows read-only state at line 247, and xterm's `src/common/services/CoreService.ts:61-81` suppresses data events while disabled. The deck's `onData` callback at lines 428-433 drops a guarded historical focus report or forwards `data` to `onInputRef` when writable. `desktop/src/components/AgentTile.tsx:361-363` passes that data onward unchanged. These paths suppress or forward the reply; they do not convert ST to BEL.

## TUI confirmation

The agent-output path in `src/embedded_pane.rs:3675-3714` feeds STREAM_OUT through `process_agent_output_chunk` and separately handles OSC 52 clipboard copies. `process_agent_output_chunk` at lines 4582-4615 scans mouse state, filters OSC 8/52 and feeds a `vt100::Parser`. `src/hyperlink.rs:216-229` sends other OSC sequences, including colour queries, onward to that parser unchanged. `src/embedded_pane.rs:4523-4545` feeds the parser and reads screen/cursor state; the processing function takes neither an input writer nor a response callback. `src/terminal_widget.rs:85-90,300-315` renders the resulting screen as styled cells. The separate input writer at `src/embedded_pane.rs:3765-3774` writes queued `StreamCmd::Input` bytes as STREAM_IN. There is no colour-query response generation in this inspected TUI agent-output/render path, nor a forwarding of the raw query to the enclosing terminal for it to answer. This confirms the plan's premise for agent OSC 4/10/11/12 queries; it does not assert that the TUI never emits an unrelated terminal control sequence of its own.

## `061` and Q35-19 implications

`tests/e2e_input_fence.rs:468-475` deliberately constructs ST-ended queries and forces either BEL or ST on the replies. `probe_061_whole_retained_reports_are_not_native_even_after_saturation`, `probe_061_unretained_bel_reports_number_one_native_candidate`, `probe_061_split_bel_reports_number_one_native_candidate`, and the during-check cases remain valid coverage of the accepted protocol and conservative classification. Their BEL branch is synthetic input coverage, not evidence that the pinned desktop generates BEL colour replies. Keep both terminators and both whole/split transport cases.

R-OB as defined by amendment 2 §5 needs a saturated ledger plus a BEL-ended reply to an unretained OSC query. The automatic desktop reply lacks that BEL; the TUI supplies no such reply. The result closes E-XT's pinned-client terminator premise for Q35-16. It does not eliminate foreign clients or arbitrary typed/injected BEL-shaped input, prove planner correctness, certify segmentation on the wire, or discharge Q35-19's admission and writer-thread revalidation work. Q35-19 should retain S-19's `061` FIFO/expiry and during-check assertions, conservative R-OB handling, strict Ground probe predicates and its other independent evidence gates.

## Validation and scope

Only this evidence Markdown file changed; production code and tests were not edited. `cargo xtask affected-checks --run .dot-agent-deck/r5wake-measurements/e-xt.md` selected the full gates because this evidence directory has no text-only mapping. `cargo fmt --check` passed locally; `cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings` and `cargo test-fast` passed in RWX sandbox run `01a18a4e30e67ad1999af8f044ab52b7`. Nextest run `c386bd3d-6859-4408-8225-5cf91903373b` reported 9,073 passed and 5 skipped in 75.857 seconds. The named `probe_061_*` cases above were inspected for implications, not rerun; this task changes evidence only and makes no runtime-validation claim for those L2 tests. No real agent or production daemon was exercised for this inspection.
