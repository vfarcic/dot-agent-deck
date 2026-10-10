//! PRD #1401 — the in-app pull request browser: GitHub's own PR page in a
//! webview drawn over the agent's screen, with the app's toolbar around it.
//!
//! # How it is built (decision 7 of 2026-10-10)
//!
//! A **child webview of the main window** (`Window::add_child`, which is why
//! `tauri` is built with `unstable`), positioned in the rectangle the frontend
//! reports and resized whenever that rectangle moves. The toolbar — Close, Back,
//! Open in browser — is React, rendered by the main webview around that
//! rectangle, so it belongs to the app and the page cannot draw over it.
//!
//! On Linux a child webview cannot be positioned by Tauri alone:
//! `tauri-runtime-wry` packs every webview of a window into the window's
//! vertical `GtkBox`, and wry's `set_bounds` moves only a webview whose parent
//! is a `GtkFixed`, so a second webview would split the window in half rather
//! than float over it. [`overlay`] moves it into a `GtkOverlay` laid over the
//! main webview and places it there itself. macOS and Windows take Tauri's own
//! `set_bounds`.
//!
//! # The page cannot control the app (decision 4)
//!
//! Commands flow from the app to the page, never back. Three things hold it:
//!
//! - **No capability names the PR webview.** `capabilities/default.json` is
//!   scoped to the main WEBVIEW by label, not to the main window (which the PR
//!   webview is a child of), and no capability is `remote`. A plugin or core
//!   command from this webview therefore resolves to no permission and is
//!   refused, from any origin — all but `plugin:__TAURI_CHANNEL__|fetch`,
//!   which Tauri exempts from the ACL and answers only for a channel made for
//!   the asking webview, of which this one has none.
//!   `capability_files_grant_nothing_to_the_pr_webview` pins the files;
//!   `the_pr_webview_cannot_invoke_commands` drives Tauri's IPC.
//! - **The app's own commands refuse a remote origin.** Tauri checks an app
//!   command against the ACL only when the request is remote (this app has no
//!   app manifest), and with no `remote` capability a remote request is always
//!   refused. That is what keeps a GitHub page from invoking one.
//! - **The webview never holds a local origin.** [`classify`] keeps it on
//!   `https` GitHub hosts and refuses `tauri:`, `ipc:`, `file:` and every other
//!   scheme, so the one origin an app command does not check is never loaded
//!   here. A local origin in this webview is the case the second point leaves
//!   open, and this is what closes it.
//!
//! The only page → app signal is a data-free navigation to [`CLOSE_URL`],
//! which [`decide_navigation`] cancels and answers by closing the browser. It
//! carries nothing and invokes no command.
//!
//! # The profile is persistent (decision 7)
//!
//! [`profile`] puts the webview's data in `<app data dir>/pr-browser` on Linux
//! and Windows and in the fixed data store [`DATA_STORE_ID`] on macOS 14+, and
//! never `incognito`, so a GitHub sign-in survives a restart. macOS before 14
//! cannot give a webview a store of its own; there the page shares the app's
//! default store, which is still persistent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tauri::webview::{NewWindowResponse, PageLoadEvent, PlatformWebview, WebviewBuilder};
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Runtime, Url, WebviewUrl};
use tokio::sync::oneshot;

/// The PR webview's label. No capability may name it.
pub const PR_WEBVIEW_LABEL: &str = "pr-browser";

/// The hidden window "Sign out of GitHub" uses when no PR is open: it opens
/// the same profile only to clear it. No capability names it either.
pub const SIGN_OUT_WEBVIEW_LABEL: &str = "pr-browser-sign-out";

/// The label of the app's own webview, the only one a capability names.
pub const MAIN_WEBVIEW_LABEL: &str = "main";

/// The reserved address the page's Escape navigates to. `.invalid` never
/// resolves (RFC 2606), so a navigation the hook failed to cancel would reach
/// nothing; the hook cancels every navigation to this host whatever its path,
/// query or fragment, and reads none of them.
macro_rules! close_url {
    () => {
        "https://close.dot-agent-deck.invalid/"
    };
}
// Production code reaches the address through `close_url!` (the script) and
// `CLOSE_HOST` (the hook); the constant is the name the tests and docs use.
#[allow(dead_code)]
pub const CLOSE_URL: &str = close_url!();
const CLOSE_HOST: &str = "close.dot-agent-deck.invalid";

/// Told to the main webview when the browser closed itself (the page's
/// Escape, or a sign-out that failed), so the app returns to the screen under
/// it. Carries the generation of the open it closed ([`open`]'s answer), so
/// the app ignores one that is about a page it has since replaced.
pub const CLOSED_EVENT: &str = "pr-browser://closed";

/// The profile's directory under the app's data directory.
pub const PROFILE_DIR: &str = "pr-browser";

/// The macOS 14+ data store the PR webview uses — fixed, so every launch opens
/// the same store and a sign-in survives a restart. Sixteen bytes of ASCII so
/// the value is recognisable in a debugger; it is an identifier, not a secret.
pub const DATA_STORE_ID: [u8; 16] = *b"dad-pr-browser-1";

/// The least time between two pages handed to the system browser by the
/// navigation hooks. A page can ask for any number of navigations by script;
/// this is what keeps that from becoming any number of browser windows.
pub const HAND_OFF_INTERVAL: Duration = Duration::from_millis(1500);

/// How long sign-out waits for the GitHub page to be replaced by an empty one
/// before it clears the profile. Waiting is all it is: running out is a
/// failure, reported as one.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long sign-out waits for the platform to confirm the profile is
/// cleared. An answer that never comes is reported as a failure — elapsed time
/// is never taken as proof that the delete ran.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(30);

/// Said when the platform did not confirm the clear in [`CLEAR_TIMEOUT`].
const CLEAR_UNCONFIRMED: &str = "the system did not confirm that the sign-in was cleared. Try again; if it keeps failing, sign out on GitHub's page instead: your avatar, then Sign out.";

/// The webview's storage, as [`profile`] decides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Linux and Windows: where cookies and site data live.
    pub data_directory: PathBuf,
    /// macOS 14+: the data store's identifier. Ignored elsewhere.
    pub data_store_identifier: [u8; 16],
    /// Always `false` — an incognito webview forgets the sign-in on close.
    pub incognito: bool,
}

/// The PR webview's profile for an app whose data directory is
/// `app_data_dir`. A pure function of it, so it is the same on every launch.
pub fn profile(app_data_dir: &Path) -> Profile {
    Profile {
        data_directory: app_data_dir.join(PROFILE_DIR),
        data_store_identifier: DATA_STORE_ID,
        incognito: false,
    }
}

/// What the navigation hook does with one URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Navigation {
    /// A GitHub page: load it here.
    Stay,
    /// A web page off GitHub: cancel it here and open it in the system browser.
    External,
    /// The page's Escape: cancel it and close the browser.
    Close,
    /// Anything else (`javascript:`, `file:`, `data:`, `tauri:`, …): cancel it
    /// and open nothing.
    Refuse,
}

/// The hosts that stay in the webview: GitHub's own, and what its sign-in
/// flow loads in frames (the captcha). A host matches exactly, or as a
/// subdomain of an entry, never by a bare suffix — `evilgithub.com` and
/// `github.com.evil.example` are not GitHub.
const GITHUB_HOSTS: &[&str] = &[
    "github.com",
    // avatars, attachments, rendered notebooks and other framed previews
    "githubusercontent.com",
    // the page's own scripts and styles, and what it frames from there
    "githubassets.com",
    // the sign-in captcha, and the provider it frames
    "octocaptcha.com",
    "arkoselabs.com",
];

fn is_github_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    GITHUB_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

/// Where a navigation to `url` goes. The whole policy, so it is tested as one
/// function rather than through a webview.
pub fn classify(url: &Url) -> Navigation {
    match url.scheme() {
        "https" => match url.host_str() {
            Some(host) if host.eq_ignore_ascii_case(CLOSE_HOST) => Navigation::Close,
            Some(host) if is_github_host(host) => Navigation::Stay,
            Some(_) => Navigation::External,
            None => Navigation::Refuse,
        },
        // Plain http is never loaded here; the system browser upgrades or
        // refuses it as it would any other link.
        "http" if url.host_str().is_some() => Navigation::External,
        // A frame's empty document: no network, no origin of its own.
        "about" if matches!(url.path(), "blank" | "srcdoc") => Navigation::Stay,
        // A GitHub page's own object URL (a preview, a download it built).
        "blob" => match Url::parse(url.path()) {
            Ok(inner)
                if inner.scheme() == "https" && inner.host_str().is_some_and(is_github_host) =>
            {
                Navigation::Stay
            }
            _ => Navigation::Refuse,
        },
        _ => Navigation::Refuse,
    }
}

/// Whether `url` is a pull request page this browser may be OPENED on:
/// `https://github.com/<owner>/<repo>/pull/<number>`, nothing else. The
/// daemon's URL comes from `gh`, so this is a check rather than a filter.
pub fn is_pull_request_url(url: &Url) -> bool {
    if url.scheme() != "https"
        || !url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
    {
        return false;
    }
    if url.username() != "" || url.password().is_some() || url.port().is_some() {
        return false;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|parts| parts.collect())
        .unwrap_or_default();
    matches!(
        segments.as_slice(),
        [owner, repo, "pull", number, ..]
            if !owner.is_empty() && !repo.is_empty() && !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
    )
}

/// What the navigation hooks do besides answering allow/deny. A trait so the
/// decisions are tested with a recording fake rather than a webview.
pub trait Effects {
    /// Open `url` in the system browser.
    fn open_external(&self, url: &Url);
    /// Close the browser and tell the main webview.
    fn close(&self);
    /// Load `url` in the browser itself (a GitHub link that asked for a new window).
    fn navigate(&self, url: &Url);
}

/// The `on_navigation` hook: `true` lets the navigation happen.
pub fn decide_navigation(url: &Url, effects: &impl Effects) -> bool {
    match classify(url) {
        Navigation::Stay => true,
        Navigation::External => {
            effects.open_external(url);
            false
        }
        Navigation::Close => {
            effects.close();
            false
        }
        Navigation::Refuse => false,
    }
}

/// The `on_new_window` hook. No new window is ever created: a GitHub link
/// loads in this browser, anything off GitHub goes to the system browser, and
/// the rest is dropped.
pub fn decide_new_window(url: &Url, effects: &impl Effects) {
    match classify(url) {
        Navigation::Stay => effects.navigate(url),
        Navigation::External => effects.open_external(url),
        Navigation::Close => effects.close(),
        Navigation::Refuse => {}
    }
}

/// The `on_navigation` hook while sign-out is clearing the profile: nothing
/// loads but the empty page sign-out put there — not the toolbar's Back, not a
/// script the old page left running — and nothing is handed off or closed.
pub fn decide_navigation_while_clearing(url: &Url) -> bool {
    url.as_str() == BLANK
}

/// The empty page sign-out puts in the browser before it clears the profile.
const BLANK: &str = "about:blank";

