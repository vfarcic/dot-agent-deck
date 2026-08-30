import { useEffect, useMemo, useState } from "react";
import type { DeckProject } from "../types";
import { modeScopedKey } from "../lib/bridge";

const PROJECTS_STORAGE_KEY = modeScopedKey("dot-agent-deck.desktop.projects.v1");

interface StoredProjects {
  activeId?: string;
  projects?: DeckProject[];
}

function validProject(value: unknown): value is DeckProject {
  if (!value || typeof value !== "object") return false;
  const project = value as Partial<DeckProject>;
  return typeof project.id === "string"
    && typeof project.name === "string"
    && typeof project.cwd === "string"
    && typeof project.workflowName === "string"
    && typeof project.notes === "string";
}

function mintProjectId(): string {
  return typeof crypto.randomUUID === "function"
    ? crypto.randomUUID()
    : `project-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

export function useProjects(defaultCwd: string, defaultName: string) {
  const [projects, setProjects] = useState<DeckProject[]>(() => {
    try {
      const stored = JSON.parse(window.localStorage.getItem(PROJECTS_STORAGE_KEY) ?? "null") as StoredProjects | null;
      return stored?.projects?.filter(validProject) ?? [];
    } catch {
      return [];
    }
  });
  const [activeId, setActiveId] = useState<string>(() => {
    try {
      const stored = JSON.parse(window.localStorage.getItem(PROJECTS_STORAGE_KEY) ?? "null") as StoredProjects | null;
      return typeof stored?.activeId === "string" ? stored.activeId : "";
    } catch {
      return "";
    }
  });

  useEffect(() => {
    if (projects.length || !defaultCwd.startsWith("/")) return;
    const seeded: DeckProject = {
      id: mintProjectId(),
      name: defaultName && defaultName !== "No active project" ? defaultName : defaultCwd.split("/").filter(Boolean).at(-1) ?? "Project",
      cwd: defaultCwd,
      workflowName: "dot-agent-deck",
      notes: "",
    };
    setProjects([seeded]);
    setActiveId(seeded.id);
  }, [defaultCwd, defaultName, projects.length]);

  useEffect(() => {
    if (activeId && !projects.some((project) => project.id === activeId)) {
      setActiveId(projects[0]?.id ?? "");
    }
  }, [activeId, projects]);

  useEffect(() => {
    try {
      window.localStorage.setItem(PROJECTS_STORAGE_KEY, JSON.stringify({ projects, activeId } satisfies StoredProjects));
    } catch {
      // Project switching remains usable for the current session when storage is unavailable.
    }
  }, [activeId, projects]);

  const activeProject = useMemo(() => projects.find((project) => project.id === activeId), [activeId, projects]);

  const addProject = () => {
    const project: DeckProject = {
      id: mintProjectId(),
      name: "New project",
      cwd: "",
      workflowName: "dot-agent-deck",
      notes: "",
    };
    setProjects((current) => [...current, project]);
    return project.id;
  };

  const updateProject = (id: string, updates: Partial<DeckProject>) => {
    setProjects((current) => current.map((project) => project.id === id ? { ...project, ...updates, id: project.id } : project));
  };

  const removeProject = (id: string) => {
    setProjects((current) => current.filter((project) => project.id !== id));
  };

  return { projects, activeId, activeProject, setActiveId, addProject, updateProject, removeProject };
}
