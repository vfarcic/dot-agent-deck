//! PRD #1666 part 3 (Q35-13): the editor helper's installation
//! (`src/composer_probe/install.rs`) and its instance directory
//! (`src/composer_probe/instance.rs`), on real files in a temporary
//! directory: the pane classes and the rollout, what an installed spawn gets,
//! each file's mode and exclusive creation, each setup failure, the readers,
//! and retention. No daemon, PTY or agent; `tests/e2e_editor_installation.rs`
//! drives installation through a real daemon's spawns.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs::{File, Permissions};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use dot_agent_deck::composer_probe::classifier::{FileId, Identity};
use dot_agent_deck::composer_probe::install::{
    GENERATION_ENV, HELPER_ENV, INSTALL_ROLLOUT, NotInstalled, Originals, PaneClass, ProbeAgent,
    SAVED_EDITOR_ENV, SAVED_VISUAL_ENV, SHELLS, SetupError, assumed_classes, at_spawn,
    codex_policy_args, eligible, env_file, install, pane_class, saved_editor, shell_quote,
    with_args,
};
use dot_agent_deck::composer_probe::instance::{
    DAEMON_FILE, DaemonId, HELPER_NAME, INV_DIR, Instance, InstanceReader, Kind, Liveness,
    PAGE_BYTES, READ_MAX, ScanError, Sweep, candidate_roots, choose_root, decode_identity,
    encode_identity, generation_name_ok, identify, instance_name, liveness, liveness_from,
    may_be_alive, open_root, owned_by, parse_instance_name, removable, remove_identified,
    same_contents, scan_processes, scan_processes_in, scan_processes_with, splittable, status_uid,
    sweep, sweep_root, sweep_with,
};
use dot_agent_deck::doorbell_activation::{
    EditorProbe, EditorSelection, ExportShape, Measurement, PasteToggles,
};
use dot_agent_deck::event::AgentType;
use semver::Version;
use spec::spec;

const CLAUDE: PaneClass = PaneClass::Direct(ProbeAgent::Claude);
const CODEX: PaneClass = PaneClass::Direct(ProbeAgent::Codex);

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
}

/// A temporary directory whose path an agent can split, in the system's
/// temporary directory: every directory above it must be one no other user
/// can change, which a checkout made under a group-writable umask isn't.
fn tempdir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("installer")
        .rand_bytes(8)
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    assert!(splittable(dir.path()), "{}", dir.path().display());
    // Whatever the umask made it, so a case's parent is refused only when the
    // case means it to be.
    std::fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
    assert!(
        open_root(dir.path()).is_ok(),
        "{} passes through a directory another user could change: point TMPDIR at one that doesn't",
        dir.path().display()
    );
    dir
}

fn daemon_id(pid: u32, start: u64) -> DaemonId {
    DaemonId {
        pid,
        start,
        boot: "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0".to_string(),
        nonce: "00112233445566778899aabbccddeeff".to_string(),
    }
}

/// A small stand-in for the daemon's executable.
fn source(dir: &Path) -> File {
    let path = dir.join("source-exe");
    std::fs::write(
        &path,
        (0..70_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    File::open(path).unwrap()
}

/// An instance under `<dir>/state/editor` for a daemon that isn't running.
fn instance(dir: &Path) -> Instance {
    let root = dir.join("state").join("editor");
    Instance::create(&root, daemon_id(4_000_000, 7), &mut source(dir)).unwrap()
}

/// `path`, made if missing and set to 0700, so a root under it is trusted.
fn private_dir(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, Permissions::from_mode(0o700)).unwrap();
    path.to_path_buf()
}

/// What an instance's `daemon` file holds for `pid` and `start`.
fn daemon_record(pid: u32, start: u64) -> String {
    format!(
        "pid={pid}\nstart={start}\nboot=0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\nnonce=00112233445566778899aabbccddeeff\n"
    )
}

/// A directory under `root` laid out as `pid`'s instance: 0700, its 0400
/// `daemon` record naming it, and `inv/7/marker`.
fn fake_instance(root: &Path, pid: u32, start: u64) -> PathBuf {
    let dir = root.join(instance_name(pid, start));
    std::fs::create_dir_all(dir.join("inv/7")).unwrap();
    std::fs::write(dir.join("inv/7/marker"), b"x").unwrap();
    std::fs::write(dir.join(DAEMON_FILE), daemon_record(pid, start)).unwrap();
    std::fs::set_permissions(dir.join(DAEMON_FILE), Permissions::from_mode(0o400)).unwrap();
    std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
    dir
}

fn originals(visual: Option<&str>, editor: Option<&str>, preset: Option<&str>) -> Originals {
    Originals {
        visual: visual.map(str::to_string),
        editor: editor.map(str::to_string),
        claude_env_file: preset.map(str::to_string),
    }
}

/// `env` as a lookup.
fn get<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Scenario: Classify pane commands: no command, each shell by basename (also after an assignment or with a path), declared and recognised agents run directly or below something else, agents the probe doesn't cover, and undeclared commands no agent is recognised in. Only a plain argv whose first word is the agent's executable is direct; a shell is a shell whatever runs inside it.
#[spec("inbox/probe/064")]
#[test]
fn probe_064_a_pane_class_comes_from_the_command_and_its_declaration() {
    let claude = AgentType::ClaudeCode;
    let codex = AgentType::Codex;
    for (command, declared, class) in [
        (None, None, PaneClass::Shell),
        (Some("  "), None, PaneClass::Shell),
        (Some("FOO=1"), None, PaneClass::Shell),
        (
            Some("/bin/bash --noprofile -c 'exec claude'"),
            None,
            PaneClass::Shell,
        ),
        (Some("FOO=1 zsh -l"), Some(&claude), PaneClass::Shell),
        (Some("claude"), None, CLAUDE),
        (Some("'/opt/a b/claude' --resume x"), None, CLAUDE),
        (Some("claude"), Some(&claude), CLAUDE),
        (Some("codex 'a prompt'"), None, CODEX),
        (Some("/usr/local/bin/codex"), Some(&codex), CODEX),
        (
            Some("codex; echo done"),
            Some(&codex),
            PaneClass::Launcher(ProbeAgent::Codex),
        ),
        (
            Some("codex > log"),
            Some(&codex),
            PaneClass::Launcher(ProbeAgent::Codex),
        ),
        (
            Some("codex # note"),
            Some(&codex),
            PaneClass::Launcher(ProbeAgent::Codex),
        ),
        (
            Some("codex $(cat args)"),
            Some(&codex),
            PaneClass::Launcher(ProbeAgent::Codex),
        ),
        (
            Some("codex `cat args`"),
            Some(&codex),
            PaneClass::Launcher(ProbeAgent::Codex),
        ),
        (
            Some("codex a\\ b"),
            Some(&codex),
            PaneClass::Launcher(ProbeAgent::Codex),
        ),
        (
            Some("FOO=1 codex"),
            Some(&codex),
            PaneClass::Launcher(ProbeAgent::Codex),
        ),
        (Some("codex; echo done"), None, PaneClass::Wrapper),
        (
            Some("devbox run agent"),
            Some(&claude),
            PaneClass::Launcher(ProbeAgent::Claude),
        ),
        (
            Some("launcher"),
            Some(&codex),
            PaneClass::Launcher(ProbeAgent::Codex),
        ),
        (Some("opencode"), None, PaneClass::Unprobed),
        (Some("pi"), None, PaneClass::Unprobed),
        (
            Some("wrapper"),
            Some(&AgentType::OpenCode),
            PaneClass::Unprobed,
        ),
        (Some("my-wrapper --flag"), None, PaneClass::Wrapper),
        (
            Some("my-wrapper"),
            Some(&AgentType::None),
            PaneClass::Wrapper,
        ),
    ] {
        assert_eq!(
            pane_class(command, declared),
            class,
            "{command:?} {declared:?}"
        );
    }
    for shell in SHELLS {
        assert_eq!(pane_class(Some(shell), None), PaneClass::Shell, "{shell}");
        let path = format!("/usr/bin/{shell} -c 'codex'");
        assert_eq!(pane_class(Some(&path), Some(&codex)), PaneClass::Shell);
    }
}

fn probe() -> EditorProbe<'static> {
    EditorProbe {
        export: ExportShape::ClaudeTmpdir,
        selection: EditorSelection::ClaudeCode,
        paste_toggles: PasteToggles {
            disables: 1,
            enables: 1,
        },
        keymap_sources: &[],
        keymap_loaded_at_start: Measurement::Absent,
        inert_candidates: &[],
        pasted_bel_is_text: Measurement::Absent,
    }
}