/// What [`HandOffGate::admit`] says about one request for the system browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Open it.
    Launch,
    /// Drop it. `report` is true for the first one dropped since the last
    /// launch, so a burst is logged once rather than once per request.
    Drop { report: bool },
}

#[derive(Debug)]
struct GateState {
    last: Option<Instant>,
    in_flight: bool,
    reported: bool,
}

/// The bound on the navigation hooks' hand-offs to the system browser: one
/// launch at a time, and at most one per `interval`; every other request is
/// dropped. A page's script can navigate in a loop, and the hooks cannot tell
/// a click from a script, nor on every platform a frame from the page
/// (`docs/develop/desktop-gui.md` says which), so the bound is here rather than
/// in the page. The clock is a parameter so the bound is tested without time
/// passing.
#[derive(Debug)]
pub struct HandOffGate {
    interval: Duration,
    state: Mutex<GateState>,
}

impl HandOffGate {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new(GateState {
                last: None,
                in_flight: false,
                reported: false,
            }),
        }
    }

    /// Whether a request made at `now` may open the system browser. A
    /// [`Admission::Launch`] must be followed by [`HandOffGate::finished`]
    /// once the launch returns.
    pub fn admit(&self, now: Instant) -> Admission {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let rested = state
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.interval);
        if !state.in_flight && rested {
            state.last = Some(now);
            state.in_flight = true;
            state.reported = false;
            Admission::Launch
        } else {
            let report = !state.reported;
            state.reported = true;
            Admission::Drop { report }
        }
    }

    /// The launch [`HandOffGate::admit`] allowed has returned.
    pub fn finished(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .in_flight = false;
    }
}

/// The app's one gate: every hand-off from the navigation hooks goes through it.
static HAND_OFF: HandOffGate = HandOffGate::new(HAND_OFF_INTERVAL);

/// Injected into the page's main frame. An Escape the page did not use
/// becomes a navigation to [`CLOSE_URL`] — data-free, and invoking nothing.
///
/// "Did not use" is decided twice, because the page can use a key without
/// saying so: the listener runs FIRST (capture, on `window`) and notes whether
/// focus is in something editable or a menu or dialog is open — what Escape
/// would close inside the page — and the close is then decided after every
/// other listener has run, when `defaultPrevented` is final.
pub const ESCAPE_SCRIPT: &str = concat!(
    r#"(function () {
  if (window.top !== window) return;
  var CLOSE_URL = ""#,
    close_url!(),
    r#"";
  function visible(element) {
    return !element.hidden && element.getClientRects().length > 0;
  }
  function busy() {
    var active = document.activeElement;
    if (active && (active.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(active.tagName))) return true;
    var open = document.querySelectorAll('dialog[open], [role="dialog"], [role="menu"], [role="listbox"], details[open] > details-menu, details[open] > details-dialog');
    for (var i = 0; i < open.length; i += 1) {
      if (visible(open[i])) return true;
    }
    return false;
  }
  window.addEventListener("keydown", function (event) {
    if (event.key !== "Escape" || event.isComposing || event.repeat) return;
    if (busy()) return;
    setTimeout(function () {
      if (!event.defaultPrevented) window.location.assign(CLOSE_URL);
    }, 0);
  }, true);
})();"#
);

/// Where a spoken scroll moves the page (issue #1492's four phrases).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scroll {
    Down,
    Up,
    Top,
    Bottom,
}

/// The script a scroll evaluates in the page — fixed per direction, built
/// from no input, and returning nothing the app reads.
pub fn scroll_script(scroll: Scroll) -> &'static str {
    match scroll {
        Scroll::Down => {
            "window.scrollBy({ top: Math.round(window.innerHeight * 0.85), behavior: 'smooth' });"
        }
        Scroll::Up => {
            "window.scrollBy({ top: -Math.round(window.innerHeight * 0.85), behavior: 'smooth' });"
        }
        Scroll::Top => "window.scrollTo({ top: 0, behavior: 'smooth' });",
        Scroll::Bottom => {
            "window.scrollTo({ top: document.documentElement.scrollHeight, behavior: 'smooth' });"
        }
    }
}

/// The frame the frontend reports: its rectangle and the size of the page it
/// sits in, all in the main webview's CSS pixels.
///
/// CSS pixels rather than the window's, because the two differ by more than
/// the app's zoom: WebKitGTK also scales CSS pixels by the screen's DPI over 96
/// (measured: a page sized from CSS pixels times the zoom came out 4% short
/// under Xvfb's default 100 DPI). So the rectangle is placed in PROPORTION to
/// the page — [`Bounds::onto`] maps it onto whatever size the main webview
/// really has — and no scale factor has to be known on either side.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// The main webview's own width and height (`innerWidth`/`innerHeight`).
    pub viewport_width: f64,
    pub viewport_height: f64,
}

/// A rectangle in the target's own units, from [`Bounds::onto`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The largest coordinate accepted, far past any real window.
const MAX_COORDINATE: f64 = 100_000.0;

impl Bounds {
    /// Refuses a frame that is not finite, sits off the page's origin, is too
    /// small to show a page, or does not fit in the page it was measured in.
    pub fn checked(self) -> Result<Self, String> {
        let values = [
            self.x,
            self.y,
            self.width,
            self.height,
            self.viewport_width,
            self.viewport_height,
        ];
        if values
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0 || *value > MAX_COORDINATE)
        {
            return Err("The pull request browser was given an impossible position.".into());
        }
        if self.width < 1.0
            || self.height < 1.0
            || self.viewport_width < 1.0
            || self.viewport_height < 1.0
        {
            return Err("The pull request browser was given no room to draw in.".into());
        }
        Ok(self)
    }

    /// The frame on a target `width` × `height` (the main webview's real
    /// size, in the units the platform places a webview in), kept inside it.
    pub fn onto(self, width: f64, height: f64) -> Placed {
        let sx = width / self.viewport_width;
        let sy = height / self.viewport_height;
        let x = (self.x * sx).clamp(0.0, width);
        let y = (self.y * sy).clamp(0.0, height);
        Placed {
            x,
            y,
            width: (self.width * sx).min(width - x).max(1.0),
            height: (self.height * sy).min(height - y).max(1.0),
        }
    }
}

struct AppEffects<R: Runtime>(AppHandle<R>);

impl<R: Runtime> Effects for AppEffects<R> {
    fn open_external(&self, url: &Url) {
        hand_off(url);
    }

    fn close(&self) {
        // Never inside the hook: the webview is in the middle of deciding a
        // navigation, and destroying it there would pull it out from under
        // its own callback. Tagged with the session it was asked in, so it
        // never closes one opened after it (`in_session`).
        let app = self.0.clone();
        let generation = GENERATIONS.current();
        tauri::async_runtime::spawn(async move {
            in_session(&SESSION, &GENERATIONS, generation, || {
                close_browser(&app);
                let _ = app.emit_to(MAIN_WEBVIEW_LABEL, CLOSED_EVENT, generation);
            })
            .await;
        });
    }

    fn navigate(&self, url: &Url) {
        let app = self.0.clone();
        let url = url.clone();
        let generation = GENERATIONS.current();
        tauri::async_runtime::spawn(async move {
            in_session(&SESSION, &GENERATIONS, generation, || {
                if let Some(webview) = app.get_webview(PR_WEBVIEW_LABEL) {
                    let _ = webview.navigate(url);
                }
            })
            .await;
        });
    }
}

/// Which open of the browser is current. Every [`open`] starts a new one —
/// including one that reuses the open webview for another pull request — so
/// a page effect asked for under one open can tell that it is now another's.
#[derive(Debug)]
pub struct Generations(AtomicU64);

