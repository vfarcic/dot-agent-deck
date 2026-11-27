import { useEffect, useState, type KeyboardEvent } from "react";
import { RefreshCw, X } from "lucide-react";

import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { CardActionFormValues, CardActionsEditor, CardActionsEditorView, CardActionsReread } from "../hooks/useCardActionsEditor";
import { CardActionIcon, CARD_ICON_NAMES, cardIconName, GENERIC_CARD_ICON, searchCardIcons } from "../lib/cardIcons";
import { DISPLAY_LIMITS, displayText, displayTitle } from "../lib/displayText";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import type { EditableCardAction } from "../types";
import { ExactText, ExactTextNote } from "./AgentCard";

/** The editor's members as its controls dispatch them: the hook's, with refusals said in the editor. */
type EditorControls = CardActionsEditor["controls"] & { reportRefused: (sentence: string) => void };

/** Where an action in the list came from, in the editor's words. */
function sourceWords(action: EditableCardAction): string {
  if (!action.default) return "yours";
  if (action.overridden) return "deck default, your version";
  return "deck default";
}

/**
 * PRD #2374 decision 2026-11-24 — the Card actions editor: the card actions
 * of one daemon, the one whose ⋯ menu opened it (PR #2491 decision
 * 2026-11-25), named in its title, each with where it came from, the exact text it sends and what
 * it may be changed by — Edit on every one, Remove on the user's own, Hide on
 * a deck default in use and Restore on one hidden or changed. Add action and
 * Edit open a form whose icon is chosen from a searchable grid of the icons
 * the app bundles. Every change is the daemon's: one edit is sent, and the
 * list shows what the daemon answers, or its reason for refusing; Refresh
 * reads the list again, keeping the form. A daemon from an earlier release is
 * shown read-only, with a sentence saying so.
 */
