//! workflows — user-saved parameterized command templates.
//!
//! A workflow is a small TOML file in `~/.config/jterm4/workflows/`
//! that names a reusable command with `{placeholder}` slots. The
//! Ctrl+Shift+W palette lists them; selecting one opens a dialog
//! asking for each placeholder's value, then writes the substituted
//! command into the live PTY (no auto-Enter — the user reviews and
//! presses Return).
//!
//! Format (one workflow per file):
//!
//! ```toml
//! name = "Deploy to staging"
//! description = "Push the current branch and trigger the staging deploy"
//! command = "git push origin {branch} && ssh staging 'deploy {branch} --env={env}'"
//!
//! [[args]]
//! name = "branch"
//! description = "Branch to deploy"
//! default = "main"
//!
//! [[args]]
//! name = "env"
//! description = "Target environment"
//! default = "staging"
//! ```
//!
//! Placeholder syntax is the simplest thing that survives shell quoting
//! and is unambiguous: `{name}`. We do NOT support `${name}` because
//! that collides with shell variable expansion (a perfectly valid
//! workflow template containing `${HOME}` would silently get mangled).

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workflow {
    pub name: String,
    pub description: String,
    pub command: String,
    pub args: Vec<WorkflowArg>,
    /// Absolute path the workflow was loaded from. Used so the palette
    /// can offer "open file" / "reveal in folder" actions later.
    pub source_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowArg {
    pub name: String,
    pub description: String,
    pub default: String,
}

/// `~/.config/jterm4/workflows/`. Created lazily on first save; we never
/// `mkdir -p` on read — a missing dir just means "no workflows yet".
pub fn workflows_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        });
    base.join("jterm4").join("workflows")
}

