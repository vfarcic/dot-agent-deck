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
 * It holds a row per tenant and nothing speculative: #741's daemon endpoints
 * and #802's voice backends each add their own when they land. Pre-creating
 * empty sections for them would be this container growing opinions about its
 * contents, which is the specific failure PRD #803 exists to prevent — a
 * container with opinions blocks the dependents it was built for.
 *
 * Below two entries the sheet drops the section column and renders the one
 * panel full width. **PRD #744's Zoom row is what brought the column back**,
 * and it arrived with no layout work on this side, which is the claim the
 * collapse was built to make good. Both directions stay pinned with stub
 * sections in `SettingsSheet.test.tsx` rather than relying on the live registry
 * having two rows, so a later PRD removing one does not silently delete the
 * coverage.
 */
import { Palette, ZoomIn } from "lucide-react";
import { AppearancePanel } from "../components/AppearancePanel";
import { ZoomPanel } from "../components/ZoomPanel";
import type { SettingsSection } from "./settingsContract";

/** The registry. Adding a section is one row here and one component. */
export const SETTINGS_SECTIONS: SettingsSection[] = [
  { id: "appearance", label: "Appearance", icon: Palette, component: AppearancePanel },
  { id: "zoom", label: "Zoom", icon: ZoomIn, component: ZoomPanel },
];
