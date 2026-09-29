/**
 * "Select a deck" — what a screen that cannot merge across decks shows under
 * **All Decks** (#1083).
 *
 * # The rule it states
 *
 * A screen that cannot merge across decks says so; it never silently picks
 * one. The overview merges because its rows are read-only. The deck screen owns
 * a terminal per tile, and the Projects and Workflows sheets ask one deck and
 * launch there — so under All Decks each of them shows this instead of the local
 * deck, which is what "the selected deck" resolves to underneath and what they
 * used to render without a word.
 *
 * # Same register as the overview's first-run note
 *
 * Not an error: no alert role, no alert styling, the overview's own
 * `overview-note` chrome. Its title says what to do next rather than what is
 * missing, and the Deck selector it points at stays fully live — choosing there
 * changes the global selection exactly as it does anywhere else, and this note
 * goes away because the selection did.
 */
import type { ReactNode } from "react";
import { Server } from "lucide-react";

export function SelectDeckNote({ testId, title, children, className }: { testId: string; title: string; children: ReactNode; className?: string }) {
  return (
    <div className={className ? `overview-note ${className}` : "overview-note"} data-testid={testId}>
      <Server size={26} />
      <h3>{title}</h3>
      {children}
    </div>
  );
}