/// Scenario: Decide eligibility from the rollout: production's rollout is empty, so every class, the plain agent commands among them, is not installed and the decision at a spawn says so without making anything; a shell is never installed, even under a rollout naming it; the assumption's agents roll out Claude's direct, launcher and wrapper classes and Codex's direct and launcher classes, each once.
#[spec("inbox/probe/064")]
#[test]
fn probe_064_the_rollout_decides_and_production_installs_nothing() {
    assert!(INSTALL_ROLLOUT.is_empty());
    for class in [
        CLAUDE,
        CODEX,
        PaneClass::Launcher(ProbeAgent::Claude),
        PaneClass::Wrapper,
        PaneClass::Unprobed,
    ] {
        assert_eq!(eligible(class, INSTALL_ROLLOUT), Err(NotInstalled::Rollout));
        assert_eq!(eligible(class, &[class]), Ok(()), "{class:?}");
    }
    assert_eq!(
        eligible(PaneClass::Shell, &[PaneClass::Shell]),
        Err(NotInstalled::Shell)
    );
    for (command, why) in [
        (Some("claude"), NotInstalled::Rollout),
        (Some("codex"), NotInstalled::Rollout),
        (Some("my-wrapper"), NotInstalled::Rollout),
        (Some("bash"), NotInstalled::Shell),
        (None, NotInstalled::Shell),
    ] {
        assert_eq!(
            at_spawn(command, None, "1", &[]).err(),
            Some(why),
            "{command:?}"
        );
    }

    let release = Version::new(2, 1, 310);
    let claude = (AgentType::ClaudeCode, release.clone(), probe());
    let codex = (AgentType::Codex, Version::new(0, 164, 0), probe());
    let claude_classes = [
        CLAUDE,
        PaneClass::Launcher(ProbeAgent::Claude),
        PaneClass::Wrapper,
    ];
    let codex_classes = [CODEX, PaneClass::Launcher(ProbeAgent::Codex)];
    assert_eq!(assumed_classes(&[]), []);
    assert_eq!(
        assumed_classes(std::slice::from_ref(&claude)),
        claude_classes
    );
    assert_eq!(assumed_classes(std::slice::from_ref(&codex)), codex_classes);
    let both = assumed_classes(&[claude.clone(), codex.clone(), claude.clone()]);
    assert_eq!(both, [&claude_classes[..], &codex_classes[..]].concat());
    assert_eq!(
        assumed_classes(&[(AgentType::OpenCode, release, probe())]),
        []
    );
}

/// Scenario: Resolve the editor settings a spawn would have had: the spawn's own last value wins over the daemon's, an exported-empty value stays empty, a value set nowhere is unset, and a daemon value that isn't UTF-8 refuses. The helper's saved settings read back from an environment, unset staying unset.
#[spec("inbox/probe/064")]
#[test]
fn probe_064_originals_are_what_the_child_would_have_seen() {
    let env = vec![
        ("VISUAL".to_string(), "first".to_string()),
        ("VISUAL".to_string(), "last".to_string()),
        ("EDITOR".to_string(), String::new()),
    ];
    let inherited = |name: &str| match name {
        "VISUAL" => Some(OsString::from("daemon-visual")),
        "CLAUDE_ENV_FILE" => Some(OsString::from("/home/u/env.sh")),
        _ => None,
    };
    assert_eq!(
        Originals::resolve(&env, inherited),
        Ok(originals(Some("last"), Some(""), Some("/home/u/env.sh")))
    );
    assert_eq!(
        Originals::resolve(&[], |_| None),
        Ok(originals(None, None, None))
    );
    assert_eq!(
        Originals::resolve(&[], |name| (name == "EDITOR")
            .then(|| OsString::from_vec(vec![b'v', 0xff]))),
        Err(SetupError::NotUtf8)
    );

    let saved = saved_editor(|name| match name {
        SAVED_VISUAL_ENV => Some(OsString::from("")),
        _ => None,
    });
    assert_eq!(saved.visual, Some(OsString::from("")));
    assert_eq!(saved.editor, None);
}

/// `script` run by bash with `env`, its stdout.
fn bash(script: &str, args: &[&str], env: &[(&str, &str)]) -> String {
    let mut command = Command::new("/bin/bash");
    command
        .args(["--noprofile", "--norc", "-c", script, "bash"])
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin");
    for (k, v) in env {
        command.env(k, v);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

const SHOW: &str = r#"source "$1" || exit; printf '%s|%s|%s|%s' "${VISUAL-unset}" "${VISUAL+set}" "${EDITOR-unset}" "${EDITOR+set}""#;

/// Scenario: Write the Claude-style environment file for exported, exported-empty and unset settings, including values with quotes, `$(…)`, `;` and spaces, and source it from bash with the helper in both variables: each comes back exactly, never evaluated. With a user file preset at a path with spaces and an apostrophe, it is sourced after the restoration and its settings win.
#[spec("inbox/probe/064")]
#[test]
fn probe_064_the_environment_file_restores_then_sources_the_users() {
    let dir = tempdir();
    let file = dir.path().join("env");
    let helper = [
        ("VISUAL", "/x/dot-agent-deck-editor"),
        ("EDITOR", "/x/dot-agent-deck-editor"),
    ];
    for (visual, editor, shown) in [
        (Some("vim -f"), Some(""), "vim -f|set||set"),
        (None, None, "unset||unset|"),
        (
            Some("it's $(touch pwned); echo `id` | x"),
            Some("a  b"),
            "it's $(touch pwned); echo `id` | x|set|a  b|set",
        ),
    ] {
        std::fs::write(&file, env_file(&originals(visual, editor, None))).unwrap();
        assert_eq!(bash(SHOW, &[file.to_str().unwrap()], &helper), shown);
    }
    assert!(!Path::new("pwned").exists());

    let theirs = dir.path().join("user env ' ; x.sh");
    std::fs::write(&theirs, "export VISUAL='theirs'\nunset EDITOR\n").unwrap();
    let contents = env_file(&originals(Some("mine"), Some("mine"), theirs.to_str()));
    std::fs::write(&file, &contents).unwrap();
    assert_eq!(
        bash(SHOW, &[file.to_str().unwrap()], &helper),
        "theirs|set|unset|"
    );
    let text = String::from_utf8(contents).unwrap();
    assert!(
        text.find("export VISUAL").unwrap() < text.find(". ").unwrap(),
        "{text}"
    );
    // An empty preset is no preset.
    let empty = String::from_utf8(env_file(&originals(None, None, Some("")))).unwrap();
    assert!(!empty.contains(". "), "{empty}");
}

/// Scenario: Build Codex's tool-environment arguments: one `-c shell_environment_policy.set.<NAME>=` pair per set value, an exported-empty value included, as a TOML string with quotes, backslashes and control characters escaped; none for an unset value, and never an `exclude`. Appended to a command, each argument reaches the program as one word.
#[spec("inbox/probe/064")]
#[test]
fn probe_064_codex_gets_set_values_only() {
    assert_eq!(
        codex_policy_args(&originals(None, None, None)),
        Vec::<String>::new()
    );
    assert_eq!(
        codex_policy_args(&originals(Some(""), None, Some("/ignored"))),
        ["-c", "shell_environment_policy.set.VISUAL=\"\""]
    );
    let args = codex_policy_args(&originals(Some("say \"hi\" \\ there"), Some("ed\n"), None));
    assert_eq!(
        args,
        [
            "-c",
            r#"shell_environment_policy.set.VISUAL="say \"hi\" \\ there""#,
            "-c",
            r#"shell_environment_policy.set.EDITOR="ed\u000A""#,
        ]
    );
    assert!(!args.iter().any(|a| a.contains("exclude")));

    let words = ["plain", "it's", "a b", "$(x) `y`; z | w", ""];
    let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    let line = with_args("printf '%s\\n'", &words);
    let printed = bash(&line, &[], &[]);
    assert_eq!(
        printed,
        words.iter().map(|w| format!("{w}\n")).collect::<String>()
    );
    assert_eq!(shell_quote("it's"), r"'it'\''s'");
}

/// Scenario: Make an instance for a daemon: the directory, named for the daemon's pid and start, is 0700 and holds `daemon` at 0400 recording the daemon, `inv/` at 0700, and the helper at 0500 with the source's bytes and its `(dev, ino)` as recorded, and no temporary copy. The reader reads `daemon` back.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_an_instance_is_made_with_its_modes() {
    let dir = tempdir();
    let instance = instance(dir.path());
    let path = instance.dir().to_path_buf();
    assert_eq!(
        path,
        dir.path()
            .join("state/editor")
            .join(instance_name(4_000_000, 7))
    );
    assert_eq!(mode(&path), 0o700);
    assert_eq!(mode(&path.join(DAEMON_FILE)), 0o400);
    assert_eq!(mode(&path.join(INV_DIR)), 0o700);
    assert_eq!(instance.helper(), path.join(HELPER_NAME));
    assert_eq!(mode(instance.helper()), 0o500);
    let metadata = std::fs::metadata(instance.helper()).unwrap();
    assert_eq!(
        instance.helper_id(),
        FileId {
            dev: metadata.dev(),
            ino: metadata.ino()
        }
    );
    assert!(
        same_contents(
            &mut source(dir.path()),
            &mut File::open(instance.helper()).unwrap()
        )
        .unwrap()
    );
    let mut names: Vec<_> = std::fs::read_dir(&path)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, [DAEMON_FILE, HELPER_NAME, INV_DIR]);
    let reader = InstanceReader::open(&path).unwrap();
    assert_eq!(reader.daemon(), Some(daemon_id(4_000_000, 7)));
    assert_eq!(instance.daemon(), &daemon_id(4_000_000, 7));

    let me = DaemonId::this_process().unwrap();
    assert_eq!(me.pid, std::process::id());
    assert_eq!(me.nonce.len(), 32);
    assert_ne!(me.nonce, DaemonId::this_process().unwrap().nonce);
    assert_eq!(DaemonId::decode(me.boot.as_bytes()), None);
}

