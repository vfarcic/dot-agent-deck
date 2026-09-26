// Issue #953 — the driver tier's WebDriver client.
//
// A W3C WebDriver session is plain JSON over HTTP, and this suite needs eight
// of its endpoints, so it speaks the protocol directly with Node's own `fetch`
// rather than pulling in WebdriverIO: no new dependency to pin, audit or bump,
// and nothing between a failing assertion and the wire it came from.
//
// `tauri-driver` is the server. It accepts the session request, launches the
// app binary named in `tauri:options`, and proxies every later call to the
// platform's native driver — `WebKitWebDriver` on Linux — which drives the
// app's real WebKitGTK webview.

/** WebDriver's name for the element reference key in a JSON response. */
const ELEMENT_KEY = "element-6066-11e4-a52e-4f735466cecf";

function elementOf(value: Record<string, string>): Element {
  const id = value[ELEMENT_KEY];
  if (typeof id !== "string") throw new WebDriverError("invalid element reference", JSON.stringify(value));
  return { id };
}

/** The Enter key, as WebDriver's key table spells it. */
export const ENTER = "";

export class WebDriverError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(`${code}: ${message}`);
    this.code = code;
  }
}

async function call(base: string, method: string, path: string, body?: unknown): Promise<unknown> {
  const response = await fetch(`${base}${path}`, {
    method,
    headers: body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const parsed = (await response.json()) as { value?: unknown };
  const value = parsed.value;
  if (!response.ok) {
    const failure = (value ?? {}) as { error?: string; message?: string };
    throw new WebDriverError(failure.error ?? `http ${response.status}`, failure.message ?? "");
  }
  return value;
}

/** True once the server answers `/status` as ready. */
export async function serverReady(base: string): Promise<boolean> {
  try {
    const value = (await call(base, "GET", "/status")) as { ready?: boolean } | undefined;
    return value?.ready !== false;
  } catch {
    return false;
  }
}

export type Element = { readonly id: string };

export class Session {
  readonly base: string;
  readonly id: string;

  private constructor(base: string, id: string) {
    this.base = base;
    this.id = id;
  }

  /** Launches `application` through tauri-driver and returns the session on its window. */
  static async open(base: string, application: string): Promise<Session> {
    const value = (await call(base, "POST", "/session", {
      capabilities: { alwaysMatch: { "tauri:options": { application } } },
    })) as { sessionId: string };
    return new Session(base, value.sessionId);
  }

  private request(method: string, path: string, body?: unknown): Promise<unknown> {
    return call(this.base, method, `/session/${this.id}${path}`, body);
  }

  async close(): Promise<void> {
    await this.request("DELETE", "");
  }

  /** The first element matching `css`, or `null` — never throws for "not there yet". */
  async find(css: string): Promise<Element | null> {
    try {
      const value = (await this.request("POST", "/element", { using: "css selector", value: css })) as Record<
        string,
        string
      >;
      return elementOf(value);
    } catch (error) {
      if (error instanceof WebDriverError && error.code === "no such element") return null;
      throw error;
    }
  }

  async click(element: Element): Promise<void> {
    await this.request("POST", `/element/${element.id}/click`, {});
  }

  /** Real key events into `element`, which is what a React-controlled input and xterm's textarea both need. */
  async type(element: Element, text: string): Promise<void> {
    await this.request("POST", `/element/${element.id}/value`, { text });
  }

  /** The element's DOM property (`value` for an input), not its attribute. */
  async property(element: Element, name: string): Promise<unknown> {
    return this.request("GET", `/element/${element.id}/property/${encodeURIComponent(name)}`);
  }

  async attribute(element: Element, name: string): Promise<string | null> {
    return (await this.request("GET", `/element/${element.id}/attribute/${encodeURIComponent(name)}`)) as
      | string
      | null;
  }

  async execute<T>(script: string, args: unknown[] = []): Promise<T> {
    return (await this.request("POST", "/execute/sync", { script, args })) as T;
  }

  /** Base64 PNG of the window, for the failure record. */
  async screenshot(): Promise<string> {
    return (await this.request("GET", "/screenshot")) as string;
  }
}
