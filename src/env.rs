//! Rendering of resolved environment variables for shell `eval`.
//!
//! `eph env` resolves the variables declared in a `.eph` file and prints them
//! in a shell-specific format. This module owns the pure rendering half of that
//! command: escaping values for a double-quoted context and formatting the
//! `export` / fish / json output. The workspace lookup and interpolation that
//! produce the resolved pairs live in the binary.

use anyhow::{Result, bail};

/// Render resolved environment variables in the requested shell format.
///
/// `format` is one of `export` (bash/sh/zsh, see [`render_export`]), `fish`
/// (see [`render_fish`]), `powershell` (see [`render_powershell`]), or `json`
/// (see [`render_json`]). The `export`, `fish`, and `powershell` variants
/// delegate directly; `json` appends a trailing newline so the output occupies
/// a clean terminal line.
///
/// # Errors
///
/// Returns an error if `format` is none of `export`, `fish`, `powershell`, or
/// `json`, or if JSON serialization fails (see [`render_json`]).
///
/// # Examples
///
/// ```
/// # fn main() -> anyhow::Result<()> {
/// let vars = vec![("PORT".to_string(), "6379".to_string())];
/// assert_eq!(eph::render(&vars, "export")?, "export PORT=\"6379\"\n");
/// assert_eq!(eph::render(&vars, "powershell")?, "$env:PORT = '6379'\n");
/// # Ok(())
/// # }
/// ```
pub fn render(env_vars: &[(String, String)], format: &str) -> Result<String> {
    render_with_unsets(env_vars, &[], format)
}

/// Render resolved assignments and clear variables that could not be resolved.
///
/// Shell formats have an explicit unset operation, preventing a previous
/// workspace's value from surviving an `eval`. They end with a failing command
/// so command substitution cannot hide eph's own failure status. JSON has no
/// environment mutation semantics, so unresolved names are absent from the
/// object; the caller still returns a failure status.
pub fn render_with_unsets(
    env_vars: &[(String, String)],
    unset_names: &[String],
    format: &str,
) -> Result<String> {
    match format {
        "export" => Ok(format!(
            "{}{}{}",
            render_export(env_vars),
            unset_names
                .iter()
                .map(|name| format!("unset {name}\n"))
                .collect::<String>(),
            if unset_names.is_empty() {
                ""
            } else {
                "false\n"
            }
        )),
        "fish" => Ok(format!(
            "{}{}{}",
            render_fish(env_vars),
            unset_names
                .iter()
                .map(|name| format!("set -e {name}\n"))
                .collect::<String>(),
            if unset_names.is_empty() {
                ""
            } else {
                "false\n"
            }
        )),
        "powershell" => Ok(format!(
            "{}{}{}",
            render_powershell(env_vars),
            unset_names
                .iter()
                .map(|name| format!("Remove-Item Env:{name} -ErrorAction SilentlyContinue\n"))
                .collect::<String>(),
            if unset_names.is_empty() {
                ""
            } else {
                "throw 'eph env: unresolved variables'\n"
            }
        )),
        // The `export`/`fish`/`powershell` variants already terminate each
        // line; the JSON object does not, so add a trailing newline for a
        // clean terminal line.
        "json" => Ok(format!("{}\n", render_json(env_vars)?)),
        _ => bail!(
            "unknown format: {}, use: export, fish, powershell, json",
            format
        ),
    }
}

/// Render `export NAME="value"` lines for bash/sh/zsh.
pub fn render_export(env_vars: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in env_vars {
        out.push_str(&format!("export {}=\"{}\"\n", name, escape_bash(value)));
    }
    out
}

/// Render `set -gx NAME "value"` lines for fish.
pub fn render_fish(env_vars: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in env_vars {
        out.push_str(&format!("set -gx {} \"{}\"\n", name, escape_fish(value)));
    }
    out
}