/// Scenario: Each way setting an instance up fails: an instance directory that exists already is never reused; a root whose path an agent can't split, or that is relative; a root, or the directory above it, that is a symlink or writable by its group or others; and a source that can't be read. Each refuses with its cause. The root chosen is the state directory's unless its path can't be split, then the runtime directory's, else none.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_each_setup_failure_refuses() {
    let dir = tempdir();
    let base = dir.path();
    let _ = instance(base);
    let root = base.join("state").join("editor");
    assert_eq!(
        Instance::create(&root, daemon_id(4_000_000, 7), &mut source(base)).err(),
        Some(SetupError::InstanceExists)
    );
    for root in [
        base.join("with space/editor"),
        PathBuf::from("relative/editor"),
    ] {
        assert_eq!(
            Instance::create(&root, daemon_id(1, 1), &mut source(base)).err(),
            Some(SetupError::PathCharacters),
            "{}",
            root.display()
        );
    }

    let open = base.join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, Permissions::from_mode(0o775)).unwrap();
    let shared_root = base.join("shared");
    std::fs::create_dir(&shared_root).unwrap();
    std::fs::set_permissions(&shared_root, Permissions::from_mode(0o707)).unwrap();
    let target = base.join("target");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, base.join("link")).unwrap();
    // The last root, `/usr`, is another user's.
    for root in [
        open.join("editor"),
        shared_root,
        base.join("link"),
        PathBuf::from("/usr"),
    ] {
        assert_eq!(
            Instance::create(&root, daemon_id(2, 2), &mut source(base)).err(),
            Some(SetupError::UntrustedDirectory),
            "{}",
            root.display()
        );
    }

    let unreadable = File::create(base.join("write-only")).unwrap();
    let error = Instance::create(&base.join("fresh/editor"), daemon_id(3, 3), &mut {
        unreadable
    });
    assert!(matches!(error, Err(SetupError::Io(Some(_)))), "{error:?}");

    let state = Path::new("/var/lib/state");
    let runtime = Path::new("/run/user/1000");
    assert_eq!(choose_root(state, Some(runtime)), Ok(state.join("editor")));
    assert_eq!(
        choose_root(Path::new("/home/a b/state"), Some(runtime)),
        Ok(runtime.join("dot-agent-deck/editor"))
    );
    assert_eq!(
        choose_root(Path::new("state"), Some(runtime)),
        Ok(runtime.join("dot-agent-deck/editor"))
    );
    assert_eq!(
        choose_root(Path::new("/home/a b/state"), None),
        Err(SetupError::PathCharacters)
    );
    assert_eq!(
        choose_root(Path::new("/home/a b"), Some(Path::new("/run/a:b"))),
        Err(SetupError::PathCharacters)
    );
    assert!(splittable(Path::new("/a/B-9_.x")));
    for bad in ["/a b", "/a'b", "/a$b", "/a\\b", "/é"] {
        assert!(!splittable(Path::new(bad)), "{bad}");
    }
}

/// Scenario: Compare a copy with its source byte for byte: the same bytes are the same, one byte changed or one byte longer is not, across a buffer's boundary.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_the_copy_check_compares_every_byte() {
    let dir = tempdir();
    let bytes: Vec<u8> = (0..(3 << 20) + 5).map(|i: u32| (i % 253) as u8).collect();
    let write = |name: &str, bytes: &[u8]| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        File::open(path).unwrap()
    };
    let mut changed = bytes.clone();
    changed[(1 << 20) + 3] ^= 1;
    let mut longer = bytes.clone();
    longer.push(0);
    let mut a = write("a", &bytes);
    assert!(same_contents(&mut a, &mut write("b", &bytes)).unwrap());
    assert!(!same_contents(&mut a, &mut write("c", &changed)).unwrap());
    assert!(!same_contents(&mut a, &mut write("d", &longer)).unwrap());
    assert!(!same_contents(&mut write("e", &longer), &mut a).unwrap());
    assert!(same_contents(&mut write("f", &[]), &mut write("g", &[])).unwrap());
}