/// Read all `*.toml` files from `dir` and parse each as a Workflow.
/// Files that fail to parse are silently skipped — a malformed template
/// shouldn't kill the palette for every other one. Returns workflows
/// sorted by name for stable palette order.
pub fn load_all_from(dir: &Path) -> Vec<Workflow> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else { continue };
        if let Some(wf) = parse_workflow(&contents, &path) {
            out.push(wf);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Convenience: load all workflows from the default directory.
pub fn load_all() -> Vec<Workflow> {
    load_all_from(&workflows_dir())
}

fn parse_workflow(toml_src: &str, source_path: &Path) -> Option<Workflow> {
    let table: toml::Table = toml::from_str(toml_src).ok()?;
    let name = table.get("name")?.as_str()?.to_string();
    let command = table.get("command")?.as_str()?.to_string();
    let description = table
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut args = Vec::new();
    if let Some(raw_args) = table.get("args").and_then(|v| v.as_array()) {
        for entry in raw_args {
            let t = match entry.as_table() {
                Some(t) => t,
                None => continue,
            };
            let Some(name) = t.get("name").and_then(|v| v.as_str()) else { continue };
            let description = t
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let default = t
                .get("default")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            args.push(WorkflowArg {
                name: name.to_string(),
                description,
                default,
            });
        }
    }

    Some(Workflow {
        name,
        description,
        command,
        args,
        source_path: source_path.to_path_buf(),
    })
}

/// Substitute `{name}` placeholders in `template` with values from
/// `bindings`. Unknown placeholders are left as-is (so the user sees
/// them in the rendered command and can fix the typo). Escape `{{` and
/// `}}` for literal braces, mirroring `format!` semantics — workflows
/// occasionally need to emit JSON or shell brace expansions.
pub fn substitute(template: &str, bindings: &[(String, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'{' {
            // Escaped literal `{{`
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                out.push('{');
                i += 2;
                continue;
            }
            // Find the closing `}`.
            if let Some(close_rel) = bytes[i + 1..].iter().position(|&c| c == b'}') {
                let close = i + 1 + close_rel;
                let name = &template[i + 1..close];
                if let Some((_, v)) = bindings.iter().find(|(n, _)| n == name) {
                    out.push_str(v);
                } else {
                    // Unknown placeholder — keep verbatim so the user notices.
                    out.push_str(&template[i..=close]);
                }
                i = close + 1;
                continue;
            }
            // Unterminated `{` — emit literally and move on.
            out.push('{');
            i += 1;
        } else if b == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
            out.push('}');
            i += 2;
        } else {
            // Multi-byte UTF-8: push the whole codepoint by reading char_indices.
            let rest = &template[i..];
            if let Some(c) = rest.chars().next() {
                out.push(c);
                i += c.len_utf8();
            } else {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_minimal_workflow() {
        let src = r#"
            name = "Echo"
            command = "echo hi"
        "#;
        let wf = parse_workflow(src, Path::new("/tmp/x.toml")).unwrap();
        assert_eq!(wf.name, "Echo");
        assert_eq!(wf.command, "echo hi");
        assert_eq!(wf.description, "");
        assert!(wf.args.is_empty());
    }

    #[test]
    fn parse_workflow_with_args() {
        let src = r#"
            name = "Greet"
            description = "Say hello to someone"
            command = "echo hello {name}"

            [[args]]
            name = "name"
            description = "Who to greet"
            default = "world"
        "#;
        let wf = parse_workflow(src, Path::new("/tmp/x.toml")).unwrap();
        assert_eq!(wf.args.len(), 1);
        assert_eq!(wf.args[0].name, "name");
        assert_eq!(wf.args[0].default, "world");
    }

    #[test]
    fn parse_missing_required_fields_returns_none() {
        let src = r#"description = "no name or command""#;
        assert!(parse_workflow(src, Path::new("/tmp/x.toml")).is_none());
    }

    #[test]
    fn substitute_replaces_named_placeholders() {
        let out = substitute(
            "deploy {env} {target}",
            &[("env".into(), "prod".into()), ("target".into(), "api".into())],
        );
        assert_eq!(out, "deploy prod api");
    }

    #[test]
    fn substitute_leaves_unknown_placeholders_intact() {
        let out = substitute(
            "hi {name}, your role is {role}",
            &[("name".into(), "Bea".into())],
        );
        // {role} unresolved — keep it visible so the user sees the typo.
        assert_eq!(out, "hi Bea, your role is {role}");
    }

    #[test]
    fn substitute_double_brace_escape() {
        let out = substitute(
            "shell brace expansion: {{a,b,c}}",
            &[],
        );
        assert_eq!(out, "shell brace expansion: {a,b,c}");
    }

    #[test]
    fn substitute_no_braces_passthrough() {
        let s = "git status --porcelain";
        assert_eq!(substitute(s, &[]), s);
    }

    #[test]
    fn substitute_handles_utf8_around_braces() {
        let out = substitute(
            "🚀 deploy {env} 完了",
            &[("env".into(), "prod".into())],
        );
        assert_eq!(out, "🚀 deploy prod 完了");
    }

    #[test]
    fn load_all_from_skips_non_toml_and_malformed() {
        let dir = tempdir();
        // good
        write_file(&dir.path().join("a.toml"), "name = \"A\"\ncommand = \"echo a\"\n");
        // good
        write_file(&dir.path().join("b.toml"), "name = \"B\"\ncommand = \"echo b\"\n");
        // wrong extension
        write_file(&dir.path().join("c.txt"), "not a workflow");
        // malformed toml
        write_file(&dir.path().join("d.toml"), "this is = not valid =");
        // missing required field
        write_file(&dir.path().join("e.toml"), "description = \"oops\"");

        let wfs = load_all_from(dir.path());
        assert_eq!(wfs.len(), 2);
        assert_eq!(wfs[0].name, "A");
        assert_eq!(wfs[1].name, "B");
    }

    #[test]
    fn load_all_from_missing_dir_returns_empty() {
        let wfs = load_all_from(Path::new("/nonexistent/jterm4/workflows/never"));
        assert!(wfs.is_empty());
    }

    // ----- test helpers (no external `tempfile` dep) -----

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn path(&self) -> &Path { &self.0 }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }
    fn tempdir() -> TmpDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("jterm4-wf-test-{nanos}"));
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn write_file(p: &Path, contents: &str) {
        let mut f = fs::File::create(p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }
}
