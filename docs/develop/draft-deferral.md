# Draft protection for inbox doorbells

Task bodies, reports and startup prompts are delivered through the per-agent inbox. The former automatic-prompt deferral, forced write at its cap, `dispatch_one_owned` pointer-write path and `WriteAndSubmit` RPC are removed. `DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS` no longer changes behavior and warns when set at daemon startup. See [Agent messaging](agent-messaging.md#doorbell-guards) for the current wake path and [Orchestration](../orchestration.md#delivery-in-the-tui-and-desktop-app) for both clients' behavior.

The daemon still tracks human input to protect the fixed doorbell. Input from either client feeds the same per-pane draft and paste state under the PTY writer. A pending draft or open paste holds automatic ringing; no timeout forces submission. Eligibility is rechecked before each automatic write, together with the recipient generation, status, armed consumer and admission epoch.

## Limits of draft tracking

The input tracker observes bytes forwarded through the deck, rather than the agent's editable buffer. History recall, agent-generated text and partial line clears can differ from its model. Codex also requires a recognized empty composer before ringing. An unknown status, permission prompt or unrecognized dialog holds the doorbell. A user-started turn can still receive inbox work through the agent's hooks or extension.

Human keystrokes from both clients use `WriteInput`. They remain user actions and do not use the removed automatic-prompt deferral or its cap.