/// Scenario: Install each class in an instance. Claude direct and launcher, and the wrapper, get the helper as `VISUAL` and `EDITOR`, the generation, each set value saved (unset stays absent) and `CLAUDE_ENV_FILE` naming the generation's 0600 file; Codex direct gets the set values as its policy arguments and no environment file; Codex below a launcher neither. `kind-G` names a known agent and is absent for the wrapper; `inv/G/` is 0700; the arm page is one zeroed page at 0600 that the daemon's publisher writes through. The spawn's variables replace its own editor settings and any stale helper variable.
#[spec("inbox/probe/064")]
#[test]
fn probe_064_an_installed_spawn_gets_its_helper_and_files() {
    let dir = tempdir();
    let instance = instance(dir.path());
    let path = instance.dir().to_path_buf();
    let helper = instance.helper().to_str().unwrap().to_string();
    let reader = InstanceReader::open(&path).unwrap();
    let set = originals(Some("vim -f"), None, Some("/home/u/env.sh"));
    for (generation, class, kind, env_file_expected, args) in [
        (
            "1",
            CLAUDE,
            Kind::Agent(ProbeAgent::Claude),
            true,
            Vec::<&str>::new(),
        ),
        (
            "2",
            PaneClass::Launcher(ProbeAgent::Claude),
            Kind::Agent(ProbeAgent::Claude),
            true,
            vec![],
        ),
        ("3", PaneClass::Wrapper, Kind::Learned, true, vec![]),
        (
            "4",
            CODEX,
            Kind::Agent(ProbeAgent::Codex),
            false,
            vec!["-c", "shell_environment_policy.set.VISUAL=\"vim -f\""],
        ),
        (
            "5",
            PaneClass::Launcher(ProbeAgent::Codex),
            Kind::Agent(ProbeAgent::Codex),
            false,
            vec![],
        ),
    ] {
        let mut installation = install(&instance, class, generation, &set).unwrap();
        assert_eq!(installation.class(), class);
        let env = installation.env().to_vec();
        assert_eq!(get(&env, "VISUAL"), Some(helper.as_str()), "{class:?}");
        assert_eq!(get(&env, "EDITOR"), Some(helper.as_str()));
        assert_eq!(get(&env, GENERATION_ENV), Some(generation));
        assert_eq!(get(&env, SAVED_VISUAL_ENV), Some("vim -f"));
        assert_eq!(get(&env, SAVED_EDITOR_ENV), None);
        assert_eq!(installation.args(), args, "{class:?}");
        let env_path = path.join(format!("env-{generation}"));
        if env_file_expected {
            assert_eq!(get(&env, "CLAUDE_ENV_FILE"), env_path.to_str());
            assert_eq!(installation.files().env_file(), Some(env_path.as_path()));
            assert_eq!(mode(&env_path), 0o600);
            assert_eq!(std::fs::read(&env_path).unwrap(), env_file(&set));
        } else {
            assert_eq!(get(&env, "CLAUDE_ENV_FILE"), None);
            assert!(!env_path.exists());
        }
        assert_eq!(reader.kind(generation), Some(kind));
        if let Kind::Agent(_) = kind {
            assert_eq!(mode(&path.join(format!("kind-{generation}"))), 0o400);
        }
        assert_eq!(mode(&path.join(INV_DIR).join(generation)), 0o700);
        let arm = path.join(format!("arm-{generation}"));
        assert_eq!(mode(&arm), 0o600);
        assert_eq!(std::fs::read(&arm).unwrap(), vec![0; PAGE_BYTES]);
        installation
            .files()
            .arm()
            .publisher()
            .publish_native(2, 3, 4);
        let page = std::fs::read(&arm).unwrap();
        let words: Vec<u64> = page[..24]
            .chunks(8)
            .map(|c| u64::from_ne_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(words, [2 << 32 | 2, 3, 4]);
        assert!(reader.identity(generation).is_none());
    }

    let installation = install(&instance, CODEX, "6", &originals(None, None, None)).unwrap();
    assert!(installation.args().is_empty());
    assert_eq!(installation.command(Some("codex")), None);
    let installation = install(&instance, CODEX, "7", &originals(Some("e"), None, None)).unwrap();
    assert_eq!(
        installation.command(Some("codex")).as_deref(),
        Some(
            "codex '-c' 'shell_environment_policy.set={}' \
             '-c' 'shell_environment_policy.set.VISUAL=\"e\"'"
        )
    );
    let mut env = vec![
        ("VISUAL".to_string(), "mine".to_string()),
        ("KEEP".to_string(), "1".to_string()),
        (HELPER_ENV[1].to_string(), "stale".to_string()),
        ("EDITOR".to_string(), "mine".to_string()),
    ];
    installation.apply_env(&mut env);
    assert_eq!(get(&env, "KEEP"), Some("1"));
    assert_eq!(get(&env, "VISUAL"), Some(helper.as_str()));
    assert_eq!(get(&env, SAVED_VISUAL_ENV), Some("e"));
    assert_eq!(get(&env, SAVED_EDITOR_ENV), None);
    assert_eq!(env.iter().filter(|(k, _)| k == "VISUAL").count(), 1);
    assert_eq!(env.iter().filter(|(k, _)| k == "EDITOR").count(), 1);
}

/// Scenario: Install a direct Codex whose saved editor value holds shell syntax (`;`, `&`, `|`, parentheses), and one whose value is plain, and run each installed command through the wrapper's launch rewrite. Either way the command carries Codex's embedded-mode pair once, right after the executable and before the editor settings: through the wrapper's shell for the value with syntax, where the wrapper adds nothing, and on the plain launch, where the wrapper finds the pair and adds no second one. A command that already opens with the pair gets no second one, and a quoted executable keeps its quoting.
#[spec("inbox/probe/064")]
#[test]
fn probe_064_a_direct_codex_carries_its_embedded_pair_on_every_launch_shape() {
    let dir = tempdir();
    let instance = instance(dir.path());
    let pair = ["-c", "shell_environment_policy.set={}"];
    let opens_with_pair = |argv: &[String]| argv.len() >= 3 && argv[1..3] == pair;
    let count_pair = |argv: &[String]| argv.windows(2).filter(|w| *w == pair).count();
    for (generation, value, through_shell) in [
        ("1", "ed; rm -rf x", true),
        ("2", "a & b | c (d)", true),
        ("3", "vim -f", false),
    ] {
        let installation = install(
            &instance,
            CODEX,
            generation,
            &originals(Some(value), None, None),
        )
        .unwrap();
        let installed = installation.command(Some("codex --model o3")).unwrap();
        let argv = shlex::split(&installed).unwrap();
        assert!(opens_with_pair(&argv), "{value}: {argv:?}");
        assert_eq!(count_pair(&argv), 1, "{value}: once");
        assert_eq!(&argv[3..5], ["--model", "o3"], "{value}: its own arguments");
        assert!(
            argv[5..]
                .iter()
                .any(|a| a == &format!("shell_environment_policy.set.VISUAL=\"{value}\"")),
            "{value}: the settings after the pair: {argv:?}"
        );
        match dot_agent_deck::wrap::wrap_launch_command(&installed, &AgentType::Codex, "/bin/sh") {
            dot_agent_deck::wrap::LaunchCommand::Argv(wrapper) => {
                assert!(through_shell, "{value}: {wrapper:?}");
                let shell_line = wrapper.last().unwrap();
                assert_eq!(shell_line, &installed, "the shell runs the installed line");
            }
            dot_agent_deck::wrap::LaunchCommand::Line(line) => {
                assert!(!through_shell, "{value}: {line}");
                let words = shlex::split(&line).unwrap();
                let after = words.iter().position(|w| w == "--").unwrap() + 1;
                assert!(opens_with_pair(&words[after..]), "{line}");
                assert_eq!(count_pair(&words), 1, "{line}");
            }
        }
    }
    let installation = install(&instance, CODEX, "4", &originals(Some("e"), None, None)).unwrap();
    let theirs = installation
        .command(Some(
            "codex -c 'shell_environment_policy.set={}' --model o3",
        ))
        .unwrap();
    assert_eq!(count_pair(&shlex::split(&theirs).unwrap()), 1, "{theirs}");
    let quoted = installation
        .command(Some("'/opt/my tools/codex' --model o3"))
        .unwrap();
    let argv = shlex::split(&quoted).unwrap();
    assert_eq!(argv[0], "/opt/my tools/codex");
    assert!(opens_with_pair(&argv), "{argv:?}");
    let launcher = install(
        &instance,
        PaneClass::Launcher(ProbeAgent::Codex),
        "5",
        &originals(Some("e"), None, None),
    )
    .unwrap();
    assert_eq!(
        launcher.command(Some("devbox run codex")),
        None,
        "no settings below a launcher"
    );
}

/// Scenario: Refuse a generation's files when its name can't be a file name (empty, a path, a dot, too long) or when any of them exists already: a second installation of the same generation, or a `kind-G` or `gen-G` written twice, refuses without replacing what is there. Neither a shell nor an uncovered agent installs.
#[spec("inbox/probe/064")]
#[test]
fn probe_064_a_generation_is_never_reused() {
    let dir = tempdir();
    let instance = instance(dir.path());
    let set = originals(None, None, None);
    for name in ["", "../x", "a/b", ".", "x.y", &"9".repeat(65)] {
        assert!(!generation_name_ok(name), "{name:?}");
        assert_eq!(
            install(&instance, CLAUDE, name, &set).err(),
            Some(SetupError::GenerationName),
            "{name:?}"
        );
    }
    assert!(generation_name_ok(&"9".repeat(64)));
    assert!(generation_name_ok("a-B_9"));
    install(&instance, CLAUDE, "1", &set).unwrap();
    let kind = instance.dir().join("kind-1");
    assert_eq!(
        install(&instance, CLAUDE, "1", &set).err(),
        Some(SetupError::GenerationExists)
    );
    assert_eq!(
        instance.write_kind("1", ProbeAgent::Codex).err(),
        Some(SetupError::GenerationExists)
    );
    assert_eq!(std::fs::read_to_string(kind).unwrap(), "claude\n");
    // Another generation whose arm page is already there.
    std::fs::write(instance.dir().join("arm-2"), b"x").unwrap();
    assert_eq!(
        install(&instance, CLAUDE, "2", &set).err(),
        Some(SetupError::GenerationExists)
    );
    for class in [PaneClass::Shell, PaneClass::Unprobed] {
        assert_eq!(
            install(&instance, class, "3", &set).err(),
            Some(SetupError::NotInstallable),
            "{class:?}"
        );
    }
    assert!(!instance.dir().join("arm-3").exists());
}

fn identity(root: &Path) -> Identity {
    let metadata = std::fs::metadata(root).unwrap();
    Identity {
        pid: 1234,
        start: 99,
        exe: FileId { dev: 8, ino: 77 },
        install_root: root.to_path_buf(),
        install_root_id: FileId {
            dev: metadata.dev(),
            ino: metadata.ino(),
        },
        basename: OsString::from_vec(b"claude\xff".to_vec()),
    }
}

/// Scenario: Write a generation's identity and read it back whole, a basename that isn't UTF-8 included, at 0400. An identity whose install root doesn't exist, isn't a directory, reads as another directory, or is relative, or whose basename is empty, writes nothing, and the reader reads no identity for it; nor for a malformed or symlinked file.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_the_identity_file_is_written_only_for_a_real_root() {
    let dir = tempdir();
    let instance = instance(dir.path());
    let reader = InstanceReader::open(instance.dir()).unwrap();
    let root = dir.path().join("versions");
    std::fs::create_dir(&root).unwrap();
    let good = identity(&root);
    instance.write_identity("1", &good).unwrap();
    assert_eq!(reader.identity("1"), Some(good.clone()));
    assert_eq!(mode(&instance.dir().join("gen-1")), 0o400);
    assert_eq!(
        instance.write_identity("1", &good).err(),
        Some(SetupError::GenerationExists)
    );

    let file = dir.path().join("a-file");
    std::fs::write(&file, b"").unwrap();
    let bad = [
        Identity {
            install_root: dir.path().join("missing"),
            ..good.clone()
        },
        Identity {
            install_root: file,
            ..good.clone()
        },
        Identity {
            install_root_id: FileId {
                dev: good.install_root_id.dev,
                ino: good.install_root_id.ino + 1,
            },
            ..good.clone()
        },
        Identity {
            install_root: PathBuf::from("versions"),
            ..good.clone()
        },
        Identity {
            basename: OsString::new(),
            ..good.clone()
        },
    ];
    for (i, identity) in bad.iter().enumerate() {
        let generation = format!("b{i}");
        assert_eq!(
            instance.write_identity(&generation, identity).err(),
            Some(SetupError::InstallRoot),
            "{identity:?}"
        );
        assert!(!instance.dir().join(format!("gen-{generation}")).exists());
        assert_eq!(reader.identity(&generation), None);
    }

    let text = encode_identity(&good);
    assert_eq!(decode_identity(text.as_bytes()), Some(good.clone()));
    let malformed = [
        text.trim_end().to_string(),
        format!("{text}extra=1\n"),
        text.replacen("pid=1234", "pid=12a4", 1),
        text.replacen("pid=1234", "pid=", 1),
        text.replacen("exe=8:77", "exe=8-77", 1),
        text.replacen("start=99\n", "", 1),
        text.replacen("pid=1234\nstart=99", "start=99\npid=1234", 1),
        text.replacen("basename=636c61756465ff", "basename=636C61756465FF", 1),
        text.replacen("basename=636c61756465ff", "basename=636c61756465f", 1),
    ];
    for bytes in &malformed {
        assert_eq!(decode_identity(bytes.as_bytes()), None, "{bytes}");
    }
    std::fs::write(dir.path().join("elsewhere"), text).unwrap();
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), instance.dir().join("gen-9")).unwrap();
    assert_eq!(reader.identity("9"), None);
    assert_eq!(reader.identity("../1"), None);
}

