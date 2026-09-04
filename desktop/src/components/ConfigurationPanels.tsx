import { useEffect, useState } from "react";
import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  BookMarked,
  Bot,
  Check,
  FolderCheck,
  FolderGit2,
  GripVertical,
  Plus,
  RefreshCw,
  RotateCcw,
  Save,
  SlidersHorizontal,
  Trash2,
  X,
} from "lucide-react";
import { defaultCliForProvider, permissionModeLabel, permissionModeOptions, resolveProfileCommand } from "../lib/profileCommands";
import type { DaemonProjectsState } from "../hooks/useDaemonProjects";
import type { AgentProfile, DaemonResolvedProject, DeckPrompt, Provider, RuntimeMode, WorkflowLaunchConfig } from "../types";

/**
 * PRD #819 M6: the project PICKER, and nothing else.
 *
 * It used to be a project LIBRARY — add, rename, edit a directory, write notes,
 * name a workflow, remove — all persisted to `localStorage` and all of it
 * client-held project state that a remote daemon could not honour. What
 * replaced it is a picker over the projects the daemon reports, plus a field
 * for a path the daemon has nothing live in. Nothing here is saved: choosing a
 * project is a step in assembling one launch.
 */
interface ProjectsPanelProps {
  open: boolean;
  state: DaemonProjectsState;
  onClose: () => void;
  onConfigureWorkflow: () => void;
}

