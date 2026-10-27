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
    DAEMON_FILE, DaemonId, HELPER_NAME, INV_DIR, Instance, InstanceReader, Kind, PAGE_BYTES,
    READ_MAX, ScanError, Sweep, choose_root, decode_identity, encode_identity, generation_name_ok,
    instance_name, parse_instance_name, removable, same_contents, splittable, sweep, sweep_with,
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

/// A temporary directory whose path an agent can split, on disk.
fn tempdir() -> tempfile::TempDir {
    std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("installer")
        .rand_bytes(8)
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    assert!(splittable(dir.path()), "{}", dir.path().display());
    // Whatever the umask made it, so a case's parent is refused only when the
    // case means it to be.
    std::fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
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
    // The last root's parent, `/`, is another user's.
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
        Some("codex '-c' 'shell_environment_policy.set.VISUAL=\"e\"'")
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

/// Scenario: Sweep a real root with this host's processes: directories for this process (live), a gone daemon, a gone daemon whose path a running child's environment names, a name that isn't an instance's and a symlink to a gone daemon's directory. When every process of this user could be read, only the unnamed gone daemon's directory is removed; when one couldn't, nothing is.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_sweep_keeps_what_a_process_names() {
    let dir = tempdir();
    let root = dir.path().join("editor");
    std::fs::create_dir(&root).unwrap();
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
    for name in [&live, &gone[0], &gone[1], &"not-an-instance".to_string()] {
        std::fs::create_dir(root.join(name)).unwrap();
        std::fs::write(root.join(name).join("file"), b"x").unwrap();
    }
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
        None => {
            assert_eq!(result.removed, [gone[0].clone()], "{result:?}");
            assert!(!left(&gone[0]));
            assert_eq!(result.kept, 3, "{result:?}");
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
}

/// Scenario: Sweep a real root with liveness and the scan given: a gone daemon's directory with files in it that nothing names is removed whole, one the scan names is kept, as are a live daemon's, the caller's own and a name that isn't an instance's, and a symlink named like a gone daemon's isn't followed or counted; a failed scan removes nothing and says why.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_sweep_removes_whole_directories_and_follows_no_link() {
    let dir = tempdir();
    let root = dir.path().join("editor");
    for name in ["1-1", "2-2", "3-3", "4-4", "junk"] {
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
    assert_eq!((failed.removed.len(), failed.kept), (0, 4));
    let swept = sweep_with(&root, "4-4", alive, |visit| {
        visit(&named);
        Ok(())
    });
    assert_eq!(swept.removed, ["1-1"], "{swept:?}");
    assert_eq!((swept.kept, swept.failed, swept.scan_error), (3, 0, None));
    assert!(!root.join("1-1").exists());
    for kept in ["2-2", "3-3", "4-4", "junk"] {
        assert!(root.join(kept).join("inv/7/marker").exists(), "{kept}");
    }
    assert!(target.join("keep").exists());
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

/// Scenario: Sweep a root in which a gone daemon's directory holds a directory its owner can't write: the removal fails, the directory stays, and the sweep counts the failure rather than a removal.
#[spec("inbox/probe/065")]
#[test]
fn probe_065_a_removal_that_fails_is_counted() {
    let dir = tempdir();
    let root = dir.path().join("editor");
    let locked = root.join("1-1").join("inv");
    std::fs::create_dir_all(&locked).unwrap();
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
}
