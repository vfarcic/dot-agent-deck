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
  RotateCcw,
  Save,
  SlidersHorizontal,
  Trash2,
  X,
} from "lucide-react";
import { defaultCliForProvider, permissionModeLabel, permissionModeOptions, resolveProfileCommand } from "../lib/profileCommands";
import type { AgentProfile, DeckProject, DeckPrompt, Provider, RuntimeMode, WorkflowLaunchConfig } from "../types";

interface ProjectsPanelProps {
  open: boolean;
  projects: DeckProject[];
  activeId: string;
  selectedId: string;
  onSelect: (id: string) => void;
  onClose: () => void;
  onAdd: () => void;
  onUpdate: (id: string, updates: Partial<DeckProject>) => void;
  onRemove: (id: string) => void;
  onActivate: (id: string) => void;
  onConfigureWorkflow: () => void;
}

export function ProjectsPanel({ open, projects, activeId, selectedId, onSelect, onClose, onAdd, onUpdate, onRemove, onActivate, onConfigureWorkflow }: ProjectsPanelProps) {
  const [removeArmed, setRemoveArmed] = useState(false);
  const project = projects.find((candidate) => candidate.id === selectedId) ?? projects[0];
  useEffect(() => setRemoveArmed(false), [selectedId, open]);
  if (!open) return null;
  const validDirectory = Boolean(project?.cwd.startsWith("/"));
  const validWorkflow = Boolean(project?.workflowName.trim());

  return (
    <div className="sheet-backdrop" role="presentation" onMouseDown={onClose}>
      <section className="config-sheet projects-sheet" role="dialog" aria-modal="true" aria-labelledby="projects-title" data-testid="projects-panel" onMouseDown={(event) => event.stopPropagation()}>
        <header className="sheet-header">
          <div><span className="eyebrow">PROJECT LIBRARY</span><h2 id="projects-title">Projects</h2><p>Save the repositories and workflow names you launch most often.</p></div>
          <button className="icon-button" aria-label="Close projects" onClick={onClose}><X size={18} /></button>
        </header>
        <div className="local-only-notice project-notice"><FolderCheck size={15} /><span><strong>Local library</strong> — project entries are stored on this Mac. Agent Deck never moves or deletes the repository folder.</span></div>

        <div className="projects-layout">
          <aside className="project-library">
            <button className="add-project" onClick={onAdd}><Plus size={14} /> Add project</button>
            <nav aria-label="Saved projects">
              {projects.map((item) => (
                <button key={item.id} className={item.id === project?.id ? "is-selected" : ""} onClick={() => onSelect(item.id)}>
                  <span className={item.id === activeId ? "project-icon is-active" : "project-icon"}><FolderGit2 size={15} /></span>
                  <span><strong>{item.name || "Untitled project"}</strong><small>{item.cwd || "Directory not set"}</small></span>
                  {item.id === activeId && <em>ACTIVE</em>}
                </button>
              ))}
            </nav>
            {!projects.length && <div className="project-library-empty"><FolderGit2 size={22} /><span>No saved projects yet.</span></div>}
          </aside>

          {project ? (
            <form className="project-form" onSubmit={(event) => { event.preventDefault(); if (validDirectory && validWorkflow) onActivate(project.id); }}>
              <div className="form-heading">
                <div><span>PROJECT SETTINGS</span><h3>{project.name || "Untitled project"}</h3></div>
                {project.id === activeId ? <span className="active-project-badge"><Check size={11} /> Active project</span> : null}
              </div>
              <div className="project-fields">
                <label><span>Display name</span><input aria-label="Project display name" value={project.name} onChange={(event) => onUpdate(project.id, { name: event.target.value })} placeholder="My coding project" /></label>
                <label><span>Absolute project directory</span><input aria-label="Project directory" value={project.cwd} onChange={(event) => onUpdate(project.id, { cwd: event.target.value })} placeholder="/Users/you/dev/project" spellCheck={false} /><small>The folder must contain the workflow's <code>.dot-agent-deck.toml</code>.</small></label>
                {project.cwd && !validDirectory && <small className="project-field-error"><AlertTriangle size={12} /> Use an absolute directory beginning with <code>/</code>.</small>}
                <label><span>Workflow name</span><input aria-label="Project workflow name" value={project.workflowName} onChange={(event) => onUpdate(project.id, { workflowName: event.target.value })} placeholder="dot-agent-deck" spellCheck={false} /><small>This must match an orchestration defined in the project's TOML file.</small></label>
                <label><span>Notes</span><textarea aria-label="Project notes" rows={5} value={project.notes} onChange={(event) => onUpdate(project.id, { notes: event.target.value })} placeholder="Stack, constraints, setup reminders, or the kind of work you use this loop for…" /></label>
              </div>
              <div className="project-readiness">
                <FolderCheck size={16} />
                <span><strong>{validDirectory && validWorkflow ? "Ready to select" : "Needs configuration"}</strong>{validDirectory && validWorkflow ? "The workflow launch screen will use this directory and workflow name." : "Add an absolute directory and workflow name before selecting this project."}</span>
              </div>
              <footer className="sheet-footer project-footer">
                <button type="button" className={removeArmed ? "button danger" : "button secondary"} onClick={() => { if (removeArmed) onRemove(project.id); else setRemoveArmed(true); }}><Trash2 size={14} /> {removeArmed ? "Confirm remove" : "Remove entry"}</button>
                <div>
                  {project.id === activeId ? <button type="button" className="button secondary" onClick={onConfigureWorkflow}>Configure workflow</button> : null}
                  <button type="submit" className="button primary" disabled={!validDirectory || !validWorkflow}><FolderCheck size={14} /> {project.id === activeId ? "Active project" : "Use this project"}</button>
                </div>
              </footer>
            </form>
          ) : <div className="configuration-empty">Add a project to configure its launch directory.</div>}
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
  defaultName: string;
  defaultCwd: string;
  onClose: () => void;
  onToggle: (id: string) => void;
  onMove: (id: string, direction: -1 | 1) => void;
  onLaunch: (config: WorkflowLaunchConfig) => void;
  platformIssue?: string;
  prompts?: DeckPrompt[];
}

export function WorkflowPanel({ open, profiles, order, mode, defaultName, defaultCwd, onClose, onToggle, onMove, onLaunch, platformIssue, prompts = [] }: WorkflowPanelProps) {
  const [name, setName] = useState(defaultName);
  const [cwd, setCwd] = useState(defaultCwd.startsWith("/") ? defaultCwd : "");
  const [taskPrompt, setTaskPrompt] = useState("");
  useEffect(() => {
    if (defaultCwd.startsWith("/") && (!cwd || cwd === "/dev/active/dot-agent-deck-gui")) setCwd(defaultCwd);
  }, [cwd, defaultCwd]);
  if (!open) return null;
  const ordered = [...order.map((id) => profiles.find((profile) => profile.id === id)).filter((profile): profile is AgentProfile => Boolean(profile)), ...profiles.filter((profile) => !order.includes(profile.id))];
  const enabled = ordered.filter((profile) => profile.enabled);
  const resolved = enabled.map((profile) => ({ profile, resolution: resolveProfileCommand(profile) }));
  const invalidCommands = resolved.filter(({ resolution }) => resolution.issue);
  const allRequiredRolesEnabled = enabled.length === ordered.length && enabled.some((profile) => profile.roleId === "orchestrator");
  const canLaunch = mode === "live" && !platformIssue && name.trim().length > 0 && cwd.startsWith("/") && taskPrompt.trim().length > 0 && allRequiredRolesEnabled && invalidCommands.length === 0;
  const roles = resolved.map(({ profile, resolution }) => ({ role: profile.roleId, command: resolution.command, start: profile.roleId === "orchestrator" }));
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
            <label><span>Workflow name</span><input value={name} onChange={(event) => setName(event.target.value)} placeholder="coding-loop" /></label>
            <label><span>Absolute project directory</span><input value={cwd} onChange={(event) => setCwd(event.target.value)} placeholder="/Users/you/dev/project" spellCheck={false} /></label>
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
            {cwd && !cwd.startsWith("/") && <small><AlertTriangle size={12} /> Use an absolute directory path.</small>}
            {!taskPrompt.trim() && <small><AlertTriangle size={12} /> Add the task you want the coordinator to run.</small>}
            {platformIssue && <small data-testid="workflow-platform-issue"><AlertTriangle size={12} /> {platformIssue}</small>}
            {!allRequiredRolesEnabled && <small><AlertTriangle size={12} /> This project config requires orchestrator, coder, reviewer, auditor, tester, and release for a live launch.</small>}
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
          {mode === "live" ? <button className="button primary" data-testid="launch-live-loop" disabled={!canLaunch} onClick={() => onLaunch({ name: name.trim(), cwd, taskPrompt: taskPrompt.trim(), roles, rows: 32, cols: 120, customCommandCount, generatedFullAccessCount })}><Bot size={14} /> Launch live loop</button> : <button className="button primary" onClick={onClose}><Check size={14} /> Use preview</button>}
        </footer>
      </section>
    </div>
  );
}
