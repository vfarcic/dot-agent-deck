/**
 * The deck vocabulary on the webview's side — the field checks the Decks panel
 * types against (PRD #741 M7) and the selection value the Deck selector carries
 * (M9).
 */
import { describe, expect, it } from "vitest";
import { LOCAL_ENDPOINT_SELECTION, type EndpointSettingsDto } from "./bridge";
import {
  deckChoices,
  describeEndpoint,
  type EndpointField,
  FIELD_PLACEHOLDERS,
  type FieldProblem,
  hostProblem,
  identityProblem,
  jumpProblem,
  LOCAL_DECK_SELECTION,
  mintEndpointId,
  parseSelection,
  portProblem,
  rowProblems,
  sameSelection,
  selectionToken,
  socketProblem,
  SPECIMEN_PLACEHOLDER_FIELDS,
  userProblem,
} from "./endpoints";
import validationCases from "./endpointValidationCases.json";

const ID = "a1b2c3d4e5f60718";

/** One row of the shared table. `repeat` defaults to 1. */
interface ParityCase {
  field: EndpointField;
  value: string;
  repeat?: number;
  accepted: boolean;
  why: string;
}

const parityCases: ParityCase[] = validationCases.cases as ParityCase[];

/** The value a row stands for: `value` repeated `repeat` times. */
function caseValue(row: ParityCase): string {
  return row.value.repeat(row.repeat ?? 1);
}

/** A row's value, short enough to read in a test name. */
function describeCaseValue(row: ParityCase): string {
  const repeat = row.repeat ?? 1;
  return repeat === 1 ? JSON.stringify(row.value) : `${JSON.stringify(row.value)} × ${repeat}`;
}

/** Which predicate owns which field. The panel wires exactly these five. */
const PREDICATE: Record<EndpointField, (raw: string) => FieldProblem> = {
  host: hostProblem,
  user: userProblem,
  identity: identityProblem,
  jump: jumpProblem,
  socket: socketProblem,
};

function section(selection: string): EndpointSettingsDto {
  return {
    remote: [
      { host: "build-box.example.com", id: ID, port: 22, user: "vf" },
      { host: "relay.example.com", id: "0f1e2d3c4b5a6978", port: 2222 },
    ],
    selection,
  };
}

describe("the selection value", () => {
  it("round-trips both variants through their stored token", () => {
    expect(selectionToken(parseSelection(LOCAL_ENDPOINT_SELECTION))).toBe(LOCAL_ENDPOINT_SELECTION);
    expect(selectionToken(parseSelection(ID))).toBe(ID);
    expect(parseSelection(LOCAL_ENDPOINT_SELECTION)).toEqual(LOCAL_DECK_SELECTION);
    expect(parseSelection(ID)).toEqual({ kind: "one", id: ID });
  });

  it("degrades a token this build does not know rather than throwing", () => {
    /*
      `all` is the token PRD #742 will write, and this is the build that predates
      it. It parses as `One`, matches no row, and is therefore rendered as the
      local deck — with the substitution reported by `connection.selectionFallback`
      — exactly as `Selection`'s own deserializer degrades it Rust-side. The
      stored bytes are untouched, so the newer build's choice survives a save
      made from here.
    */
    const future = parseSelection("all");
    expect(future).toEqual({ kind: "one", id: "all" });
    expect(deckChoices(section(LOCAL_ENDPOINT_SELECTION)).find((choice) => sameSelection(choice.selection, future))).toBeUndefined();
    expect(selectionToken(future)).toBe("all");
  });

  it("compares by the stored token, so two readings of one deck are one deck", () => {
    expect(sameSelection(parseSelection(ID), { kind: "one", id: ID })).toBe(true);
    expect(sameSelection(LOCAL_DECK_SELECTION, parseSelection(ID))).toBe(false);
  });
});

describe("deckChoices", () => {
  it("always offers the local deck first, even with nothing configured", () => {
    // The local deck needs no configuration — `Endpoint::local()` resolves it
    // from the platform paths — so this list is never empty and the selector is
    // useful before anything is stored.
    expect(deckChoices(undefined)).toEqual([
      { token: LOCAL_ENDPOINT_SELECTION, selection: LOCAL_DECK_SELECTION, label: "This machine" },
    ]);
  });

  it("names each remote deck the way Rust describes it", () => {
    const labels = deckChoices(section(LOCAL_ENDPOINT_SELECTION)).map((choice) => choice.label);
    // `user@host`, with `:port` appended only when the port is not 22 — the same
    // derivation `RemoteEndpoint::describe()` performs, because there is no
    // stored display name to use instead.
    expect(labels).toEqual(["This machine", "vf@build-box.example.com", "relay.example.com:2222"]);
  });

  it("gives a deck with no host yet a label rather than an empty row", () => {
    const blank: EndpointSettingsDto = { remote: [{ host: "", id: ID, port: 22 }], selection: ID };
    expect(deckChoices(blank)[1].label).toBe("New deck");
  });
});

