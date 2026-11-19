use serde::Deserialize;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// Prompt template for instructing an AI agent to generate a `.dot-agent-deck.toml`
/// configuration file by analyzing the project structure.
const CONFIG_GEN_PROMPT_TEMPLATE: &str = include_str!("../assets/config_gen_prompt.md");

/// PRD #2119: the section put ahead of [`CONFIG_GEN_PROMPT_TEMPLATE`] when the
/// project already has a `.dot-agent-deck.toml`, turning the request into an
/// update of the user's file rather than a fresh one. `{link}` is where the
/// symbolic-link instructions go when the file is a link ([`SYMLINK_NOTE`]).
const CONFIG_GEN_EXISTING_TEMPLATE: &str = include_str!("../assets/config_gen_existing.md");

/// PRD #2119: the whole request when the project's `.dot-agent-deck.toml` is a
/// symbolic link to nothing: there is no file to read or edit, so the agent is
/// told to say so rather than to write.
const CONFIG_GEN_DANGLING_TEMPLATE: &str = include_str!("../assets/config_gen_dangling.md");

/// PRD #2119: the section put ahead of the request when the deck could not
/// tell, within [`CONFIG_CHECK_TIMEOUT`], whether the project already has a
/// `.dot-agent-deck.toml`. Every instruction is conditional on what the agent
/// finds there.
const CONFIG_GEN_UNKNOWN_TEMPLATE: &str = include_str!("../assets/config_gen_unknown.md");

/// PRD #2119: the bullet [`CONFIG_GEN_EXISTING_TEMPLATE`]'s `{link}` becomes
/// when the existing config is a symbolic link that resolves.
const SYMLINK_NOTE: &str = "- **`{dir}/.dot-agent-deck.toml` is a symbolic link** (to {target}). Edit the file it points to in place, through the link, and keep the link: never replace it with a regular file, remove it or re-point it.\n";

/// Generic agent role library. Embedded at build time; the config-gen agent picks
/// from these roles when proposing an orchestration.
const ROLES_TOML: &str = include_str!("../assets/roles.toml");

#[derive(Debug, Deserialize)]
struct RoleTemplate {
    name: String,
    description: String,
    #[serde(default = "default_true")]
    clear: bool,
    #[serde(default)]
    prompt_template: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RoleLibrary {
    role: Vec<RoleTemplate>,
}

static ROLE_LIBRARY: OnceLock<RoleLibrary> = OnceLock::new();

fn role_library() -> &'static RoleLibrary {
    ROLE_LIBRARY
        .get_or_init(|| toml::from_str(ROLES_TOML).expect("embedded roles.toml must be valid"))
}

fn render_roles_section(library: &RoleLibrary) -> String {
    let mut out = String::new();
    for role in &library.role {
        writeln!(&mut out, "### `{}`\n", role.name).unwrap();
        writeln!(&mut out, "- **Description:** {}", role.description).unwrap();
        writeln!(&mut out, "- **`clear` default:** `{}`", role.clear).unwrap();
        writeln!(
            &mut out,
            "- **Suggested `prompt_template`:** {}\n",
            role.prompt_template
        )
        .unwrap();
    }
    out
}

/// PRD #2119: what is at `<dir>/.dot-agent-deck.toml`, as far as generating
/// or updating a config is concerned ([`existing_config`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExistingConfig {
    /// Nothing the user wrote is there: no entry at that name, or one that is
    /// a directory, FIFO, socket or device. The request is the plain one.
    Absent,
    /// A regular file.
    File,
    /// A symbolic link, whatever it points to. `target` is what the link
    /// holds (`None` when it could not be read); `dangling` is whether
    /// following it finds nothing.
    Symlink {
        target: Option<std::path::PathBuf>,
        dangling: bool,
    },
    /// The check did not finish within [`CONFIG_CHECK_TIMEOUT`] (or could not
    /// be run), so the deck does not know.
    Unknown,
}

impl ExistingConfig {
    /// Whether a client should offer Update rather than Generate: `Some(true)`
    /// for a file or a symlink (a dangling one included), `Some(false)` when
    /// nothing is there, `None` when unknown.
    pub fn exists(&self) -> Option<bool> {
        match self {
            Self::Absent => Some(false),
            Self::File | Self::Symlink { .. } => Some(true),
            Self::Unknown => None,
        }
    }
}