impl Generations {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// A new open has begun; its generation.
    pub fn begin(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn current(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// The app's generations, advanced by [`open`] while it holds [`SESSION`].
static GENERATIONS: Generations = Generations::new();

/// Runs a page effect that was spawned outside the command queue — the
/// Escape close, or a GitHub link loaded in place — only if the open that
/// asked for it is still the current one. It takes [`SESSION`] first, so it
/// cannot interleave with an [`open`] (which advances the generation under
/// the same lock) or with a sign-out: an effect from an earlier open finds
/// another generation and does nothing, rather than closing or navigating the
/// page that replaced its own. Answers whether the effect ran.
///
/// What it decides from is the generation current when the page's hook
/// fired, so it cannot tell an effect asked for by the old page in the
/// moment between [`open`] advancing the generation and the new page loading
/// in a reused webview.
pub async fn in_session(
    session: &tokio::sync::Mutex<()>,
    generations: &Generations,
    generation: u64,
    effect: impl FnOnce(),
) -> bool {
    let _session = session.lock().await;
    if generations.current() != generation {
        return false;
    }
    effect();
    true
}

/// A navigation hook's hand-off to the system browser, through [`HAND_OFF`].
/// The launch runs on a thread of its own, so the hook returns at once and
/// the gate stays closed until the launch has returned.
fn hand_off(url: &Url) {
    match HAND_OFF.admit(Instant::now()) {
        Admission::Launch => {
            let url = url.clone();
            std::thread::spawn(move || {
                open_in_system_browser(&url);
                HAND_OFF.finished();
            });
        }
        Admission::Drop { report: true } => eprintln!(
            "dot-agent-deck-desktop: the pull request page asked for more pages in the system browser than one every {} ms; dropping the rest until it pauses",
            HAND_OFF_INTERVAL.as_millis()
        ),
        Admission::Drop { report: false } => {}
    }
}

/// Hands `url` to the operating system's default browser. Only `http(s)`
/// ever reaches it — the callers classify first.
fn system_browser(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "https" | "http") {
        return Err("The page on screen has no address the system browser can open.".into());
    }
    open::that_detached(url.as_str())
        .map_err(|error| format!("The system browser did not open: {error}"))
}

/// A navigation hook's launch: nobody is waiting on it, so a failure is logged.
fn open_in_system_browser(url: &Url) {
    if let Err(error) = system_browser(url) {
        eprintln!("dot-agent-deck-desktop: could not open the system browser: {error}");
    }
}

/// Open in browser, in order: the page must be a GitHub page on `https` —
/// the address check [`open_external`] has always made — then `open` hands
/// it to the system browser, and only once that has succeeded does `close`
/// close the in-app page. A launch that failed answers its own error and
/// leaves the page where it is, so the user still has it.
pub fn handoff_page(
    url: &Url,
    open: impl FnOnce(&Url) -> Result<(), String>,
    close: impl FnOnce(),
) -> Result<(), String> {
    if !matches!(classify(url), Navigation::Stay) || !matches!(url.scheme(), "https") {
        return Err("The page on screen has no address the system browser can open.".into());
    }
    open(url)?;
    close();
    Ok(())
}

fn close_browser<R: Runtime>(app: &AppHandle<R>) {
    if let Some(webview) = app.get_webview(PR_WEBVIEW_LABEL) {
        let _ = webview.close();
        release_window(app);
    }
}

fn app_profile<R: Runtime>(app: &AppHandle<R>) -> Result<Profile, String> {
    let dir = app.path().app_data_dir().map_err(|error| {
        format!("The app has no data directory for the pull request browser: {error}")
    })?;
    Ok(profile(&dir))
}

/// The builder every PR webview is made from: the profile, the navigation
/// hooks and the Escape script. One function, so a webview built without the
/// hooks cannot exist.
fn builder<R: Runtime>(
    app: &AppHandle<R>,
    label: &str,
    url: WebviewUrl,
    profile: &Profile,
) -> WebviewBuilder<R> {
    let on_navigation = AppEffects(app.clone());
    let on_new_window = AppEffects(app.clone());
    WebviewBuilder::new(label, url)
        .initialization_script(ESCAPE_SCRIPT)
        .on_navigation(move |url| {
            if CLEARING.load(Ordering::SeqCst) {
                decide_navigation_while_clearing(url)
            } else {
                decide_navigation(url, &on_navigation)
            }
        })
        .on_new_window(move |url, _features| {
            if !CLEARING.load(Ordering::SeqCst) {
                decide_new_window(&url, &on_new_window);
            }
            NewWindowResponse::Deny
        })
        .on_page_load(|webview, payload| {
            if webview.label() == PR_WEBVIEW_LABEL
                && payload.event() == PageLoadEvent::Finished
                && payload.url().as_str() == BLANK
            {
                left_page();
            }
        })
        .incognito(profile.incognito)
        .data_directory(profile.data_directory.clone())
        .data_store_identifier(profile.data_store_identifier)
}

/// Opens the browser on `url` over `bounds`, or moves an open one there, and
/// answers the open's generation — what [`CLOSED_EVENT`] carries, so the app
/// can tell a close of this page from a late one of a page before it.
/// Waits while a sign-out is clearing the profile, so a page never loads into
/// a profile that is half deleted, and is refused while a clear the platform
/// never confirmed may still be running ([`admit_open`]).
pub async fn open<R: Runtime>(
    app: &AppHandle<R>,
    url: &str,
    bounds: Bounds,
) -> Result<u64, String> {
    let bounds = bounds.checked()?;
    let url = Url::parse(url).map_err(|_| "That pull request address is not a URL.".to_string())?;
    if !is_pull_request_url(&url) {
        return Err("Only a pull request on github.com opens in the app.".into());
    }
    let _session = SESSION.lock().await;
    admit_open(&CLEARING)?;
    let generation = GENERATIONS.begin();
    if let Some(webview) = app.get_webview(PR_WEBVIEW_LABEL) {
        webview.navigate(url).map_err(|error| error.to_string())?;
        place(&webview, bounds)?;
        let _ = webview.show();
        let _ = webview.set_focus();
        return Ok(generation);
    }
    let window = app
        .get_window(MAIN_WEBVIEW_LABEL)
        .ok_or_else(|| "The app's window is not open.".to_string())?;
    let profile = app_profile(app)?;
    let at = bounds.onto_window(&window)?;
    let webview = window
        .add_child(
            builder(app, PR_WEBVIEW_LABEL, WebviewUrl::External(url), &profile),
            LogicalPosition::new(at.x, at.y),
            LogicalSize::new(at.width, at.height),
        )
        .map_err(|error| format!("The pull request browser could not open: {error}"))?;
    #[cfg(target_os = "linux")]
    overlay::attach(&webview, bounds)?;
    let _ = webview.set_focus();
    Ok(generation)
}

/// Whether a pull request may open now. Not while `clearing` is raised: that
/// is a sign-out still running — which [`open`] has already waited out by
/// taking the session — or one whose clear the platform never confirmed and
/// may still be deleting the profile ([`sign_out_with`]).
pub fn admit_open(clearing: &AtomicBool) -> Result<(), String> {
    if clearing.load(Ordering::SeqCst) {
        return Err(STILL_CLEARING.into());
    }
    Ok(())
}

/// Why a pull request cannot open, or the sign-in be cleared again, while an
/// unconfirmed clear may still be running.
const STILL_CLEARING: &str = "The app is still clearing your GitHub sign-in, and the system has not said it has finished. Try again in a moment; if this keeps happening, restart the app.";

fn place<R: Runtime>(webview: &tauri::Webview<R>, bounds: Bounds) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        overlay::place(webview, bounds)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let at = bounds.onto_window(&webview.window())?;
        webview
            .set_bounds(tauri::Rect {
                position: LogicalPosition::new(at.x, at.y).into(),
                size: LogicalSize::new(at.width, at.height).into(),
            })
            .map_err(|error| error.to_string())
    }
}

impl Bounds {
    /// The frame on `window`'s content area, in logical pixels — what
    /// `add_child` and `set_bounds` place a webview in.
    fn onto_window<R: Runtime>(self, window: &tauri::Window<R>) -> Result<Placed, String> {
        let scale = window.scale_factor().map_err(|error| error.to_string())?;
        let size = window
            .inner_size()
            .map_err(|error| error.to_string())?
            .to_logical::<f64>(scale);
        Ok(self.onto(size.width, size.height))
    }
}

fn open_webview<R: Runtime>(app: &AppHandle<R>) -> Result<tauri::Webview<R>, String> {
    app.get_webview(PR_WEBVIEW_LABEL)
        .ok_or_else(|| "No pull request is open.".to_string())
}

/// Moves the open browser to `bounds`.
pub fn set_bounds<R: Runtime>(app: &AppHandle<R>, bounds: Bounds) -> Result<(), String> {
    place(&open_webview(app)?, bounds.checked()?)
}

/// Shows or hides the open browser — hidden while a dialog of the app's is
/// over it, since a native webview is drawn above everything the page under
/// it renders.
pub fn set_visible<R: Runtime>(app: &AppHandle<R>, visible: bool) -> Result<(), String> {
    let webview = open_webview(app)?;
    let result = if visible {
        webview.show()
    } else {
        webview.hide()
    };
    result.map_err(|error| error.to_string())
}

/// The toolbar's Back: one step back in the page's own history.
pub fn back<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    open_webview(app)?
        .eval("window.history.back();")
        .map_err(|error| error.to_string())
}

/// A spoken scroll, applied to the page from outside it.
pub fn scroll<R: Runtime>(app: &AppHandle<R>, scroll: Scroll) -> Result<(), String> {
    open_webview(app)?
        .eval(scroll_script(scroll))
        .map_err(|error| error.to_string())
}

/// Open in browser: the page on screen in the system browser, then close —
/// in that order and only on success ([`handoff_page`]).
pub fn open_external<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let webview = open_webview(app)?;
    let url = webview.url().map_err(|error| error.to_string())?;
    handoff_page(&url, system_browser, || close_browser(app))
}

/// The toolbar's Close, `Escape` in the app, and voice's "close".
pub fn close<R: Runtime>(app: &AppHandle<R>) {
    close_browser(app);
}

/// Held by [`open`] and [`sign_out`] for their whole run, so a pull request
/// never opens while the profile is being cleared and a clear never starts
/// under a page that is opening.
static SESSION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Raised while sign-out clears the profile; the navigation hook then lets
/// nothing load but [`BLANK`] ([`decide_navigation_while_clearing`]).
static CLEARING: AtomicBool = AtomicBool::new(false);

/// Settings → Sign out of GitHub: clears the PR browser's profile — cookies,
/// storage and cache — so the next PR opens signed out. Refused on macOS
/// before 14, where that profile is the app's own (`MACOS_SHARED_STORE`).
///
/// Answers only once the platform has confirmed the delete, or with the
/// reason it could not; [`sign_out_with`] has the order of the steps.
/// Deleting the data directory instead was rejected: on Linux Tauri keeps a
/// profile's web context alive for the life of the app, so its cookies would
/// survive in memory and be written back.
pub async fn sign_out<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    // Before macOS 14 the page cannot have a store of its own and shares the
    // app's, so clearing it would also erase the app's own saved state.
    #[cfg(target_os = "macos")]
    if !macos_has_own_data_store() {
        return Err(MACOS_SHARED_STORE.into());
    }
    let host = AppSignOut {
        app: app.clone(),
        scratch: Mutex::new(None),
    };
    sign_out_with(
        &host,
        &SESSION,
        &CLEARING,
        SignOutTimeouts {
            leave: LEAVE_TIMEOUT,
            clear: CLEAR_TIMEOUT,
        },
    )
    .await
}

/// Whether a browser is open when sign-out starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Browser {
    Closed,
    /// Open, on `resume` — the GitHub page to put back once the profile is
    /// cleared (`None` when the page on screen is not one).
    Open {
        resume: Option<Url>,
    },
}

/// Which webview clears the profile: the open browser's own, or a hidden one
/// opened on the same profile only for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearWith {
    Browser,
    Scratch,
}

/// A step's answer, when it comes.
pub type Answer = oneshot::Receiver<Result<(), String>>;

/// What sign-out does to the platform. A trait so the ORDER of the steps is
/// tested with a fake whose answers come late, rather than through a webview.
pub trait SignOutHost {
    fn browser(&self) -> Browser;
    /// Replace the open browser's page with [`BLANK`]; answers once the empty
    /// page has finished loading, so the GitHub page and its scripts are gone.
    fn leave_page(&self) -> Answer;
    /// Open the hidden webview on the profile.
    fn open_scratch(&self) -> Result<(), String>;
    /// Ask the platform to delete the profile's data; answers when the
    /// platform says the delete is done, or that it failed.
    fn clear(&self, with: ClearWith) -> Answer;
    fn close_scratch(&self);
    /// Put the GitHub page back in the open browser.
    fn resume(&self, url: Url);
    /// Close the open browser and tell the app — what a failed sign-out does
    /// instead of putting a page back on a profile it may not have cleared.
    fn close_browser(&self);
}

#[derive(Debug, Clone, Copy)]
pub struct SignOutTimeouts {
    pub leave: Duration,
    pub clear: Duration,
}

