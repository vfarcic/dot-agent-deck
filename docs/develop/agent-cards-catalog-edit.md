# The card actions editor

PRD #2374's decision of 2026-11-24, shipped with issue #2399 on PR #2491, lets the desktop app add, change, hide and remove card actions instead of the user editing `[catalog]` by hand. The user-facing description is in [`docs/desktop/agent-cards.md`](../desktop/agent-cards.md#the-card-actions-editor). This page is for whoever changes the request, the writer or the editor.

## The request

The daemon reads the user config of the machine it runs on, so for a remote daemon "the catalog" is that machine's file. The editor therefore goes through the daemon (CLAUDE.md rule 18), with one request:

- capability `agent-cards-catalog-edit` (`CAP_AGENT_CARDS_CATALOG_EDIT`), advertised on every platform;
- `AttachRequest::CardCatalogEdit { edit: Option<CatalogActionEdit> }`, wire op `card-catalog-edit`; no `edit` is a read that writes nothing;
- `AttachResponse.card_catalog_edit: Option<EditableCardCatalog>`, the catalog after the edit, or as read.

Both are additive and the request is withheld by `DaemonClient::card_catalog_edit` unless the `Hello` on the same connection named the capability and `hello-follow-on`, so there is no `PROTOCOL_VERSION` bump, no `.breaking.md` and no `CONTRACT_BREAKS` entry. An older daemon is sent nothing; the desktop shows its editor read-only.

`CatalogActionEdit` is tagged by `kind`: `add` and `update` carry a `CatalogActionInput`, `remove`, `hide` and `restore` carry a `name`. `CatalogActionInput` has `deny_unknown_fields`, so a `path`, a `command` or any key a `[[catalog.actions]]` entry may not carry fails to parse, and the daemon answers its generic malformed-request refusal. An unknown `kind` fails the same way.

| edit | what it writes | refused when |
| --- | --- | --- |
| `add` | a new `[[catalog.actions]]` entry | the name is a deck default's or an existing user action's; the catalog already holds `MAX_CATALOG_ACTIONS` |
| `update` | replaces the user entry of that name in place (its keys keep their comments); for a deck default, writes a same-name override and takes the name out of `remove` | the name is neither a default nor a user action |
| `remove` | drops the user entry | the name is a default (hide it instead) or no user action |
| `hide` | puts a default's name in `catalog.remove` and drops any override | the name is not a default |
| `restore` | takes a default's name out of `remove` and drops any override | the name is not a default |

Every value is validated by the catalog's own `parse_action`, the code that reads the file: name syntax, reserved names (`open`, `reply`, `close`, `close-keep-worktree`, anything starting `question`), required description and prompt, length limits (`MAX_DESCRIPTION_CHARS`, `MAX_CATALOG_LABEL_CHARS`, `MAX_PROMPT_CHARS`), control characters (a prompt may hold line breaks and tabs, nothing else may), icon syntax. A refusal's reason names the field or the name at fault.

## The writer

`card_catalog::apply_action_edit` edits the raw TOML with `toml_edit`, as `config::merge_dashboard_config` does, touching only `[[catalog.actions]]` and `catalog.remove`: every other setting and table, the catalog's fields and the comments are kept. After the edit it resolves the result and refuses it when it would add a catalog problem the file did not already have, when the edited action would not be the one in effect, or when the file would be larger than the reader reads (`MAX_USER_CONFIG_BYTES`, 1 MiB). A file that is not valid TOML is refused; there is nothing to merge into.

`card_catalog::edit_user_catalog_at` reads the file, applies the edit and writes it through `config::write_config_atomically` (a temp file beside it, renamed over it; the fall-backs that function documents apply). A file it cannot read — larger than 1 MiB, not UTF-8, not TOML — is refused for an edit and left byte for byte; a read still answers, with the problem in `errors`. A missing file and its folder are created by an edit. One process-wide mutex serializes edits, so two editors' read-change-write cycles never interleave. The daemon passes `config::user_config_path()`, never a path from the client, runs the call on a blocking task, and forgets its cached catalog after an edit, so the next card read resolves the new file whatever its modification time says. Nothing in the request runs a command.

`editable_catalog` lists the deck defaults in catalog order (a hidden one with the deck's own values), then the user's own actions in file order; `source` is `default` or `user`, and `default`, `overridden` and `hidden` say the rest.

## The desktop

- Tauri command `agent_card_catalog_edit { deckId, edit|null }`, one deadline (`DECK_REPLY_TIMEOUT`) for reaching the deck and the exchange. A refusal is the daemon's reason as given; a daemon without the capability answers `CARD_CATALOG_EDIT_UNSUPPORTED`; an edit whose answer was lost says it may have been saved. Transport tests sit beside the batch ones in `desktop/src-tauri/src/restart_binding_tests.rs`.
- The cards snapshot's `catalogEdit` carries the capability, as `batch` does. The editor (`useCardActionsEditor`, `AgentCardActionsEditor`) lists the daemons whose cards are on the Dashboard, in its order, and starts on the local one (`deckKind: "local"`), wherever it is listed, else the first. A read or an edit answering after the daemon shown changed is dropped (a read sequence number), and changing the daemon closes an open form, so nothing typed for one daemon is saved to another.
- The desktop names `dot_agent_deck::card_catalog`'s wire types; linkage-check rule 12 allows the module, with the argument in `desktop_project_boundary.rs`.
- The fixture bridge answers the editor for the `cards=tree` scene from `createFixtureCardCatalog`, the deck's two defaults, applying edits for the session (Playwright `desktop/agent-cards/061`, screenshot scene `agent-cards-actions-editor`).

## Icons

The app draws card action icons from a curated set of the lucide icons it already bundles, keyed by the catalog's `icon` name (`desktop/src/lib/cardIcons.tsx`, `CARD_ICON_NAMES`); the editor's picker offers exactly that set. The daemon validates only the icon's syntax, so a catalog may name an icon a newer app has; this app draws its generic icon for it. A card draws up to `CARD_ICON_LIMIT` (3) icons, then `+N`, which keeps a 224-pixel card's name readable; they sit at the end of the status line, under Close, because three icons beside Close in the header left a card with Needs you and a tree toggle no room for its name.

The card's icons and the batch buttons are `ActionTipButton`s (`desktop/src/components/ActionTipButton.tsx`). A pointer gets the native `title`; keyboard focus gets the same text as a `role="tooltip"` portalled to `document.body`, positioned `fixed` from the button's box so no card's `overflow` clips it, and named by the button's `aria-describedby` while shown. A disabled action is `aria-disabled`, not `disabled`: a native disabled button leaves the Tab order, and with it the only keyboard route to the reason. Its press does nothing.

## Voice

`open_card_actions_editor`, `add_card_action`, `edit_card_action` and `remove_card_action` are voice rows (the last two take a `card_action` param, the action's label as said). Every write is `no_voice`: `saveCardAction`, `confirmRemoveCardAction`, `hideCardAction`, `restoreCardAction`, so a misheard word never becomes text an agent is sent. Choosing the daemon (`chooseCardActionsDaemon`) and an icon (`chooseCardActionIcon`) are `no_voice` with their reasons; `desktop/agent-cards/060` fails when one of the editor's registry entries has neither a row nor a reason. The form's field edits (Placement included) are listed in linkage-check rule 14's control-site ratchet. Pressing a card's icon is the existing `run_card_action` row.

## The TUI

The TUI has no cards view, so it has no editor; its users edit `[catalog]` in the file, which `dot-agent-deck config set` keeps with its comments.
