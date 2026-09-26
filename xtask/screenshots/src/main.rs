//! `cargo docs-screenshots` — regenerate the docs screenshots of both clients
//! from named scenarios (issue #1322).
//!
//! ```text
//! cargo docs-screenshots --list
//! cargo docs-screenshots                                  # every scenario, both clients
//! cargo docs-screenshots --scenario dashboard             # one scenario, both clients
//! cargo docs-screenshots --client desktop                 # every desktop image
//! cargo docs-screenshots --out <dir>                      # somewhere other than docs/img
//! ```
//!
//! Two stages. The TUI stage runs the `#[ignore]`d captures in
//! `tests/e2e_docs_screenshots.rs` under `--features e2e`: each drives the
//! real binary in the L2 PTY harness against an isolated sandbox and writes
//! `<scenario>-tui.html`. The browser stage runs
//! `desktop/playwright.screenshots.config.ts` in Chromium: it screenshots the
//! desktop scenarios off the production web build and rasterizes the TUI HTML,
//! writing every PNG into `--out`. `docs/develop/docs-screenshots.md` has the
//! rest.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use xtask_screenshots::scenarios::{self, Client, SCENARIOS, Scenario};
use xtask_screenshots::{OUT_DIR_ENV, TUI_HTML_DIR_ENV, WEB_BUILD_ENV};

const USAGE: &str = "\
usage: cargo docs-screenshots [--list] [--scenario <name>]... [--client tui|desktop]... [--out <dir>]

  --list               print the scenarios and exit
  --scenario <name>    only this scenario (repeatable; default: all)
  --client <client>    only this client, tui or desktop (repeatable; default: both)
  --out <dir>          write the PNGs here instead of docs/img";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    list: bool,
    scenarios: Vec<String>,
    clients: BTreeSet<Client>,
    out: Option<PathBuf>,
}

