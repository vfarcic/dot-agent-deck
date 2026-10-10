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
//!   refused, from any origin. `capability_files_grant_nothing_to_the_pr_webview`
//!   pins the files; `the_pr_webview_cannot_invoke_commands` drives Tauri's IPC.
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

use serde::Deserialize;
use tauri::webview::{NewWindowResponse, WebviewBuilder};
use tauri::{AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Runtime, Url, WebviewUrl};

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
/// Escape), so the app returns to the screen under it.
pub const CLOSED_EVENT: &str = "pr-browser://closed";

/// The profile's directory under the app's data directory.
pub const PROFILE_DIR: &str = "pr-browser";

/// The macOS 14+ data store the PR webview uses — fixed, so every launch opens
/// the same store and a sign-in survives a restart. Sixteen bytes of ASCII so
/// the value is recognisable in a debugger; it is an identifier, not a secret.
pub const DATA_STORE_ID: [u8; 16] = *b"dad-pr-browser-1";

/// How long the hidden sign-out window lives after asking for the clear, so
/// the platform's asynchronous delete runs before its webview goes away.
const SIGN_OUT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

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
        open_in_system_browser(url);
    }

    fn close(&self) {
        // Never inside the hook: the webview is in the middle of deciding a
        // navigation, and destroying it there would pull it out from under
        // its own callback.
        let app = self.0.clone();
        tauri::async_runtime::spawn(async move {
            close_browser(&app);
            let _ = app.emit_to(MAIN_WEBVIEW_LABEL, CLOSED_EVENT, ());
        });
    }

    fn navigate(&self, url: &Url) {
        let app = self.0.clone();
        let url = url.clone();
        tauri::async_runtime::spawn(async move {
            if let Some(webview) = app.get_webview(PR_WEBVIEW_LABEL) {
                let _ = webview.navigate(url);
            }
        });
    }
}

/// Hands `url` to the operating system's default browser. Only `http(s)`
/// ever reaches it — the callers classify first.
fn open_in_system_browser(url: &Url) {
    if !matches!(url.scheme(), "https" | "http") {
        return;
    }
    if let Err(error) = open::that_detached(url.as_str()) {
        eprintln!("dot-agent-deck-desktop: could not open the system browser: {error}");
    }
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
        .on_navigation(move |url| decide_navigation(url, &on_navigation))
        .on_new_window(move |url, _features| {
            decide_new_window(&url, &on_new_window);
            NewWindowResponse::Deny
        })
        .incognito(profile.incognito)
        .data_directory(profile.data_directory.clone())
        .data_store_identifier(profile.data_store_identifier)
}

/// Opens the browser on `url` over `bounds`, or moves an open one there.
pub async fn open<R: Runtime>(app: &AppHandle<R>, url: &str, bounds: Bounds) -> Result<(), String> {
    let bounds = bounds.checked()?;
    let url = Url::parse(url).map_err(|_| "That pull request address is not a URL.".to_string())?;
    if !is_pull_request_url(&url) {
        return Err("Only a pull request on github.com opens in the app.".into());
    }
    if let Some(webview) = app.get_webview(PR_WEBVIEW_LABEL) {
        webview.navigate(url).map_err(|error| error.to_string())?;
        place(&webview, bounds)?;
        let _ = webview.show();
        let _ = webview.set_focus();
        return Ok(());
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
    Ok(())
}

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

/// Open in browser: the page on screen in the system browser, then close.
pub fn open_external<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let webview = open_webview(app)?;
    let url = webview.url().map_err(|error| error.to_string())?;
    if !matches!(classify(&url), Navigation::Stay) || !matches!(url.scheme(), "https") {
        return Err("The page on screen has no address the system browser can open.".into());
    }
    open_in_system_browser(&url);
    close_browser(app);
    Ok(())
}

/// The toolbar's Close, `Escape` in the app, and voice's "close".
pub fn close<R: Runtime>(app: &AppHandle<R>) {
    close_browser(app);
}