export function AgentCardActionsEditor({ view, editor }: { view: CardActionsEditorView; editor: Pick<CardActionsEditor, "controls" | "setField"> }) {
  /* Why the last press in the editor did nothing, said in the editor. */
  const [notice, setNotice] = useState<string | undefined>(undefined);
  const controls = { ...editor.controls, reportRefused: setNotice };
  const { setField } = editor;
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, () => editor.controls.closeCardActionsEditor());
  const { daemon, catalog, form, removing } = view;
  const daemonName = displayText(daemon.name, DISPLAY_LIMITS.name);
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key !== "Escape" || event.metaKey || event.ctrlKey || event.altKey) return;
    event.preventDefault();
    event.stopPropagation();
    VOICE_ACTIONS.closeCardActionsEditor.run(controls);
  };
  return (
    <div className="dialog-backdrop card-actions-editor-backdrop" role="presentation">
      <section
        ref={ref}
        tabIndex={-1}
        className="card-actions-editor"
        role="dialog"
        aria-modal="true"
        aria-labelledby="card-actions-editor-title"
        data-testid="agent-card-actions-editor"
        onKeyDown={onKeyDown}
      >
        <header className="card-actions-editor-header">
          <h2 id="card-actions-editor-title" data-testid="agent-card-actions-editor-title">Card actions: {daemonName}</h2>
          <button type="button" className="icon-button" aria-label="Close Card actions" title="Close (Escape)" data-testid="agent-card-actions-editor-close" onClick={() => VOICE_ACTIONS.closeCardActionsEditor.run(controls)}><X size={15} aria-hidden="true" /></button>
        </header>
        <div className="card-actions-editor-body">
          <p className="agent-card-hint">The actions agents can offer on their cards. A change reaches every card of this daemon at once.</p>
          {!daemon.editable && (
            <p className="card-actions-editor-readonly" role="status" data-testid="agent-card-actions-editor-readonly">
              {daemonName}'s daemon is from an earlier release, so its card actions cannot be edited from the app. Upgrade that daemon, or edit the <code>[catalog]</code> table of the user config file on its machine.
            </p>
          )}
          {daemon.editable && view.loading && <p role="status">Reading {daemonName}'s card actions…</p>}
          {notice && <p className="card-actions-editor-notice" role="status" data-testid="agent-card-actions-editor-notice">{displayText(notice, DISPLAY_LIMITS.message)}</p>}
          {view.error && !form?.error && <p className="card-actions-editor-error" role="alert">{view.error}</p>}
          {view.reread && <RereadNote reread={view.reread} />}
          {daemon.editable && catalog && (
            <>
              {catalog.user_config && <p className="card-actions-editor-file">Saved in <code>{displayText(catalog.user_config, DISPLAY_LIMITS.detail)}</code> on {daemonName}'s machine.</p>}
              {catalog.errors.length > 0 && (
                <div className="card-actions-editor-problems" data-testid="agent-card-actions-editor-problems">
                  <p>The file has problems the deck works around; fix them in the file:</p>
                  <ul>{catalog.errors.map((problem, index) => <li key={index}>{displayText(problem, DISPLAY_LIMITS.message)}</li>)}</ul>
                </div>
              )}
              <div className="card-actions-editor-toolbar">
                {!form && <button type="button" className="button secondary compact" disabled={view.busy} onClick={() => VOICE_ACTIONS.addCardAction.run(controls)}>Add action</button>}
                <button type="button" className="button secondary compact" title="Read this daemon's card actions again; what the form holds is kept" disabled={view.refreshing} data-testid="agent-card-actions-editor-refresh" onClick={() => VOICE_ACTIONS.refreshCardActions.run(controls)}><RefreshCw size={13} aria-hidden="true" />{view.refreshing ? "Reading…" : "Refresh"}</button>
              </div>
              {form && <ActionForm form={form} busy={view.busy} editor={{ controls, setField }} />}
              <ul className="card-actions-editor-list" aria-label={`${daemonName}'s card actions`}>
                {catalog.actions.map((action) => <ActionRow key={action.name} action={action} busy={view.busy} editor={{ controls }} />)}
              </ul>
            </>
          )}
        </div>
      </section>
      {removing && <RemoveConfirm action={removing} daemonName={daemonName} busy={view.busy} controls={controls} />}
    </div>
  );
}

/**
 * Issue #2399 re-review N3 — how reading the list again went, said on its own
 * line whatever error the form shows: under way and read only after an edit
 * whose outcome is unknown (Refresh says it on its button), failed for either.
 * "Read again" is said only once a read has answered; what the form holds is
 * kept, and Refresh stays offered.
 */
function RereadNote({ reread }: { reread: CardActionsReread }) {
  if (reread.status === "failed") {
    return (
      <p className="card-actions-editor-error" role="alert" data-testid="agent-card-actions-editor-reread">
        {reread.kind === "check" ? "Whether the change was saved is not known yet: the" : "The"} card actions could not be read again: {reread.reason} What the form holds is kept; press Refresh to try again.
      </p>
    );
  }
  if (reread.saved !== undefined) {
    return (
      <p className="card-actions-editor-notice" role="status" data-testid="agent-card-actions-editor-reread">
        {displayText(reread.saved, DISPLAY_LIMITS.name)} was saved: the list below was read again and shows it as the form sent it.
      </p>
    );
  }
  if (reread.kind === "refresh") return null;
  return (
    <p className="card-actions-editor-notice" role="status" data-testid="agent-card-actions-editor-reread">
      {reread.status === "reading" ? "Reading the list again to see whether the change was saved…" : "The list below was read again; check it before saving again."}
    </p>
  );
}

/**
 * Issue #2399 review 13 — the confirmation of a removal: a dialog of its own,
 * on the overlay stack over the editor, so the editor behind it is inert while
 * it is open, focus moves into it, Escape or voice's `close` dismisses it
 * (the editor's close member closes the removal first), and focus goes back
 * to the Remove that opened it.
 */
function RemoveConfirm({ action, daemonName, busy, controls }: { action: EditableCardAction; daemonName: string; busy: boolean; controls: EditorControls }) {
  const ref = useInertBackground<HTMLDivElement>(true, OVERLAY_LAYER.dialog, () => controls.closeCardActionsEditor());
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key !== "Escape" || event.metaKey || event.ctrlKey || event.altKey) return;
    event.preventDefault();
    event.stopPropagation();
    VOICE_ACTIONS.closeCardActionsEditor.run(controls);
  };
  const label = displayText(action.label, DISPLAY_LIMITS.name);
  return (
    <div className="dialog-backdrop card-actions-editor-remove-backdrop" role="presentation">
      <div ref={ref} tabIndex={-1} className="card-actions-editor-confirm" role="alertdialog" aria-modal="true" aria-labelledby="card-actions-editor-remove-title" aria-describedby="card-actions-editor-remove-body" data-testid="agent-card-actions-editor-remove" onKeyDown={onKeyDown}>
        <h3 id="card-actions-editor-remove-title">Remove {label}?</h3>
        <p id="card-actions-editor-remove-body">Agents stop offering it on their cards. This removes it from the user config on {daemonName}'s machine.</p>
        <div className="card-actions-editor-buttons">
          <button type="button" className="button secondary" onClick={() => VOICE_ACTIONS.closeCardActionsEditor.run(controls)}>Cancel</button>
          <button type="button" className="button danger" disabled={busy} onClick={() => VOICE_ACTIONS.confirmRemoveCardAction.run(controls)}>Remove</button>
        </div>
      </div>
    </div>
  );
}

