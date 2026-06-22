//! Region-based structured diff between a regenerated baseline
//! `.dot-agent-deck.toml` and the user-improved one (PRD #116, M1.3).
//!
//! Both files are parsed with the deck's own `ProjectConfig` types so field
//! semantics (defaults for `watch`, `reactive_panes`, `clear`, …) match the
//! running binary exactly. Ordering is normalized (modes/orchestrations/roles
//! matched by name case-insensitively; panes by command; rules by pattern) so
//! the diff reflects content, not declaration order. Output is Markdown on
//! stdout. The repeated regions from decision #2 — `[[modes]]`,
//! `[[modes.panes]]`, `[[modes.rules]]`, `[[orchestrations]]`,
//! `[[orchestrations.roles]]` — each get their own heading. The per-mode
//! scalars (`init_command`, `reactive_panes`, `seed_prompt`) and per-role
//! scalars (including `prompt_template`, whose full text expands in a
//! `<details>` block when it differs) are compared as rows in a table under
//! the matched mode/role, not as separate sections.
//!
//! Usage:
//!   cargo run --quiet --example diff_config -- <baseline.toml> <improved.toml>

use std::fs;

use dot_agent_deck::project_config::{
    ModeConfig, OrchestrationConfig, OrchestrationRoleConfig, ProjectConfig,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let baseline_path = args
        .next()
        .expect("usage: diff_config <baseline> <improved>");
    let improved_path = args
        .next()
        .expect("usage: diff_config <baseline> <improved>");

    let baseline = load(&baseline_path);
    let improved = load(&improved_path);

    let mut out = String::new();
    out.push_str("# Structured config diff (PRD #116, M1.3)\n\n");
    out.push_str(&format!(
        "- **Baseline** (regenerated): `{baseline_path}`\n"
    ));
    out.push_str(&format!("- **Improved** (user): `{improved_path}`\n\n"));
    out.push_str(
        "Regions are compared per decision #2. \"B\" = regenerated baseline, \"U\" = \
         user-improved. Modes/orchestrations/roles are matched by name \
         (case-insensitive); panes by command; rules by pattern.\n\n",
    );

    diff_modes(&baseline.modes, &improved.modes, &mut out);
    diff_orchestrations(&baseline.orchestrations, &improved.orchestrations, &mut out);

    print!("{out}");
}

fn load(path: &str) -> ProjectConfig {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn opt(s: &Option<String>) -> String {
    match s {
        Some(v) => format!("`{}`", v.replace('\n', " ")),
        None => "_(none)_".to_string(),
    }
}

fn opt_interval(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "—".to_string(),
    }
}

/// Pop the first element of `pool` whose name matches `name` (case-insensitive),
/// returning its index-erased value. Used to greedily pair like-named items.
/// `get` is a higher-ranked `fn` pointer so the borrow it returns is tied to its
/// own argument, not to the pool's `'a`.
fn take_named<'a, T>(pool: &mut Vec<&'a T>, name: &str, get: fn(&T) -> &str) -> Option<&'a T> {
    let pos = pool
        .iter()
        .position(|x| get(x).eq_ignore_ascii_case(name))?;
    Some(pool.remove(pos))
}

fn diff_modes(b: &[ModeConfig], u: &[ModeConfig], out: &mut String) {
    out.push_str("## `[[modes]]`\n\n");
    out.push_str(&format!(
        "Mode count — B: **{}**, U: **{}**.\n\n",
        b.len(),
        u.len()
    ));

    let mut u_pool: Vec<&ModeConfig> = u.iter().collect();
    for bm in b {
        match take_named(&mut u_pool, &bm.name, |m| m.name.as_str()) {
            Some(um) => diff_mode_pair(bm, um, out),
            None => out.push_str(&format!(
                "### Mode `{}` — **B-only (user removed)**\n\n",
                bm.name
            )),
        }
    }
    for um in &u_pool {
        out.push_str(&format!(
            "### Mode `{}` — **U-only (user added)**: {} pane(s), {} rule(s), \
             reactive_panes={}\n\n",
            um.name,
            um.panes.len(),
            um.rules.len(),
            um.reactive_panes
        ));
    }
}

