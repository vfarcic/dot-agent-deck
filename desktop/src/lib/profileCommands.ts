import type { AgentProfile, PermissionMode, ProfileCommandMode, Provider } from "../types";

export const PROFILE_COMMAND_MAX_BYTES = 64 * 1024;

const PROVIDERS: Provider[] = ["OpenAI", "Anthropic", "OpenCode", "Custom"];
const EFFORTS: AgentProfile["effort"][] = ["low", "medium", "high", "xhigh"];
const PERMISSION_MODES: PermissionMode[] = ["default", "read-only", "workspace-write", "full-access"];
const COMMAND_MODES: ProfileCommandMode[] = ["generated", "custom"];

export interface ProfileCommandResolution {
  command: string;
  source: ProfileCommandMode;
  issue?: string;
  note?: string;
}

/**
 * Canonical permission modes are provider-neutral, but each CLI names them
 * differently. These maps are the single source of truth for both the
 * generated command and the labels shown in the profile editor, so the
 * selector can never drift from the flag it actually produces.
 */
const CLAUDE_PERMISSION_ARGUMENT: Record<PermissionMode, string> = {
  default: "default",
  "read-only": "plan",
  "workspace-write": "acceptEdits",
  "full-access": "bypassPermissions",
};

const CODEX_SANDBOX_ARGUMENT: Record<PermissionMode, string> = {
  default: "default",
  "read-only": "read-only",
  "workspace-write": "workspace-write",
  "full-access": "danger-full-access",
};

export interface PermissionModeOption {
  value: PermissionMode;
  /** The name the selected CLI actually uses for this mode. */
  label: string;
  /** The flag this mode produces, so "full access" is never ambiguous. */
  detail: string;
}

/**
 * Permission choices labelled in the vocabulary of the selected provider.
 * Claude Code calls these plan / acceptEdits / bypassPermissions; Codex calls
 * them sandboxes. Showing one CLI's words for another's behaviour is how
 * "Workspace write" ends up silently meaning "accept edits".
 */
export function permissionModeOptions(provider: Provider): PermissionModeOption[] {
  if (provider === "Anthropic") {
    return [
      { value: "default", label: "Ask each time (default)", detail: `--permission-mode ${CLAUDE_PERMISSION_ARGUMENT.default}` },
      { value: "read-only", label: "Plan — read-only", detail: `--permission-mode ${CLAUDE_PERMISSION_ARGUMENT["read-only"]}` },
      { value: "workspace-write", label: "Accept edits", detail: `--permission-mode ${CLAUDE_PERMISSION_ARGUMENT["workspace-write"]}` },
      { value: "full-access", label: "Bypass permissions", detail: `--permission-mode ${CLAUDE_PERMISSION_ARGUMENT["full-access"]}` },
    ];
  }
  if (provider === "OpenAI") {
    return [
      { value: "default", label: "Ask on request (default)", detail: "--ask-for-approval on-request" },
      { value: "read-only", label: "Read only", detail: `--sandbox ${CODEX_SANDBOX_ARGUMENT["read-only"]}` },
      { value: "workspace-write", label: "Workspace write", detail: `--sandbox ${CODEX_SANDBOX_ARGUMENT["workspace-write"]}` },
      { value: "full-access", label: "Full access — no sandbox", detail: `--sandbox ${CODEX_SANDBOX_ARGUMENT["full-access"]}` },
    ];
  }
  if (provider === "OpenCode") {
    return [
      { value: "default", label: "CLI default", detail: "no permission override" },
      { value: "read-only", label: "Read only", detail: "OPENCODE_PERMISSION deny edit/bash" },
      { value: "workspace-write", label: "Edits allowed, bash asks", detail: "OPENCODE_PERMISSION allow edit" },
      { value: "full-access", label: "Allow everything", detail: "OPENCODE_PERMISSION allow *" },
    ];
  }
  return [
    { value: "default", label: "CLI default", detail: "encoded by the custom command" },
    { value: "read-only", label: "Read only", detail: "encoded by the custom command" },
    { value: "workspace-write", label: "Workspace write", detail: "encoded by the custom command" },
    { value: "full-access", label: "Full access", detail: "encoded by the custom command" },
  ];
}

/** The provider-specific name for one mode, for prose outside the selector. */
export function permissionModeLabel(provider: Provider, mode: PermissionMode): string {
  const match = permissionModeOptions(provider).find((option) => option.value === mode);
  return match ? match.label : mode;
}

export function defaultCliForProvider(provider: Provider): string {
  if (provider === "OpenAI") return "codex";
  if (provider === "Anthropic") return "claude";
  if (provider === "OpenCode") return "opencode";
  return "";
}

/**
 * Quote one untrusted value as exactly one POSIX-shell word.
 *
 * PR #416 review Part C finding 1: `=` is allowed mid-word but not LEADING.
 * The daemon hands the command to `$SHELL -c`, which on macOS is zsh, and
 * zsh's default-on EQUALS expansion rewrites a bare leading-`=` word into a
 * PATH lookup — `--model =codex` becomes a path or aborts the launch with
 * "zsh: codex not found". Expansion, not evaluation; quoting removes it.
 */
export function quoteShellWord(value: string): string {
  if (/^[A-Za-z0-9_@%+:,./-][A-Za-z0-9_@%+=:,./-]*$/.test(value)) return value;
  return `'${value.replace(/'/g, `'"'"'`)}'`;
}