/** One listed action: its icon, label, where it came from, the exact text it sends, and what it may be changed by. */
function ActionRow({ action, busy, editor }: { action: EditableCardAction; busy: boolean; editor: { controls: EditorControls } }) {
  const { controls } = editor;
  const label = displayText(action.label, DISPLAY_LIMITS.name);
  const canRestore = action.default && (action.hidden || action.overridden);
  return (
    <li className={`card-actions-editor-row${action.hidden ? " is-hidden" : ""}`} data-testid="agent-card-actions-editor-row" data-action={action.name} data-source={action.source === "default" ? "deck" : "user"}>
      <div className="card-actions-editor-row-head">
        <span className="card-actions-editor-row-icon" data-icon={cardIconName(action.icon)}><CardActionIcon name={action.icon} size={15} /></span>
        <strong className="card-actions-editor-row-label">{label}</strong>
        <span className={`card-actions-editor-source is-${action.default ? "deck" : "user"}`} data-testid="agent-card-actions-editor-source">{sourceWords(action)}</span>
        {action.hidden && <span className="card-actions-editor-hidden">hidden</span>}
        {action.confirm && <span className="card-actions-editor-flag" title="Asks before sending">asks first</span>}
        {action.always && <span className="card-actions-editor-flag" title="Offered on every card">every card</span>}
      </div>
      <blockquote className="agent-card-prompt-text card-actions-editor-prompt"><ExactText text={action.prompt} /></blockquote>
      <ExactTextNote text={action.prompt} />
      <p className="card-actions-editor-when" title={displayTitle(action.description)}>{displayText(action.description, DISPLAY_LIMITS.message)}</p>
      <div className="card-actions-editor-buttons">
        <button type="button" className="button secondary compact" aria-label={`Edit ${label}`} disabled={busy} onClick={() => VOICE_ACTIONS.editCardAction.run(controls, { cardActionId: action.name })}>Edit</button>
        {!action.default && <button type="button" className="button secondary compact" aria-label={`Remove ${label}`} disabled={busy} onClick={() => VOICE_ACTIONS.removeCardAction.run(controls, { cardActionId: action.name })}>Remove</button>}
        {action.default && !action.hidden && <button type="button" className="button secondary compact" aria-label={`Hide ${label}`} title="Stop agents offering it; Restore brings it back" disabled={busy} onClick={() => VOICE_ACTIONS.hideCardAction.run(controls, action.name)}>Hide</button>}
        {canRestore && <button type="button" className="button secondary compact" aria-label={`Restore ${label}`} title="Bring it back as the deck ships it" disabled={busy} onClick={() => VOICE_ACTIONS.restoreCardAction.run(controls, action.name)}>Restore</button>}
      </div>
    </li>
  );
}

/**
 * Issue #2399 review 11 — where an action is offered, as the form says it:
 * an icon on its agents' cards (`card`), or only in the agent pane's Card tab
 * (`detail`). An action whose file says `compact` is shown as on the card and
 * keeps `compact` unless the other choice is picked.
 */
const PLACEMENTS: readonly { value: "card" | "detail"; label: string }[] = [
  { value: "card", label: "Show on the card" },
  { value: "detail", label: "Only in the Card tab" },
];