fn diff_mode_pair(b: &ModeConfig, u: &ModeConfig, out: &mut String) {
    out.push_str(&format!(
        "### Mode match: B `{}` ↔ U `{}`\n\n",
        b.name, u.name
    ));

    // Scalar per-mode regions.
    out.push_str("| Region | Baseline | User-improved | Same? |\n");
    out.push_str("|---|---|---|---|\n");
    let same_init = b.init_command == u.init_command;
    out.push_str(&format!(
        "| `init_command` | {} | {} | {} |\n",
        opt(&b.init_command),
        opt(&u.init_command),
        if same_init { "✓" } else { "✗" }
    ));
    out.push_str(&format!(
        "| `reactive_panes` | {} | {} | {} |\n",
        b.reactive_panes,
        u.reactive_panes,
        if b.reactive_panes == u.reactive_panes {
            "✓"
        } else {
            "✗"
        }
    ));
    out.push_str(&format!(
        "| `seed_prompt` | {} | {} | {} |\n\n",
        opt(&b.seed_prompt),
        opt(&u.seed_prompt),
        if b.seed_prompt == u.seed_prompt {
            "✓"
        } else {
            "✗"
        }
    ));

    // Persistent panes, matched by command.
    out.push_str("#### `[[modes.panes]]`\n\n");
    let mut u_panes: Vec<&_> = u.panes.iter().collect();
    for bp in &b.panes {
        let pos = u_panes.iter().position(|p| p.command == bp.command);
        match pos {
            Some(i) => {
                let up = u_panes.remove(i);
                out.push_str(&format!(
                    "- **both**: `{}` (B name={:?} watch={}; U name={:?} watch={})\n",
                    bp.command,
                    bp.name,
                    yn(bp.watch),
                    up.name,
                    yn(up.watch)
                ));
            }
            None => out.push_str(&format!(
                "- **B-only**: `{}` (name={:?}, watch={})\n",
                bp.command,
                bp.name,
                yn(bp.watch)
            )),
        }
    }
    for up in &u_panes {
        out.push_str(&format!(
            "- **U-only**: `{}` (name={:?}, watch={})\n",
            up.command,
            up.name,
            yn(up.watch)
        ));
    }
    out.push('\n');

    // Reactive rules, matched by pattern.
    out.push_str("#### `[[modes.rules]]`\n\n");
    let mut u_rules: Vec<&_> = u.rules.iter().collect();
    for br in &b.rules {
        let pos = u_rules.iter().position(|r| r.pattern == br.pattern);
        match pos {
            Some(i) => {
                let ur = u_rules.remove(i);
                // Same pattern still leaves two comparable fields — `watch` and
                // `interval`. A same-pattern/different-watch (or interval) delta
                // is a real divergence (e.g. user flipped a rule to re-run on a
                // timer): surface it instead of collapsing the pair to "both".
                if br.watch == ur.watch && br.interval == ur.interval {
                    out.push_str(&format!(
                        "- **both**: `{}` (watch={})\n",
                        br.pattern,
                        yn(br.watch)
                    ));
                } else {
                    out.push_str(&format!(
                        "- **both, differ ✗**: `{}` — B (watch={}, interval={}) vs \
                         U (watch={}, interval={})\n",
                        br.pattern,
                        yn(br.watch),
                        opt_interval(br.interval),
                        yn(ur.watch),
                        opt_interval(ur.interval)
                    ));
                }
            }
            None => out.push_str(&format!(
                "- **B-only**: `{}` (watch={})\n",
                br.pattern,
                yn(br.watch)
            )),
        }
    }
    for ur in &u_rules {
        out.push_str(&format!(
            "- **U-only**: `{}` (watch={})\n",
            ur.pattern,
            yn(ur.watch)
        ));
    }
    out.push('\n');
}