/// Scenario: Read the instance's other files: `daemon` back as written, and nothing for one with a line missing, added or reordered, a pid that isn't a number, uppercase hex or no final newline; `kind-G` as the agent written, as learned when absent, and as unreadable when it holds anything else or is a symlink.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_the_reader_reads_only_exact_files() {
    let dir = tempdir();
    let fake = dir.path().join("fake");
    std::fs::create_dir(&fake).unwrap();
    let reader = InstanceReader::open(&fake).unwrap();
    assert_eq!(reader.daemon(), None);
    let good = "pid=12\nstart=34\nboot=0f1e-2d\nnonce=00ff\n";
    std::fs::write(fake.join(DAEMON_FILE), good).unwrap();
    assert_eq!(
        reader.daemon(),
        Some(DaemonId {
            pid: 12,
            start: 34,
            boot: "0f1e-2d".into(),
            nonce: "00ff".into()
        })
    );
    for bad in [
        "pid=12\nstart=34\nboot=0f1e-2d\n",
        "pid=12\nstart=34\nboot=0f1e-2d\nnonce=00ff\nmore=1\n",
        "start=34\npid=12\nboot=0f1e-2d\nnonce=00ff\n",
        "pid=x\nstart=34\nboot=0f1e-2d\nnonce=00ff\n",
        "pid=12\nstart=34\nboot=0F1E\nnonce=00ff\n",
        "pid=12\nstart=34\nboot=0f1e-2d\nnonce=00-ff\n",
        "pid=12\nstart=34\nboot=\nnonce=00ff\n",
        "pid=12\nstart=34\nboot=0f1e-2d\nnonce=00ff",
        "pid=+12\nstart=34\nboot=0f1e-2d\nnonce=00ff\n",
    ] {
        assert_eq!(DaemonId::decode(bad.as_bytes()), None, "{bad:?}");
    }
    std::fs::write(fake.join("kind-1"), "claude\n").unwrap();
    std::fs::write(fake.join("kind-2"), "codex\n").unwrap();
    std::fs::write(fake.join("kind-3"), "claude").unwrap();
    std::fs::write(fake.join("kind-4"), "opencode\n").unwrap();
    std::os::unix::fs::symlink(fake.join("kind-1"), fake.join("kind-5")).unwrap();
    assert_eq!(reader.kind("1"), Some(Kind::Agent(ProbeAgent::Claude)));
    assert_eq!(reader.kind("2"), Some(Kind::Agent(ProbeAgent::Codex)));
    assert_eq!(reader.kind("3"), None);
    assert_eq!(reader.kind("4"), None);
    assert_eq!(reader.kind("5"), None);
    assert_eq!(reader.kind("6"), Some(Kind::Learned));
    assert_eq!(reader.kind("a/b"), None);
    std::fs::write(fake.join("kind-7"), vec![b'x'; 5000]).unwrap();
    assert_eq!(reader.kind("7"), None);
    std::fs::create_dir(fake.join("kind-8")).unwrap();
    assert_eq!(reader.kind("8"), None);
    std::os::unix::fs::symlink(&fake, dir.path().join("link")).unwrap();
    assert!(InstanceReader::open(&dir.path().join("link")).is_err());
}

/// Scenario: Put a FIFO with no writer at `arm-1`, `kind-2` and `daemon` in an instance directory, a symlink to a regular file at `arm-3` and nothing at `arm-4`, and read each on another thread. Every read returns within five seconds instead of blocking in its open: the FIFO arm as an error, the kind as unreadable and the daemon record as absent; the linked arm is refused as a link and the missing one reads as absent.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_the_arm_reader_refuses_a_fifo_or_a_link_without_blocking() {
    let dir = tempdir();
    let fake = dir.path().join("fake");
    std::fs::create_dir(&fake).unwrap();
    for name in ["arm-1", "kind-2", DAEMON_FILE] {
        let path = std::ffi::CString::new(fake.join(name).into_os_string().into_vec()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }
    std::fs::write(dir.path().join("page"), [0u8; 24]).unwrap();
    std::os::unix::fs::symlink(dir.path().join("page"), fake.join("arm-3")).unwrap();
    let (done, results) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = InstanceReader::open(&fake).unwrap();
        let arm = |generation| {
            reader
                .arm(generation)
                .map(|file| file.is_some())
                .map_err(|e| e.raw_os_error())
        };
        let arms = [arm("1"), arm("3"), arm("4")];
        done.send((arms, reader.kind("2"), reader.daemon()))
            .unwrap();
    });
    let (arms, kind, daemon) = results
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("a read blocked on a FIFO");
    assert_eq!(
        arms,
        [Err(Some(libc::EINVAL)), Err(Some(libc::ELOOP)), Ok(false)]
    );
    assert_eq!(kind, None);
    assert_eq!(daemon, None);
}

fn candidates(names: &[&str]) -> Vec<(String, Vec<u8>)> {
    names
        .iter()
        .map(|n| (n.to_string(), format!("/root/editor/{n}").into_bytes()))
        .collect()
}

/// Scenario: Decide retention from liveness and a scan: a gone daemon's directory that no process names is removed; a live daemon's, one a process's environment or maps name, and one whose name isn't an instance's are kept; a scan that fails keeps everything; with no daemon gone nothing is scanned.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_retention_removes_only_what_nothing_uses() {
    let all = candidates(&["10-1", "11-1", "12-1", "13-1", "x-1", "14", "15-1x"]);
    let alive = |pid: u32, start: u64| pid == 10 && start == 1;
    let removed = removable(&all, alive, |visit| {
        visit(b"VISUAL=/root/editor/11-1/dot-agent-deck-editor\0HOME=/h\0");
        visit(b"7f00-7f01 r-xp 0 08:01 5 /root/editor/12-1/dot-agent-deck-editor\n");
        Ok(())
    });
    assert_eq!(removed, Ok(vec!["13-1".to_string()]));
    assert_eq!(
        removable(&all, alive, |_| Err(ScanError::Unreadable)),
        Err(ScanError::Unreadable)
    );
    let live_only = candidates(&["10-1", "nope"]);
    assert_eq!(
        removable(&live_only, alive, |_| panic!("nothing to scan for")),
        Ok(vec![])
    );
    // A path that is another's prefix is named only conservatively: kept.
    let prefix = candidates(&["20-1", "20-12"]);
    assert_eq!(
        removable(
            &prefix,
            |_, _| false,
            |visit| {
                visit(b"/root/editor/20-12/x");
                Ok(())
            }
        ),
        Ok(vec![])
    );
    assert_eq!(parse_instance_name("12-34"), Some((12, 34)));
    for bad in [
        "12",
        "-34",
        "12-",
        "a-1",
        "1-b",
        "+1-2",
        "1-2-3",
        "99999999999-1",
    ] {
        assert_eq!(parse_instance_name(bad), None, "{bad}");
    }
}

/// Scenario: Sweep a real root with this host's processes: directories for this process (live), a gone daemon, a gone daemon whose path a running child's environment names, a name that isn't an instance's and a symlink to a gone daemon's directory. When every process of this user could be read and none showed an empty entry, only the unnamed gone daemon's directory is removed; when one couldn't be read, or a live one showed an empty entry, nothing is.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_sweep_keeps_what_a_process_names() {
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    let me = std::process::id();
    let my_start = dot_agent_deck::platform::proc::process_start_of(me).unwrap();
    let mut gone = Vec::new();
    for _ in 0..2 {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let start = dot_agent_deck::platform::proc::process_start_of(child.id()).unwrap();
        gone.push(instance_name(child.id(), start));
        child.kill().unwrap();
        child.wait().unwrap();
    }
    let live = instance_name(me, my_start);
    let target = dir.path().join("elsewhere");
    std::fs::create_dir(&target).unwrap();
    for name in [&live, &gone[0], &gone[1]] {
        let (pid, start) = parse_instance_name(name).unwrap();
        fake_instance(&root, pid, start);
    }
    std::fs::create_dir(root.join("not-an-instance")).unwrap();
    std::fs::write(root.join("not-an-instance").join("file"), b"x").unwrap();
    std::os::unix::fs::symlink(&target, root.join("999999-1")).unwrap();
    let named = root.join(&gone[1]);
    let mut user = Command::new("sleep")
        .arg("30")
        .env("VISUAL", named.join(HELPER_NAME))
        .spawn()
        .unwrap();
    let result = sweep(&root, "own-0");
    user.kill().unwrap();
    user.wait().unwrap();
    let left = |name: &str| root.join(name).exists();
    match result.scan_error {
        None if result.unknown == 0 => {
            assert_eq!(result.removed, [gone[0].clone()], "{result:?}");
            assert!(!left(&gone[0]));
            assert_eq!(result.kept, 3, "{result:?}");
        }
        None => {
            eprintln!(
                "SKIP-REMOVAL: a live process of this user showed an empty entry ({result:?})"
            );
            assert!(result.removed.is_empty());
            assert!(left(&gone[0]));
        }
        Some(error) => {
            eprintln!("SKIP-REMOVAL: a process of this user can't be read ({error:?})");
            assert!(result.removed.is_empty());
            assert!(left(&gone[0]));
        }
    }
    assert!(left(&live) && left(&gone[1]) && left("not-an-instance"));
    assert!(std::fs::symlink_metadata(root.join("999999-1")).is_ok());
    assert!(target.exists());
    assert_eq!(sweep(&dir.path().join("missing"), "x"), Default::default());
    assert!(
        !dir.path().join("missing").exists(),
        "a sweep makes no root"
    );
}

/// Scenario: Sweep a real root with liveness and the scan given: a gone daemon's instance with files in it that nothing names is removed whole, one the scan names is kept, as are a live daemon's, the caller's own, a numeric directory holding no daemon record and a name that isn't an instance's, and a symlink named like a gone daemon's isn't followed or counted; a failed scan removes nothing and says why.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_sweep_removes_whole_directories_and_follows_no_link() {
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    for n in 1..=4 {
        fake_instance(&root, n, u64::from(n));
    }
    for name in ["6-6", "junk"] {
        std::fs::create_dir_all(root.join(name).join("inv/7")).unwrap();
        std::fs::write(root.join(name).join("inv/7/marker"), b"x").unwrap();
    }
    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("keep"), b"x").unwrap();
    std::os::unix::fs::symlink(&target, root.join("5-5")).unwrap();
    let named = root.join("2-2").into_os_string().into_vec();
    let alive = |pid: u32, _| pid == 3;
    let failed = sweep_with(&root, "4-4", alive, |_| Err(ScanError::Unreadable));
    assert_eq!(failed.scan_error, Some(ScanError::Unreadable));
    assert_eq!((failed.removed.len(), failed.kept), (0, 5));
    let swept = sweep_with(&root, "4-4", alive, |visit| {
        visit(&named);
        Ok(())
    });
    assert_eq!(swept.removed, ["1-1"], "{swept:?}");
    assert_eq!((swept.kept, swept.failed, swept.scan_error), (4, 0, None));
    assert!(!root.join("1-1").exists());
    for kept in ["2-2", "3-3", "4-4", "6-6", "junk"] {
        assert!(root.join(kept).join("inv/7/marker").exists(), "{kept}");
    }
    assert!(target.join("keep").exists());
}