/// Lowers [`CLEARING`] however sign-out ends — at once, or, for a clear the
/// platform has not confirmed, when it does ([`Raised::lower_when`]).
struct Raised(&'static AtomicBool);

impl Raised {
    fn raise(flag: &'static AtomicBool) -> Self {
        flag.store(true, Ordering::SeqCst);
        Self(flag)
    }

    /// Keep the flag raised until the platform answers `answer` — the delete
    /// may still be running after sign-out gave up waiting on it, and a page
    /// opened meanwhile would load into a profile being deleted under it. A
    /// platform that drops its callback without answering ends the wait too:
    /// nothing will run it now.
    fn lower_when(self, answer: Answer) {
        let flag = self.0;
        std::mem::forget(self);
        tokio::spawn(async move {
            let _ = answer.await;
            flag.store(false, Ordering::SeqCst);
        });
    }

    /// Lower the flag as `clear` says: now for an answered one, later for
    /// one still unconfirmed, which is a failure either way.
    fn settle(self, clear: Clear) -> Result<(), String> {
        match clear {
            Clear::Answered(result) => {
                drop(self);
                result
            }
            Clear::Unconfirmed(answer) => {
                self.lower_when(answer);
                Err(CLEAR_UNCONFIRMED.into())
            }
        }
    }
}

impl Drop for Raised {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What became of a clear the platform was asked for.
enum Clear {
    /// The platform answered, in time, or dropped its callback (no answer).
    Answered(Result<(), String>),
    /// No answer in time; the delete may still be running.
    Unconfirmed(Answer),
}

/// `answer` to a clear within `limit`, keeping it when it is late.
async fn clear_within(mut answer: Answer, limit: Duration) -> Clear {
    match tokio::time::timeout(limit, &mut answer).await {
        Ok(Ok(result)) => Clear::Answered(result),
        Ok(Err(_)) => Clear::Answered(Err("the system gave no answer.".into())),
        Err(_) => Clear::Unconfirmed(answer),
    }
}

/// `answer` within `limit`, or why not. Running out of time is a failure.
async fn awaited(answer: Answer, limit: Duration, late: &str) -> Result<(), String> {
    match tokio::time::timeout(limit, answer).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the system gave no answer.".into()),
        Err(_) => Err(late.into()),
    }
}

/// Sign-out, in order:
///
/// 1. Hold `session`, so no pull request opens until this returns, and raise
///    `clearing`, so the browser loads nothing but [`BLANK`].
/// 2. With a browser open: replace its page with [`BLANK`] and wait until it
///    has loaded, so the signed-in page is gone before its data is; then
///    clear with the browser's own webview. With none open: open the hidden
///    webview and clear with that.
/// 3. Wait for the platform to confirm the delete. Only then is the GitHub
///    page put back (signed out) or the hidden webview closed. A delete the
///    platform reports as failed, or does not confirm in time, is an error,
///    and the open browser is closed rather than reloaded.
/// 4. A delete not confirmed in time leaves `clearing` raised until the
///    platform does answer, since it may still be running: until then no pull
///    request opens ([`admit_open`]) and a second sign-out is refused, each
///    saying why, rather than either touching a profile being deleted.
pub async fn sign_out_with(
    host: &impl SignOutHost,
    session: &tokio::sync::Mutex<()>,
    clearing: &'static AtomicBool,
    timeouts: SignOutTimeouts,
) -> Result<(), String> {
    let _session = session.lock().await;
    admit_open(clearing)?;
    let raised = Raised::raise(clearing);
    match host.browser() {
        Browser::Open { resume } => {
            let cleared = match awaited(
                host.leave_page(),
                timeouts.leave,
                "the pull request page did not close in time.",
            )
            .await
            {
                Ok(()) => clear_within(host.clear(ClearWith::Browser), timeouts.clear).await,
                Err(error) => Clear::Answered(Err(error)),
            };
            match raised.settle(cleared) {
                Ok(()) => {
                    if let Some(url) = resume {
                        host.resume(url);
                    }
                    Ok(())
                }
                Err(error) => {
                    host.close_browser();
                    Err(error)
                }
            }
        }
        Browser::Closed => {
            host.open_scratch()?;
            let cleared = clear_within(host.clear(ClearWith::Scratch), timeouts.clear).await;
            let cleared = raised.settle(cleared);
            host.close_scratch();
            cleared
        }
    }
}

/// One answer, sent from whichever native callback gets there first; later
/// ones are ignored. Clone it into each callback that may answer.
#[derive(Clone)]
struct Reply(Arc<Mutex<Option<Sender>>>);

/// The sending half of an [`Answer`].
type Sender = oneshot::Sender<Result<(), String>>;

impl Reply {
    fn new() -> (Self, Answer) {
        let (sender, answer) = oneshot::channel();
        (Self(Arc::new(Mutex::new(Some(sender)))), answer)
    }

    fn send(&self, result: Result<(), String>) {
        let sender = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }
}

/// The waiter [`leave_page`](SignOutHost::leave_page) arms, answered by the
/// page-load hook when [`BLANK`] finishes loading in the browser.
static LEAVING: Mutex<Option<Reply>> = Mutex::new(None);

fn left_page() {
    let waiter = LEAVING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(waiter) = waiter {
        waiter.send(Ok(()));
    }
}

struct AppSignOut<R: Runtime> {
    app: AppHandle<R>,
    scratch: Mutex<Option<(tauri::Window<R>, tauri::Webview<R>)>>,
}

impl<R: Runtime> SignOutHost for AppSignOut<R> {
    fn browser(&self) -> Browser {
        match self.app.get_webview(PR_WEBVIEW_LABEL) {
            None => Browser::Closed,
            Some(webview) => Browser::Open {
                resume: webview.url().ok().filter(|url| {
                    url.scheme() == "https" && matches!(classify(url), Navigation::Stay)
                }),
            },
        }
    }

    fn leave_page(&self) -> Answer {
        let (reply, answer) = Reply::new();
        *LEAVING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reply.clone());
        let navigated = open_webview(&self.app).and_then(|webview| {
            webview
                .navigate(Url::parse(BLANK).expect("a constant URL"))
                .map_err(|error| error.to_string())
        });
        if let Err(error) = navigated {
            reply.send(Err(error));
        }
        answer
    }

    fn open_scratch(&self) -> Result<(), String> {
        let profile = app_profile(&self.app)?;
        let blank = WebviewUrl::External(Url::parse(BLANK).expect("a constant URL"));
        let window = tauri::window::WindowBuilder::new(&self.app, SIGN_OUT_WEBVIEW_LABEL)
            .visible(false)
            .build()
            .map_err(|error| format!("Could not open the sign-in profile: {error}"))?;
        let webview = match window.add_child(
            builder(&self.app, SIGN_OUT_WEBVIEW_LABEL, blank, &profile),
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(1.0, 1.0),
        ) {
            Ok(webview) => webview,
            Err(error) => {
                let _ = window.close();
                return Err(format!("Could not open the sign-in profile: {error}"));
            }
        };
        *self
            .scratch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((window, webview));
        Ok(())
    }

    fn clear(&self, with: ClearWith) -> Answer {
        let (reply, answer) = Reply::new();
        let webview = match with {
            ClearWith::Browser => open_webview(&self.app).ok(),
            ClearWith::Scratch => self
                .scratch
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .map(|(_, webview)| webview.clone()),
        };
        let Some(webview) = webview else {
            reply.send(Err(
                "the pull request browser closed during sign-out.".into()
            ));
            return answer;
        };
        let native = reply.clone();
        if let Err(error) = webview.with_webview(move |platform| clear_profile(platform, native)) {
            reply.send(Err(error.to_string()));
        }
        answer
    }

    fn close_scratch(&self) {
        let scratch = self
            .scratch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some((window, _)) = scratch {
            let _ = window.close();
        }
    }

    fn resume(&self, url: Url) {
        if let Ok(webview) = open_webview(&self.app) {
            let _ = webview.navigate(url);
        }
    }

    fn close_browser(&self) {
        close_browser(&self.app);
        let _ = self
            .app
            .emit_to(MAIN_WEBVIEW_LABEL, CLOSED_EVENT, GENERATIONS.current());
    }
}

/// Deletes every kind of data in `platform`'s profile and answers `reply`
/// from the platform's own completion callback — the step wry's
/// `clear_all_browsing_data` starts but never reports the end of.
/// Runs on the main thread (`with_webview`).
#[cfg(target_os = "linux")]
fn clear_profile(platform: PlatformWebview, reply: Reply) {
    use webkit2gtk::{WebContextExt, WebViewExt, WebsiteDataManagerExtManual, WebsiteDataTypes};

    let Some(manager) = platform
        .inner()
        .context()
        .and_then(|context| context.website_data_manager())
    else {
        reply.send(Err(
            "the pull request browser has no storage to clear.".into()
        ));
        return;
    };
    // `webkit_website_data_manager_clear`; a zero time span means all of it.
    manager.clear(
        WebsiteDataTypes::ALL,
        gtk::glib::TimeSpan::from_seconds(0),
        None::<&gtk::gio::Cancellable>,
        move |result| reply.send(result.map_err(|error| error.to_string())),
    );
}

/// macOS: `-[WKWebsiteDataStore removeDataOfTypes:modifiedSince:completionHandler:]`
/// on the webview's own store, from the start of time.
#[cfg(target_os = "macos")]
fn clear_profile(platform: PlatformWebview, reply: Reply) {
    use objc2_foundation::NSDate;
    use objc2_web_kit::{WKWebView, WKWebsiteDataStore};

    let Some(main_thread) = objc2::MainThreadMarker::new() else {
        reply.send(Err("the profile was not cleared on the main thread.".into()));
        return;
    };
    // SAFETY: Tauri's `PlatformWebview::inner` is the `WKWebView` it created
    // for this webview, alive for the duration of the `with_webview` closure,
    // which runs on the main thread (checked above).
    unsafe {
        let webview: &WKWebView = &*platform.inner().cast::<WKWebView>();
        let store = webview.configuration().websiteDataStore();
        let types = WKWebsiteDataStore::allWebsiteDataTypes(main_thread);
        let since = NSDate::dateWithTimeIntervalSince1970(0.0);
        let done = block2::RcBlock::new(move || reply.send(Ok(())));
        store.removeDataOfTypes_modifiedSince_completionHandler(&types, &since, &done);
    }
}

/// Windows: `ICoreWebView2Profile2::ClearBrowsingDataAll` on the webview's
/// profile, answered with the status its completion handler reports.
#[cfg(windows)]
fn clear_profile(platform: PlatformWebview, reply: Reply) {
    use webview2_com::ClearBrowsingDataCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2_13, ICoreWebView2Profile2};
    use windows_core::Interface;

    let done = reply.clone();
    // SAFETY: COM calls on the controller Tauri created for this webview, on
    // the thread that owns it (`with_webview` runs there).
    let started = unsafe {
        platform
            .controller()
            .CoreWebView2()
            .and_then(|core| core.cast::<ICoreWebView2_13>())
            .and_then(|core| core.Profile())
            .and_then(|profile| profile.cast::<ICoreWebView2Profile2>())
            .and_then(|profile| {
                profile.ClearBrowsingDataAll(&ClearBrowsingDataCompletedHandler::create(Box::new(
                    move |status| {
                        done.send(status.map_err(|error| error.to_string()));
                        Ok(())
                    },
                )))
            })
    };
    if let Err(error) = started {
        reply.send(Err(error.to_string()));
    }
}

/// Any other platform: nothing to clear with, said as a failure.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn clear_profile(_platform: PlatformWebview, reply: Reply) {
    reply.send(Err("signing out is not supported on this platform.".into()));
}

/// What sign-out answers on macOS before 14, where the profile is the app's own.
#[cfg(any(target_os = "macos", test))]
const MACOS_SHARED_STORE: &str = "On this version of macOS the pull request browser shares the app's own storage, so the app cannot clear its GitHub sign-in without also clearing its own saved state. Sign out on GitHub's page instead: your avatar, then Sign out.";

/// Whether this Mac gives a webview a data store of its own (macOS 14+).
/// Read from `sw_vers`; a version it cannot read counts as too old, which is
/// the side that refuses a clear rather than risking the app's own storage.
#[cfg(target_os = "macos")]
fn macos_has_own_data_store() -> bool {
    std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|version| macos_major_at_least(&version, 14))
}