/// Settings → Sign out of GitHub: clears the PR browser's profile — cookies,
/// storage and cache — so the next PR opens signed out. Refused on macOS
/// before 14, where that profile is the app's own (`MACOS_SHARED_STORE`).
///
/// With a PR open, its own webview clears the store and reloads. With none
/// open, a hidden window is opened on the same profile only to clear it, and
/// closed again once the platform's asynchronous delete has had time to run.
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
    if let Some(webview) = app.get_webview(PR_WEBVIEW_LABEL) {
        webview
            .clear_all_browsing_data()
            .map_err(|error| error.to_string())?;
        let _ = webview.reload();
        return Ok(());
    }
    let profile = app_profile(app)?;
    let blank = WebviewUrl::External(Url::parse("about:blank").expect("a constant URL"));
    let window = tauri::window::WindowBuilder::new(app, SIGN_OUT_WEBVIEW_LABEL)
        .visible(false)
        .build()
        .map_err(|error| format!("Could not open the sign-in profile: {error}"))?;
    let webview = window
        .add_child(
            builder(app, SIGN_OUT_WEBVIEW_LABEL, blank, &profile),
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(1.0, 1.0),
        )
        .map_err(|error| format!("Could not open the sign-in profile: {error}"))?;
    let cleared = webview
        .clear_all_browsing_data()
        .map_err(|error| error.to_string());
    tokio::time::sleep(SIGN_OUT_GRACE).await;
    let _ = window.close();
    cleared
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
    /// webview is refused for an app command at a GitHub origin, and for a
    /// core command at ANY origin — while the same requests from the main
    /// webview are answered, which is what shows the refusals are the ACL's and
    /// not the harness failing everything. (An app command from the PR webview
    /// at a LOCAL origin is the one request Tauri's ACL does not check; that
    /// origin never loads there — `the_apps_own_origins_never_load_in_the_browser`
    /// — and every app command refuses a caller other than the main webview
    /// anyway — `every_app_command_refuses_a_webview_other_than_main`.)
    ///
    /// Not on Windows, for the reason `Cargo.toml` gives for the `test` feature.
    #[cfg(not(windows))]
    #[test]
    fn the_pr_webview_cannot_invoke_commands() {
        use tauri::ipc::{CallbackFn, InvokeBody};
        use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder};
        use tauri::webview::InvokeRequest;

        /// Stands in for every app command: the ACL decides before any
        /// command runs, and it decides the same way for all of them.
        #[tauri::command]
        fn probe() -> &'static str {
            "reached"
        }

        let app = mock_builder()
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

        let request = |cmd: &str, origin: &str| InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: url(origin),
            body: InvokeBody::default(),
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
        struct Child<'a>(&'a tauri::Webview<tauri::test::MockRuntime>);
        impl AsRef<tauri::Webview<tauri::test::MockRuntime>> for Child<'_> {
            fn as_ref(&self) -> &tauri::Webview<tauri::test::MockRuntime> {
                self.0
            }
        }
        let pr = Child(&pr);

        // The control: the main webview reaches both.
        assert!(get_ipc_response(&main, request("plugin:app|version", local)).is_ok());
        assert!(get_ipc_response(&main, request("probe", local)).is_ok());

        // The PR webview at GitHub's origin reaches neither.
        let github = "https://github.com/o/r/pull/7";
        assert!(get_ipc_response(&pr, request("probe", github)).is_err());
        assert!(get_ipc_response(&pr, request("plugin:app|version", github)).is_err());
        assert!(
            get_ipc_response(&pr, request("plugin:clipboard-manager|write_text", github)).is_err()
        );
        assert!(get_ipc_response(&pr, request("plugin:event|listen", github)).is_err());

        // Nor a core command at the app's own origin: no capability names this
        // webview, which is what scoping to the main WEBVIEW rather than the
        // main window buys.
        assert!(get_ipc_response(&pr, request("plugin:app|version", local)).is_err());
    }

    /// The app's own commands carry a check of their own: each one refuses a
    /// caller that is not the main webview, so even the request the ACL does
    /// not check (an app command at a local origin) is refused from the PR
    /// webview. A text scan of every `#[tauri::command]` in `lib.rs`, so a new
    /// command without the check fails here.
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
            let body = &rest[open..body_end];
            assert!(
                body.contains("ensure_main_webview(&webview)"),
                "`{name}` does not refuse a webview other than the main one"
            );
            commands += 1;
        }
        assert!(
            commands > 30,
            "the scan found only {commands} commands; it is not reading lib.rs as written"
        );
    }
}