describe("describeEndpoint", () => {
  it("drops the default port and keeps any other", () => {
    expect(describeEndpoint({ host: "h", id: ID, port: 22 })).toBe("h");
    expect(describeEndpoint({ host: "h", id: ID, port: 2222 })).toBe("h:2222");
    expect(describeEndpoint({ host: "h", id: ID, port: 22, user: "u" })).toBe("u@h");
  });
});

describe("mintEndpointId", () => {
  it("is sixteen hex characters, which no reserved word can be", () => {
    const id = mintEndpointId();
    expect(id).toMatch(/^[0-9a-f]{16}$/);
    // Long enough and hex enough that it cannot collide with `local`, nor with a
    // word a future `Selection` variant would reserve.
    expect(id).not.toBe(LOCAL_ENDPOINT_SELECTION);
  });
});

/*
 * The field predicates, against the shared table.
 *
 * `endpointValidationCases.json` is read here and by
 * `desktop/src-tauri/src/endpoint_field_parity.rs`, which asserts the *Rust*
 * deserializers decide the same verdicts. So a row is a claim about both
 * languages at once, and a predicate that drifts from its newtype reddens one
 * of the two gates rather than shipping.
 *
 * This is a shared table rather than a test that literally calls both
 * implementations in one process. Crossing the boundary for real would mean a
 * Rust test shelling out to `node` over compiled TypeScript — a toolchain and a
 * build step inside `cargo test-fast`, for a check the two readers already make
 * against one file. The property that matters is that neither side owns the
 * expectations, and one file with two readers has it.
 */
describe("the field predicates and their Rust newtypes", () => {
  it("is checked against every field, with both verdicts represented for each", () => {
    /*
      Without this the table could be gutted a field at a time and every
      remaining row would still pass. The `*Problem` predicates shipped with no
      test at all, which is how four disagreements with Rust — a universal set
      holding seven bytes Rust allows, an `@` on the jump charset, `_` and `%`
      missing from the host charset, and one 255-byte cap standing in for 64,
      4096 and 253 — went unnoticed through two milestone reviews.
    */
    for (const field of Object.keys(PREDICATE) as EndpointField[]) {
      const rows = parityCases.filter((row) => row.field === field);
      expect(rows.filter((row) => row.accepted).length, `${field} has an accepted case`).toBeGreaterThan(0);
      expect(rows.filter((row) => !row.accepted).length, `${field} has a refused case`).toBeGreaterThan(0);
    }
  });

  it("names no field the panel does not validate", () => {
    for (const row of parityCases) {
      expect(Object.keys(PREDICATE), `${row.field} is a field the panel validates`).toContain(row.field);
    }
  });

  for (const row of parityCases) {
    it(`${row.accepted ? "accepts" : "refuses"} ${row.field} ${describeCaseValue(row)} — ${row.why}`, () => {
      const problem = PREDICATE[row.field](caseValue(row));
      // `undefined` means "fine", so accepted is the absence of a problem.
      expect(problem === undefined, problem ?? "accepted").toBe(row.accepted);
    });
  }

  it("reads an empty optional field as unset, which is the one place Rust disagrees on purpose", () => {
    /*
      Rust refuses the empty string for all five types — a stored field that
      exists must have a value. Here empty means *not set*, and
      `EndpointsPanel.tsx` maps it back with `value || undefined` before a save,
      so no newtype ever sees one. `endpoint_field_parity.rs`'s
      `the_empty_string_is_refused_here_while_the_panel_reads_it_as_unset` is
      the other half of this pair; the shared table deliberately holds no empty
      row, because the two sides are meant to disagree about it.
    */
    expect(userProblem("")).toBeUndefined();
    expect(identityProblem("")).toBeUndefined();
    expect(jumpProblem("")).toBeUndefined();
    expect(socketProblem("")).toBeUndefined();
    // The host is the one required field, so empty is a problem rather than a
    // choice — and `rowProblems` reports it, which is what disables the save.
    expect(hostProblem("")).toBe("A host is required.");
    expect(rowProblems({ host: "", id: ID, port: 22 })).toEqual(["A host is required."]);
  });
});

