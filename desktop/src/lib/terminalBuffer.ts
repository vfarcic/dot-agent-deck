import type { TerminalBuffer, TerminalChunk } from "../types";

// Mirrors the daemon's per-agent SCROLLBACK_CAP_BYTES (1 MiB). This buffer
// only backs remount replay — live bytes stream straight to xterm — so the
// cost is memory (~1 MiB x agents), not render time. A small cap made the
// buffer trim every few seconds under TUI repaint traffic.
export const MAX_TERMINAL_BUFFER_BYTES = 1024 * 1024;

function boundedReplacement(data: Uint8Array, generation: number | undefined, maxBytes: number): TerminalBuffer {
  const dropped = Math.max(0, data.byteLength - maxBytes);
  return {
    data: data.slice(dropped),
    baseOffset: dropped,
    generation,
  };
}

export function applyTerminalChunk(
  previous: TerminalBuffer | undefined,
  event: TerminalChunk,
  maxBytes = MAX_TERMINAL_BUFFER_BYTES,
): TerminalBuffer {
  if (event.operation === "replace") {
    return boundedReplacement(event.data, event.generation, maxBytes);
  }

  const current = previous ?? { data: new Uint8Array(), baseOffset: 0, generation: event.generation };
  if (
    event.generation !== undefined
    && current.generation !== undefined
    && event.generation !== current.generation
  ) {
    return current;
  }

  const total = current.data.byteLength + event.data.byteLength;
  const dropped = Math.max(0, total - maxBytes);
  const previousStart = Math.min(dropped, current.data.byteLength);
  const suffixStart = Math.max(0, dropped - current.data.byteLength);
  const keptPrevious = current.data.subarray(previousStart);
  const keptSuffix = event.data.subarray(suffixStart);
  const combined = new Uint8Array(keptPrevious.byteLength + keptSuffix.byteLength);
  combined.set(keptPrevious, 0);
  combined.set(keptSuffix, keptPrevious.byteLength);
  return {
    data: combined,
    baseOffset: current.baseOffset + dropped,
    generation: current.generation ?? event.generation,
  };
}