/// `"14.2.1"` → at least 14. Anything unreadable is not.
#[cfg(any(target_os = "macos", test))]
fn macos_major_at_least(version: &str, major: u32) -> bool {
    version
        .trim()
        .split('.')
        .next()
        .and_then(|first| first.parse::<u32>().ok())
        .is_some_and(|found| found >= major)
}

/// Linux: once the PR webview is closed, put the main webview back where it
/// was. A no-op elsewhere.
fn release_window<R: Runtime>(app: &AppHandle<R>) {
    #[cfg(target_os = "linux")]
    if let Some(window) = app.get_window(MAIN_WEBVIEW_LABEL) {
        let main_thread = window.clone();
        let _ = window.run_on_main_thread(move || overlay::release(&main_thread));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = app;
}

/// Linux: the PR webview, floated over the main webview in a `GtkOverlay`.
///
/// See the module doc for why: the window's `GtkBox` would otherwise give the
/// two webviews half the window each. The overlay is built when a PR opens,
/// by moving the main webview into it, and taken apart again when the PR
/// closes (`overlay::release` says why it does not stay); each PR webview is then moved from the box into the overlay, and
/// placed by the overlay's `get-child-position` from the last bounds the
/// frontend reported.
#[cfg(target_os = "linux")]
mod overlay {
    use std::sync::Mutex;

    use gtk::prelude::*;
    use tauri::{Runtime, Webview};

    use super::Bounds;

    const OVERLAY_NAME: &str = "dot-agent-deck-pr-overlay";

    /// The frame the frontend last reported. One browser exists at a time,
    /// so one slot; the overlay's `get-child-position` maps it onto the main
    /// webview's own allocation each time GTK lays the overlay out.
    static PLACED: Mutex<Option<Bounds>> = Mutex::new(None);

    fn remember(bounds: Bounds) {
        *PLACED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(bounds);
    }

    fn rectangle(overlay: &gtk::Overlay) -> Option<gtk::gdk::Rectangle> {
        let bounds = (*PLACED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))?;
        let at = bounds.onto(
            f64::from(overlay.allocated_width()),
            f64::from(overlay.allocated_height()),
        );
        Some(gtk::gdk::Rectangle::new(
            at.x.round() as i32,
            at.y.round() as i32,
            at.width.round().max(1.0) as i32,
            at.height.round().max(1.0) as i32,
        ))
    }

    pub fn attach<R: Runtime>(webview: &Webview<R>, bounds: Bounds) -> Result<(), String> {
        remember(bounds);
        webview
            .with_webview(|platform| {
                let child: gtk::Widget = platform.inner().upcast();
                let Some(parent) = child.parent() else { return };
                let Ok(vbox) = parent.downcast::<gtk::Box>() else {
                    return;
                };
                let Some(overlay) = overlay_in(&vbox, &child) else {
                    return;
                };
                vbox.remove(&child);
                overlay.add_overlay(&child);
                child.show();
            })
            .map_err(|error| error.to_string())
    }

    /// Takes the overlay apart once the PR webview has gone: the main webview
    /// goes back into the window's box where it was, and the overlay is
    /// destroyed.
    ///
    /// The overlay lives only while a pull request is open because the main
    /// webview's WebGL canvases — the terminals — did not draw while it sat in
    /// one (measured under Xvfb with software GL; the DOM renderer drew fine).
    /// The page covers the agent's terminal while it is open, so nothing the
    /// user is looking at depends on them then. Runs on the main thread, after
    /// the PR webview's own close, which the event loop delivers first.
    pub fn release<R: Runtime>(window: &tauri::Window<R>) {
        let Ok(vbox) = window.default_vbox() else {
            return;
        };
        let Some(overlay) = existing_overlay(&vbox) else {
            return;
        };
        let Some(main) = overlay.child() else {
            return;
        };
        let position = vbox
            .children()
            .iter()
            .position(|child| child == overlay.upcast_ref::<gtk::Widget>())
            .unwrap_or(0);
        overlay.remove(&main);
        vbox.remove(&overlay);
        vbox.pack_start(&main, true, true, 0);
        vbox.reorder_child(&main, position as i32);
        main.show();
    }

    pub fn place<R: Runtime>(webview: &Webview<R>, bounds: Bounds) -> Result<(), String> {
        remember(bounds);
        webview
            .with_webview(|platform| platform.inner().queue_resize())
            .map_err(|error| error.to_string())
    }

    /// The overlay in `vbox`, if it was built.
    fn existing_overlay(vbox: &gtk::Box) -> Option<gtk::Overlay> {
        vbox.children()
            .into_iter()
            .find(|child| child.widget_name() == OVERLAY_NAME)
            .and_then(|child| child.downcast::<gtk::Overlay>().ok())
    }

    /// Moves `main` (the main webview) into a new overlay at its own place in
    /// `vbox`.
    fn wrap(vbox: &gtk::Box, main: &gtk::Widget) -> gtk::Overlay {
        let position = vbox
            .children()
            .iter()
            .position(|child| child == main)
            .unwrap_or(0);
        let overlay = gtk::Overlay::new();
        overlay.set_widget_name(OVERLAY_NAME);
        overlay.connect_get_child_position(|overlay, _| rectangle(overlay));
        vbox.remove(main);
        overlay.add(main);
        vbox.pack_start(&overlay, true, true, 0);
        vbox.reorder_child(&overlay, position as i32);
        overlay.show();
        main.show();
        overlay
    }

    /// The overlay the PR webview `pr` goes into: the one built at start-up,
    /// else one built now around the other child of `vbox`.
    fn overlay_in(vbox: &gtk::Box, pr: &gtk::Widget) -> Option<gtk::Overlay> {
        existing_overlay(vbox).or_else(|| {
            let main = vbox.children().into_iter().find(|child| child != pr)?;
            Some(wrap(vbox, &main))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn url(value: &str) -> Url {
        Url::parse(value).expect("a test URL")
    }

    #[derive(Default)]
    struct Recorder {
        external: RefCell<Vec<String>>,
        navigated: RefCell<Vec<String>>,
        closed: RefCell<usize>,
    }

    impl Effects for Recorder {
        fn open_external(&self, url: &Url) {
            self.external.borrow_mut().push(url.to_string());
        }
        fn close(&self) {
            *self.closed.borrow_mut() += 1;
        }
        fn navigate(&self, url: &Url) {
            self.navigated.borrow_mut().push(url.to_string());
        }
    }

    #[test]
    fn a_pull_request_and_the_sign_in_flow_stay_in_the_browser() {
        for allowed in [
            "https://github.com/vfarcic/dot-agent-deck/pull/1401",
            "https://github.com/vfarcic/dot-agent-deck/pull/1401/files#diff-1",
            "https://github.com/login?return_to=%2Fvfarcic%2Fdot-agent-deck%2Fpull%2F1401",
            "https://github.com/session",
            "https://github.com/sessions/two-factor",
            "https://GitHub.com/vfarcic/dot-agent-deck/pull/1",
            "https://gist.github.com/someone/abc",
            "https://avatars.githubusercontent.com/u/1",
            "https://viewscreen.githubusercontent.com/view/ipynb",
            "https://octocaptcha.com/",
            "https://github-api.arkoselabs.com/fc/gt2",
            "about:blank",
            "about:srcdoc",
            "blob:https://github.com/6c1d7b2e-0b9a-4d6f-9f0e-2d5f3a1c9e11",
        ] {
            assert_eq!(classify(&url(allowed)), Navigation::Stay, "{allowed}");
        }
    }

    #[test]
    fn a_page_off_github_goes_to_the_system_browser() {
        for external in [
            "https://example.com/",
            "https://github.com.evil.example/login",
            "https://evilgithub.com/",
            "https://notgithubusercontent.com/",
            "https://githubusercontent.com.evil.example/",
            "http://github.com/vfarcic/dot-agent-deck/pull/1401",
            "http://example.com/",
            "https://docs.rs/tauri",
        ] {
            assert_eq!(classify(&url(external)), Navigation::External, "{external}");
        }
    }

    #[test]
    fn script_file_and_app_schemes_are_refused() {
        for refused in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<script>alert(1)</script>",
            "tauri://localhost/",
            "ipc://localhost/desktop_features",
            "asset://localhost/x",
            "mailto:someone@example.com",
            "about:config",
            "blob:https://example.com/6c1d7b2e",
            "blob:null/6c1d7b2e",
            "ftp://github.com/",
        ] {
            assert_eq!(classify(&url(refused)), Navigation::Refuse, "{refused}");
        }
    }

    /// The app's own origin is local, and a local origin is the one an app
    /// command does not check against the ACL — so it must never load here.
    #[test]
    fn the_apps_own_origins_never_load_in_the_browser() {
        for local in [
            "tauri://localhost/",
            "http://tauri.localhost/",
            "https://tauri.localhost/",
            "http://localhost:1420/",
        ] {
            assert_ne!(classify(&url(local)), Navigation::Stay, "{local}");
        }
    }

    #[test]
    fn the_hook_lets_github_through_and_hands_the_rest_off() {
        let effects = Recorder::default();
        assert!(decide_navigation(
            &url("https://github.com/o/r/pull/7"),
            &effects
        ));
        assert!(!decide_navigation(&url("https://example.com/a"), &effects));
        assert!(!decide_navigation(&url("javascript:alert(1)"), &effects));
        assert!(!decide_navigation(&url("file:///etc/passwd"), &effects));
        assert_eq!(
            *effects.external.borrow(),
            vec!["https://example.com/a".to_string()]
        );
        assert_eq!(*effects.closed.borrow(), 0);
    }

    /// Scenario: inside the page the user presses Escape that nothing on the
    /// page used; the script navigates to the reserved close address, and the
    /// hook cancels that navigation, closes the browser and opens nothing —
    /// whatever the page appended to the address.
    #[test]
    fn the_close_navigation_is_cancelled_and_closes_the_browser() {
        for close in [
            CLOSE_URL,
            "https://close.dot-agent-deck.invalid/anything?token=secret#frag",
            "https://CLOSE.dot-agent-deck.invalid/",
        ] {
            let effects = Recorder::default();
            assert!(
                !decide_navigation(&url(close), &effects),
                "{close} must be cancelled"
            );
            assert_eq!(*effects.closed.borrow(), 1, "{close}");
            assert!(
                effects.external.borrow().is_empty(),
                "{close} must open nothing"
            );
        }
    }

    #[test]
    fn the_escape_script_navigates_to_the_close_address_and_nothing_else() {
        assert_eq!(classify(&url(CLOSE_URL)), Navigation::Close);
        assert!(ESCAPE_SCRIPT.contains(&format!("var CLOSE_URL = \"{CLOSE_URL}\";")));
        assert!(ESCAPE_SCRIPT.contains("event.defaultPrevented"));
        // It talks to the app only by navigating: no IPC, no message, no fetch.
        for forbidden in [
            "__TAURI",
            "invoke",
            "postMessage",
            "fetch(",
            "XMLHttpRequest",
            "ipc",
        ] {
            assert!(
                !ESCAPE_SCRIPT.contains(forbidden),
                "the Escape script must not use {forbidden}"
            );
        }
    }

    #[test]
    fn a_new_window_never_opens_a_window() {
        let effects = Recorder::default();
        decide_new_window(&url("https://github.com/o/r/pull/7/files"), &effects);
        decide_new_window(&url("https://example.com/"), &effects);
        decide_new_window(&url("javascript:alert(1)"), &effects);
        decide_new_window(&url(CLOSE_URL), &effects);
        assert_eq!(
            *effects.navigated.borrow(),
            vec!["https://github.com/o/r/pull/7/files".to_string()]
        );
        assert_eq!(
            *effects.external.borrow(),
            vec!["https://example.com/".to_string()]
        );
        assert_eq!(*effects.closed.borrow(), 1);
    }

    #[test]
    fn only_a_github_pull_request_opens_the_browser() {
        for good in [
            "https://github.com/vfarcic/dot-agent-deck/pull/1401",
            "https://github.com/o/r/pull/7/files",
        ] {
            assert!(is_pull_request_url(&url(good)), "{good}");
        }
        for bad in [
            "https://github.com/vfarcic/dot-agent-deck/issues/1401",
            "https://github.com/vfarcic/dot-agent-deck/pull/abc",
            "https://github.com/login",
            "https://github.com.evil.example/o/r/pull/1",
            "https://gist.github.com/o/r/pull/1",
            "http://github.com/o/r/pull/1",
            "https://user:pass@github.com/o/r/pull/1",
            "https://github.com:8443/o/r/pull/1",
            "javascript:alert(1)",
        ] {
            assert!(!is_pull_request_url(&url(bad)), "{bad}");
        }
    }

    /// The profile is persistent and stable across launches: the same data
    /// directory and data store every time, under the app's data directory,
    /// and never incognito.
    #[test]
    fn the_profile_is_persistent_and_the_same_on_every_launch() {
        let data = Path::new("/home/someone/.local/share/ai.devopstoolkit.agentdeck.desktop");
        let first = profile(data);
        let second = profile(data);
        assert_eq!(first, second);
        assert_eq!(first.data_directory, data.join("pr-browser"));
        assert!(first.data_directory.starts_with(data));
        assert_eq!(first.data_store_identifier, *b"dad-pr-browser-1");
        assert!(!first.incognito);
    }

    #[test]
    fn scroll_scripts_are_fixed_and_return_nothing() {
        for scroll in [Scroll::Down, Scroll::Up, Scroll::Top, Scroll::Bottom] {
            let script = scroll_script(scroll);
            assert!(script.starts_with("window.scroll"), "{script}");
            assert!(!script.contains("return"), "{script}");
        }
        let parsed: Scroll = serde_json::from_value(serde_json::json!("bottom")).unwrap();
        assert_eq!(parsed, Scroll::Bottom);
    }

    #[test]
    fn only_macos_14_and_later_can_clear_the_profile_alone() {
        assert!(macos_major_at_least("14.0", 14));
        assert!(macos_major_at_least("15.3.1\n", 14));
        assert!(!macos_major_at_least("13.6.7", 14));
        assert!(!macos_major_at_least("", 14));
        assert!(!macos_major_at_least("garbage", 14));
        assert!(MACOS_SHARED_STORE.ends_with('.'));
    }

    #[test]
    fn impossible_bounds_are_refused() {
        let good = Bounds {
            x: 14.0,
            y: 60.0,
            width: 1200.0,
            height: 700.0,
            viewport_width: 1440.0,
            viewport_height: 920.0,
        };
        assert_eq!(good.checked(), Ok(good));
        for bad in [
            Bounds {
                x: f64::NAN,
                ..good
            },
            Bounds { y: -1.0, ..good },
            Bounds { width: 0.0, ..good },
            Bounds {
                height: f64::INFINITY,
                ..good
            },
            Bounds { x: 1e9, ..good },
            Bounds {
                viewport_width: 0.0,
                ..good
            },
            Bounds {
                viewport_height: f64::NAN,
                ..good
            },
        ] {
            assert!(bad.checked().is_err(), "{bad:?}");
        }
    }

    /// The frame is placed in proportion to the page it was measured in, so
    /// a page whose CSS pixels are not the window's (the app's zoom, or
    /// WebKitGTK's DPI scaling) still gets a browser exactly over its frame.
    #[test]
    fn the_frame_maps_onto_the_real_size_of_the_page() {
        let frame = Bounds {
            x: 15.0,
            y: 57.0,
            width: 1408.0,
            height: 805.0,
            viewport_width: 1440.0,
            viewport_height: 920.0,
        };
        // Same units: unchanged.
        assert_eq!(
            frame.onto(1440.0, 920.0),
            Placed {
                x: 15.0,
                y: 57.0,
                width: 1408.0,
                height: 805.0
            }
        );
        // CSS pixels 1.5 times the window's (150% zoom): the page measured
        // 960 CSS pixels across a 1440-pixel window.
        let zoomed = Bounds {
            x: 10.0,
            y: 38.0,
            width: 938.0,
            height: 536.0,
            viewport_width: 960.0,
            viewport_height: 613.0,
        };
        let placed = zoomed.onto(1440.0, 920.0);
        assert!(
            (placed.x - 15.0).abs() < 0.5 && (placed.width - 1407.0).abs() < 1.0,
            "{placed:?}"
        );
        // Never past the page's edge.
        let overflowing = Bounds {
            x: 1400.0,
            width: 400.0,
            ..frame
        };
        let placed = overflowing.onto(1440.0, 920.0);
        assert!(placed.x + placed.width <= 1440.0, "{placed:?}");
    }

    /// Decision 4: the files that grant permissions name only the app's own
    /// webview. No `remote` key anywhere, no wildcard, and neither the PR
    /// webview's label nor the sign-out window's.
    #[test]
    fn capability_files_grant_nothing_to_the_pr_webview() {
        fn walk(value: &serde_json::Value, path: &str, found: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, child) in map {
                        if key == "remote" {
                            found.push(format!("{path}.remote"));
                        }
                        walk(child, &format!("{path}.{key}"), found);
                    }
                }
                serde_json::Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        walk(child, &format!("{path}[{index}]"), found);
                    }
                }
                _ => {}
            }
        }

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("the capabilities directory")
            .map(|entry| entry.expect("a directory entry").path())
            .collect();
        files.sort();
        assert!(
            !files.is_empty(),
            "no capability files found in {}",
            dir.display()
        );
        for file in &files {
            let text = std::fs::read_to_string(file).expect("a readable capability file");
            let value: serde_json::Value = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{} is not JSON (a capability this test cannot read is a capability it cannot vouch for): {error}", file.display()));
            let mut remote = Vec::new();
            walk(&value, &file.display().to_string(), &mut remote);
            assert!(
                remote.is_empty(),
                "a `remote` capability lets a web page call the app: {remote:?}"
            );
            let labels = |key: &str| -> Vec<String> {
                value
                    .get(key)
                    .and_then(|labels| labels.as_array())
                    .map(|labels| {
                        labels
                            .iter()
                            .map(|label| label.as_str().expect("a label string").to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            assert!(
                labels("windows").is_empty(),
                "{}: a capability scoped to a WINDOW reaches every webview in it, the PR browser included; scope it to the main webview",
                file.display()
            );
            assert_eq!(
                labels("webviews"),
                vec![MAIN_WEBVIEW_LABEL.to_string()],
                "{}",
                file.display()
            );
        }

        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert!(
            config.pointer("/app/security/capabilities").is_none(),
            "a capability declared inline in tauri.conf.json escapes this test"
        );
        for label in [PR_WEBVIEW_LABEL, SIGN_OUT_WEBVIEW_LABEL] {
            assert_ne!(label, MAIN_WEBVIEW_LABEL);
            assert!(!label.contains('*'));
        }
    }

    /// Decision 4, driven through Tauri's own IPC: a request from the PR
    /// webview is refused BY THE ACL for an app command at a GitHub origin,
    /// and for a core or plugin command at ANY origin — while the same
    /// requests from the main webview pass the ACL and are answered, which is
    /// what shows the refusals are the ACL's and not the harness failing
    /// everything. Each refusal is matched on the ACL's own message, so a
    /// command that failed for another reason (a plugin not installed, a body
    /// it could not read) cannot pass for one. (An app command from the PR
    /// webview at a LOCAL origin is the one request Tauri's ACL does not check
    /// for an app command; that origin never loads there —
    /// `the_apps_own_origins_never_load_in_the_browser` — and every app
    /// command refuses a caller other than the main webview anyway —
    /// `every_app_command_refuses_a_webview_other_than_main`.)
    ///
    /// The clipboard plugin is a stand-in under the real one's name and
    /// command: the ACL decides by those names before any plugin code runs,
    /// and the real plugin would write the developer's clipboard on every run.
    ///
    /// Not on Windows, for the reason `Cargo.toml` gives for the `test` feature.
    #[cfg(not(windows))]
    #[test]
    fn the_pr_webview_cannot_invoke_commands() {
        use tauri::ipc::{CallbackFn, InvokeBody};
        use tauri::test::{INVOKE_KEY, MockRuntime, get_ipc_response, mock_builder};
        use tauri::webview::InvokeRequest;

        /// Stands in for every app command: the ACL decides before any
        /// command runs, and it decides the same way for all of them.
        #[tauri::command]
        fn probe() -> &'static str {
            "reached"
        }

        /// The clipboard plugin's `write_text`, writing nothing.
        #[tauri::command]
        fn write_text(text: String) -> usize {
            text.len()
        }

        let clipboard = tauri::plugin::Builder::<MockRuntime>::new("clipboard-manager")
            .invoke_handler(tauri::generate_handler![write_text])
            .build();
        let app = mock_builder()
            .plugin(clipboard)
            .invoke_handler(tauri::generate_handler![probe])
            .build(tauri::generate_context!())
            .expect("a mock app");
        let main =
            tauri::WebviewWindowBuilder::new(&app, MAIN_WEBVIEW_LABEL, WebviewUrl::default())
                .build()
                .expect("the main webview");
        let pr = main
            .as_ref()
            .window()
            .add_child(
                WebviewBuilder::new(
                    PR_WEBVIEW_LABEL,
                    WebviewUrl::External(url("https://github.com/o/r/pull/7")),
                ),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(100.0, 100.0),
            )
            .expect("the PR webview");

        let request = |cmd: &str, origin: &str, body: serde_json::Value| InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: url(origin),
            body: InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        };
        let local = if cfg!(any(windows, target_os = "android")) {
            "http://tauri.localhost"
        } else {
            "tauri://localhost"
        };
        // `get_ipc_response` takes anything that is `AsRef<Webview>`, which a
        // window's webview is and a child webview is not.
        struct Child<'a>(&'a tauri::Webview<MockRuntime>);
        impl AsRef<tauri::Webview<MockRuntime>> for Child<'_> {
            fn as_ref(&self) -> &tauri::Webview<MockRuntime> {
                self.0
            }
        }
        let pr = Child(&pr);

        let none = serde_json::json!({});
        let clipboard_text = serde_json::json!({ "text": "from a test" });
        let listen = serde_json::json!({
            "event": "pr-browser-test",
            "target": { "kind": "Any" },
            "handler": 7,
        });
        let emit = serde_json::json!({ "event": "pr-browser-test", "payload": null });
        let calls = [
            ("probe", &none),
            ("plugin:app|version", &none),
            ("plugin:clipboard-manager|write_text", &clipboard_text),
            ("plugin:event|listen", &listen),
            ("plugin:event|emit", &emit),
        ];

        /// The ACL's refusal: Tauri's `resolve_access_message` in a debug
        /// build ("… not allowed on …" / "… not allowed. …") and "Command …
        /// not allowed by ACL" in a release one.
        fn refused_by_acl<T>(answer: &Result<T, serde_json::Value>) -> bool {
            matches!(answer, Err(serde_json::Value::String(message)) if message.contains("not allowed"))
        }

        // The control: the main webview, at the app's own origin, reaches
        // every one of them and each is answered.
        for (cmd, body) in calls {
            let answer = get_ipc_response(&main, request(cmd, local, body.clone()));
            assert!(answer.is_ok(), "main webview, {cmd}: {answer:?}");
        }

        // The PR webview at GitHub's origin reaches none of them.
        let github = "https://github.com/o/r/pull/7";
        for (cmd, body) in calls {
            let answer = get_ipc_response(&pr, request(cmd, github, body.clone()));
            assert!(
                refused_by_acl(&answer),
                "PR webview at GitHub, {cmd}: {answer:?}"
            );
        }

        // Nor a core or plugin command at the app's own origin: no capability
        // names this webview, which is what scoping to the main WEBVIEW rather
        // than the main window buys. (`probe` at this origin is the case the
        // ACL does not check, so it is not in this list — see above.)
        for (cmd, body) in &calls[1..] {
            let answer = get_ipc_response(&pr, request(cmd, local, (*body).clone()));
            assert!(
                refused_by_acl(&answer),
                "PR webview at the app's origin, {cmd}: {answer:?}"
            );
        }

        // The one internal command Tauri exempts from the ACL is the channel
        // fetch (`plugin:__TAURI_CHANNEL__|fetch`), answered for whichever
        // webview the channel was made for; the PR webview has none, so it
        // gets the command's own error, and the ACL does not refuse it. Pinned
        // here so a Tauri that closes or widens the exemption is noticed.
        let fetch = get_ipc_response(
            &pr,
            request("plugin:__TAURI_CHANNEL__|fetch", github, none.clone()),
        );
        assert!(fetch.is_err() && !refused_by_acl(&fetch), "{fetch:?}");
    }

    /// A function body's first statement: past blank lines, comments and
    /// `use` items, which run nothing.
    fn first_statement(body: &str) -> &str {
        let mut rest = body;
        loop {
            rest = rest.trim_start();
            if rest.starts_with("//") {
                rest = rest.split_once('\n').map_or("", |(_, tail)| tail);
            } else if rest.starts_with("use ") {
                rest = rest.split_once(';').map_or("", |(_, tail)| tail);
            } else {
                return rest;
            }
        }
    }

    #[test]
    fn the_first_statement_skips_only_what_runs_nothing() {
        assert_eq!(
            first_statement(
                "\n    // why\n    use a::b;\n    use c::{d, e};\n    ensure_main_webview(&webview)?;\n    go()"
            ),
            "ensure_main_webview(&webview)?;\n    go()"
        );
        assert!(
            !first_statement("\n    let x = run();\n    ensure_main_webview(&webview)?;")
                .starts_with("ensure_main_webview")
        );
    }

    /// The app's own commands carry a check of their own: each one refuses a
    /// caller that is not the main webview, as its first statement (after
    /// `use` items), so nothing a command does runs for the PR webview — even
    /// the request the ACL does not check (an app command at a local origin).
    /// A text scan of every `#[tauri::command]` in `lib.rs`, so a new command
    /// without the check, or with work before it, fails here.
    #[test]
    fn every_app_command_refuses_a_webview_other_than_main() {
        let source = include_str!("lib.rs");
        let mut commands = 0;
        for (index, _) in source.match_indices("#[tauri::command]") {
            let rest = &source[index..];
            let open = rest.find('{').expect("a command body");
            let name = rest[..open]
                .split("fn ")
                .nth(1)
                .and_then(|tail| tail.split(['(', '<']).next())
                .expect("a command name")
                .trim()
                .to_string();
            // The body runs to the first line that closes the function.
            let body_end = rest[open..]
                .find("\n}\n")
                .map(|end| open + end)
                .unwrap_or(rest.len());
            let body = &rest[open + 1..body_end];
            assert!(
                first_statement(body).starts_with("ensure_main_webview(&webview)"),
                "`{name}` does not refuse a webview other than the main one as its first statement"
            );
            commands += 1;
        }
        assert!(
            commands > 30,
            "the scan found only {commands} commands; it is not reading lib.rs as written"
        );
    }

    /// A recorder whose hand-offs go through a gate, on a clock the test sets.
    struct Gated<'a> {
        gate: &'a HandOffGate,
        now: std::cell::Cell<Instant>,
        launched: RefCell<Vec<String>>,
    }

    impl Effects for Gated<'_> {
        fn open_external(&self, url: &Url) {
            if self.gate.admit(self.now.get()) == Admission::Launch {
                self.launched.borrow_mut().push(url.to_string());
                self.gate.finished();
            }
        }
        fn close(&self) {}
        fn navigate(&self, _url: &Url) {}
    }

    /// Scenario: a page's script navigates off GitHub twenty times in a
    /// second, by navigation and by new-window requests; the system browser
    /// opens once. Two seconds later the user clicks a link off GitHub, and
    /// that one opens.
    #[test]
    fn a_burst_of_hand_offs_opens_the_system_browser_once() {
        let gate = HandOffGate::new(HAND_OFF_INTERVAL);
        let start = Instant::now();
        let effects = Gated {
            gate: &gate,
            now: std::cell::Cell::new(start),
            launched: RefCell::new(Vec::new()),
        };
        for step in 0..20u64 {
            effects.now.set(start + Duration::from_millis(step * 50));
            let target = url(&format!("https://example.com/{step}"));
            if step % 2 == 0 {
                assert!(!decide_navigation(&target, &effects));
            } else {
                decide_new_window(&target, &effects);
            }
        }
        assert_eq!(*effects.launched.borrow(), vec!["https://example.com/0"]);

        effects.now.set(start + Duration::from_secs(3));
        assert!(!decide_navigation(&url("https://docs.rs/tauri"), &effects));
        assert_eq!(
            *effects.launched.borrow(),
            vec!["https://example.com/0", "https://docs.rs/tauri"]
        );
    }

    #[test]
    fn the_gate_is_single_flight_and_reports_a_burst_once() {
        let gate = HandOffGate::new(Duration::from_millis(1500));
        let start = Instant::now();
        assert_eq!(gate.admit(start), Admission::Launch);
        // Still launching, however long that takes: nothing else goes.
        assert_eq!(
            gate.admit(start + Duration::from_secs(10)),
            Admission::Drop { report: true }
        );
        assert_eq!(
            gate.admit(start + Duration::from_secs(11)),
            Admission::Drop { report: false }
        );
        gate.finished();
        let later = start + Duration::from_secs(12);
        assert_eq!(gate.admit(later), Admission::Launch);
        gate.finished();
        // Within the interval of the last launch: dropped, and reported again
        // because a launch happened since the last report.
        assert_eq!(
            gate.admit(later + Duration::from_millis(1499)),
            Admission::Drop { report: true }
        );
        assert_eq!(
            gate.admit(later + Duration::from_millis(1500)),
            Admission::Launch
        );
    }

    #[test]
    fn while_clearing_nothing_loads_but_the_empty_page() {
        assert!(decide_navigation_while_clearing(&url("about:blank")));
        for refused in [
            "https://github.com/o/r/pull/7",
            "https://example.com/",
            CLOSE_URL,
            "about:srcdoc",
        ] {
            assert!(
                !decide_navigation_while_clearing(&url(refused)),
                "{refused}"
            );
        }
    }

    /// How a fake step answers.
    #[derive(Clone)]
    enum FakeAnswer {
        After(Duration, Result<(), String>),
        Never,
        Dropped,
    }

    /// The sign-out host, recording each step in order. Its answers come from
    /// tasks on tokio's paused clock, so "late" costs no wall time.
    struct FakeSignOut {
        browser: Browser,
        leave: FakeAnswer,
        clear: FakeAnswer,
        clearing: &'static AtomicBool,
        log: Arc<Mutex<Vec<String>>>,
        kept: Mutex<Vec<oneshot::Sender<Result<(), String>>>>,
    }

    impl FakeSignOut {
        fn new(browser: Browser, leave: FakeAnswer, clear: FakeAnswer) -> Self {
            Self {
                browser,
                leave,
                clear,
                clearing: Box::leak(Box::new(AtomicBool::new(false))),
                log: Arc::new(Mutex::new(Vec::new())),
                kept: Mutex::new(Vec::new()),
            }
        }

        fn note(&self, step: impl Into<String>) {
            self.log.lock().unwrap().push(step.into());
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn answer(&self, how: &FakeAnswer, done: &'static str) -> Answer {
            let (sender, answer) = oneshot::channel();
            match how.clone() {
                FakeAnswer::After(delay, result) => {
                    let log = self.log.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        log.lock().unwrap().push(done.to_string());
                        let _ = sender.send(result);
                    });
                }
                FakeAnswer::Never => self.kept.lock().unwrap().push(sender),
                FakeAnswer::Dropped => drop(sender),
            }
            answer
        }
    }

    impl SignOutHost for FakeSignOut {
        fn browser(&self) -> Browser {
            self.browser.clone()
        }
        fn leave_page(&self) -> Answer {
            assert!(
                self.clearing.load(Ordering::SeqCst),
                "left the page before raising the flag"
            );
            self.note("leave");
            self.answer(&self.leave, "left")
        }
        fn open_scratch(&self) -> Result<(), String> {
            self.note("scratch:open");
            Ok(())
        }
        fn clear(&self, with: ClearWith) -> Answer {
            assert!(
                self.clearing.load(Ordering::SeqCst),
                "cleared without raising the flag"
            );
            self.note(format!("clear:{with:?}"));
            self.answer(&self.clear, "cleared")
        }
        fn close_scratch(&self) {
            self.note("scratch:close");
        }
        fn resume(&self, url: Url) {
            assert!(
                !self.clearing.load(Ordering::SeqCst),
                "resumed with the flag still raised"
            );
            self.note(format!("resume:{url}"));
        }
        fn close_browser(&self) {
            self.note("browser:close");
        }
    }

    const PR: &str = "https://github.com/o/r/pull/7";
    const TIMEOUTS: SignOutTimeouts = SignOutTimeouts {
        leave: LEAVE_TIMEOUT,
        clear: CLEAR_TIMEOUT,
    };

    fn open_browser() -> Browser {
        Browser::Open {
            resume: Some(url(PR)),
        }
    }

    /// Runs sign-out as its own task and looks at it one second in, while the
    /// platform has not yet confirmed a clear it was asked for five seconds
    /// ago: returns what the log held then, whether the session was free
    /// (an `open` would not have waited) and the final result.
    async fn run_with_a_look(
        host: Arc<FakeSignOut>,
    ) -> (Vec<String>, bool, Result<(), String>, Vec<String>) {
        let session: &'static tokio::sync::Mutex<()> =
            Box::leak(Box::new(tokio::sync::Mutex::const_new(())));
        let running = {
            let host = host.clone();
            tokio::spawn(
                async move { sign_out_with(&*host, session, host.clearing, TIMEOUTS).await },
            )
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        let midway = host.log();
        let session_free = session.try_lock().is_ok();
        let result = running.await.expect("sign-out ran");
        // Raised afterwards exactly when the platform never confirmed a clear
        // it was asked for — it may still be deleting (`Raised::lower_when`).
        let unconfirmed =
            result == Err(CLEAR_UNCONFIRMED.into()) && matches!(host.clear, FakeAnswer::Never);
        assert_eq!(
            host.clearing.load(Ordering::SeqCst),
            unconfirmed,
            "the flag is raised after sign-out only while a clear is unconfirmed"
        );
        assert!(session.try_lock().is_ok(), "the session stayed held");
        (midway, session_free, result, host.log())
    }

    /// Scenario: with a pull request open, the user signs out; the platform
    /// takes five seconds to confirm the delete. The page is emptied first,
    /// nothing reopens and no pull request can open until the confirmation,
    /// and only then does the page come back — signed out.
    #[tokio::test(start_paused = true)]
    async fn sign_out_with_a_browser_open_reopens_only_after_the_clear_completes() {
        let host = Arc::new(FakeSignOut::new(
            open_browser(),
            FakeAnswer::After(Duration::from_millis(100), Ok(())),
            FakeAnswer::After(Duration::from_secs(5), Ok(())),
        ));
        let (midway, session_free, result, log) = run_with_a_look(host).await;
        assert_eq!(midway, ["leave", "left", "clear:Browser"]);
        assert!(!session_free, "a pull request could open mid-clear");
        assert_eq!(result, Ok(()));
        assert_eq!(
            log,
            [
                "leave",
                "left",
                "clear:Browser",
                "cleared",
                &format!("resume:{PR}")
            ]
        );
    }

    /// Scenario: with no pull request open, the user signs out; the hidden
    /// webview stays open until the platform confirms the delete five seconds
    /// later, and only then is "signed out" answered.
    #[tokio::test(start_paused = true)]
    async fn sign_out_with_no_browser_closes_the_hidden_webview_only_after_the_clear_completes() {
        let host = Arc::new(FakeSignOut::new(
            Browser::Closed,
            FakeAnswer::Never,
            FakeAnswer::After(Duration::from_secs(5), Ok(())),
        ));
        let (midway, session_free, result, log) = run_with_a_look(host).await;
        assert_eq!(midway, ["scratch:open", "clear:Scratch"]);
        assert!(!session_free, "a pull request could open mid-clear");
        assert_eq!(result, Ok(()));
        assert_eq!(
            log,
            ["scratch:open", "clear:Scratch", "cleared", "scratch:close"]
        );
    }

    /// A delete the platform reports as failed is an error, and the open
    /// browser is closed rather than put back on a profile that may still
    /// hold the sign-in.
    #[tokio::test(start_paused = true)]
    async fn a_failed_clear_is_reported_and_nothing_reopens() {
        let host = Arc::new(FakeSignOut::new(
            open_browser(),
            FakeAnswer::After(Duration::ZERO, Ok(())),
            FakeAnswer::After(Duration::from_secs(2), Err("disk full".into())),
        ));
        let (_, _, result, log) = run_with_a_look(host).await;
        assert_eq!(result, Err("disk full".into()));
        assert_eq!(
            log,
            ["leave", "left", "clear:Browser", "cleared", "browser:close"]
        );

        let host = Arc::new(FakeSignOut::new(
            Browser::Closed,
            FakeAnswer::Never,
            FakeAnswer::After(Duration::from_secs(2), Err("disk full".into())),
        ));
        let (_, _, result, log) = run_with_a_look(host).await;
        assert_eq!(result, Err("disk full".into()));
        assert_eq!(
            log,
            ["scratch:open", "clear:Scratch", "cleared", "scratch:close"]
        );
    }

    /// Time passing is never taken as the delete having run: a platform that
    /// never confirms makes sign-out fail, on either path.
    #[tokio::test(start_paused = true)]
    async fn a_clear_that_is_never_confirmed_is_a_failure_not_a_success() {
        let host = Arc::new(FakeSignOut::new(
            open_browser(),
            FakeAnswer::After(Duration::ZERO, Ok(())),
            FakeAnswer::Never,
        ));
        let (_, _, result, log) = run_with_a_look(host).await;
        assert_eq!(result, Err(CLEAR_UNCONFIRMED.into()));
        assert_eq!(log, ["leave", "left", "clear:Browser", "browser:close"]);

        let host = Arc::new(FakeSignOut::new(
            Browser::Closed,
            FakeAnswer::Never,
            FakeAnswer::Never,
        ));
        let (_, _, result, log) = run_with_a_look(host).await;
        assert_eq!(result, Err(CLEAR_UNCONFIRMED.into()));
        assert_eq!(log, ["scratch:open", "clear:Scratch", "scratch:close"]);

        // A platform that drops its callback without calling it is no answer either.
        let host = Arc::new(FakeSignOut::new(
            Browser::Closed,
            FakeAnswer::Never,
            FakeAnswer::Dropped,
        ));
        let (_, _, result, _) = run_with_a_look(host).await;
        assert_eq!(result, Err("the system gave no answer.".into()));
    }

    /// A page that does not empty in time is never cleared under: sign-out
    /// fails, closes the browser and asks the platform for nothing.
    #[tokio::test(start_paused = true)]
    async fn a_page_that_does_not_leave_is_never_cleared_under() {
        let host = Arc::new(FakeSignOut::new(
            open_browser(),
            FakeAnswer::Never,
            FakeAnswer::After(Duration::ZERO, Ok(())),
        ));
        let (_, _, result, log) = run_with_a_look(host).await;
        assert_eq!(
            result,
            Err("the pull request page did not close in time.".into())
        );
        assert_eq!(log, ["leave", "browser:close"]);
    }

    /// Scenario: the platform confirms nothing within the timeout, then
    /// finishes the delete later. Until it does, a pull request is refused
    /// with a message saying why, and so is a second sign-out; once the late
    /// completion arrives, both may run again.
    #[tokio::test(start_paused = true)]
    async fn open_is_refused_after_an_unconfirmed_clear_until_the_late_completion() {
        let host = Arc::new(FakeSignOut::new(
            open_browser(),
            FakeAnswer::After(Duration::ZERO, Ok(())),
            FakeAnswer::Never,
        ));
        let session: &'static tokio::sync::Mutex<()> =
            Box::leak(Box::new(tokio::sync::Mutex::const_new(())));
        let result = sign_out_with(&*host, session, host.clearing, TIMEOUTS).await;
        assert_eq!(result, Err(CLEAR_UNCONFIRMED.into()));
        assert!(session.try_lock().is_ok(), "the session stayed held");
        assert_eq!(admit_open(host.clearing), Err(STILL_CLEARING.into()));
        assert_eq!(
            sign_out_with(&*host, session, host.clearing, TIMEOUTS).await,
            Err(STILL_CLEARING.into()),
            "a second clear started over one still running"
        );

        let late = host.kept.lock().unwrap().remove(0);
        // Long after the timeout: the platform's own completion.
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(admit_open(host.clearing), Err(STILL_CLEARING.into()));
        late.send(Ok(())).unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(admit_open(host.clearing), Ok(()));
    }

    /// Scenario: the user presses Open in browser and the system browser
    /// refuses to start. The launch's own error comes back and the in-app
    /// page is not closed; a launch that works closes it, after.
    #[test]
    fn a_failed_hand_off_keeps_the_page_and_a_working_one_closes_it_after() {
        let page = url("https://github.com/o/r/pull/7/files");
        let steps = RefCell::new(Vec::new());
        let result = handoff_page(
            &page,
            |opened| {
                steps.borrow_mut().push(format!("open:{opened}"));
                Err("no browser".to_string())
            },
            || steps.borrow_mut().push("close".into()),
        );
        assert_eq!(result, Err("no browser".into()));
        assert_eq!(*steps.borrow(), [format!("open:{page}")]);

        steps.borrow_mut().clear();
        let result = handoff_page(
            &page,
            |opened| {
                steps.borrow_mut().push(format!("open:{opened}"));
                Ok(())
            },
            || steps.borrow_mut().push("close".into()),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(*steps.borrow(), [format!("open:{page}"), "close".into()]);
    }

    /// The address check stays on the hand-off: a page that is not GitHub's
    /// on `https` is neither handed to the system browser nor closed.
    #[test]
    fn the_hand_off_refuses_an_address_off_github() {
        for refused in [
            "https://example.com/",
            "http://github.com/o/r/pull/7",
            "about:blank",
        ] {
            let steps = RefCell::new(Vec::new());
            let result = handoff_page(
                &url(refused),
                |_| {
                    steps.borrow_mut().push("open");
                    Ok(())
                },
                || steps.borrow_mut().push("close"),
            );
            assert!(result.is_err(), "{refused}");
            assert!(steps.borrow().is_empty(), "{refused}");
        }
    }

    /// Scenario: the page's Escape asks for a close, and before the spawned
    /// close runs the app opens another pull request. The stale close does
    /// nothing to the new page; a close asked under the new one closes it.
    #[tokio::test]
    async fn a_close_from_an_earlier_open_does_not_close_the_new_one() {
        let session = tokio::sync::Mutex::const_new(());
        let generations = Generations::new();
        let closed = RefCell::new(Vec::new());

        let first = generations.begin();
        let asked_under_first = generations.current();
        let second = generations.begin();
        assert_ne!(first, second);

        let ran = in_session(&session, &generations, asked_under_first, || {
            closed.borrow_mut().push(asked_under_first)
        })
        .await;
        assert!(!ran);
        assert!(
            closed.borrow().is_empty(),
            "a stale close closed the new page"
        );

        let ran = in_session(&session, &generations, generations.current(), || {
            closed.borrow_mut().push(second)
        })
        .await;
        assert!(ran);
        assert_eq!(*closed.borrow(), [second]);
    }

    /// A page effect waits for an open in progress, and then sees its
    /// generation: it cannot slip in between the open's check and its page.
    #[tokio::test(start_paused = true)]
    async fn a_page_effect_waits_for_an_open_that_holds_the_session() {
        let session: &'static tokio::sync::Mutex<()> =
            Box::leak(Box::new(tokio::sync::Mutex::const_new(())));
        let generations: &'static Generations = Box::leak(Box::new(Generations::new()));
        let first = generations.begin();
        let opening = session.lock().await;
        let effect =
            tokio::spawn(async move { in_session(session, generations, first, || {}).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        generations.begin();
        drop(opening);
        assert!(
            !effect.await.unwrap(),
            "the effect ran against the newer open"
        );
    }
}