describe("the placeholders the fields display", () => {
  /*
    A placeholder the field's own validator rejects is the defect this pairing
    exists to prevent: as shipped, `~/.ssh/id_ed25519` — the literal text the
    Key file input shows — went `aria-invalid` with a hint that contradicted it,
    and because `rowProblems` disables `Test connection`, the one path that
    discovers a deck's socket went with it.
  */
  it("accepts every placeholder that is a specimen value, and pins each in the shared table too", () => {
    for (const field of SPECIMEN_PLACEHOLDER_FIELDS) {
      const placeholder = FIELD_PLACEHOLDERS[field];
      expect(PREDICATE[field](placeholder), `${field} accepts its own placeholder`).toBeUndefined();
      // In the table as well, so the Rust side is pinned to accept it and
      // neither half can be quietly dropped.
      const row = parityCases.find((entry) => entry.field === field && caseValue(entry) === placeholder);
      expect(row, `${placeholder} is a row of the shared table`).toBeDefined();
      expect(row?.accepted).toBe(true);
    }
  });

  it("refuses every placeholder that is prose, which is what keeps the split honest", () => {
    /*
      The other three placeholders tell the user where the value comes from
      rather than showing one, and each contains spaces — so a prose hint
      mistakenly listed as a specimen fails the test above, and a specimen
      mistakenly left out of `SPECIMEN_PLACEHOLDER_FIELDS` fails this one
      instead of silently losing its coverage.
    */
    for (const field of Object.keys(FIELD_PLACEHOLDERS) as EndpointField[]) {
      if (SPECIMEN_PLACEHOLDER_FIELDS.includes(field)) continue;
      expect(PREDICATE[field](FIELD_PLACEHOLDERS[field]), `${field}'s placeholder is prose`).toBeDefined();
    }
  });
});

describe("portProblem", () => {
  // The only field with no newtype behind it: the stored value is a `u16` and
  // the input is `type="number"`, so this is the whole check.
  it("takes 1 through 65535 and nothing else", () => {
    const refusal = "A port is a whole number from 1 to 65535.";
    expect(portProblem(22)).toBeUndefined();
    expect(portProblem(1)).toBeUndefined();
    expect(portProblem(65535)).toBeUndefined();
    expect(portProblem(0)).toBe(refusal);
    expect(portProblem(65536)).toBe(refusal);
    expect(portProblem(-1)).toBe(refusal);
    expect(portProblem(22.5)).toBe(refusal);
    // An emptied number input yields NaN, which is not an integer.
    expect(portProblem(Number.NaN)).toBe(refusal);
  });
});

describe("rowProblems", () => {
  it("reports every field's problem, so one bad field is enough to hold Test connection", () => {
    // `Test connection` is M10's only socket-discovery path and it is disabled
    // while this is non-empty, which is why a validator that is wrong about a
    // legitimate value costs the user the feature rather than a warning.
    expect(rowProblems({
      host: "build-box.example.com",
      id: ID,
      identity: "~/.ssh/id_ed25519",
      jump: "bastion",
      port: 22,
      socket: "/run/user/1000/dot-agent-deck-attach.sock",
      user: "vf",
    })).toEqual([]);
    expect(rowProblems({ host: "build box", id: ID, port: 0, user: "-u" })).toHaveLength(3);
  });
});

describe("the universal refusals", () => {
  it("are the twelve shell metacharacters, in every field, and nothing beyond printable ASCII", () => {
    /*
      The table above proves each field's verdict on a value; this proves the
      *shape* of the universal half, which is the piece that was wrong. Rust's
      `SHELL_METACHARACTERS` is twelve bytes and `validate` refuses them in
      every field before consulting any charset. The characters the shipped
      build added to that set — `* ? [ ] { } ~ ! # ^` — are deliberately absent
      here, because two of them are on charsets Rust accepts (`[` on
      `Hostname`'s, `~` on `KeyPath`'s) and the rest are already refused by the
      charset that should refuse them.
    */
    const metacharacters = ["`", "$", ";", "&", "|", "<", ">", "(", ")", "'", "\"", "\\"];
    expect(metacharacters).toHaveLength(12);
    for (const character of metacharacters) {
      // Wrapped in text each field would otherwise accept, so the refusal is
      // the metacharacter's doing and not the shape check's.
      expect(hostProblem(`a${character}b`), `host refuses ${character}`).toBeDefined();
      expect(userProblem(`a${character}b`), `user refuses ${character}`).toBeDefined();
      expect(identityProblem(`/a${character}b`), `identity refuses ${character}`).toBeDefined();
      expect(jumpProblem(`a${character}b`), `jump refuses ${character}`).toBeDefined();
      expect(socketProblem(`/a${character}b`), `socket refuses ${character}`).toBeDefined();
    }
    /*
      Everything outside 0x21..0x7e, whatever the field: a space, a tab, a
      newline, NUL, DEL, a non-ASCII letter, and a bidi control — the last of
      which is why `describeEndpoint` sanitises on the way out as well.
      Built with `fromCharCode` rather than written as literals so this file
      carries no invisible byte of its own.
    */
    const outsidePrintableAscii = [" ", "\t", "\n", String.fromCharCode(0), String.fromCharCode(0x7f), "é", String.fromCharCode(0x200e)];
    for (const character of outsidePrintableAscii) {
      expect(hostProblem(`a${character}b`), `host refuses ${JSON.stringify(character)}`).toBeDefined();
      expect(identityProblem(`/a${character}b`), `identity refuses ${JSON.stringify(character)}`).toBeDefined();
    }
  });
});