function quoteShellExecutable(value: string): string {
  // Always quote argv[0]. Besides metacharacters, this prevents a CLI value
  // such as FOO=bar from being parsed as a shell assignment or keyword.
  return `'${value.replace(/'/g, `'"'"'`)}'`;
}

function utf8ByteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function normalizedEffort(value: AgentProfile["effort"]): AgentProfile["effort"] {
  return EFFORTS.includes(value) ? value : "medium";
}

function normalizedPermission(value: PermissionMode): PermissionMode {
  return PERMISSION_MODES.includes(value) ? value : "default";
}

function generatedCommand(profile: AgentProfile): ProfileCommandResolution {
  const cli = profile.cli.trim();
  const model = profile.model.trim();
  if (!cli) return { command: "", source: "generated", issue: "CLI executable is required." };
  if (!model) return { command: "", source: "generated", issue: "Model is required." };
  if (cli.includes("\0") || model.includes("\0")) {
    return { command: "", source: "generated", issue: "CLI executable and model must not contain NUL." };
  }
  if (profile.provider === "Custom") {
    return { command: "", source: "generated", issue: "Custom providers require an explicit command override." };
  }

  const executable = quoteShellExecutable(cli);
  const modelArgument = quoteShellWord(model);
  const effort = normalizedEffort(profile.effort);
  const permissionMode = normalizedPermission(profile.permissionMode);
  let command: string;
  let note: string | undefined;

  if (profile.provider === "OpenAI") {
    const sandbox = CODEX_SANDBOX_ARGUMENT[permissionMode];
    const permissionArguments = sandbox === "default"
      ? "--ask-for-approval on-request"
      : `--sandbox ${sandbox} --ask-for-approval on-request`;
    command = `${executable} --model ${modelArgument} ${permissionArguments} -c ${quoteShellWord(`model_reasoning_effort=${effort}`)}`;
  } else if (profile.provider === "Anthropic") {
    command = `${executable} --model ${modelArgument} --effort ${effort} --permission-mode ${CLAUDE_PERMISSION_ARGUMENT[permissionMode]}`;
  } else {
    const modelWithoutVariant = model.split("#", 1)[0];
    if (!modelWithoutVariant) {
      return { command: "", source: "generated", issue: "OpenCode model must include a model name before its variant." };
    }
    const permission: Record<PermissionMode, string | undefined> = {
      default: undefined,
      "read-only": JSON.stringify({ edit: "deny", bash: "deny", external_directory: "deny" }),
      "workspace-write": JSON.stringify({ edit: "allow", bash: "ask", external_directory: "deny" }),
      "full-access": JSON.stringify({ "*": "allow" }),
    };
    const permissionPrefix = permission[permissionMode]
      ? `OPENCODE_PERMISSION=${quoteShellWord(permission[permissionMode])} `
      : "";
    command = `${permissionPrefix}${executable} --model ${quoteShellWord(`${modelWithoutVariant}#${effort}`)}`;
    note = "OpenCode receives effort as a model variant, replacing any variant typed in Model; that variant must exist for the selected model.";
  }

  if (utf8ByteLength(command) > PROFILE_COMMAND_MAX_BYTES) {
    return { command, source: "generated", issue: `Generated command exceeds ${PROFILE_COMMAND_MAX_BYTES} bytes.` };
  }
  return { command, source: "generated", note };
}

export function resolveProfileCommand(profile: AgentProfile): ProfileCommandResolution {
  if (profile.commandMode !== "custom") return generatedCommand(profile);
  const command = profile.customCommand ?? profile.command;
  if (!command.trim()) return { command, source: "custom", issue: "Custom command must not be blank." };
  if (command.includes("\0")) return { command, source: "custom", issue: "Custom command must not contain NUL." };
  if (utf8ByteLength(command) > PROFILE_COMMAND_MAX_BYTES) {
    return { command, source: "custom", issue: `Custom command exceeds ${PROFILE_COMMAND_MAX_BYTES} bytes.` };
  }
  return {
    command,
    source: "custom",
    note: "This exact shell command bypasses the provider, CLI, model, effort, and permission fields. Its permissions are unmanaged here and encoded by the command itself.",
  };
}

/** Upgrade old local-storage records and keep the persisted command in sync. */
export function normalizeAgentProfile(profile: AgentProfile): AgentProfile {
  const provider = PROVIDERS.includes(profile.provider) ? profile.provider : "Custom";
  const effort = EFFORTS.includes(profile.effort) ? profile.effort : "medium";
  const permissionMode = PERMISSION_MODES.includes(profile.permissionMode) ? profile.permissionMode : "default";
  const storedMode = COMMAND_MODES.includes(profile.commandMode) ? profile.commandMode : undefined;
  const commandMode = storedMode ?? (provider === "Custom" ? "custom" : "generated");
  const customCommand = commandMode === "custom"
    ? (typeof profile.customCommand === "string" ? profile.customCommand : profile.command)
    : profile.customCommand;
  const normalized: AgentProfile = {
    ...profile,
    provider,
    cli: typeof profile.cli === "string" ? profile.cli : defaultCliForProvider(provider),
    model: typeof profile.model === "string" ? profile.model : "",
    effort,
    permissionMode,
    commandMode,
    customCommand,
  };
  return { ...normalized, command: resolveProfileCommand(normalized).command };
}