/// Scenario: Sweep a root of gone daemons' directories, every one unnamed by any process, where only one is laid out as its instance. The others are kept whole: one with no daemon record, one whose record names another daemon, one whose record is malformed, one whose directory others can read, one whose record is writable, one whose record is a symlink to a good record, and one whose record is a directory.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_sweep_removes_only_directories_it_identifies() {
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    fake_instance(&root, 1, 1);
    let unidentified = ["2-2", "3-3", "4-4", "5-5", "6-6", "7-7", "8-8"];
    for name in unidentified {
        std::fs::create_dir_all(root.join(name).join("inv/7")).unwrap();
        std::fs::write(root.join(name).join("inv/7/marker"), b"x").unwrap();
        std::fs::set_permissions(root.join(name), Permissions::from_mode(0o700)).unwrap();
    }
    let record = |name: &str, text: &str, mode: u32| {
        let path = root.join(name).join(DAEMON_FILE);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    };
    record("3-3", &daemon_record(9, 9), 0o400);
    record("4-4", "pid=4\nstart=4\n", 0o400);
    record("5-5", &daemon_record(5, 5), 0o400);
    std::fs::set_permissions(root.join("5-5"), Permissions::from_mode(0o750)).unwrap();
    record("6-6", &daemon_record(6, 6), 0o600);
    std::fs::write(dir.path().join("good"), daemon_record(7, 7)).unwrap();
    std::fs::set_permissions(dir.path().join("good"), Permissions::from_mode(0o400)).unwrap();
    std::os::unix::fs::symlink(dir.path().join("good"), root.join("7-7").join(DAEMON_FILE))
        .unwrap();
    std::fs::create_dir(root.join("8-8").join(DAEMON_FILE)).unwrap();
    let swept = sweep_with(&root, "own", |_, _| false, |_| Ok(()));
    assert_eq!(swept.removed, ["1-1"], "{swept:?}");
    assert_eq!((swept.kept, swept.failed), (unidentified.len(), 0));
    for name in unidentified {
        assert!(root.join(name).join("inv/7/marker").exists(), "{name}");
    }
}

/// Scenario: Make instance roots below a directory others can write, below a directory its group can write, through a symlink partway down, and through a `..`; each refuses, and nothing is made below the refused directory. A root below a directory this user can't write refuses with that failure. A root below a sticky directory others can write, which only its owners can rename entries in, is accepted.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_every_ancestor_of_the_root_is_trusted() {
    let dir = tempdir();
    let base = dir.path();
    let ancestor = |name: &str, mode: u32| {
        let path = base.join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
        private_dir(&path.join("victim"))
    };
    let open = ancestor("open", 0o777);
    let group = ancestor("group", 0o770);
    let sticky = ancestor("sticky", 0o1777);
    let real = private_dir(&base.join("real"));
    std::os::unix::fs::symlink(&real, base.join("hop")).unwrap();
    for (root, error) in [
        (open.join("state/editor"), SetupError::UntrustedDirectory),
        (group.join("state/editor"), SetupError::UntrustedDirectory),
        (
            base.join("hop/state/editor"),
            SetupError::UntrustedDirectory,
        ),
        (
            base.join("real/../dotdot/editor"),
            SetupError::PathCharacters,
        ),
    ] {
        assert_eq!(
            Instance::create(&root, daemon_id(4_000_000, 7), &mut source(base)).err(),
            Some(error),
            "{}",
            root.display()
        );
    }
    assert!(!open.join("state").exists() && !group.join("state").exists());
    assert!(!real.join("state").exists() && !base.join("dotdot").exists());
    // A trusted directory this user can't write: the root can't be made there,
    // and the failure is the one that stopped it.
    let closed = private_dir(&base.join("closed"));
    std::fs::set_permissions(&closed, Permissions::from_mode(0o500)).unwrap();
    let refused = Instance::create(
        &closed.join("state/editor"),
        daemon_id(4_000_000, 7),
        &mut source(base),
    )
    .err();
    std::fs::set_permissions(&closed, Permissions::from_mode(0o700)).unwrap();
    assert_eq!(refused, Some(SetupError::Io(Some(libc::EACCES))));
    let instance = Instance::create(
        &sticky.join("state/editor"),
        daemon_id(4_000_000, 7),
        &mut source(base),
    )
    .unwrap();
    assert!(instance.helper().starts_with(&sticky));
}

/// Scenario: Run a process of this user whose name holds a byte that isn't UTF-8, its environment naming a gone daemon's instance, and sweep with a scan of that process's real `/proc` entries: the instance is kept, because the process's owner is read from its status as bytes. Once the process exits, the same sweep removes the instance.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_process_whose_name_is_not_utf8_keeps_its_instance() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let start = dot_agent_deck::platform::proc::process_start_of(child.id()).unwrap();
    let gone = fake_instance(&root, child.id(), start);
    child.kill().unwrap();
    child.wait().unwrap();
    // The process's name is its executable's basename: a script, so the shell
    // running it keeps the name and starts no child that would also name it.
    let script = dir.path().join(std::ffi::OsStr::from_bytes(b"na\xffme"));
    std::fs::write(&script, "#!/bin/sh\nread line\n").unwrap();
    std::fs::set_permissions(&script, Permissions::from_mode(0o700)).unwrap();
    let mut user = Command::new(&script)
        .env("VISUAL", gone.join(HELPER_NAME))
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let status = format!("/proc/{}/status", user.id());
    assert!(
        (0..200).any(|_| {
            let named =
                std::fs::read(&status).is_ok_and(|s| s.windows(5).any(|w| w == b"na\xffme"));
            if !named {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            named
        }),
        "the process's status names it with the byte"
    );
    // The scan reads that process alone, so another process of this host's
    // that can't be read doesn't decide the outcome.
    let pid = user.id();
    let result = sweep_with(
        &root,
        "own-0",
        |_, _| false,
        |visit| scan_processes([pid], visit),
    );
    user.kill().unwrap();
    user.wait().unwrap();
    assert_eq!(result.scan_error, None, "{result:?}");
    assert!(result.removed.is_empty(), "{result:?}");
    assert!(gone.join(DAEMON_FILE).exists());
    // Gone, its instance named by nothing, it is removed.
    let swept = sweep_with(
        &root,
        "own-0",
        |_, _| false,
        |visit| scan_processes([pid], visit),
    );
    assert_eq!(swept.removed.len(), 1, "{swept:?}");
}

/// Scenario: Sweep a gone daemon's instance with a scripted `/proc` holding one process of this user. While it lives, an empty `environ`, or an empty `maps`, keeps the instance and is counted as unknown, with no scan error, and so does a zombie leader whose other thread still runs; a single-threaded zombie with both empty doesn't keep it, and a live process whose environment is there but doesn't name the instance doesn't either.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_an_empty_entry_of_a_live_process_keeps_every_instance() {
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    let proc = dir.path().join("proc");
    let uid = unsafe { libc::getuid() };
    // Process 7, this user's, in `state` with `threads`, with these entries.
    let process = |state: &str, threads: u32, environ: &[u8], maps: &[u8]| {
        let pid = proc.join("7");
        std::fs::create_dir_all(&pid).unwrap();
        let mut status = b"Name:\tna\xffme\n".to_vec();
        status.extend(
            format!("State:\t{state}\nThreads:\t{threads}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n")
                .bytes(),
        );
        std::fs::write(pid.join("status"), status).unwrap();
        std::fs::write(pid.join("environ"), environ).unwrap();
        std::fs::write(pid.join("maps"), maps).unwrap();
    };
    let swept = || {
        sweep_with(
            &root,
            "own-0",
            |_, _| false,
            |visit| scan_processes_in(&proc, [7], visit),
        )
    };
    let other = b"HOME=/h\0VISUAL=/elsewhere/dot-agent-deck-editor\0".as_slice();
    let maps = b"7f00-7f01 r-xp 0 08:01 5 /usr/bin/sh\n".as_slice();
    let gone = fake_instance(&root, 1, 1);
    for (environ, entry) in [(b"".as_slice(), maps), (other, b"".as_slice())] {
        process("R (running)", 1, environ, entry);
        let result = swept();
        assert_eq!(result.scan_error, None, "{result:?}");
        assert!(result.removed.is_empty(), "{result:?}");
        assert_eq!((result.kept, result.unknown), (1, 1), "{result:?}");
        assert!(gone.join(DAEMON_FILE).exists());
    }
    // A zombie leader whose other thread still runs: the process lives, and
    // its entries, empty, keep the instance.
    process("Z (zombie)", 2, b"", b"");
    let result = swept();
    assert_eq!(result.scan_error, None, "{result:?}");
    assert!(result.removed.is_empty(), "{result:?}");
    assert_eq!((result.kept, result.unknown), (1, 2), "{result:?}");
    // Exited, its one thread a zombie, its entries read empty and it keeps
    // nothing.
    process("Z (zombie)", 1, b"", b"");
    let result = swept();
    assert_eq!(result.removed, ["1-1"], "{result:?}");
    assert_eq!((result.kept, result.unknown), (0, 0), "{result:?}");
    // Live, its environment there and naming another path, it keeps nothing.
    let gone = fake_instance(&root, 1, 1);
    process("S (sleeping)", 1, other, maps);
    let result = swept();
    assert_eq!(result.removed, ["1-1"], "{result:?}");
    assert_eq!((result.kept, result.unknown), (0, 0), "{result:?}");
    assert!(!gone.exists());
    // The decision alone: an empty entry keeps every gone daemon's directory.
    let all = candidates(&["10-1", "11-1"]);
    let kept = removable(
        &all,
        |_, _| false,
        |visit| {
            visit(b"");
            Ok(())
        },
    );
    assert_eq!(kept, Ok(vec![]));
}

