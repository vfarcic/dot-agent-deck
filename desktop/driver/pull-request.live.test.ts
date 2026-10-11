// Local, opt-in native/network coverage. Desktop-driver CI leaves it disabled.
// Run after scripts/driver-test.sh builds the app, with DAD_DRIVER_REAL_GITHUB=1.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { connect } from "node:net";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import { test } from "node:test";
import { type Deck, WAIT_MS, waitFor, withDeck } from "./harness.ts";

const exec = promisify(execFile);
const run = (file: string, args: string[]) => exec(file, args, { timeout: WAIT_MS });
const enabled = process.env.DAD_DRIVER_REAL_GITHUB === "1";
const prUrl = "https://github.com/vfarcic/dot-agent-deck/pull/1601";
const scenario = `pull-request-live-${process.pid}`;
const results = resolve(process.env.DAD_DRIVER_RESULTS ?? "driver-results", scenario);
const browser = '[data-testid="pr-browser"]';
const pane = '[data-testid="agent-pane-overlay"]';
const observations: Record<string, unknown> = {};
const ocr = process.env.DAD_DRIVER_TESSERACT ?? "tesseract";

/// Scenario: open a real GitHub PR from a shell agent's screen in the real Tauri app, and compare opening the same PR from its overview row.
/// GitHub content must occupy the native child view inside the frame, the toolbar must stay usable, and Close and Escape must remove the page and overlay.
test("pull_request_001 real native GitHub page and controls from agent and overview", { skip: !enabled }, async () => {
  mkdirSync(results, { recursive: true });
  console.log(`Native PR evidence: ${results}`);
  await run(ocr, ["--version"]);
  const { stdout } = await run("gh", ["pr", "view", "1601", "--repo", "vfarcic/dot-agent-deck", "--json", "headRefName,title"]);
  const pr = JSON.parse(stdout) as { headRefName: string };
  const failures: string[] = [];
  const opener = join(results, "record-browser.py");
  const opened = join(results, "external-url.txt");
  writeFileSync(opener, `#!/usr/bin/python3\nimport pathlib,sys\npathlib.Path(${JSON.stringify(opened)}).write_text(sys.argv[1])\n`, {mode:0o755});
  await withDeck(scenario, { daemonFirst: false, defaultCommand: "bash --noprofile --norc", env: { BROWSER: opener } }, async (deck) => {
    const check = async (name: string, body: () => Promise<void>) => {
      try { await body(); observations[name] = "PASS"; }
      catch (error) { failures.push(`${name}: ${String(error)}`); observations[name] = String(error); }
    };
    // A real gh lookup on the PR's branch name in a synthetic repository.
    // Point gh at its existing configuration without copying credentials.
    deck.env.GH_CONFIG_DIR = process.env.GH_CONFIG_DIR ?? join(homedir(), ".config", "gh");
    deck.env.DOT_AGENT_DECK_HOOK_PROVENANCE = "warn";
    await run("git", ["init", "-b", pr.headRefName, deck.project]);
    await run("git", ["-C", deck.project, "remote", "add", "origin", "https://github.com/vfarcic/dot-agent-deck.git"]);
    await run("git", ["-C", deck.project, "-c", "user.name=Native PR test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-m", "Native PR scenario"]);
    await run("git", ["-C", deck.project, "update-ref", "refs/remotes/origin/main", "HEAD"]);
    await run("git", ["-C", deck.project, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
    await deck.startDaemon();
    await deck.element('[data-testid="daemon-group"][data-deck-connected="yes"]');
    await deck.session.click(await deck.element('[data-testid="overview-new-agent"]'));
    await deck.textOf('[data-testid="new-agent-current-path"]', (text) => text.trim() === deck.project, "the sandbox project");
    await deck.session.click(await deck.element('[data-testid="new-agent-use-directory"]'));
    await waitFor("the name field to be seeded", async () => await deck.session.property(await deck.element('[data-testid="new-agent-name"]'), "value") === "driver-project");
    await deck.session.click(await deck.element('[data-testid="new-agent-start"]'));
    await deck.element(pane);
    const [agent] = await deck.daemonAgents() as { agent_id: string; pane_id: string }[];
    // The shell has no agent hooks. Seed the existing synthetic-session seam
    // exactly as tests/e2e_pull_request_badge.rs does; gh and the child page
    // remain real. This test covers the browser, not agent execution.
    await new Promise<void>((resolve, reject) => {
      const socket = connect(deck.env.DOT_AGENT_DECK_SOCKET as string);
      socket.setTimeout(WAIT_MS, () => socket.destroy(new Error("the synthetic session hook timed out")));
      socket.once("error", reject);
      socket.once("connect", () => socket.end(`${JSON.stringify({session_id:"native-pr-session", agent_type:"claude_code", event_type:"session_start", timestamp:new Date().toISOString(), agent_id:agent.agent_id, pane_id:agent.pane_id, cwd:deck.project})}\n`));
      socket.once("close", () => resolve());
    });
    const agentBadge = `${pane} [data-testid^="pr-badge-"]`;
    await deck.textOf(agentBadge, (text) => text.includes("1601"), "the real PR #1601 badge on the agent screen");
    for (const origin of ["agent", "overview"] as const) {
      if (origin === "overview") {
        await deck.session.click(await deck.element(`${pane} [aria-label="Back to dashboard"]`));
        await waitFor("the agent pane to close", async () => (await deck.session.find(pane)) === null);
      }
      const badge = origin === "agent" ? agentBadge : '[data-testid="overview-pr-badge"]';
      try {
        await deck.session.click(await deck.element(badge));
        await deck.element(browser);
        await check(`${origin}-content`, () => inspectNativePage(deck, origin));
        for (const name of ["Back", "Open in browser", "Close pull request"]) {
          const css = `${browser} button[aria-label="${name}"]`;
          const state = await deck.session.execute<{ visible: boolean; inert: boolean; disabled: boolean }>(`
            const b = document.querySelector(arguments[0]);
            const r = b && b.getBoundingClientRect();
            return { visible: !!r && r.width > 0 && r.height > 0, inert: !!b?.closest('[inert]'), disabled: !!b?.disabled };
          `, [css]);
          assert.deepEqual(state, { visible: true, inert: false, disabled: false }, `${origin}: ${name} is not visible and usable`);
        }
        await deck.session.click(await deck.element(`${browser} [aria-label="Back"]`));
        await deck.session.click(await deck.element(`${browser} [aria-label="Close pull request"]`));
        await closed(deck, `${origin}: Close`);
        observations[`${origin}-close`] = "overlay and native view removed";
        await deck.session.click(await deck.element(badge));
        await deck.element(browser);
        // A real key in the app's focused toolbar (child-page focus is checked separately).
        await deck.session.click(await deck.element(`${browser} [aria-label="Back"]`));
        await deck.session.chord(["\uE00C"]);
        await closed(deck, `${origin}: Escape`);
        observations[`${origin}-escape`] = "overlay and native view removed";
        await deck.session.click(await deck.element(badge));
        await deck.element(browser);
        await check(`${origin}-page-escape`, async () => {
          await inspectNativePage(deck, `${origin}-escape`);
          const point = await deck.session.execute<{x:number;y:number}>(`
            const r=document.querySelector('[data-testid="pr-browser-frame"]').getBoundingClientRect();
            return {x:Math.round(window.screenX+r.right-40),y:Math.round(window.screenY+r.bottom-20)};
          `);
          // XTest input reaches the native child, unlike WebDriver actions
          // which are delivered to its sole automated context (the main view).
          await run(process.env.DAD_DRIVER_XDOTOOL ?? "xdotool", ["mousemove", "--sync", String(point.x), String(point.y), "click", "1", "key", "Escape"]);
          await closed(deck, `${origin}: Escape with GitHub focused`);
        });
        // Restore via the toolbar if the native-page Escape failed, so the
        // system-browser control is still exercised independently.
        if (await deck.session.find(browser)) {
          await deck.session.click(await deck.element(`${browser} [aria-label="Close pull request"]`));
          await closed(deck, `${origin}: recovery Close`);
        }
        await deck.session.click(await deck.element(badge));
        await deck.element(browser);
        writeFileSync(opened, "");
        await deck.session.click(await deck.element(`${browser} [aria-label="Open in browser"]`));
        await closed(deck, `${origin}: Open in browser`);
        await waitFor(`${origin}: system-browser recorder receives the URL`, async () => readFileSync(opened, "utf8") === prUrl);
        assert.equal(readFileSync(opened, "utf8"), prUrl, `${origin}: system-browser handoff must receive the native page's actual PR URL`);
        observations[`${origin}-external`] = prUrl;
      } catch (error) {
        failures.push(`${origin}: ${String(error)}`);
        await evidence(deck, `${origin}-failure`);
        // Continue to the overview control after a failure, through the public
        // close command so the failed UI is not mistaken for the control.
        await deck.session.execute("return window.__TAURI_INTERNALS__.invoke('desktop_pr_browser_close')").catch(() => undefined);
        await deck.session.chord(["\uE00C"]).catch(() => undefined);
      }
    }
    writeFileSync(join(results, "observations.json"), JSON.stringify(observations, null, 2));
    for (const log of ["driver.log", "daemon.out", "deck.log"]) {
      try { copyFileSync(join(deck.sandbox, log), join(results, log)); } catch { /* Optional log. */ }
    }
    assert.deepEqual(failures, [], `Native PR browser failures:\n${failures.join("\n")}`);
  });
});

async function views(deck: Deck): Promise<{ label: string; window_label: string }[]> {
  return deck.session.execute("return window.__TAURI_INTERNALS__.invoke('plugin:webview|get_all_webviews')");
}

async function closed(deck: Deck, description: string): Promise<void> {
  await waitFor(`${description} removes overlay and native view`, async () =>
    (await deck.session.find(browser)) === null && !(await views(deck)).some((view) => view.label === "pr-browser"), 15_000);
}

async function inspectNativePage(deck: Deck, origin: string): Promise<void> {
  await waitFor(`${origin}: a native PR child view`, async () => (await views(deck)).some((view) => view.label === "pr-browser"));
  const geometry = await deck.session.execute<{size:{width:number;height:number};frame:{x:number;y:number;width:number;height:number}}>(`
    return Promise.all([
      window.__TAURI_INTERNALS__.invoke('plugin:webview|webview_position', {label: 'pr-browser'}),
      window.__TAURI_INTERNALS__.invoke('plugin:webview|webview_size', {label: 'pr-browser'})
    ]).then(([position,size]) => ({position,size,frame:document.querySelector('[data-testid="pr-browser-frame"]').getBoundingClientRect().toJSON()}));
  `);
  observations[`${origin}-geometry`] = geometry;
  assert.ok(geometry.size.width > 0 && geometry.size.height > 0, `${origin}: native PR view has zero area`);
  assert.ok(Math.abs(geometry.size.width - geometry.frame.width) < 2 && Math.abs(geometry.size.height - geometry.frame.height) < 2,
    `${origin}: native PR view does not fit the frame: ${JSON.stringify(geometry)}`);
  // Record the complete on-screen native composite as well as WebDriver's
  // main-view screenshot, which may omit sibling native surfaces.
  await evidence(deck, `${origin}-open`);
  // This child has its own WebKit context and is not a WebDriver browsing
  // context. OCR the actual composed X display, cropped to the frame; the
  // title must be painted there, not merely present in a daemon response.
  await waitFor(`${origin}: GitHub's PR title is painted inside the native frame`, async () => {
    await screen(deck, `${origin}-loaded`);
    const {stdout} = await run(ocr, [join(results, `${origin}-loaded-frame.png`), "stdout", "--psm", "11"]);
    writeFileSync(join(results, `${origin}-ocr.txt`), stdout);
    observations[`${origin}-visible-text`] = stdout;
    return stdout.replace(/\s+/g," ").toLowerCase().includes("upgrade daemons from any client");
  }, 45_000);
}

async function evidence(deck: Deck, name: string): Promise<void> {
  writeFileSync(join(results, `${name}-webview.png`), Buffer.from(await deck.session.screenshot(), "base64"));
  writeFileSync(join(results, `${name}-page.txt`), await deck.session.execute<string>("return document.body.innerText"));
  await screen(deck, name);
}

async function screen(deck: Deck, name: string): Promise<void> {
  const frame = await deck.session.execute<{x:number;y:number;width:number;height:number} | null>(`
    const r=document.querySelector('[data-testid="pr-browser-frame"]')?.getBoundingClientRect();
    return r ? {x:r.x+window.screenX,y:r.y+window.screenY,width:r.width,height:r.height} : null;
  `);
  await run("/usr/bin/python3", ["-c", "import gi,sys,json; gi.require_version('Gdk','3.0'); from gi.repository import Gdk; w=Gdk.get_default_root_window(); p=Gdk.pixbuf_get_from_window(w,0,0,w.get_width(),w.get_height()); p.savev(sys.argv[1]+'-screen.png','png',[],[]); r=json.loads(sys.argv[2]); r and Gdk.pixbuf_get_from_window(w,*[int(r[k]) for k in ['x','y','width','height']]).savev(sys.argv[1]+'-frame.png','png',[],[])", join(results, name), JSON.stringify(frame)]);
}