export function ProjectsPanel({ open, state, onClose, onConfigureWorkflow }: ProjectsPanelProps) {
  const [pasted, setPasted] = useState("");
  if (!open) return null;
  const { projects, primary, listing, listingError, selected, resolving, resolveError, vanished } = state;
  const submitPasted = async () => {
    const path = pasted.trim();
    if (!path) return;
    if (await state.select(path)) setPasted("");
  };
  // One sentence for the two states that are NOT faults, said in the words that
  // are true of each. The daemon having nothing live is the first-run state,
  // and it is the only one — there is no second, remembered list behind it.
  const emptyNotice = vanished
    ? "That project is no longer one this daemon knows — nothing is running there any more. Pick another, or paste its path again below."
    : listing === "empty"
      ? "This daemon has nothing live and its own directory is not a project, so it knows of none to offer. Paste a project's absolute path below; the daemon resolves it on its own machine."
      : undefined;

  return (
    <div className="sheet-backdrop" role="presentation" onMouseDown={onClose}>
      <section className="config-sheet projects-sheet" role="dialog" aria-modal="true" aria-labelledby="projects-title" data-testid="projects-panel" onMouseDown={(event) => event.stopPropagation()}>
        <header className="sheet-header">
          <div><span className="eyebrow">PROJECT SELECTION</span><h2 id="projects-title">Projects</h2><p>Choose the project this launch runs in. The daemon answers with what it knows.</p></div>
          <button className="icon-button" aria-label="Close projects" onClick={onClose}><X size={18} /></button>
        </header>
        <div className="local-only-notice project-notice"><FolderCheck size={15} /><span><strong>From the daemon</strong> — these are the projects the connected daemon can see on its own machine, and nothing is remembered between launches.</span></div>

        <div className="projects-layout">
          <aside className="project-library">
            <button className="add-project" onClick={() => void state.refresh()} data-testid="refresh-projects"><RefreshCw size={14} /> Refresh</button>
            <nav aria-label="Daemon projects">
              {projects.map((item) => (
                <button key={item.path} className={item.path === selected?.path ? "is-selected" : ""} onClick={() => void state.select(item.path)} disabled={resolving}>
                  <span className={item.path === primary ? "project-icon is-active" : "project-icon"}><FolderGit2 size={15} /></span>
                  <span><strong>{item.name}</strong><small>{item.path}</small></span>
                  {item.path === primary && <em>ACTIVE</em>}
                </button>
              ))}
            </nav>
            {listing === "loading" && <div className="project-library-empty"><FolderGit2 size={22} /><span>Asking the daemon…</span></div>}
            {listing === "unavailable" && <div className="project-library-empty" data-testid="projects-unavailable"><AlertTriangle size={22} /><span>{listingError ?? "The daemon did not answer."}</span></div>}
            {!projects.length && listing !== "loading" && listing !== "unavailable" && <div className="project-library-empty" data-testid="projects-empty"><FolderGit2 size={22} /><span>No projects reported.</span></div>}
          </aside>

          <form className="project-form" onSubmit={(event) => { event.preventDefault(); void submitPasted(); }}>
            <div className="form-heading">
              <div><span>SELECTED PROJECT</span><h3>{selected?.name ?? "No project chosen"}</h3></div>
              {selected ? <span className="active-project-badge"><Check size={11} /> Ready to launch</span> : null}
            </div>
            <div className="project-fields">
              {emptyNotice && <small className="project-field-note" data-testid="projects-nothing-known">{emptyNotice}</small>}
              <label><span>Resolve a project by path</span><input aria-label="Project directory" value={pasted} onChange={(event) => setPasted(event.target.value)} placeholder="/Users/you/dev/project" spellCheck={false} /><small>The daemon resolves this on its own filesystem, which may not be this one.</small></label>
              <button type="submit" className="button secondary" disabled={!pasted.trim() || resolving} data-testid="resolve-project">{resolving ? "Resolving…" : "Resolve"}</button>
              {resolveError && <small className="project-field-error" data-testid="project-resolve-error"><AlertTriangle size={12} /> {resolveError}</small>}
              {selected && (
                <div className="project-fields" data-testid="selected-project">
                  <label><span>Daemon path</span><input aria-label="Resolved project path" value={selected.path} readOnly spellCheck={false} /><small>The daemon&apos;s own spelling. The launch uses this exact string.</small></label>
                  <span>Workflows in this project: {selected.orchestrations.length ? selected.orchestrations.map((orchestration) => orchestration.name).join(", ") : "none configured"}</span>
                </div>
              )}
            </div>
            <div className="project-readiness">
              <FolderCheck size={16} />
              <span><strong>{selected ? "Project resolved" : "No project chosen"}</strong>{selected ? "Open Workflows to pick one of its orchestrations and launch." : "Pick one above, or resolve a path, before configuring a workflow."}</span>
            </div>
            <footer className="sheet-footer project-footer">
              <span />
              <div>
                <button type="button" className="button secondary" onClick={() => state.clearSelection()} disabled={!selected}>Clear selection</button>
                <button type="button" className="button primary" onClick={onConfigureWorkflow} disabled={!selected}><FolderCheck size={14} /> Configure workflow</button>
              </div>
            </footer>
          </form>
        </div>
      </section>
    </div>
  );
}

interface PromptLibraryPanelProps {
  open: boolean;
  prompts: DeckPrompt[];
  selectedId: string;
  onSelect: (id: string) => void;
  onClose: () => void;
  onAdd: () => void;
  onUpdate: (id: string, updates: Partial<DeckPrompt>) => void;
  onRemove: (id: string) => void;
}