/// Scenario: Sweep a gone daemon's instance with one scripted process of this user that is live when first read, its `environ` or both entries empty, and change what its `status` reads the second time. Gone, a single-threaded zombie or dead by then, it keeps nothing; still live, a zombie leader with another thread, a zombie or dead task whose thread count reads 0, a state or thread count that can't be parsed, it keeps the instance and its empty entries are counted; a re-read that fails keeps it as a scan error, and so does a later process that can't be read, with no empty entry counted.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_the_status_re_read_decides_an_empty_entry() {
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    let uid = unsafe { libc::getuid() };
    let status =
        |rest: &str| format!("Name:\tsh\n{rest}Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n").into_bytes();
    let live = status("State:\tS (sleeping)\nThreads:\t1\n");
    let maps = b"7f00-7f01 r-xp 0 08:01 5 /usr/bin/sh\n".as_slice();
    // Process 7, live at its first `status` read, with these entries, and
    // `again` (a status, or an errno) at its second.
    let swept = |environ: &[u8], maps: &[u8], again: Result<Vec<u8>, i32>| {
        let mut reads = 0;
        sweep_with(
            &root,
            "own-0",
            |_, _| false,
            |visit| {
                scan_processes_with(
                    [7],
                    &mut |pid, file| {
                        assert_eq!(pid, 7);
                        match file {
                            "environ" => Ok(environ.to_vec()),
                            "maps" => Ok(maps.to_vec()),
                            "status" => {
                                reads += 1;
                                match reads {
                                    1 => Ok(live.clone()),
                                    2 => again.clone().map_err(std::io::Error::from_raw_os_error),
                                    _ => panic!("status read {reads} times"),
                                }
                            }
                            other => panic!("{other}"),
                        }
                    },
                    visit,
                )
            },
        )
    };
    // It exited after its entries were read: they say nothing, and the
    // instance goes.
    for again in [
        Err(libc::ENOENT),
        Err(libc::ESRCH),
        Ok(status("State:\tZ (zombie)\nThreads:\t1\n")),
        Ok(status("State:\tX (dead)\nThreads:\t1\n")),
    ] {
        let gone = fake_instance(&root, 1, 1);
        let result = swept(b"", maps, again.clone());
        assert_eq!(result.removed, ["1-1"], "{again:?} {result:?}");
        assert_eq!(
            (result.kept, result.unknown),
            (0, 0),
            "{again:?} {result:?}"
        );
        assert_eq!(result.scan_error, None, "{again:?} {result:?}");
        assert!(!gone.exists());
    }
    // It may still live: its empty entries keep the instance.
    let gone = fake_instance(&root, 1, 1);
    for again in [
        live.clone(),
        status("State:\tZ (zombie)\nThreads:\t2\n"),
        status("State:\tZ (zombie)\nThreads:\t0\n"),
        status("State:\tX (dead)\nThreads:\t0\n"),
        status("State:\tZ (zombie)\n"),
        status("State:\tZ (zombie)\nThreads:\tmany\n"),
        status("State:\t?\nThreads:\t1\n"),
        status("Threads:\t1\n"),
        b"".to_vec(),
    ] {
        let shown = String::from_utf8_lossy(&again).into_owned();
        let result = swept(b"", maps, Ok(again));
        assert_eq!(result.scan_error, None, "{shown} {result:?}");
        assert!(result.removed.is_empty(), "{shown} {result:?}");
        assert_eq!((result.kept, result.unknown), (1, 1), "{shown} {result:?}");
        assert!(gone.join(DAEMON_FILE).exists());
    }
    // Both entries empty: each one counts.
    let result = swept(b"", b"", Ok(live.clone()));
    assert!(result.removed.is_empty(), "{result:?}");
    assert_eq!((result.kept, result.unknown), (1, 2), "{result:?}");
    // The re-read fails: the scan can't be trusted, and keeps everything.
    let result = swept(b"", maps, Err(libc::EACCES));
    assert_eq!(result.scan_error, Some(ScanError::Unreadable), "{result:?}");
    assert!(result.removed.is_empty(), "{result:?}");
    assert_eq!((result.kept, result.unknown), (1, 0), "{result:?}");
    // A process that can't be read after one with an empty entry: the scan
    // fails, and the empty entry isn't counted.
    let result = sweep_with(
        &root,
        "own-0",
        |_, _| false,
        |visit| {
            scan_processes_with(
                [7, 8],
                &mut |pid, file| match (pid, file) {
                    (7, "status") => Ok(live.clone()),
                    (7, "environ") => Ok(Vec::new()),
                    (7, "maps") => Ok(maps.to_vec()),
                    _ => Err(std::io::Error::from_raw_os_error(libc::EACCES)),
                },
                visit,
            )
        },
    );
    assert_eq!(result.scan_error, Some(ScanError::Unreadable), "{result:?}");
    assert!(result.removed.is_empty(), "{result:?}");
    assert_eq!((result.kept, result.unknown), (1, 0), "{result:?}");
    assert!(gone.join(DAEMON_FILE).exists());
}

/// Scenario: Make an instance in a root already holding a directory that isn't an instance's, and read what the sweep at creation reports: it was kept, nothing was removed, and no scan was needed. Install a generation, find its arm page among this process's mappings, drop the installation, and find it no longer mapped while its file stays.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_an_instance_reports_its_sweep_and_a_generation_unmaps_its_page() {
    let dir = tempdir();
    let root = dir.path().join("state").join("editor");
    std::fs::create_dir_all(root.join("junk")).unwrap();
    std::fs::set_permissions(dir.path().join("state"), Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
    let instance =
        Instance::create(&root, daemon_id(4_000_001, 7), &mut source(dir.path())).unwrap();
    assert_eq!(
        instance.swept(),
        &Sweep {
            kept: 1,
            ..Default::default()
        }
    );
    let installation = install(&instance, CLAUDE, "1", &originals(None, None, None)).unwrap();
    let arm = instance.dir().join("arm-1");
    let mapped = || {
        std::fs::read_to_string("/proc/self/maps")
            .unwrap()
            .contains(arm.to_str().unwrap())
    };
    assert!(mapped());
    drop(installation);
    assert!(!mapped());
    assert!(arm.exists());
}

/// Scenario: Read identity files at the reader's size bound: a well-formed one exactly as long as the bound reads whole, and the same file one byte longer reads as no identity; a well-formed identity whose install root is a path several kilobytes long reads back.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_the_reader_reads_files_up_to_its_bound() {
    let dir = tempdir();
    let fake = dir.path().join("fake");
    std::fs::create_dir(&fake).unwrap();
    let reader = InstanceReader::open(&fake).unwrap();
    let with_root = |len: usize| Identity {
        install_root: PathBuf::from(format!("/{}", "r".repeat(len - 1))),
        ..identity(dir.path())
    };
    let long = with_root(4000);
    std::fs::write(fake.join("gen-1"), encode_identity(&long)).unwrap();
    assert_eq!(reader.identity("1"), Some(long));
    // Each root byte is two hex digits, so a pid one digit longer evens out
    // an odd gap.
    let base = encode_identity(&with_root(1)).len() as u64;
    let gap = READ_MAX - base;
    let mut exact = with_root(1 + (gap / 2) as usize);
    if gap % 2 == 1 {
        exact.pid *= 10;
    }
    let text = encode_identity(&exact);
    assert_eq!(text.len() as u64, READ_MAX);
    std::fs::write(fake.join("gen-2"), &text).unwrap();
    assert_eq!(reader.identity("2"), Some(exact));
    std::fs::write(fake.join("gen-3"), format!("{text}x")).unwrap();
    assert_eq!(reader.identity("3"), None);
}

/// Scenario: Sweep a root in which a gone daemon's directory holds a directory its owner can't write: the removal fails, the directory stays, and the sweep counts the failure rather than a removal. A gone daemon's directory nested forty levels deep is refused the same way and keeps its record, while one nested twenty deep is removed whole.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_removal_that_fails_is_counted() {
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    let locked = fake_instance(&root, 1, 1).join("inv");
    std::fs::write(locked.join("marker"), b"x").unwrap();
    std::fs::set_permissions(&locked, Permissions::from_mode(0o500)).unwrap();
    let swept = sweep_with(&root, "own", |_, _| false, |_| Ok(()));
    std::fs::set_permissions(&locked, Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        swept,
        Sweep {
            failed: 1,
            ..Default::default()
        }
    );
    assert!(locked.join("marker").exists());

    // A tree deeper than the removal goes is refused, and its record stays;
    // one within it is removed whole.
    let deep = fake_instance(&root, 2, 2);
    let shallow = fake_instance(&root, 3, 3);
    let nest = |top: &Path, levels: usize| {
        let mut path = top.join("inv");
        for _ in 0..levels {
            path.push("d");
        }
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("leaf"), b"x").unwrap();
    };
    nest(&deep, 40);
    nest(&shallow, 20);
    let swept = sweep_with(&root, "own", |pid, _| pid == 1, |_| Ok(()));
    assert_eq!((swept.removed, swept.failed), (vec!["3-3".to_string()], 1));
    assert!(deep.join(DAEMON_FILE).exists() && !shallow.exists());
}

