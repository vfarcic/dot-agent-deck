/**
 * PR #1451 round 3, change 3 — the number an item shows while voice is on,
 * before its name: "3." The item's accessible name starts with it too, which
 * an item named from its content gets from this text and an item with an
 * `aria-label` of its own states there (and passes `hidden`, so the number is
 * not read twice). Renders nothing for an item with no number.
 */
export function VoiceNumber({ number, hidden }: { number: number | undefined; hidden?: boolean }) {
  if (number === undefined) return null;
  return (
    <>
      <span className="voice-number" aria-hidden={hidden || undefined}>{`${number}.`}</span>
      {" "}
    </>
  );
}