export function PromptLibraryPanel({ open, prompts, selectedId, onSelect, onClose, onAdd, onUpdate, onRemove }: PromptLibraryPanelProps) {
  const [removeArmed, setRemoveArmed] = useState(false);
  const prompt = prompts.find((candidate) => candidate.id === selectedId) ?? prompts[0];
  useEffect(() => setRemoveArmed(false), [selectedId, open]);
  if (!open) return null;

  return (
    <div className="sheet-backdrop" role="presentation" onMouseDown={onClose}>
      <section className="config-sheet projects-sheet" role="dialog" aria-modal="true" aria-labelledby="prompts-title" data-testid="prompt-library-panel" onMouseDown={(event) => event.stopPropagation()}>
        <header className="sheet-header">
          <div><span className="eyebrow">PROMPT LIBRARY</span><h2 id="prompts-title">Saved prompts</h2><p>Reusable text for the launch task prompt and for messaging a running agent.</p></div>
          <button className="icon-button" aria-label="Close prompt library" onClick={onClose}><X size={18} /></button>
        </header>
        <div className="local-only-notice project-notice"><BookMarked size={15} /><span><strong>Local library</strong> — prompts are stored on this Mac only. Nothing is written to the project's <code>.dot-agent-deck.toml</code>.</span></div>

        <div className="projects-layout">
          <aside className="project-library">
            <button className="add-project" onClick={onAdd}><Plus size={14} /> Add prompt</button>
            <nav aria-label="Saved prompts">
              {prompts.map((item) => (
                <button key={item.id} className={item.id === prompt?.id ? "is-selected" : ""} onClick={() => onSelect(item.id)}>
                  <span className="project-icon"><BookMarked size={15} /></span>
                  <span><strong>{item.name || "Untitled prompt"}</strong><small>{item.body.trim().split("\n")[0] || "Empty prompt"}</small></span>
                </button>
              ))}
            </nav>
            {!prompts.length && <div className="project-library-empty"><BookMarked size={22} /><span>No saved prompts yet.</span></div>}
          </aside>

          {prompt ? (
            <form className="project-form" onSubmit={(event) => event.preventDefault()}>
              <div className="form-heading">
                <div><span>PROMPT</span><h3>{prompt.name || "Untitled prompt"}</h3></div>
              </div>
              <div className="project-fields">
                <label><span>Name</span><input aria-label="Prompt name" value={prompt.name} onChange={(event) => onUpdate(prompt.id, { name: event.target.value })} placeholder="Fix the failing test" /></label>
                <label><span>Prompt body</span><textarea aria-label="Prompt body" rows={10} value={prompt.body} onChange={(event) => onUpdate(prompt.id, { body: event.target.value })} placeholder="The text to send to the coordinator or insert at launch…" /></label>
                <label><span>Note</span><input aria-label="Prompt note" value={prompt.note ?? ""} onChange={(event) => onUpdate(prompt.id, { note: event.target.value })} placeholder="When you reach for this one" /></label>
              </div>
              <footer className="sheet-footer project-footer">
                <button type="button" className={removeArmed ? "button danger" : "button secondary"} onClick={() => { if (removeArmed) onRemove(prompt.id); else setRemoveArmed(true); }}><Trash2 size={14} /> {removeArmed ? "Confirm remove" : "Remove prompt"}</button>
                <div><span>Auto-saved locally</span></div>
              </footer>
            </form>
          ) : <div className="configuration-empty">Add a prompt to reuse it at launch and in the agent composer.</div>}
        </div>
      </section>
    </div>
  );
}

interface ProfilesPanelProps {
  open: boolean;
  profiles: AgentProfile[];
  onClose: () => void;
  onUpdate: (id: string, updates: Partial<AgentProfile>) => void;
  onReset: () => void;
  onSaved: () => void;
}