/** The form for a new action, or for one listed action (its name fixed). */
function ActionForm({ form, busy, editor }: { form: NonNullable<CardActionsEditorView["form"]>; busy: boolean; editor: { controls: EditorControls; setField: CardActionsEditor["setField"] } }) {
  const { controls, setField } = editor;
  const { values } = form;
  const [picking, setPicking] = useState(false);
  const [query, setQuery] = useState("");
  /* Another form (a voice request for another action) starts with the grid closed. */
  const formKey = `${form.mode}\u0000${form.original?.name ?? ""}`;
  useEffect(() => { setPicking(false); setQuery(""); }, [formKey]);
  const shownIcons = query.trim() ? searchCardIcons(query) : CARD_ICON_NAMES;
  const iconShown = cardIconName(values.icon);
  const id = (field: keyof CardActionFormValues) => `card-action-form-${field}`;
  const title = form.mode === "add" ? "New card action" : `Edit ${displayText(form.original?.label ?? values.name, DISPLAY_LIMITS.name)}`;
  return (
    <form className="card-actions-editor-form" aria-label={title} data-testid="agent-card-actions-editor-form" onSubmit={(event) => event.preventDefault()}>
      <h3>{title}</h3>
      {/* Locked while it is being saved, so what is saved is what the form shows (issue #2399 review 6). */}
      <fieldset disabled={busy}>
      <div className="card-actions-editor-field">
        <label htmlFor={id("name")}>Name</label>
        <input id={id("name")} value={values.name} readOnly={form.mode === "edit"} aria-describedby={`${id("name")}-hint`} maxLength={40} spellCheck={false} autoComplete="off" onChange={(event) => setField("name", event.target.value)} />
        <small id={`${id("name")}-hint`}>{form.mode === "edit" ? "An action's name cannot change; add a new action for another name." : "Lower-case letters, digits and dashes, such as deploy-it. It cannot change later."}</small>
      </div>
      <div className="card-actions-editor-field">
        <label htmlFor={id("label")}>Label</label>
        <input id={id("label")} value={values.label} maxLength={40} aria-describedby={`${id("label")}-hint`} onChange={(event) => setField("label", event.target.value)} />
        <small id={`${id("label")}-hint`}>What the icon's hover and the Card tab call it. Left empty, the name is used.</small>
      </div>
      <div className="card-actions-editor-field">
        <label htmlFor={id("prompt")}>Text it sends</label>
        <textarea id={id("prompt")} value={values.prompt} rows={3} maxLength={2000} aria-describedby={`${id("prompt")}-hint`} onChange={(event) => setField("prompt", event.target.value)} />
        <small id={`${id("prompt")}-hint`}>Sent to the agent exactly as written, line breaks included.</small>
      </div>
      <div className="card-actions-editor-field">
        <label htmlFor={id("description")}>Tell agents when to offer it</label>
        <input id={id("description")} value={values.description} maxLength={300} aria-describedby={`${id("description")}-hint`} onChange={(event) => setField("description", event.target.value)} />
        <small id={`${id("description")}-hint`}>Agents read this to decide when to put the action on their card; it is not shown on the card.</small>
      </div>
      <div className="card-actions-editor-field">
        <span className="card-actions-editor-field-name">Icon</span>
        <div className="card-actions-editor-icon-row">
          <span className="card-actions-editor-icon" data-testid="agent-card-actions-editor-icon" data-icon={iconShown} title={iconShown === GENERIC_CARD_ICON ? "No icon chosen: the generic icon" : iconShown}><CardActionIcon name={values.icon} size={16} /></span>
          <button type="button" className="button secondary compact" aria-expanded={picking} onClick={() => setPicking((open) => !open)}>Choose icon</button>
        </div>
        {picking && (
          <div className="card-actions-editor-picker">
            <input type="search" aria-label="Search icons" placeholder="Search icons" value={query} onChange={(event) => setQuery(event.target.value)} />
            <div className="card-actions-editor-icons" role="listbox" aria-label="Icons">
              {shownIcons.map((name) => (
                <button key={name} type="button" role="option" aria-label={name} aria-selected={values.icon === name} title={name} data-icon={name} className={`icon-button card-actions-editor-icon-option${values.icon === name ? " is-chosen" : ""}`} onClick={() => { VOICE_ACTIONS.chooseCardActionIcon.run(controls, name); setPicking(false); }}><CardActionIcon name={name} size={16} /></button>
              ))}
              {shownIcons.length === 0 && <p className="agent-card-hint">No icon matches.</p>}
            </div>
          </div>
        )}
      </div>
      <div className="card-actions-editor-field">
        <label htmlFor={id("placement")}>Placement</label>
        <select id={id("placement")} value={values.placement === "detail" ? "detail" : "card"} aria-describedby={`${id("placement")}-hint`} onChange={(event) => VOICE_ACTIONS[event.target.value === "detail" ? "showCardActionOnlyInCardTab" : "showCardActionOnCard"].run(controls)}>
          {PLACEMENTS.map((placement) => <option key={placement.value} value={placement.value}>{placement.label}</option>)}
        </select>
        <small id={`${id("placement")}-hint`}>On the card, the action is an icon on each card that offers it (up to three per card); only in the Card tab, it is an icon in the Card tab of the agent's pane and not on the card.</small>
      </div>
      <label className="card-actions-editor-check"><input type="checkbox" checked={values.confirm} onChange={(event) => VOICE_ACTIONS[event.target.checked ? "askBeforeSendingCardAction" : "sendCardActionWithoutAsking"].run(controls)} />Ask before sending</label>
      <label className="card-actions-editor-check"><input type="checkbox" checked={values.always} onChange={(event) => VOICE_ACTIONS[event.target.checked ? "offerCardActionOnEveryCard" : "offerCardActionWhenAgentsChoose"].run(controls)} />Offer on every card</label>
      </fieldset>
      {/* PR #2491 re-audit (F1, Edit): keyed by the form's changed-since alerts, so each change is a new alert, announced again. */}
      {form.error && <p key={form.alertKey ?? 0} className="card-actions-editor-error" role="alert">{form.error}</p>}
      <div className="card-actions-editor-buttons">
        <button type="button" className="button secondary" onClick={() => VOICE_ACTIONS.cancelCardActionForm.run(controls)}>Cancel</button>
        <button type="button" className="button primary" disabled={busy} onClick={() => VOICE_ACTIONS.saveCardAction.run(controls)}>Save</button>
      </div>
    </form>
  );
}
