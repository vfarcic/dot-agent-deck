import { useCallback, useEffect, useState } from "react";
import { normalizeAgentProfile } from "../lib/profileCommands";
import type { AgentProfile } from "../types";
import { modeScopedKey } from "../lib/bridge";

const STORAGE_KEY = modeScopedKey("dot-agent-deck.desktop.agent-profiles.v1");

function readStoredProfiles(): AgentProfile[] | undefined {
  try {
    const value = window.localStorage.getItem(STORAGE_KEY);
    if (!value) return undefined;
    const parsed: unknown = JSON.parse(value);
    if (!Array.isArray(parsed)) return undefined;
    const valid = parsed.filter((profile): profile is AgentProfile => (
      typeof profile === "object" && profile !== null &&
      typeof (profile as AgentProfile).id === "string" &&
      typeof (profile as AgentProfile).role === "string" &&
      typeof (profile as AgentProfile).command === "string" &&
      // PR #416 review Part C finding 3: roleId reaches StartWorkflow as the
      // role name; a persisted non-string yields an opaque serde error with no
      // correctable UI field. Absent is fine (derived below); wrong-typed is not.
      ["string", "undefined"].includes(typeof (profile as AgentProfile).roleId)
    ));
    return valid.length ? valid.map((profile) => normalizeAgentProfile({
      ...profile,
      roleId: profile.roleId || profile.id.replace(/^profile-/, ""),
      savedToProject: false,
    })) : undefined;
  } catch {
    return undefined;
  }
}

export function useAgentProfiles(defaultProfiles: AgentProfile[]) {
  const [profiles, setProfiles] = useState<AgentProfile[]>(() => readStoredProfiles() ?? defaultProfiles.map(normalizeAgentProfile));

  useEffect(() => {
    if (profiles.length === 0 && defaultProfiles.length > 0) setProfiles(defaultProfiles.map(normalizeAgentProfile));
  }, [defaultProfiles, profiles.length]);

  const updateProfile = useCallback((id: string, updates: Partial<AgentProfile>) => {
    setProfiles((current) => current.map((profile) => {
      if (profile.id !== id) return profile;
      const switchingProvider = updates.provider !== undefined && updates.provider !== profile.provider;
      const enteringCustomMode = updates.commandMode === "custom" && profile.commandMode !== "custom";
      const merged: AgentProfile = { ...profile, ...updates, savedToProject: false };
      if (switchingProvider && updates.commandMode === undefined) {
        merged.commandMode = updates.provider === "Custom" ? "custom" : "generated";
      }
      if ((enteringCustomMode || (switchingProvider && updates.provider === "Custom")) && updates.customCommand === undefined) {
        merged.customCommand = profile.command;
      }
      if (updates.command !== undefined && merged.commandMode === "custom" && updates.customCommand === undefined) {
        merged.customCommand = updates.command;
      }
      return normalizeAgentProfile(merged);
    }));
  }, []);

  useEffect(() => {
    if (!profiles.length) return;
    try {
      window.localStorage.setItem(STORAGE_KEY, JSON.stringify(profiles));
    } catch {
      // A read-only/private storage context should not make the control deck unusable.
    }
  }, [profiles]);

  const resetProfiles = useCallback(() => {
    try {
      window.localStorage.removeItem(STORAGE_KEY);
    } catch {
      // Keep the in-memory reset even when storage is unavailable.
    }
    setProfiles(defaultProfiles.map(normalizeAgentProfile));
  }, [defaultProfiles]);

  return { profiles, updateProfile, resetProfiles };
}