export function ProfilesPanel({ open, profiles, onClose, onUpdate, onReset, onSaved }: ProfilesPanelProps) {
  const [selectedId, setSelectedId] = useState(profiles[0]?.id);
  useEffect(() => {
    if (!profiles.some((profile) => profile.id === selectedId)) setSelectedId(profiles[0]?.id);
  }, [profiles, selectedId]);
  if (!open) return null;
  const profile = profiles.find((candidate) => candidate.id === selectedId);
  const commandResolution = profile ? resolveProfileCommand(profile) : undefined;

  return (
    <div className="sheet-backdrop" role="presentation" onMouseDown={onClose}>
      <section
        className="config-sheet profiles-sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby="profiles-title"
        data-testid="agent-profiles-panel"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <header className="sheet-header">
          <div>
            <span className="eyebrow">EXECUTION CONFIGURATION</span>
            <h2 id="profiles-title">Agent profiles</h2>
            <p>Generate a launch command from structured fields or explicitly opt into an unmanaged custom command.</p>
          </div>
          <button className="icon-button" aria-label="Close agent profiles" onClick={onClose}><X size={18} /></button>
        </header>

        <div className="local-only-notice">
          <AlertTriangle size={15} />
          <span><strong>Local draft</strong> — edits persist on this device but are not written to <code>.dot-agent-deck.toml</code> yet.</span>
        </div>

        <div className="profiles-layout">
          <nav className="profile-list" aria-label="Agent profiles">
            {profiles.map((item) => (
              <button key={item.id} className={item.id === selectedId ? "is-active" : ""} onClick={() => setSelectedId(item.id)}>
                <span className={`profile-enabled ${item.enabled ? "is-enabled" : ""}`}><Bot size={15} /></span>
                <span><strong>{item.role}</strong><small>{item.cli} · {item.model}</small></span>
                {!item.savedToProject && <em>LOCAL</em>}
              </button>
            ))}
          </nav>

          {profile ? (
            <form className="profile-form" onSubmit={(event) => { event.preventDefault(); onSaved(); }}>
              <div className="form-heading">
                <div><span>ROLE PROFILE</span><h3>{profile.role}</h3></div>
                <label className="switch-field">
                  <input type="checkbox" checked={profile.enabled} onChange={(event) => onUpdate(profile.id, { enabled: event.target.checked })} />
                  <span aria-hidden="true" />
                  Enabled in loop
                </label>
              </div>

              <div className="form-grid">
                <label>
                  <span>Role name</span>
                  <input value={profile.role} onChange={(event) => onUpdate(profile.id, { role: event.target.value })} />
                </label>
                <label>
                  <span>Provider</span>
                  <select value={profile.provider} onChange={(event) => {
                    const provider = event.target.value as Provider;
                    onUpdate(profile.id, {
                      provider,
                      cli: defaultCliForProvider(provider),
                      commandMode: provider === "Custom" ? "custom" : "generated",
                    });
                  }}>
                    <option>OpenAI</option><option>Anthropic</option><option>OpenCode</option><option>Custom</option>
                  </select>
                </label>
                <label>
                  <span>CLI</span>
                  <input value={profile.cli} spellCheck={false} onChange={(event) => onUpdate(profile.id, { cli: event.target.value })} />
                </label>
                <label>
                  <span>Model</span>
                  <input value={profile.model} spellCheck={false} onChange={(event) => onUpdate(profile.id, { model: event.target.value })} />
                </label>
                <label>
                  <span>Reasoning effort</span>
                  <select value={profile.effort} onChange={(event) => onUpdate(profile.id, { effort: event.target.value as AgentProfile["effort"] })}>
                    <option value="low">Low</option><option value="medium">Medium</option><option value="high">High</option><option value="xhigh">Extra high</option>
                  </select>
                </label>
                <label>
                  <span>Permission mode</span>
                  <select aria-label="Permission mode" value={profile.permissionMode} onChange={(event) => onUpdate(profile.id, { permissionMode: event.target.value as AgentProfile["permissionMode"] })}>
                    {permissionModeOptions(profile.provider).map((option) => (
                      <option key={option.value} value={option.value}>{option.label}</option>
                    ))}
                  </select>
                  <small className="field-hint">
                    {permissionModeOptions(profile.provider).find((option) => option.value === profile.permissionMode)?.detail}
                  </small>
                </label>
                <div className="form-wide command-control">
                  <label className="command-override-toggle">
                    <input
                      type="checkbox"
                      checked={profile.commandMode === "custom"}
                      disabled={profile.provider === "Custom"}
                      onChange={(event) => onUpdate(profile.id, { commandMode: event.target.checked ? "custom" : "generated" })}
                    />
                    <span>Use advanced custom command override</span>
                  </label>
                  {profile.commandMode === "custom" ? (
                    <label>
                      <span>Custom launch command</span>
                      <textarea aria-label="Custom launch command" rows={3} value={profile.customCommand ?? ""} spellCheck={false} onChange={(event) => onUpdate(profile.id, { customCommand: event.target.value })} />
                      <small className="command-warning"><AlertTriangle size={11} /> Runs as an exact shell command and bypasses every provider field above. Permissions may be arbitrary and must be reviewed in this command. Never paste API keys or tokens here.</small>
                    </label>
                  ) : (
                    <label>
                      <span>Generated launch command</span>
                      <textarea aria-label="Generated launch command" rows={3} value={commandResolution?.command ?? ""} spellCheck={false} readOnly />
                      <small>Read-only preview. Provider, CLI, model, effort, and permission changes regenerate this command.</small>
                    </label>
                  )}
                  {commandResolution?.issue && <small className="command-error"><AlertTriangle size={11} /> {commandResolution.issue}</small>}
                  {commandResolution?.note && <small className="command-note">{commandResolution.note}</small>}
                </div>
              </div>

              <div className="profile-summary">
                <SlidersHorizontal size={15} />
                {commandResolution?.source === "custom"
                  ? <span><strong>{profile.role}</strong> will launch the exact custom command. Permissions are unmanaged here and must be encoded and reviewed in that command.</span>
                  : <span><strong>{profile.role}</strong> will launch the {profile.provider} command generated from these fields, running in <strong>{permissionModeLabel(profile.provider, profile.permissionMode)}</strong>.</span>}
              </div>

              <footer className="sheet-footer">
                <button type="button" className="button secondary" onClick={onReset}><RotateCcw size={14} /> Reset defaults</button>
                <div>
                  <span>Auto-saved locally</span>
                  <button type="submit" className="button primary"><Save size={14} /> Confirm draft</button>
                </div>
              </footer>
            </form>
          ) : <div className="configuration-empty">No agent profile selected.</div>}
        </div>
      </section>
    </div>
  );
}

