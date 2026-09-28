//! Region-based structured diff between a regenerated baseline
//! `.dot-agent-deck.toml` and the user-improved one (PRD #116, M1.3).
//!
//! Both files are parsed with the deck's own `ProjectConfig` types so field
//! semantics (defaults for `clear`, …) match the running binary exactly.
//! Ordering is normalized (orchestrations/roles matched by name
//! case-insensitively) so the diff reflects content, not declaration order.
//! Output is Markdown on stdout. The repeated regions from decision #2 —
//! `[[orchestrations]]`, `[[orchestrations.roles]]` — each get their own
//! heading. The per-role scalars (including `prompt_template`, whose full text
//! expands in a `<details>` block when it differs) are compared as rows in a
//! table under the matched role, not as separate sections.
//!
//! Issue #1199: a `[[modes]]` block is not diffed; it is ignored on parse.
//!
//! Usage:
//!   cargo run --quiet --example diff_config -- <baseline.toml> <improved.toml>

use std::fs;

use dot_agent_deck::project_config::{OrchestrationConfig, OrchestrationRoleConfig, ProjectConfig};

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
         user-improved. Orchestrations/roles are matched by name \
         (case-insensitive).\n\n",
    );

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
