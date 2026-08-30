import { describe, expect, it } from "vitest";
import { applyTerminalChunk } from "./terminalBuffer";

const bytes = (value: string) => new TextEncoder().encode(value);
const text = (value: Uint8Array) => new TextDecoder().decode(value);

describe("applyTerminalChunk", () => {
  it("replaces an earlier generation with the authoritative reattach replay", () => {
    const previous = { data: bytes("prompt\r\nold\r\n"), baseOffset: 0, generation: 7 };
    const replayed = applyTerminalChunk(previous, {
      agentId: "agent-1",
      data: bytes("prompt\r\nold\r\nnew\r\n"),
      stream: "output",
      operation: "replace",
      generation: 8,
    });

    expect(text(replayed.data)).toBe("prompt\r\nold\r\nnew\r\n");
    expect(text(replayed.data)).not.toBe("prompt\r\nold\r\nprompt\r\nold\r\nnew\r\n");
    expect(replayed.generation).toBe(8);
  });

  it("ignores stale-generation appends and accepts only the active generation", () => {
    const current = { data: bytes("replay"), baseOffset: 0, generation: 8 };
    const stale = applyTerminalChunk(current, {
      agentId: "agent-1",
      data: bytes("-stale"),
      stream: "output",
      operation: "append",
      generation: 7,
    });
    const live = applyTerminalChunk(stale, {
      agentId: "agent-1",
      data: bytes("-live"),
      stream: "output",
      operation: "append",
      generation: 8,
    });

    expect(stale).toBe(current);
    expect(text(live.data)).toBe("replay-live");
  });

  it("clears stale history when a new generation has an empty replay", () => {
    const cleared = applyTerminalChunk(
      { data: bytes("old screen"), baseOffset: 0, generation: 3 },
      {
        agentId: "agent-1",
        data: new Uint8Array(),
        stream: "output",
        operation: "replace",
        generation: 4,
      },
    );

    expect(cleared.data).toHaveLength(0);
    expect(cleared.generation).toBe(4);
  });
});