interface WorkflowPanelProps {
  open: boolean;
  profiles: AgentProfile[];
  order: string[];
  mode: RuntimeMode;
  /**
   * The resolved project this launch runs in, or `undefined` for none chosen.
   *
   * PRD #819 M6 replaced a free-typed `defaultCwd` string with this. The
   * ordering is `daemon → project → workflow` and it does not commute: the
   * workflow list comes out of the project's own config, so there is nothing to
   * offer until a project is picked. And the path is no longer typeable here at
   * all — the launch must send the daemon's canonical spelling, because
   * canonicalising a symlinked path changes its basename and an empty
   * orchestration name is derived from that basename (PRD #220).
   */
  project?: DaemonResolvedProject;
  onChooseProject: () => void;
  onClose: () => void;
  onToggle: (id: string) => void;
  onMove: (id: string, direction: -1 | 1) => void;
  onLaunch: (config: WorkflowLaunchConfig) => void;
  platformIssue?: string;
  prompts?: DeckPrompt[];
}

export function WorkflowPanel({ open, profiles, order, mode, project, onChooseProject, onClose, onToggle, onMove, onLaunch, platformIssue, prompts = [] }: WorkflowPanelProps) {
  const orchestrations = project?.orchestrations ?? [];
  const [name, setName] = useState("");
  const [taskPrompt, setTaskPrompt] = useState("");
  // Pre-select the project's default orchestration, or its only one. A
  // selection that no longer names an orchestration this project offers is
  // dropped rather than sent — the config can have changed under it.
  useEffect(() => {
    if (orchestrations.some((orchestration) => orchestration.name === name)) return;
    setName(orchestrations.find((orchestration) => orchestration.default)?.name ?? orchestrations[0]?.name ?? "");
  }, [name, orchestrations]);
  if (!open) return null;
  const cwd = project?.path ?? "";
  const orchestration = orchestrations.find((candidate) => candidate.name === name);
  const ordered = [...order.map((id) => profiles.find((profile) => profile.id === id)).filter((profile): profile is AgentProfile => Boolean(profile)), ...profiles.filter((profile) => !order.includes(profile.id))];
  const enabled = ordered.filter((profile) => profile.enabled);
  const resolved = enabled.map((profile) => ({ profile, resolution: resolveProfileCommand(profile) }));
  const invalidCommands = resolved.filter(({ resolution }) => resolution.issue);
  const roles = resolved.map(({ profile, resolution }) => ({ role: profile.roleId, command: resolution.command, start: profile.roleId === "orchestrator" }));
  // The required set is now the ORCHESTRATION's own role list, straight off the
  // daemon's projection, rather than a sentence naming six roles this app
  // happened to ship defaults for. The daemon refuses a mismatch either way;
  // saying so here is what stops the refusal arriving as a surprise.
  const requiredRoles = orchestration?.roles.map((role) => role.name) ?? [];
  const missingRoles = requiredRoles.filter((role) => !roles.some((candidate) => candidate.role === role));
  const extraRoles = roles.filter((role) => !requiredRoles.includes(role.role)).map((role) => role.role);
  const allRequiredRolesEnabled = Boolean(orchestration) && !missingRoles.length && !extraRoles.length && enabled.some((profile) => profile.roleId === "orchestrator");
  const canLaunch = mode === "live" && !platformIssue && Boolean(project) && name.trim().length > 0 && cwd.startsWith("/") && taskPrompt.trim().length > 0 && allRequiredRolesEnabled && invalidCommands.length === 0;
  const customCommandCount = resolved.filter(({ resolution }) => resolution.source === "custom").length;
  const generatedFullAccessCount = resolved.filter(({ profile, resolution }) => resolution.source === "generated" && profile.permissionMode === "full-access").length;
  return (
    <div className="sheet-backdrop" role="presentation" onMouseDown={onClose}>
      <section className="config-sheet workflow-sheet" role="dialog" aria-modal="true" aria-labelledby="workflow-editor-title" data-testid="workflow-editor" onMouseDown={(event) => event.stopPropagation()}>
        <header className="sheet-header">
          <div><span className="eyebrow">LOOP CONFIGURATION</span><h2 id="workflow-editor-title">Workflow order</h2><p>Shape the role sequence used by the cockpit preview.</p></div>
          <button className="icon-button" aria-label="Close workflow editor" onClick={onClose}><X size={18} /></button>
        </header>
        <div className="local-only-notice"><AlertTriangle size={15} /><span><strong>Local ordering</strong> — role order is a desktop draft. Launch uses these commands but does not rewrite project TOML.</span></div>
        {mode === "live" && (
          <div className="workflow-launch-form">
            {/*
              A SELECT, not a text field: the workflows on offer are the ones
              this project defines, and the daemon is the only party that can
              say what those are.
            */}
            <label><span>Workflow name</span><select aria-label="Workflow name" value={name} onChange={(event) => setName(event.target.value)} disabled={!orchestrations.length}>{orchestrations.length ? orchestrations.map((candidate) => <option key={candidate.name} value={candidate.name}>{candidate.name}{candidate.default ? " (default)" : ""}</option>) : <option value="">No workflow available</option>}</select></label>
            {/*
              READ-ONLY, and the whole point. This used to be a free-text
              directory that went straight into the launch; it is now the
              daemon's canonical spelling of the chosen project, and the only
              way to change it is to choose a different project.
            */}
            <label><span>Project directory (from the daemon)</span><input aria-label="Absolute project directory" value={cwd} readOnly placeholder="Choose a project first" spellCheck={false} data-testid="workflow-project-path" /></label>
            <label className="workflow-task-prompt">
              <span className="task-prompt-label">
                Task prompt
                {prompts.length > 0 && (
                  <select
                    aria-label="Insert saved prompt"
                    value=""
                    onChange={(event) => {
                      const prompt = prompts.find((candidate) => candidate.id === event.target.value);
                      if (prompt) setTaskPrompt(prompt.body);
                    }}
                  >
                    <option value="">Insert saved prompt…</option>
                    {prompts.map((prompt) => <option key={prompt.id} value={prompt.id}>{prompt.name || "Untitled prompt"}</option>)}
                  </select>
                )}
              </span>
              <textarea aria-label="Task prompt" value={taskPrompt} onChange={(event) => setTaskPrompt(event.target.value)} placeholder="Tell the orchestrator what to build, fix, or investigate..." rows={5} />
            </label>
            {!project && <small data-testid="workflow-needs-project"><AlertTriangle size={12} /> No project chosen. <button type="button" className="link-button" onClick={onChooseProject}>Choose one</button> — the daemon offers the projects it can see, and workflows come from the project.</small>}
            {project && !orchestrations.length && <small data-testid="workflow-no-orchestrations"><AlertTriangle size={12} /> The daemon resolved this project but it defines no workflow with roles.</small>}
            {!taskPrompt.trim() && <small><AlertTriangle size={12} /> Add the task you want the coordinator to run.</small>}
            {platformIssue && <small data-testid="workflow-platform-issue"><AlertTriangle size={12} /> {platformIssue}</small>}
            {orchestration && (missingRoles.length > 0 || extraRoles.length > 0) && <small data-testid="workflow-role-mismatch"><AlertTriangle size={12} /> {orchestration.name} defines {requiredRoles.join(", ") || "no roles"}.{missingRoles.length ? ` Enable a profile for: ${missingRoles.join(", ")}.` : ""}{extraRoles.length ? ` Not in this workflow: ${extraRoles.join(", ")}.` : ""}</small>}
            {invalidCommands.length > 0 && <small><AlertTriangle size={12} /> Fix the launch command for: {invalidCommands.map(({ profile }) => profile.role).join(", ")}.</small>}
          </div>
        )}
        <div className="workflow-editor-list">
          {ordered.map((profile, index) => (
            <div className={`workflow-editor-row ${profile.enabled ? "" : "is-disabled"}`} key={profile.id}>
              <GripVertical size={16} aria-hidden="true" />
              <span className="workflow-order">{String(index + 1).padStart(2, "0")}</span>
              <div><strong>{profile.role}{profile.enabled && profile.roleId === "orchestrator" ? <em className="start-role">START</em> : null}{profile.commandMode === "custom" ? <em className="custom-command-badge">CUSTOM CMD</em> : null}</strong><small><code>{profile.roleId}</code> · {profile.commandMode === "custom" ? "exact shell command · permissions unmanaged" : `${profile.cli} · ${profile.model}`}</small></div>
              <label className="compact-check"><input type="checkbox" checked={profile.enabled} onChange={() => onToggle(profile.id)} /><span>{profile.enabled ? <Check size={12} /> : null}</span><em>{profile.enabled ? "Enabled" : "Skipped"}</em></label>
              <div className="order-buttons">
                <button aria-label={`Move ${profile.role} up`} disabled={index === 0} onClick={() => onMove(profile.id, -1)}><ArrowUp size={14} /></button>
                <button aria-label={`Move ${profile.role} down`} disabled={index === ordered.length - 1} onClick={() => onMove(profile.id, 1)}><ArrowDown size={14} /></button>
              </div>
            </div>
          ))}
        </div>
        <footer className="sheet-footer workflow-footer">
          <span>{enabled.length} active roles · {ordered.length - enabled.length} skipped</span>
          {mode === "live" ? <button className="button primary" data-testid="launch-live-loop" disabled={!canLaunch} onClick={() => onLaunch({ name: name.trim(), cwd, taskPrompt: taskPrompt.trim(), roles, rows: 32, cols: 120, customCommandCount, generatedFullAccessCount, configRevision: project?.configRevision })}><Bot size={14} /> Launch live loop</button> : <button className="button primary" onClick={onClose}><Check size={14} /> Use preview</button>}
        </footer>
      </section>
    </div>
  );
}
