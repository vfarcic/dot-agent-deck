//! `dot-agent-deck upgrade`: check for a newer release, print the plan for
//! this copy and for the other copy on the machine, and carry out each one the
//! user confirms. It is also the command the clients show where they cannot
//! act themselves ([`super::UPGRADE_COMMAND`]).

use std::io::{BufRead, Write};

use super::Host;
use super::detect::{self, CopyKind};
use super::discover::{self, OtherCopy};
use super::execute::{self, ReleaseSource};
use super::plan::{self, PlanOptions, UpgradePlan};

/// What the user asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Args {
    /// Only report; change nothing.
    pub check: bool,
    /// Confirm every upgrade without asking.
    pub yes: bool,
}

/// Where the answers to the confirmation questions come from, when anywhere.
pub enum Answers<'a> {
    /// A terminal: ask, and read the answer.
    Terminal(&'a mut dyn BufRead),
    /// Not a terminal: nothing can be asked.
    None,
}

/// Run the subcommand. Returns whether everything it attempted succeeded.
pub async fn run(
    host: &dyn Host,
    source: &ReleaseSource,
    options: &PlanOptions,
    args: Args,
    mut answers: Answers<'_>,
    out: &mut dyn Write,
) -> bool {
    let running = match detect::running(host, CopyKind::Cli) {
        Ok(running) => running,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return false;
        }
    };
    let latest = match source.latest_version().await {
        Ok(latest) => latest,
        Err(e) => {
            let _ = writeln!(out, "{e}");
            return false;
        }
    };
    let mut plans = vec![plan::plan(&running, &latest, options)];
    if let OtherCopy::Found(other) = discover::other_copy(host, &running, None) {
        plans.push(plan::plan(&other, &latest, options));
    }

    for (i, plan) in plans.iter().enumerate() {
        if i > 0 {
            let _ = writeln!(out);
        }
        for line in plan.lines() {
            let _ = writeln!(out, "{line}");
        }
    }
    if args.check {
        return true;
    }

    let mut ok = true;
    for plan in plans.iter().filter(|plan| plan.is_actionable()) {
        if !confirmed(plan, args, &mut answers, out) {
            continue;
        }
        match execute::execute(host, plan, source, options).await {
            Ok(outcome) => {
                for line in outcome.lines() {
                    let _ = writeln!(out, "{line}");
                }
            }
            Err(e) => {
                let _ = writeln!(out, "{e}");
                ok = false;
            }
        }
    }
    ok
}

fn confirmed(
    plan: &UpgradePlan,
    args: Args,
    answers: &mut Answers<'_>,
    out: &mut dyn Write,
) -> bool {
    let Some(question) = plan.confirm_question() else {
        return false;
    };
    let _ = writeln!(out);
    if args.yes {
        let _ = writeln!(out, "{question} yes (--yes)");
        return true;
    }
    let Answers::Terminal(input) = answers else {
        let _ = writeln!(
            out,
            "{question} Not asked, because this is not a terminal. Run `{} --yes` to upgrade without being asked.",
            super::UPGRADE_COMMAND
        );
        return false;
    };
    let _ = write!(out, "{question} [y/N] ");
    let _ = out.flush();
    let mut answer = String::new();
    if input.read_line(&mut answer).is_err() {
        return false;
    }
    let yes = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !yes {
        let _ = writeln!(out, "Skipped.");
    }
    yes
}

/// The subcommand as `main` runs it: the real machine, this build's release
/// source, a terminal client's options, and stdin when it is a terminal.
pub fn main(args: Args) -> std::process::ExitCode {
    use std::io::IsTerminal;

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let host = super::SystemHost::default();
    let source = ReleaseSource::from_build();
    let options = PlanOptions::terminal();
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let answers = if std::io::stdin().is_terminal() {
        Answers::Terminal(&mut stdin)
    } else {
        Answers::None
    };
    let mut stdout = std::io::stdout();
    let ok = runtime.block_on(run(&host, &source, &options, args, answers, &mut stdout));
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_upgrade::detect::{InstallMethod, Installation, Platform, Tools};
    use std::path::PathBuf;

    fn actionable() -> UpgradePlan {
        let exe = PathBuf::from("/home/u/.local/bin/dot-agent-deck");
        let installation = Installation {
            copy: CopyKind::Cli,
            executable: exe.clone(),
            version: "0.45.0".into(),
            platform: Some(Platform::LinuxAmd64),
            method: InstallMethod::DownloadedWritable { binary: exe },
            tools: Tools::default(),
        };
        plan::plan(
            &installation,
            "0.46.0",
            &PlanOptions {
                staging_root: PathBuf::from("/stage"),
                can_prompt_for_privilege: false,
            },
        )
    }

    fn ask(args: Args, input: Option<&str>) -> (bool, String) {
        let mut out = Vec::new();
        let mut reader = input.map(|text| std::io::Cursor::new(text.as_bytes().to_vec()));
        let mut answers = match reader.as_mut() {
            Some(reader) => Answers::Terminal(reader),
            None => Answers::None,
        };
        let yes = confirmed(&actionable(), args, &mut answers, &mut out);
        (yes, String::from_utf8(out).unwrap())
    }

    #[test]
    fn cli_001_yes_flag_confirms_without_asking() {
        let (yes, out) = ask(
            Args {
                check: false,
                yes: true,
            },
            None,
        );
        assert!(yes);
        assert!(
            out.contains("Upgrade dot-agent-deck to v0.46.0? yes (--yes)"),
            "{out}"
        );
    }

    #[test]
    fn cli_002_terminal_answer_decides_and_default_is_no() {
        assert!(ask(Args::default(), Some("y\n")).0);
        assert!(ask(Args::default(), Some("YES\n")).0);
        let (yes, out) = ask(Args::default(), Some("\n"));
        assert!(!yes);
        assert!(out.contains("[y/N] Skipped."), "{out}");
    }

    #[test]
    fn cli_003_no_terminal_never_upgrades_unasked() {
        let (yes, out) = ask(Args::default(), None);
        assert!(!yes);
        assert!(out.contains("dot-agent-deck upgrade --yes"), "{out}");
    }
}