/// Scenario: Identify a real instance through its root's descriptor and remove it; the same instance seen as another user's isn't identified. Identify one, swap another instance in under its name, and the removal refuses, leaving both whole. Open a root, move it aside and put another root at its path: the sweep through the descriptor removes the moved root's gone instance and leaves the one now at the path alone.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_identification_is_bound_to_the_owner_and_the_inode() {
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    let root_fd = open_root(&root).unwrap();
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    fake_instance(&root, 1, 1);
    assert!(identify(root_fd.as_fd(), "1-1", uid.wrapping_add(1)).is_none());
    assert!(identify(root_fd.as_fd(), "1-2", uid).is_none());
    let found = identify(root_fd.as_fd(), "1-1", uid).unwrap();
    assert_eq!((found.name(), found.daemon().pid), ("1-1", 1));
    remove_identified(root_fd.as_fd(), found).unwrap();
    assert!(!root.join("1-1").exists());

    fake_instance(&root, 2, 2);
    let found = identify(root_fd.as_fd(), "2-2", uid).unwrap();
    std::fs::rename(root.join("2-2"), dir.path().join("moved")).unwrap();
    let other = fake_instance(&root, 2, 2);
    assert!(remove_identified(root_fd.as_fd(), found).is_err());
    assert!(other.join("inv/7/marker").exists());
    assert!(dir.path().join("moved/inv/7/marker").exists());

    let held = private_dir(&dir.path().join("held"));
    let held_fd = open_root(&held).unwrap();
    fake_instance(&held, 3, 3);
    std::fs::rename(&held, dir.path().join("held-aside")).unwrap();
    private_dir(&held);
    let replacement = fake_instance(&held, 4, 4);
    let swept = sweep_root(held_fd.as_fd(), &held, "own", |_, _| false, |_| Ok(()));
    assert_eq!(swept.removed, ["3-3"], "{swept:?}");
    assert!(!dir.path().join("held-aside/3-3").exists());
    assert!(replacement.join(DAEMON_FILE).exists());
}

/// Scenario: Read a recorded daemon's liveness from `stat` reads: absent is gone, the recorded start is alive, another start is gone, and a read refused, a truncated line or a start that isn't a number is unknown, which retention keeps; a name holding spaces, parentheses or bytes that aren't UTF-8 doesn't move the start. This process reads as alive, a child that has exited as gone.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_start_that_cannot_be_read_counts_as_alive() {
    let stat = |comm: &[u8], start: &str| {
        let mut line = b"42 (".to_vec();
        line.extend_from_slice(comm);
        line.extend_from_slice(b") S 1 42 42 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 ");
        line.extend_from_slice(start.as_bytes());
        line.extend_from_slice(b" 1 2 3\n");
        Ok(line)
    };
    let err = |errno| Err(std::io::Error::from_raw_os_error(errno));
    assert_eq!(liveness_from(err(libc::ENOENT), 7), Liveness::Gone);
    assert_eq!(liveness_from(err(libc::ESRCH), 7), Liveness::Gone);
    assert_eq!(liveness_from(err(libc::EACCES), 7), Liveness::Unknown);
    assert_eq!(liveness_from(err(libc::EIO), 7), Liveness::Unknown);
    assert_eq!(liveness_from(stat(b"deck", "7"), 7), Liveness::Alive);
    assert_eq!(liveness_from(stat(b"deck", "8"), 7), Liveness::Gone);
    assert_eq!(liveness_from(stat(b"a) (b \xff", "7"), 7), Liveness::Alive);
    assert_eq!(liveness_from(stat(b"deck", "x7"), 7), Liveness::Unknown);
    assert_eq!(
        liveness_from(Ok(b"42 (deck) S 1 42".to_vec()), 7),
        Liveness::Unknown
    );
    assert_eq!(
        liveness_from(Ok(b"42 deck S".to_vec()), 7),
        Liveness::Unknown
    );
    let me = std::process::id();
    let start = dot_agent_deck::platform::proc::process_start_of(me).unwrap();
    assert_eq!(liveness(me, start), Liveness::Alive);
    assert_eq!(liveness(me, start + 1), Liveness::Gone);
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    assert_eq!(liveness(pid, 1), Liveness::Gone);
    // Retention's reading of the same facts.
    assert!(may_be_alive(me, start));
    assert!(!may_be_alive(me, start + 1));
    assert!(!may_be_alive(pid, 1));
}

/// Scenario: Read a process's owner from `status` bytes: a name that isn't UTF-8 still yields the real uid on the `Uid:` line, which is this user's or another's; a status with no `Uid:` line, or one that isn't a number, is an unreadable owner, which keeps every instance rather than skipping the process.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_an_owner_that_cannot_be_read_fails_the_scan() {
    let status = b"Name:\tna\xffme\nUmask:\t0002\nState:\tS (sleeping)\nUid:\t1000\t1001\t1000\t1000\nGid:\t5\n";
    assert_eq!(status_uid(status), Some(1000));
    assert_eq!(owned_by(status, 1000), Ok(true));
    assert_eq!(owned_by(status, 1001), Ok(false));
    for bad in [
        &b"Name:\tx\nGid:\t5\n"[..],
        b"Name:\tx\nUid:\tabc\t1\n",
        b"Name:\tx\nUid:\t\n",
        b"",
    ] {
        assert_eq!(status_uid(bad), None, "{bad:?}");
        assert_eq!(owned_by(bad, 1000), Err(ScanError::Unreadable), "{bad:?}");
    }
    assert_eq!(
        candidate_roots(Path::new("/s"), Some(Path::new("/r"))),
        [
            PathBuf::from("/s/editor"),
            PathBuf::from("/r/dot-agent-deck/editor")
        ]
    );
    assert_eq!(
        candidate_roots(Path::new("/a b"), Some(Path::new("/r"))),
        [PathBuf::from("/r/dot-agent-deck/editor")]
    );
}

/// Scenario: Write a gone daemon's record whose first bytes, up to the reader's bound, are a well-formed record with a long nonce, followed by more bytes: the directory is not identified, and a sweep keeps it. The same record cut to exactly the bound is identified. (Q35-13 review U3, both reviewers.)
#[spec("inbox/probe/065")]
#[test]
fn probe_065_an_oversized_daemon_record_is_refused_not_cut() {
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    let root_fd = open_root(&root).unwrap();
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    let record = |extra: &str| {
        let head = "pid=5\nstart=5\nboot=0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\nnonce=";
        let nonce = "a".repeat(READ_MAX as usize - head.len() - 1);
        format!("{head}{nonce}\n{extra}")
    };
    let instance = fake_instance(&root, 5, 5);
    let write = |text: &str| {
        let path = instance.join(DAEMON_FILE);
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o400)).unwrap();
    };
    assert_eq!(record("").len() as u64, READ_MAX);
    write(&record(""));
    assert!(identify(root_fd.as_fd(), "5-5", uid).is_some());
    write(&record("trailing"));
    assert!(identify(root_fd.as_fd(), "5-5", uid).is_none());
    let swept = sweep_with(&root, "own", |_, _| false, |_| Ok(()));
    assert!(swept.removed.is_empty(), "{swept:?}");
    assert!(instance.join(DAEMON_FILE).exists());
}

/// Scenario: Remove an identified instance from a root this user can traverse but not write: everything beneath it and its record go, the final `rmdir` fails, and what is left is an empty directory that is no longer identified, as the removal's documentation now says. A root path with a `.` component opens as the path without it, and one with `..` refuses. (Q35-13 review U3, both reviewers.)
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_final_rmdir_failure_leaves_an_unidentified_directory() {
    let dir = tempdir();
    let root = private_dir(&dir.path().join("editor"));
    let root_fd = open_root(&root).unwrap();
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    let instance = fake_instance(&root, 6, 6);
    let found = identify(root_fd.as_fd(), "6-6", uid).unwrap();
    std::fs::set_permissions(&root, Permissions::from_mode(0o500)).unwrap();
    let removed = remove_identified(root_fd.as_fd(), found);
    std::fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
    assert!(removed.is_err());
    assert!(instance.is_dir());
    assert_eq!(std::fs::read_dir(&instance).unwrap().count(), 0);
    assert!(identify(root_fd.as_fd(), "6-6", uid).is_none());

    let dotted = PathBuf::from(format!("{}/./editor", dir.path().display()));
    let same = open_root(&dotted)
        .unwrap()
        .as_fd()
        .try_clone_to_owned()
        .unwrap();
    let ids = |fd: &std::os::fd::OwnedFd| {
        let file = File::from(fd.try_clone().unwrap());
        let meta = file.metadata().unwrap();
        (meta.dev(), meta.ino())
    };
    assert_eq!(ids(&same), ids(&root_fd));
    let parent = PathBuf::from(format!("{}/editor/../editor", dir.path().display()));
    assert!(matches!(
        open_root(&parent),
        Err(SetupError::PathCharacters)
    ));
}
