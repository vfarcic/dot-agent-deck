/**
 * PRD #1666 part 3, round 5-wake (Q6): the header's deck-wide attention item
 * (`deckAttentionText`), on the deck and the overview alike. It is
 * informational and offers no action, so it has no voice entry (CLAUDE.md
 * rule 23). It is derived from the snapshot each render, so it survives a
 * reconnect and a change of selection or screen, and nothing the user clicks
 * dismisses it.
 */
export function DeckAttention({ text }: { text: string | undefined }) {
  if (text === undefined) return null;
  return <div className="deck-attention" role="status" data-testid="deck-attention" title={text}>{text}</div>;
}