/// PRD #2119: what is at `<dir>/.dot-agent-deck.toml`, for generate/update
/// purposes only. **Blocking** (an `lstat`, plus a `readlink` and a `stat`
/// for a link): async callers go through [`existing_config_bounded`] or
/// [`project_config_flags`].
///
/// Deliberately not [`crate::directory_listing::holds_project_config`]: that
/// predicate answers whether the PROJECT READER would accept the file, and it
/// refuses a symlink. Here the question is whether the user already has
/// content at that name, which a symlink is, dangling or not, so the request
/// can tell the agent to keep it. The project resolver's no-follow policy is
/// unchanged. A directory at that name is not a config.
pub fn existing_config(dir: &std::path::Path) -> ExistingConfig {
    let path = dir.join(crate::project_config::CONFIG_FILE_NAME);
    match std::fs::symlink_metadata(&path) {
        Err(_) => ExistingConfig::Absent,
        Ok(meta) if meta.file_type().is_symlink() => ExistingConfig::Symlink {
            target: std::fs::read_link(&path).ok(),
            dangling: std::fs::metadata(&path).is_err(),
        },
        Ok(meta) if meta.file_type().is_file() => ExistingConfig::File,
        Ok(_) => ExistingConfig::Absent,
    }
}

/// PRD #2119: whether `dir` itself holds a `.dot-agent-deck.toml` for
/// generate/update purposes ([`existing_config`]): a regular file or a
/// symbolic link (a dangling one included), at that exact directory, with no
/// walk up the tree. **Blocking.**
pub fn project_config_exists(dir: &str) -> bool {
    existing_config(std::path::Path::new(dir))
        .exists()
        .unwrap_or(false)
}