/// Render `$env:NAME = 'value'` lines for PowerShell.
///
/// Piping this through `Out-String | Invoke-Expression` (or dot-sourcing it)
/// sets each variable in the caller's session, mirroring what `export` does
/// for bash/sh/zsh and `set -gx` does for fish.
pub fn render_powershell(env_vars: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in env_vars {
        out.push_str(&format!("$env:{} = '{}'\n", name, escape_powershell(value)));
    }
    out
}

/// Render the variables as a pretty-printed JSON object (no trailing newline).
///
/// Keys appear in `env_vars`' input order (the `.eph` file's declaration
/// order), not an arbitrary hash order: `serde_json::Map` is insertion-ordered
/// with the `preserve_order` feature this crate enables.
///
/// # Errors
///
/// Returns an error if the variables cannot be serialized to JSON. In practice
/// this does not happen for a map of strings, but the fallible signature mirrors
/// [`serde_json::to_string_pretty`].
pub fn render_json(env_vars: &[(String, String)]) -> Result<String> {
    let mut map = serde_json::Map::with_capacity(env_vars.len());
    for (name, value) in env_vars {
        map.insert(name.clone(), serde_json::Value::String(value.clone()));
    }
    Ok(serde_json::to_string_pretty(&map)?)
}

/// Escape a value for a bash/sh/zsh double-quoted string.
pub fn escape_bash(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// Escape a value for a fish double-quoted string.
pub fn escape_fish(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
}

/// Escape a value for a PowerShell single-quoted string.
///
/// A single-quoted PowerShell string is the literal-string form: `$`,
/// backticks, and double quotes all pass through unchanged (nothing is
/// interpolated), so the single quote that delimits the string is the only
/// character that needs escaping, doubled per PowerShell's own quoting rule.
pub fn escape_powershell(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests lock in the escaping behavior for the double-quoted output
    // context used by `eph env` (`export NAME="<escaped>"` / `set -gx NAME
    // "<escaped>"`). Inside double quotes, the shell only treats \ " $ ` (and
    // for fish, \ " $) specially, so those are the only characters escaped.
    // Literal newlines are preserved as-is: they are valid inside double quotes
    // for `eval`. The tests are deliberately shell-free; they assert the exact
    // produced strings rather than invoking a shell.
    //
    // Backslash must be escaped first so that backslashes introduced while
    // escaping the other characters are not themselves doubled.

    #[test]
    fn test_escape_bash_special_chars() {
        // Double quote -> \"
        assert_eq!(escape_bash("\""), "\\\"");
        // Dollar sign -> \$
        assert_eq!(escape_bash("$"), "\\$");
        // Backtick -> \`
        assert_eq!(escape_bash("`"), "\\`");
        // Backslash -> \\
        assert_eq!(escape_bash("\\"), "\\\\");
        // Newline is preserved unchanged
        assert_eq!(escape_bash("\n"), "\n");
    }

    #[test]
    fn test_escape_bash_combined() {
        // a"b$c`d\e<newline>f
        let input = "a\"b$c`d\\e\nf";
        // a \" b \$ c \` d \\ e <newline> f
        assert_eq!(escape_bash(input), "a\\\"b\\$c\\`d\\\\e\nf");
        // A literal backslash followed by a dollar must produce \\ then \$,
        // not a single escaped sequence.
        assert_eq!(escape_bash("\\$"), "\\\\\\$");
    }

    #[test]
    fn test_escape_fish_special_chars() {
        // Double quote -> \"
        assert_eq!(escape_fish("\""), "\\\"");
        // Dollar sign -> \$
        assert_eq!(escape_fish("$"), "\\$");
        // Backslash -> \\
        assert_eq!(escape_fish("\\"), "\\\\");
        // Newline is preserved unchanged
        assert_eq!(escape_fish("\n"), "\n");
        // fish does not treat backticks specially inside double quotes, so a
        // backtick is passed through untouched.
        assert_eq!(escape_fish("`"), "`");
    }

    #[test]
    fn test_escape_fish_combined() {
        let input = "a\"b$c`d\\e\nf";
        // Backtick stays literal for fish.
        assert_eq!(escape_fish(input), "a\\\"b\\$c`d\\\\e\nf");
        assert_eq!(escape_fish("\\$"), "\\\\\\$");
    }

    #[test]
    fn test_render_export_format() {
        let vars = vec![
            ("APP".to_string(), "myapp".to_string()),
            ("URL".to_string(), "a$b".to_string()),
        ];
        assert_eq!(
            render_export(&vars),
            "export APP=\"myapp\"\nexport URL=\"a\\$b\"\n"
        );
    }

    #[test]
    fn test_render_fish_format() {
        let vars = vec![("APP".to_string(), "myapp".to_string())];
        assert_eq!(render_fish(&vars), "set -gx APP \"myapp\"\n");
    }

    #[test]
    fn test_render_json_format() {
        let vars = vec![("APP".to_string(), "myapp".to_string())];
        let json = render_json(&vars).unwrap();
        assert_eq!(json, "{\n  \"APP\": \"myapp\"\n}");
    }

    #[test]
    fn test_render_json_preserves_declaration_order() {
        // A HashMap-backed implementation would scramble this; the point of
        // `preserve_order` is that it cannot.
        let vars = vec![
            ("ZEBRA".to_string(), "z".to_string()),
            ("APPLE".to_string(), "a".to_string()),
            ("MANGO".to_string(), "m".to_string()),
        ];
        let json = render_json(&vars).unwrap();
        let zebra = json.find("ZEBRA").expect("ZEBRA missing");
        let apple = json.find("APPLE").expect("APPLE missing");
        let mango = json.find("MANGO").expect("MANGO missing");
        assert!(
            zebra < apple && apple < mango,
            "keys should preserve input order, got:\n{json}"
        );
    }

    #[test]
    fn test_escape_powershell_doubles_single_quotes() {
        assert_eq!(escape_powershell("'"), "''");
        assert_eq!(escape_powershell("it's"), "it''s");
        // Nothing else is special in a single-quoted PowerShell string.
        assert_eq!(escape_powershell("$env:PATH"), "$env:PATH");
        assert_eq!(escape_powershell("`backtick`"), "`backtick`");
        assert_eq!(escape_powershell("\n"), "\n");
        assert_eq!(escape_powershell("\"quoted\""), "\"quoted\"");
    }

    #[test]
    fn test_render_powershell_format() {
        let vars = vec![
            ("APP".to_string(), "myapp".to_string()),
            ("MSG".to_string(), "it's here".to_string()),
        ];
        assert_eq!(
            render_powershell(&vars),
            "$env:APP = 'myapp'\n$env:MSG = 'it''s here'\n"
        );
    }

    #[test]
    fn test_render_unknown_format_errors() {
        let vars = vec![("APP".to_string(), "myapp".to_string())];
        let err = render(&vars, "yaml").unwrap_err().to_string();
        assert!(err.contains("export, fish, powershell, json"), "got: {err}");
    }

    #[test]
    fn unresolved_names_render_as_shell_unsets_and_stay_out_of_json() {
        let vars = vec![("READY".to_string(), "yes".to_string())];
        let unset = vec!["DATABASE_URL".to_string()];

        assert_eq!(
            render_with_unsets(&vars, &unset, "export").unwrap(),
            "export READY=\"yes\"\nunset DATABASE_URL\nfalse\n"
        );
        assert_eq!(
            render_with_unsets(&vars, &unset, "fish").unwrap(),
            "set -gx READY \"yes\"\nset -e DATABASE_URL\nfalse\n"
        );
        assert_eq!(
            render_with_unsets(&vars, &unset, "powershell").unwrap(),
            "$env:READY = 'yes'\nRemove-Item Env:DATABASE_URL -ErrorAction SilentlyContinue\nthrow 'eph env: unresolved variables'\n"
        );
        assert_eq!(
            render_with_unsets(&vars, &unset, "json").unwrap(),
            "{\n  \"READY\": \"yes\"\n}\n"
        );
    }
}