fn parse_args(raw: &[String]) -> Result<Args, String> {
    let mut args = Args {
        list: false,
        scenarios: Vec::new(),
        clients: BTreeSet::new(),
        out: None,
    };
    let mut it = raw.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            // `cargo docs-screenshots -- --list` passes a second `--`.
            "--" => {}
            "--list" => args.list = true,
            "--scenario" => {
                let name = it.next().ok_or("--scenario needs a name")?;
                if scenarios::find(name).is_none() {
                    return Err(format!("unknown scenario {name:?}; --list shows them"));
                }
                args.scenarios.push(name.clone());
            }
            "--client" => {
                let text = it.next().ok_or("--client needs tui or desktop")?;
                let client = Client::parse(text)
                    .ok_or_else(|| format!("unknown client {text:?}; use tui or desktop"))?;
                args.clients.insert(client);
            }
            "--out" => {
                let dir = it.next().ok_or("--out needs a directory")?;
                args.out = Some(PathBuf::from(dir));
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    if args.clients.is_empty() {
        args.clients.extend(Client::ALL);
    }
    Ok(args)
}

/// The (scenario, client) pairs a run produces, in registry order.
fn selected(args: &Args) -> Vec<(&'static Scenario, Client)> {
    let mut out = Vec::new();
    for scenario in SCENARIOS {
        if !args.scenarios.is_empty() && !args.scenarios.iter().any(|n| n == scenario.name) {
            continue;
        }
        for client in Client::ALL {
            if args.clients.contains(&client) && scenario.has(client) {
                out.push((scenario, client));
            }
        }
    }
    out
}

/// The nextest filterset selecting exactly these TUI captures.
fn tui_filter(scenarios: &[&Scenario]) -> String {
    scenarios
        .iter()
        .map(|s| format!("test(={})", s.tui_test_name()))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn escape_regex(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if !ch.is_ascii_alphanumeric() && ch != ' ' && ch != '_' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The Playwright `--grep` selecting exactly these pairs. Test titles are
/// `<client> <scenario>`; the `$` is what stops `dashboard` also selecting
/// `dashboard-empty`.
fn playwright_grep(pairs: &[(&Scenario, Client)]) -> String {
    let alternatives: Vec<String> = pairs
        .iter()
        .map(|(s, c)| escape_regex(&format!("{} {}", c.as_str(), s.name)))
        .collect();
    format!("(?:^|\\s)(?:{})$", alternatives.join("|"))
}

fn print_list() {
    let width = SCENARIOS.iter().map(|s| s.name.len()).max().unwrap_or(0);
    for s in SCENARIOS {
        let clients: Vec<&str> = s.clients.iter().map(|c| c.as_str()).collect();
        println!(
            "{:width$}  [{}]  {}",
            s.name,
            clients.join(", "),
            s.description
        );
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => root.join("target"),
    }
}

fn run(command: &mut Command, what: &str) -> Result<(), String> {
    eprintln!("==> {what}");
    let status = command
        .status()
        .map_err(|e| format!("{what}: could not start {:?}: {e}", command.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed ({status})"))
    }
}

fn generate(args: &Args) -> Result<Vec<PathBuf>, String> {
    let root = repo_root()
        .canonicalize()
        .map_err(|e| format!("resolve repo root: {e}"))?;
    let out = match &args.out {
        Some(dir) => std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(dir),
        None => root.join("docs").join("img"),
    };
    std::fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;

    let pairs = selected(args);
    if pairs.is_empty() {
        return Err("the selection matches no scenario".into());
    }
    let tui: Vec<&Scenario> = pairs
        .iter()
        .filter(|(_, c)| *c == Client::Tui)
        .map(|(s, _)| *s)
        .collect();

    // Under `target/`, which is disk-backed, and never the agent scratchpad or
    // a tmpfs (CLAUDE.md rule 14). Emptied first so a stale HTML file from an
    // earlier run can never be rasterized as this run's image.
    let html_dir = target_dir(&root).join("docs-screenshots").join("tui-html");
    if html_dir.exists() {
        std::fs::remove_dir_all(&html_dir)
            .map_err(|e| format!("clear {}: {e}", html_dir.display()))?;
    }
    std::fs::create_dir_all(&html_dir)
        .map_err(|e| format!("create {}: {e}", html_dir.display()))?;

    if !tui.is_empty() {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        run(
            Command::new(cargo)
                .current_dir(&root)
                .args([
                    "nextest",
                    "run",
                    "--features",
                    "e2e",
                    "--test",
                    "e2e_docs_screenshots",
                    "--run-ignored",
                    "only",
                    "--no-fail-fast",
                    "-E",
                ])
                .arg(tui_filter(&tui))
                .env(TUI_HTML_DIR_ENV, &html_dir),
            "TUI capture (real binary, L2 PTY harness)",
        )?;
        for s in &tui {
            let html = html_dir.join(format!("{}-tui.html", s.name));
            if !html.is_file() {
                return Err(format!("the TUI capture did not write {}", html.display()));
            }
        }
    }

    let desktop = root.join("desktop");
    let playwright = desktop
        .join("node_modules")
        .join(".bin")
        .join(if cfg!(windows) {
            "playwright.cmd"
        } else {
            "playwright"
        });
    if !playwright.exists() {
        return Err(format!(
            "{} is missing: run `pnpm install` and `pnpm exec playwright install chromium` in desktop/",
            playwright.display()
        ));
    }
    let needs_web = pairs.iter().any(|(_, c)| *c == Client::Desktop);
    run(
        Command::new(&playwright)
            .current_dir(&desktop)
            .args(["test", "-c", "playwright.screenshots.config.ts", "--grep"])
            .arg(playwright_grep(&pairs))
            .env(OUT_DIR_ENV, &out)
            .env(TUI_HTML_DIR_ENV, &html_dir)
            .env(WEB_BUILD_ENV, if needs_web { "1" } else { "0" }),
        "rasterize (Playwright Chromium)",
    )?;

    let mut written = Vec::new();
    for (s, c) in &pairs {
        let png = out.join(s.image_file(*c));
        if !png.is_file() {
            return Err(format!("expected {} was not written", png.display()));
        }
        written.push(png);
    }
    Ok(written)
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&raw) {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("error: {message}\n");
            }
            eprintln!("{USAGE}");
            return if message.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
        }
    };
    if args.list {
        print_list();
        return ExitCode::SUCCESS;
    }
    match generate(&args) {
        Ok(written) => {
            for png in written {
                println!("{}", png.display());
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Result<Args, String> {
        parse_args(&raw.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn defaults_to_every_scenario_on_both_clients() {
        let a = args(&[]).unwrap();
        assert!(!a.list);
        assert_eq!(a.clients, Client::ALL.into_iter().collect());
        let expected: usize = SCENARIOS.iter().map(|s| s.clients.len()).sum();
        assert_eq!(selected(&a).len(), expected);
    }

    #[test]
    fn narrows_by_scenario_and_client() {
        let a = args(&["--scenario", "dashboard", "--client", "desktop"]).unwrap();
        let picked = selected(&a);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].0.name, "dashboard");
        assert_eq!(picked[0].1, Client::Desktop);
    }

    #[test]
    fn rejects_unknown_input() {
        assert!(
            args(&["--scenario", "nope"])
                .unwrap_err()
                .contains("unknown scenario")
        );
        assert!(
            args(&["--client", "gui"])
                .unwrap_err()
                .contains("unknown client")
        );
        assert!(args(&["--scenario"]).is_err());
        assert!(args(&["--bogus"]).is_err());
        assert_eq!(args(&["--help"]).unwrap_err(), "");
        assert!(args(&["--", "--list"]).unwrap().list);
    }

    #[test]
    fn the_tui_filter_is_exact_per_test() {
        let s = [
            scenarios::find("dashboard").unwrap(),
            scenarios::find("dashboard-empty").unwrap(),
        ];
        assert_eq!(
            tui_filter(&s),
            "test(=docs_screenshot_dashboard) | test(=docs_screenshot_dashboard_empty)"
        );
    }

    #[test]
    fn the_playwright_grep_is_anchored_so_a_prefix_does_not_select_its_longer_sibling() {
        let dashboard = scenarios::find("dashboard").unwrap();
        let grep = playwright_grep(&[(dashboard, Client::Desktop)]);
        assert_eq!(grep, "(?:^|\\s)(?:desktop dashboard)$");
        let empty = scenarios::find("dashboard-empty").unwrap();
        let grep = playwright_grep(&[(empty, Client::Tui)]);
        assert_eq!(grep, "(?:^|\\s)(?:tui dashboard\\-empty)$");
    }
}
