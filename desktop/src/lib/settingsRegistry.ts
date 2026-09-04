/**
 * The settings section registry (PRD #803).
 *
 * This is the one file a feature edits to acquire a section, and it lives here
 * rather than inside `components/SettingsSheet.tsx` so that the contract in
 * {@link ./settingsContract} is literally true: adding a section touches the
 * registry and your own component, and nothing else. While the registry sat
 * inside the sheet, every dependent had to edit a #803-owned *rendering*
 * component to register — and #741 and #802 would have collided on adjacent
 * lines of the same array.
 *
 * It has exactly one entry, and that is the scope rather than an unfinished
 * state: #741's daemon endpoints and #802's voice backends each add their own
 * when they land. Pre-creating empty sections for them would be this container
 * growing opinions about its contents, which is the specific failure PRD #803
 * exists to prevent — a container with opinions blocks the dependents it was
 * built for.
 *
 * Below two entries the sheet drops the section column and renders the one
 * panel full width. The registry still drives that, so the column comes back on
 * its own the moment a second row lands here, with no layout work on the side
 * of whoever adds it.
 */
import { Palette } from "lucide-react";
import { AppearancePanel } from "../components/AppearancePanel";
import type { SettingsSection } from "./settingsContract";

/** The registry. Adding a section is one row here and one component. */
export const SETTINGS_SECTIONS: SettingsSection[] = [
  { id: "appearance", label: "Appearance", icon: Palette, component: AppearancePanel },
];