fn diff_orchestrations(b: &[OrchestrationConfig], u: &[OrchestrationConfig], out: &mut String) {
    out.push_str("## `[[orchestrations]]`\n\n");
    out.push_str(&format!(
        "Orchestration count — B: **{}**, U: **{}**.\n\n",
        b.len(),
        u.len()
    ));

    let mut u_pool: Vec<&OrchestrationConfig> = u.iter().collect();
    for bo in b {
        match take_named(&mut u_pool, &bo.name, |o| o.name.as_str()) {
            Some(uo) => diff_orch_pair(bo, uo, out),
            None => out.push_str(&format!(
                "### Orchestration `{}` — **B-only (user removed the whole orchestration)**: \
                 roles = {}\n\n",
                bo.name,
                role_names(&bo.roles)
            )),
        }
    }
    for uo in &u_pool {
        out.push_str(&format!(
            "### Orchestration `{}` — **U-only (user added)**: roles = {}\n\n",
            uo.name,
            role_names(&uo.roles)
        ));
    }
}

fn role_names(roles: &[OrchestrationRoleConfig]) -> String {
    roles
        .iter()
        .map(|r| r.name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn diff_orch_pair(b: &OrchestrationConfig, u: &OrchestrationConfig, out: &mut String) {
    out.push_str(&format!(
        "### Orchestration match: B `{}` ↔ U `{}`\n\n",
        b.name, u.name
    ));
    out.push_str("#### `[[orchestrations.roles]]`\n\n");

    let mut u_pool: Vec<&OrchestrationRoleConfig> = u.roles.iter().collect();
    for br in &b.roles {
        match take_named(&mut u_pool, &br.name, |r| r.name.as_str()) {
            Some(ur) => diff_role_pair(br, ur, out),
            None => out.push_str(&format!(
                "- **B-only role** `{}` (user dropped this role)\n\n",
                br.name
            )),
        }
    }
    for ur in &u_pool {
        out.push_str(&format!(
            "- **U-only role** `{}` (command=`{}`, clear={}, start={})\n\n",
            ur.name,
            ur.command,
            yn(ur.clear),
            yn(ur.start)
        ));
    }
}

fn diff_role_pair(b: &OrchestrationRoleConfig, u: &OrchestrationRoleConfig, out: &mut String) {
    out.push_str(&format!("##### Role `{}`\n\n", b.name));
    out.push_str("| Field | Baseline | User-improved | Same? |\n");
    out.push_str("|---|---|---|---|\n");
    let row = |field: &str, bv: String, uv: String, same: bool, out: &mut String| {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            field,
            bv,
            uv,
            if same { "✓" } else { "✗" }
        ));
    };
    row(
        "`command`",
        format!("`{}`", b.command),
        format!("`{}`", u.command),
        b.command == u.command,
        out,
    );
    row(
        "`start`",
        yn(b.start).to_string(),
        yn(u.start).to_string(),
        b.start == u.start,
        out,
    );
    row(
        "`clear`",
        yn(b.clear).to_string(),
        yn(u.clear).to_string(),
        b.clear == u.clear,
        out,
    );
    row(
        "`description`",
        opt(&b.description),
        opt(&u.description),
        b.description == u.description,
        out,
    );
    let bp_lines = b.prompt_template.as_deref().map(|s| s.lines().count());
    let up_lines = u.prompt_template.as_deref().map(|s| s.lines().count());
    row(
        "`prompt_template` (lines)",
        bp_lines
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into()),
        up_lines
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into()),
        b.prompt_template == u.prompt_template,
        out,
    );
    out.push('\n');
    if b.prompt_template != u.prompt_template {
        out.push_str("<details><summary>Baseline `prompt_template`</summary>\n\n```\n");
        out.push_str(b.prompt_template.as_deref().unwrap_or("(none)"));
        out.push_str("\n```\n\n</details>\n\n");
        out.push_str("<details><summary>User `prompt_template`</summary>\n\n```\n");
        out.push_str(u.prompt_template.as_deref().unwrap_or("(none)"));
        out.push_str("\n```\n\n</details>\n\n");
    }
}
