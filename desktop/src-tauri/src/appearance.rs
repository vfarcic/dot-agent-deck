//! The pre-paint channel for the stored appearance choice (issue #845).
//!
//! PRD #743 applies the Light/Dark choice from the webview, in a React effect.
//! That is correct and it is also, unavoidably, *late*: the document only
//! arrives once `desktop_get_settings` has answered, and the webview has
//! painted long before then. Until it does, the document root carries no
//! `data-theme`, and `styles.css` reads exactly that state as "let the OS
//! decide" — so a user who chose Dark on a light machine watches a light
//! window turn dark. Nothing does the wrong thing; the wrong thing is simply
//! what the document's *initial* state means.
//!
//! [`settings::load_snapshot`] is synchronous, so this process already knows
//! the answer before the window exists. This module puts it on the document
//! root through a Tauri initialization script, which the webview runs after the
//! global object is created and **before it parses the document** — so the
//! first painted frame already carries the choice, with no blank frame to hide
//! the gap and no second source of truth to disagree with the frontend.
//!
//! # Why a plugin rather than building the window in code
//!
//! An initialization script can also be attached to a `WebviewWindowBuilder`,
//! which would mean `"create": false` in `tauri.conf.json` and moving window
//! creation into `setup()`. A plugin's `js_init_script` reaches every webview
//! the app creates without touching how any of them are declared — and a window
//! that fails to build is an app with no window, which nothing in this repo's
//! CI would catch (#823). Same first frame, none of that risk.
//!
//! # What this deliberately does not own
//!
//! The frontend stays the source of truth the moment it has read the document:
//! this writes the root once, before anything else runs, and never again. It
//! does not observe changes, and a choice made in the settings sheet is applied
//! by `lib/appearance.ts` exactly as before. The mode is read once, when the
//! plugin is constructed, which is the same startup in which the only declared
//! window is created.
//!
//! `data-theme` and its two values are shared with `desktop/src/styles.css` and
//! `desktop/src/lib/appearance.ts`; `grep -rn data-theme desktop/` finds all of
//! them.

use tauri::Runtime;
use tauri::plugin::{Builder, TauriPlugin};

use crate::settings::{self, AppearanceMode};

/// The attribute `styles.css` reads, and the one `lib/appearance.ts` writes.
const THEME_ATTRIBUTE: &str = "data-theme";

/// What to run before the document is parsed, or `None` when there is nothing
/// to say.
///
/// **System yields no script at all**, for the same reason
/// `applyAppearance("system")` *removes* the attribute rather than writing one:
/// the dark rule is scoped to `:root:not([data-theme="light"])`, so an absent
/// attribute is precisely what lets the OS decide, in both directions and live.
/// A fresh document has no attribute, so following the OS is already what the
/// first frame does — this exists only for the case where it would be wrong.
///
/// The token is interpolated from [`AppearanceMode::as_str`], whose three arms
/// are `&'static str` literals, so nothing user-supplied reaches the script
/// body. An unrecognised token in `desktop.toml` never becomes one either:
/// `AppearanceMode`'s deserializer maps it to the default.
///
/// The script tolerates being run before `<html>` itself has been parsed. At
/// document-start the browsers this app ships on do already expose
/// `document.documentElement`, but that is an implementation detail of each
/// engine rather than something the injection point guarantees, and the cost of
/// not depending on it is four lines.
pub fn pre_paint_script(mode: AppearanceMode) -> Option<String> {
    match mode {
        AppearanceMode::System => None,
        AppearanceMode::Light | AppearanceMode::Dark => Some(format!(
            r#"(function () {{
  function write() {{
    var root = document.documentElement;
    if (!root) return false;
    root.setAttribute("{attribute}", "{mode}");
    return true;
  }}
  if (write()) return;
  var observer = new MutationObserver(function () {{
    if (write()) observer.disconnect();
  }});
  observer.observe(document, {{ childList: true }});
}})();"#,
            attribute = THEME_ATTRIBUTE,
            mode = mode.as_str(),
        )),
    }
}

/// Register the pre-paint channel, reading the stored choice as it goes.
///
/// Called from `run()` before `build()`, which is what puts the script in the
/// plugin store ahead of the config-declared window being created.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    plugin_for(settings::load_snapshot().settings.appearance.mode)
}

/// [`init`] with the mode supplied, so the wiring is testable without a
/// settings file.
fn plugin_for<R: Runtime>(mode: AppearanceMode) -> TauriPlugin<R> {
    let builder = Builder::new("appearance");
    match pre_paint_script(mode) {
        // Registered either way. A plugin carrying no script is inert, and that
        // is a cheaper thing to reason about than a conditional `.plugin(..)`
        // in the builder chain.
        Some(script) => builder.js_init_script(script).build(),
        None => builder.build(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Wry;
    use tauri::plugin::Plugin;

    /// Every mode, so a fourth one cannot be added without deciding this.
    const MODES: [AppearanceMode; 3] = [
        AppearanceMode::System,
        AppearanceMode::Light,
        AppearanceMode::Dark,
    ];

    #[test]
    fn system_emits_no_script_because_an_absent_attribute_is_the_answer() {
        assert_eq!(pre_paint_script(AppearanceMode::System), None);
    }

    #[test]
    fn an_explicit_choice_writes_its_own_token_and_no_other() {
        for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
            let script = pre_paint_script(mode).expect("an explicit choice has a script");
            assert!(
                script.contains(&format!(r#""{THEME_ATTRIBUTE}", "{}""#, mode.as_str())),
                "{mode:?} must set {THEME_ATTRIBUTE} to its own token: {script}"
            );
            for other in MODES {
                if other == mode {
                    continue;
                }
                assert!(
                    !script.contains(other.as_str()),
                    "{mode:?}'s script must not mention {other:?}: {script}"
                );
            }
        }
    }

    /// The ordering seam, asserted where it is actually decided.
    ///
    /// [`Plugin::initialization_script`] is what Tauri collects from the plugin
    /// store when it builds a webview, and an initialization script runs before
    /// the document is parsed. So a mode reaching this method is a mode that
    /// reaches the root before the first paint. What no test in this repo can
    /// assert is the paint itself (#823: no driver-level tier; #836: jsdom has
    /// no layout) — that half is a manual check against a real window.
    #[test]
    fn the_plugin_hands_the_webview_the_script_for_the_stored_mode() {
        for mode in MODES {
            let plugin: TauriPlugin<Wry> = plugin_for(mode);
            assert_eq!(
                plugin.initialization_script(),
                pre_paint_script(mode),
                "{mode:?}"
            );
        }
    }

    /// A script only helps if it runs on the main frame, which is the one that
    /// paints. `js_init_script` is the main-frame form; the all-frames sibling
    /// would also work but says something this does not mean.
    #[test]
    fn the_script_is_registered_for_the_main_frame() {
        let plugin: TauriPlugin<Wry> = plugin_for(AppearanceMode::Dark);
        let script = plugin
            .initialization_script_2()
            .expect("an explicit choice has a script");
        assert!(script.for_main_frame_only);
    }
}
