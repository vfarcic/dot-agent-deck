# PRD #20 trust-fix RED tests

| ID | Tier | File | Proposal | RED evidence |
| --- | --- | --- | --- | --- |
| `codex/trust/001` | fast | `tests/codex_hooks_safety.rs` | §4.3.3 | `codex received the deleted invocation-global trust bypass: args=["--dangerously-bypass-hook-trust"]` |
| `codex/trust/002` | fast | `tests/codex_hooks_safety.rs` | §4.3.1, §4.3.6 | `scoped trust config was not written at <temp>/config.toml: No such file or directory` after the seeded foreign hook suppresses the legacy bypass |
| `codex/trust/003` | fast | `tests/codex_hooks_safety.rs` | §4.3.2 | `repeated trust writes must not duplicate the deck table` with `left: 0`, `right: 1`; the existing commented config remains unchanged because no deck trust table is written |
| `codex/hooks/002` | targeted e2e | `tests/e2e_codex_hooks.rs` | §4.3.5 | `script-launched Codex trust config was not written at <temp>/config.toml: No such file or directory` while `hooks.json`, pinned home propagation, and no launcher bypass already hold |
| `codex/hooks/003` | targeted e2e | `tests/e2e_codex_hooks.rs` | §4.2.1, §4.3.6 | `the non-codex launcher never produced a Codex card after startup install+trust`; the captured grid shows `No agent · launcher-codex` and `0 active` |
| `codex/hooks/004` | fast | `tests/codex_hooks_safety.rs` | broken CLI / §4.2.1 | `documented Codex hook install must succeed; stderr=No hook installer for agent Codex` |

The real interactive launcher scenario from §4.3.7 is intentionally deferred to the GREEN-confirm phase. At that point extend `codex/hooks/001` to launch real cheap-model Codex through a script in a fresh home, without the bypass or manual `/hooks` review, and verify prompt/tool events plus visible live/Idle status.

`cargo xtask linkage-check` recognizes the new catalog links but remains red on four pre-existing Decision 21 sleep calls in unchanged `tests/e2e_delegate_work_done_chain.rs` lines 114, 169, 183, and 210.