/// PRD #2119: how long the daemon waits for its config checks — a listing's
/// whole batch, or the one a request is composed with — before it treats the
/// answer as unknown. An `lstat` on a healthy disk takes microseconds; this
/// bounds one on an unavailable network or FUSE mount.
pub const CONFIG_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Run `f` on a blocking thread behind the daemon's project-filesystem bound
/// ([`crate::project_resolve::run_bounded_in`]), giving up after `timeout`.
/// `None` when it did not finish in time, or could not be run. A thread that
/// stalls keeps its permit until it returns, so stalled checks cannot pile up
/// past the bound; what they cannot do is hold the caller past `timeout`.
async fn off_runtime_within<T, F>(
    limit: &std::sync::Arc<tokio::sync::Semaphore>,
    timeout: std::time::Duration,
    f: F,
) -> Option<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::time::timeout(timeout, crate::project_resolve::run_bounded_in(limit, f)).await {
        Ok(Ok(value)) => Some(value),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// PRD #2119: [`existing_config`] for each of `dirs`, as one unit of blocking
/// work off the async runtime, bounded by [`CONFIG_CHECK_TIMEOUT`]. `None`
/// when the batch did not finish in time: every answer is then unknown. Call
/// it holding no registry or `AppState` lock.
pub async fn project_config_flags(dirs: Vec<String>) -> Option<Vec<bool>> {
    project_config_flags_within(
        crate::project_resolve::project_fs_limit(),
        CONFIG_CHECK_TIMEOUT,
        dirs,
    )
    .await
}

/// [`project_config_flags`] against an explicit bound and timeout, for tests.
pub(crate) async fn project_config_flags_within(
    limit: &std::sync::Arc<tokio::sync::Semaphore>,
    timeout: std::time::Duration,
    dirs: Vec<String>,
) -> Option<Vec<bool>> {
    off_runtime_within(limit, timeout, move || {
        dirs.iter()
            .map(|dir| project_config_exists(dir))
            .collect::<Vec<_>>()
    })
    .await
}

/// PRD #2119: [`existing_config`] off the async runtime, bounded by
/// [`CONFIG_CHECK_TIMEOUT`]; [`ExistingConfig::Unknown`] when it did not finish.
pub async fn existing_config_bounded(dir: &str) -> ExistingConfig {
    existing_config_within(
        crate::project_resolve::project_fs_limit(),
        CONFIG_CHECK_TIMEOUT,
        dir,
    )
    .await
}

/// [`existing_config_bounded`] against an explicit bound and timeout, for tests.
async fn existing_config_within(
    limit: &std::sync::Arc<tokio::sync::Semaphore>,
    timeout: std::time::Duration,
    dir: &str,
) -> ExistingConfig {
    let dir = dir.to_string();
    off_runtime_within(limit, timeout, move || {
        existing_config(std::path::Path::new(&dir))
    })
    .await
    .unwrap_or(ExistingConfig::Unknown)
}

/// Build the config generation prompt for a specific directory, checking the
/// disk now. **Blocking**: the daemon composes with
/// [`config_gen_prompt_checked`] instead.
pub fn config_gen_prompt(dir: &str) -> String {
    config_gen_prompt_for(dir, &existing_config(std::path::Path::new(dir)))
}

/// PRD #2119: [`config_gen_prompt`] with the check run off the async runtime
/// and bounded ([`existing_config_bounded`]). Call it holding no registry or
/// `AppState` lock.
pub async fn config_gen_prompt_checked(dir: &str) -> String {
    config_gen_prompt_for(dir, &existing_config_bounded(dir).await)
}

/// The template's sentences that only fit a request to CREATE the file, each
/// with what an update says instead. [`config_gen_prompt_for`] swaps them in
/// for an existing config, so the request asks for one thing throughout; the
/// test `update_wording_anchors_are_in_the_template` keeps each one present in
/// the template, so a reworded template cannot silently leave the create
/// wording in an update request.
const UPDATE_REWORDS: [(&str, &str); 5] = [
    (
        "Analyze this project and create a `.dot-agent-deck.toml` configuration file in the project root.",
        "Analyze this project and propose changes to its existing `.dot-agent-deck.toml` configuration file in the project root.",
    ),
    (
        "**Present the full proposed config to the user before writing it,",
        "**Present the proposed changes to the existing config to the user before writing them,",
    ),
    (
        "otherwise I'll write the whole thing.",
        "otherwise I'll apply these changes to the existing file.",
    ),
    (
        "Only write the file after the user confirms.",
        "Only edit the file after the user confirms.",
    ),
    (
        "**After writing the file, tell the user the next steps:** \"Config created!",
        "**After updating the file, tell the user the next steps:** \"Config updated!",
    ),
];

/// The template's write step, which [`config_gen_prompt_for`] replaces for an
/// existing config (after `{dir}` is filled in).
const WRITE_STEP: &str = "5. Write the approved config to `{dir}/.dot-agent-deck.toml`.";

/// PRD #2119: the request for `dir`, given what is at its
/// `.dot-agent-deck.toml`.
///
/// - [`ExistingConfig::Absent`]: the template as it always was, byte for byte.
/// - [`ExistingConfig::File`]: the update preamble, then the template with its
///   create-only sentences reworded ([`UPDATE_REWORDS`]) and its write step
///   saying to edit the file in place.
/// - A resolving [`ExistingConfig::Symlink`]: as for a file, and the preamble
///   and the write step add that it is a link, to edit the file it points to
///   in place and to keep the link.
/// - A dangling symlink: only the instruction to tell the user the link is
///   broken and write nothing — there is nothing to analyze against.
/// - [`ExistingConfig::Unknown`]: the check timed out, so the file may well
///   exist. Sending the create request then could have the agent replace a
///   file the user wrote, so the request is phrased conditionally instead: if
///   the file exists, read it first and propose changes; only if nothing is
///   there, create it.
pub fn config_gen_prompt_for(dir: &str, existing: &ExistingConfig) -> String {
    let roles_section = render_roles_section(role_library());
    let body = CONFIG_GEN_PROMPT_TEMPLATE
        .replace("{dir}", dir)
        .replace("{roles}", &roles_section);
    let write_step = WRITE_STEP.replace("{dir}", dir);
    let update_body = |new_write_step: String| {
        let mut body = body.replace(&write_step, &new_write_step);
        for (create, update) in UPDATE_REWORDS {
            body = body.replace(create, update);
        }
        body
    };
    let link_target = |target: &Option<std::path::PathBuf>| match target {
        Some(target) => format!("`{}`", target.display()),
        None => "a path the deck could not read".to_string(),
    };
    match existing {
        ExistingConfig::Absent => body,
        ExistingConfig::File => format!(
            "{}{}",
            CONFIG_GEN_EXISTING_TEMPLATE
                .replace("{link}", "")
                .replace("{dir}", dir),
            update_body(format!(
                "5. Apply the approved changes to `{dir}/.dot-agent-deck.toml` by editing that file in place; never replace it with a fresh file."
            ))
        ),
        ExistingConfig::Symlink {
            target,
            dangling: false,
        } => {
            let target = link_target(target);
            format!(
                "{}{}",
                CONFIG_GEN_EXISTING_TEMPLATE
                    .replace("{link}", &SYMLINK_NOTE.replace("{target}", &target))
                    .replace("{dir}", dir),
                update_body(format!(
                    "5. Apply the approved changes to the file `{dir}/.dot-agent-deck.toml` links to ({target}) by editing that file in place, through the link. Keep `{dir}/.dot-agent-deck.toml` a symbolic link: never replace it with a regular file, remove it or re-point it."
                ))
            )
        }
        ExistingConfig::Symlink {
            target,
            dangling: true,
        } => CONFIG_GEN_DANGLING_TEMPLATE
            .replace("{target}", &link_target(target))
            .replace("{dir}", dir),
        ExistingConfig::Unknown => {
            let mut body = body.replace(
                &write_step,
                &format!(
                    "5. Write the approved config to `{dir}/.dot-agent-deck.toml` — or, if that file already exists, apply only the approved changes by editing it in place (through the link, if it is a symbolic link), never replacing it."
                ),
            );
            body = body.replace(
                UPDATE_REWORDS[0].0,
                "Analyze this project and create a `.dot-agent-deck.toml` configuration file in the project root, or, if it already has one, propose changes to it (see above).",
            );
            format!(
                "{}{body}",
                CONFIG_GEN_UNKNOWN_TEMPLATE.replace("{dir}", dir)
            )
        }
    }
}

/// PRD #2119: the subject of the configuration request the daemon posts to an
/// agent's inbox ([`crate::daemon_protocol::AttachRequest::GenerateConfig`]).
pub const CONFIG_GEN_SUBJECT: &str = "configuration request";

/// PRD #2119: the sentences a client shows for each outcome of
/// [`crate::daemon_client::DaemonClient::generate_config`]. They live beside
/// that call in `daemon_client`, which the desktop app may reach (this module,
/// which composes the request for a directory, is the daemon's), and are
/// re-exported here for the TUI.
pub use crate::daemon_client::{
    CONFIG_GEN_FAILED_PREFIX, CONFIG_GEN_SENT_MESSAGE, CONFIG_GEN_UPGRADE_MESSAGE,
    config_gen_failed_message,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_interpolates_directory() {
        let prompt = config_gen_prompt("/home/user/my-project");
        assert!(prompt.contains("/home/user/my-project"));
        assert!(!prompt.contains("{dir}"));
    }

    #[test]
    fn prompt_existing_config_is_read_and_updated_only_after_approval() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().to_str().unwrap();
        let original = config_gen_prompt(cwd);
        assert_eq!(
            original,
            CONFIG_GEN_PROMPT_TEMPLATE
                .replace("{dir}", cwd)
                .replace("{roles}", &render_roles_section(role_library()))
        );
        let path = directory.path().join(".dot-agent-deck.toml");
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            config_gen_prompt(cwd),
            original,
            "a directory is not a config file"
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "# user-owned configuration\n").unwrap();
        let update = config_gen_prompt(cwd);
        assert!(
            update != original,
            "an existing file must change the request to an update"
        );
        assert!(
            update.contains(path.to_str().unwrap()),
            "name the exact existing file"
        );
        let words = update.to_lowercase();
        for required in ["already exists", "read", "update", "preserve", "approval"] {
            assert!(
                words.contains(required),
                "existing-config instructions must contain {required:?}"
            );
        }
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            config_gen_prompt(cwd),
            original,
            "check the file at compose time"
        );
    }

    /// PRD #2119: every create-only sentence the update request rewords is in
    /// the template exactly once, so a reworded template fails here rather
    /// than leaving its create wording in an update request unnoticed.
    #[test]
    fn update_wording_anchors_are_in_the_template() {
        for (create, _) in UPDATE_REWORDS {
            assert_eq!(
                CONFIG_GEN_PROMPT_TEMPLATE.matches(create).count(),
                1,
                "the template must hold {create:?} exactly once"
            );
        }
        assert_eq!(CONFIG_GEN_PROMPT_TEMPLATE.matches(WRITE_STEP).count(), 1);
    }

    /// The sentences of the create request that contradict an update: create
    /// a file, write the whole thing, write a fresh config, "Config created!".
    fn create_instructions(dir: &str) -> Vec<String> {
        let mut found: Vec<String> = UPDATE_REWORDS
            .iter()
            .map(|(create, _)| create.to_string())
            .collect();
        found.push(WRITE_STEP.replace("{dir}", dir));
        found.push("\"Config created! To use it".to_string());
        found
    }

    /// PRD #2119 (reviewer nit): the request for an existing config, file or
    /// link, asks for an update throughout — no sentence of it still says to
    /// create the file, write the whole thing, or report "Config created!".
    #[test]
    fn update_request_contains_no_create_or_replace_instruction() {
        let dir = "/home/user/my-project";
        let file = config_gen_prompt_for(dir, &ExistingConfig::File);
        let link = config_gen_prompt_for(
            dir,
            &ExistingConfig::Symlink {
                target: Some("/home/user/dotfiles/deck.toml".into()),
                dangling: false,
            },
        );
        for (name, request) in [("file", &file), ("symlink", &link)] {
            for create in create_instructions(dir) {
                assert!(
                    !request.contains(&create),
                    "the {name} update request still says {create:?}"
                );
            }
            assert!(
                request.starts_with("**This project already has a configuration"),
                "the {name} request opens with the update preamble"
            );
            for update in [
                "propose changes to its existing `.dot-agent-deck.toml`",
                "otherwise I'll apply these changes to the existing file.",
                "\"Config updated! To use it",
                "by editing that file in place",
            ] {
                assert!(
                    request.contains(update),
                    "the {name} request must say {update:?}"
                );
            }
        }
        // The plain request is still today's template.
        assert_eq!(
            config_gen_prompt_for(dir, &ExistingConfig::Absent),
            CONFIG_GEN_PROMPT_TEMPLATE
                .replace("{dir}", dir)
                .replace("{roles}", &render_roles_section(role_library()))
        );
    }

    /// PRD #2119: a `.dot-agent-deck.toml` that is a symbolic link counts as an
    /// existing config for generate/update — dangling or not — while a
    /// directory of that name does not, and the project reader's own
    /// predicate still refuses the link.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_exists_for_update_and_a_directory_does_not() {
        let project = tempfile::tempdir().unwrap();
        let dotfiles = tempfile::tempdir().unwrap();
        let cwd = project.path().to_str().unwrap();
        let path = project.path().join(".dot-agent-deck.toml");
        let target = dotfiles.path().join("deck.toml");

        std::fs::create_dir(&path).unwrap();
        assert_eq!(existing_config(project.path()), ExistingConfig::Absent);
        assert!(!project_config_exists(cwd), "a directory is not a config");
        std::fs::remove_dir(&path).unwrap();

        std::fs::write(&target, "# the user's linked config\n").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(
            existing_config(project.path()),
            ExistingConfig::Symlink {
                target: Some(target.clone()),
                dangling: false
            }
        );
        assert!(
            project_config_exists(cwd),
            "a symlinked config reads Update"
        );
        assert!(
            !crate::directory_listing::holds_project_config(project.path()),
            "the project reader still refuses a symlinked config"
        );
        let request = config_gen_prompt(cwd);
        let shown = target.display().to_string();
        for needed in [
            "is a symbolic link",
            shown.as_str(),
            "Edit the file it points to in place, through the link, and keep the link",
            "never replace it with a regular file, remove it or re-point it",
        ] {
            assert!(
                request.contains(needed),
                "the symlink request must say {needed:?}"
            );
        }

        std::fs::remove_file(&target).unwrap();
        assert_eq!(
            existing_config(project.path()),
            ExistingConfig::Symlink {
                target: Some(target.clone()),
                dangling: true
            }
        );
        assert!(
            project_config_exists(cwd),
            "a dangling symlinked config still reads Update"
        );
        let request = config_gen_prompt(cwd);
        for needed in [
            "is a symbolic link to",
            shown.as_str(),
            "which does not exist",
            "Do not write anything",
            "is a broken symbolic link",
        ] {
            assert!(
                request.contains(needed),
                "the dangling-link request must say {needed:?}"
            );
        }
        for create in create_instructions(cwd) {
            assert!(
                !request.contains(&create),
                "the dangling-link request must not say {create:?}"
            );
        }
        assert!(!target.exists(), "composing the request wrote nothing");
    }

    /// PRD #2119: the check a request is composed with runs off the runtime
    /// behind the project-filesystem bound; when it cannot finish in time, the
    /// request is the conditional one — read the file first if it exists — and
    /// never the plain create request.
    #[tokio::test]
    async fn a_timed_out_check_sends_the_conditional_update_wording() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        std::fs::write(dir.path().join(".dot-agent-deck.toml"), "# user's\n").unwrap();
        let free = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        assert_eq!(
            existing_config_within(&free, std::time::Duration::from_secs(5), cwd).await,
            ExistingConfig::File
        );
        // No permit is ever free: the check cannot start, so it times out.
        let saturated = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let existing =
            existing_config_within(&saturated, std::time::Duration::from_millis(50), cwd).await;
        assert_eq!(existing, ExistingConfig::Unknown);
        assert_eq!(existing.exists(), None, "an unknown check offers no Update");
        let request = config_gen_prompt_for(cwd, &existing);
        let config = format!("{cwd}/.dot-agent-deck.toml");
        for needed in [
            format!("If `{config}` exists, read it first"),
            "Propose changes to that file, not a replacement for it".to_string(),
            "If nothing exists at that path**, create the file".to_string(),
            format!(
                "Write the approved config to `{config}` — or, if that file already exists, apply only the approved changes by editing it in place"
            ),
        ] {
            assert!(
                request.contains(&needed),
                "the conditional request must say {needed:?}"
            );
        }
        for create in [
            UPDATE_REWORDS[0].0.to_string(),
            WRITE_STEP.replace("{dir}", cwd),
        ] {
            assert!(
                !request.contains(&create),
                "the conditional request must not carry the unconditional {create:?}"
            );
        }
    }

    /// PRD #2119: a listing's batch check is bounded the same way: answered
    /// when it finishes, `None` (every record unknown) when it cannot.
    #[tokio::test]
    async fn a_listing_batch_check_is_bounded() {
        let with = tempfile::tempdir().unwrap();
        let without = tempfile::tempdir().unwrap();
        let dirs = vec![
            with.path().to_str().unwrap().to_string(),
            without.path().to_str().unwrap().to_string(),
        ];
        std::fs::write(with.path().join(".dot-agent-deck.toml"), "").unwrap();
        let free = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        assert_eq!(
            project_config_flags_within(&free, std::time::Duration::from_secs(5), dirs.clone())
                .await,
            Some(vec![true, false])
        );
        let saturated = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        assert_eq!(
            project_config_flags_within(&saturated, std::time::Duration::from_millis(50), dirs)
                .await,
            None
        );
    }

    /// PRD #2119: a listing stamps each record from its directory's answer,
    /// a record without a cwd reports unknown, and a batch that did not
    /// finish in time leaves every record unknown rather than holding the
    /// reply.
    #[tokio::test]
    async fn a_listing_stamps_answers_and_leaves_a_timed_out_batch_unknown() {
        let record = |id: &str, cwd: Option<&str>| -> crate::agent_pty::AgentRecord {
            serde_json::from_value(serde_json::json!({ "id": id, "cwd": cwd })).unwrap()
        };
        let mut records = vec![
            record("a", Some("/with")),
            record("b", Some("/without")),
            record("c", Some("/with")),
            record("d", None),
        ];
        crate::agent_pty::attach_project_config_flags(&mut records, |dirs| async move {
            assert_eq!(
                dirs,
                ["/with", "/without"],
                "one check per distinct directory"
            );
            Some(vec![true, false])
        })
        .await;
        let flags: Vec<_> = records.iter().map(|r| r.project_config_exists).collect();
        assert_eq!(flags, [Some(true), Some(false), Some(true), None]);
        crate::agent_pty::attach_project_config_flags(&mut records, |_| async { None }).await;
        assert!(
            records.iter().all(|r| r.project_config_exists.is_none()),
            "a batch that timed out leaves every record unknown"
        );
    }

    #[test]
    fn prompt_contains_key_sections() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains(".dot-agent-deck.toml"));
        assert!(prompt.contains("[[orchestrations]]"));
        // Issue #1199: the generator must never be taught to write a
        // `[[modes]]` block (it would only be ignored with a warning) — nor
        // its panes or rules.
        assert!(!prompt.contains("[[modes.panes]]"));
        assert!(!prompt.contains("[[modes.rules]]"));
        assert!(!prompt.contains("reactive_panes"));
        assert!(!prompt.contains("init_command"));
    }

    #[test]
    fn prompt_contains_orchestration_sections() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("[[orchestrations]]"));
        assert!(prompt.contains("[[orchestrations.roles]]"));
        assert!(prompt.contains("start = true"));
        assert!(prompt.contains("## Orchestrations"));
    }

    #[test]
    fn prompt_contains_orchestration_guidelines() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("Exactly one `start = true` role"));
        assert!(prompt.contains("All role names must be unique"));
        assert!(prompt.contains("prompt_template"));
        assert!(prompt.contains("description"));
        assert!(prompt.contains("clear"));
    }

    #[test]
    fn prompt_contains_orchestration_example() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("dev-flow"));
        assert!(prompt.contains("reviewer"));
        // The generator's stance on HOW MANY orchestrations to propose. One is
        // still the default answer; several are legitimate for genuinely
        // different workflows, and the prompt must carry both halves plus the
        // `extends` / `default` keys that make several maintainable (#704/#705).
        assert!(prompt.contains("One orchestration is the right answer for most projects"));
        assert!(prompt.contains("Propose more than one only when"));
        assert!(prompt.contains("use `extends` rather than copying"));
        assert!(prompt.contains("put `default = true` on exactly one"));
    }

    #[test]
    fn prompt_contains_role_library_section() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(prompt.contains("## Role Library"));
        assert!(!prompt.contains("{roles}"));
    }

    #[test]
    fn prompt_renders_every_library_role() {
        let prompt = config_gen_prompt("/tmp/test");
        for role in &role_library().role {
            assert!(
                prompt.contains(&format!("### `{}`", role.name)),
                "missing role header for `{}`",
                role.name
            );
            assert!(
                prompt.contains(&role.description),
                "missing description for role `{}`",
                role.name
            );
        }
    }

    #[test]
    fn role_library_parses_and_has_expected_roles() {
        let lib = role_library();
        let names: Vec<&str> = lib.role.iter().map(|r| r.name.as_str()).collect();
        for expected in [
            "coder",
            "reviewer",
            "auditor",
            "tester",
            "documenter",
            "release",
            "researcher",
        ] {
            assert!(
                names.contains(&expected),
                "role `{expected}` missing from library; got {names:?}"
            );
        }
        for role in &lib.role {
            assert!(!role.name.is_empty(), "role with empty name");
            assert!(
                !role.description.is_empty(),
                "role `{}` has empty description",
                role.name
            );
            assert!(
                !role.prompt_template.is_empty(),
                "role `{}` has empty prompt_template",
                role.name
            );
        }
    }

    #[test]
    fn prompt_contains_context_handoff_mandate() {
        let prompt = config_gen_prompt("/tmp/test");
        assert!(
            prompt.contains("Context-handoff rule"),
            "context-handoff mandate must appear in the orchestration composition guidance"
        );
        assert!(
            prompt.contains("clear = true"),
            "mandate must explain that workers cold-start with clear = true"
        );
    }

    #[test]
    fn every_worker_role_has_missing_context_backstop() {
        let lib = role_library();
        for role in &lib.role {
            assert!(
                role.prompt_template
                    .contains("the orchestrator will re-delegate with"),
                "role `{}` is missing the missing-context backstop sentence",
                role.name
            );
        }
    }

    /// Issue #551: the deck-operation rules every project otherwise relearns by
    /// incident. A blocked worker that waits on the user or delegates sideways
    /// stalls the run, so every role reports and stops instead.
    #[test]
    fn every_worker_role_reports_instead_of_waiting_or_delegating() {
        let lib = role_library();
        for role in &lib.role {
            for clause in [
                "Do not stop to ask the user anything and do not delegate to other roles",
                "say so in your work-done summary and stop",
                "Open that summary with one line naming the task and its outcome",
            ] {
                assert!(
                    role.prompt_template.contains(clause),
                    "role `{}` is missing the deck-operation clause {clause:?}",
                    role.name
                );
            }
        }
    }

    /// Issue #1947: agents in an orchestration message only its orchestrator,
    /// never another worker, and so do agents outside it.
    #[test]
    fn every_worker_role_messages_only_its_orchestrator() {
        let lib = role_library();
        for role in &lib.role {
            assert!(
                role.prompt_template
                    .contains("Message only your orchestrator, never another worker"),
                "role `{}` is missing the messaging convention",
                role.name
            );
        }
    }

    /// Issue #551: workers share one checkout, so a role that changes files
    /// must neither move the branch from under another agent nor sweep that
    /// agent's changes into its own commit.
    #[test]
    fn roles_that_change_files_protect_the_shared_checkout() {
        let lib = role_library();
        for name in ["coder", "tester", "documenter", "release"] {
            let role = lib
                .role
                .iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("role `{name}` present"));
            for clause in [
                "if the task names the branch or commit to work from",
                "confirm you are on it before",
                "report that and stop rather than switching, stashing or resetting",
                "Never discard changes you did not make",
                "stage the files you changed by name",
                "check what is staged before you commit",
            ] {
                assert!(
                    role.prompt_template.contains(clause),
                    "role `{name}` is missing the shared-checkout clause {clause:?}"
                );
            }
        }
        for name in ["reviewer", "auditor"] {
            let role = lib
                .role
                .iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("role `{name}` present"));
            for clause in [
                "if the task names the branch or commit to work from",
                "confirm you are on it before",
                "report that and stop rather than switching",
            ] {
                assert!(
                    role.prompt_template.contains(clause),
                    "role `{name}` is missing the branch-check clause {clause:?}"
                );
            }
        }
    }

    /// Issue #303: the prompt that generates new configs is the third surface
    /// carrying the delegation guidance, and it had no coverage at all. Round-3
    /// blocker 1 removed the heredoc fallback (a task line equal to the
    /// delimiter ends the heredoc and Bash executes the rest) and blocker 2
    /// closed the `!` hole in the inline allowlist; both have to hold here or a
    /// freshly generated config ships the version we just retracted.
    #[test]
    fn prompt_shell_safety_rule_matches_the_generated_protocol() {
        let prompt = config_gen_prompt("/tmp/test");

        assert!(
            prompt.contains("file-writing tool"),
            "the shell-safety rule must recommend a non-shell file writer"
        );
        assert!(
            !prompt.contains("<<"),
            "the shell-safety rule must not recommend a heredoc for writing the task file"
        );
        assert!(
            prompt.contains("does not already exist"),
            "the shell-safety rule must require a path that does not already exist"
        );
        // Round 4 / the #303 e2e gate: a generated config must not tell its
        // agents to depend on a file-writing tool they may not be authorized to
        // invoke — that turns the rule into a stall at an approval prompt.
        assert!(
            prompt.contains("not authorized") && prompt.contains("approval prompt"),
            "the shell-safety rule must state the fallback for an unauthorized file-writing \
             tool"
        );

        crate::state::assert_inline_allowlist_agrees_with_explanation(
            &prompt,
            "config-generation prompt",
        );

        // The worked example is what an agent copies into a real
        // `prompt_template`, so it carries the same rule in plain text. It lives
        // inside a TOML basic string, where a literal backslash is not
        // representable — hence the spelled-out form.
        let worked_example = prompt
            .split("### Worked Example")
            .nth(1)
            .expect("prompt must keep the worked example");
        assert!(
            worked_example.contains("no backticks, no $, no \", no backslash and no !"),
            "the worked example's inline-allowlist condition must exclude the same \
             characters as the rule above it"
        );
        assert!(
            worked_example.contains("never with shell redirection or a heredoc"),
            "the worked example must not reintroduce the heredoc fallback"
        );
        assert!(
            worked_example.contains("not authorized"),
            "the worked example must carry the unauthorized-file-writing-tool fallback too — \
             it is the text an agent copies into a real prompt_template"
        );
    }

    #[test]
    fn release_role_has_clear_false() {
        let lib = role_library();
        let release = lib
            .role
            .iter()
            .find(|r| r.name == "release")
            .expect("release role present");
        assert!(
            !release.clear,
            "release role should default to clear = false so it can resume after failure"
        );
    }
}
