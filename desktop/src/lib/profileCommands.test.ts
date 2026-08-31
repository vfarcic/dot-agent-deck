import { describe, expect, it } from "vitest";
import { DEFAULT_PROFILES } from "../data/fixture";
import type { AgentProfile } from "../types";
import {
  normalizeAgentProfile,
  PROFILE_COMMAND_MAX_BYTES,
  quoteShellWord,
  resolveProfileCommand,
} from "./profileCommands";

function profile(updates: Partial<AgentProfile> = {}): AgentProfile {
  return normalizeAgentProfile({ ...DEFAULT_PROFILES[1], ...updates });
}

describe("profile launch commands", () => {
  it("derives an OpenAI command from every launch field and maps full access without bypassing approval", () => {
    const result = resolveProfileCommand(profile({
      cli: "/Applications/Codex Nightly/bin/codex",
      model: "gpt-5.6-sol-fast",
      effort: "xhigh",
      permissionMode: "full-access",
    }));

    expect(result.source).toBe("generated");
    expect(result.issue).toBeUndefined();
    expect(result.command).toBe("'/Applications/Codex Nightly/bin/codex' --model gpt-5.6-sol-fast --sandbox danger-full-access --ask-for-approval on-request -c model_reasoning_effort=xhigh");
    expect(result.command).not.toContain("dangerously-bypass");
  });

  it("keeps shell metacharacters in CLI and model values inside one quoted word", () => {
    const result = resolveProfileCommand(profile({
      cli: "codex; touch /tmp/cli-injected",
      model: "gpt'$(touch /tmp/model-injected)",
    }));
    const assignmentLikeCli = resolveProfileCommand(profile({ cli: "AGENT=codex" }));

    expect(quoteShellWord("gpt'$(touch /tmp/model-injected)")).toBe("'gpt'\"'\"'$(touch /tmp/model-injected)'");
    expect(result.command).toBe("'codex; touch /tmp/cli-injected' --model 'gpt'\"'\"'$(touch /tmp/model-injected)' --sandbox workspace-write --ask-for-approval on-request -c model_reasoning_effort=medium");
    expect(assignmentLikeCli.command).toMatch(/^'AGENT=codex' --model /);
  });

  it("maps Anthropic permissions and OpenCode effort/permissions to provider-native launch inputs", () => {
    const anthropic = resolveProfileCommand(profile({
      provider: "Anthropic",
      cli: "claude",
      model: "opus",
      effort: "high",
      permissionMode: "read-only",
    }));
    const openCode = resolveProfileCommand(profile({
      provider: "OpenCode",
      cli: "opencode",
      model: "openai/gpt-5.2#low",
      effort: "high",
      permissionMode: "read-only",
    }));

    expect(anthropic.command).toBe("'claude' --model opus --effort high --permission-mode plan");
    expect(openCode.command).toBe("OPENCODE_PERMISSION='{\"edit\":\"deny\",\"bash\":\"deny\",\"external_directory\":\"deny\"}' 'opencode' --model 'openai/gpt-5.2#high'");
    expect(openCode.note).toMatch(/model variant/);
  });

  it("migrates a stale legacy command to generated mode and preserves explicit custom overrides exactly", () => {
    const legacy = normalizeAgentProfile({
      ...DEFAULT_PROFILES[1],
      commandMode: undefined as unknown as AgentProfile["commandMode"],
      model: "new-model",
      command: "stale raw command",
    });
    const custom = normalizeAgentProfile({
      ...legacy,
      commandMode: "custom",
      customCommand: "devbox run agent -- --flag='literal'",
    });

    expect(legacy.command).toContain("--model new-model");
    expect(legacy.command).not.toContain("stale raw command");
    expect(resolveProfileCommand(custom)).toMatchObject({
      source: "custom",
      command: "devbox run agent -- --flag='literal'",
      note: expect.stringMatching(/bypasses/),
    });
  });

  it("enforces the backend command limit in UTF-8 bytes", () => {
    const custom = profile({
      commandMode: "custom",
      customCommand: "é".repeat(PROFILE_COMMAND_MAX_BYTES / 2 + 1),
    });

    expect(resolveProfileCommand(custom).issue).toMatch(/exceeds/);
  });
});
