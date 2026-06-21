//! Rendering of resolved environment variables for shell `eval`.
//!
//! `eph env` resolves the variables declared in a `.eph` file and prints them
//! in a shell-specific format. This module owns the pure rendering half of that
//! command: escaping values for a double-quoted context and formatting the
//! `export` / fish / json output. The workspace lookup and interpolation that
//! produce the resolved pairs live in the binary.

use anyhow::{Result, bail};
use std::collections::HashMap;

/// Render resolved environment variables in the requested shell format.
///
/// `format` is one of `export` (bash/sh/zsh, see [`render_export`]), `fish`
/// (see [`render_fish`]), or `json` (see [`render_json`]). The `export` and
/// `fish` variants delegate directly; `json` appends a trailing newline so the
/// output occupies a clean terminal line.
///
/// # Errors
///
/// Returns an error if `format` is none of `export`, `fish`, or `json`, or if
/// JSON serialization fails (see [`render_json`]).
///
/// # Examples
///
/// ```
/// # fn main() -> anyhow::Result<()> {
/// let vars = vec![("PORT".to_string(), "6379".to_string())];
/// assert_eq!(eph::render(&vars, "export")?, "export PORT=\"6379\"\n");
/// assert!(eph::render(&vars, "powershell").is_err());
/// # Ok(())
/// # }
/// ```
pub fn render(env_vars: &[(String, String)], format: &str) -> Result<String> {
    match format {
        "export" => Ok(render_export(env_vars)),
        "fish" => Ok(render_fish(env_vars)),
        // The `export`/`fish` variants already terminate each line; the JSON
        // object does not, so add a trailing newline for a clean terminal line.
        "json" => Ok(format!("{}\n", render_json(env_vars)?)),
        _ => bail!("unknown format: {}, use: export, fish, json", format),
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

/// Render the variables as a pretty-printed JSON object (no trailing newline).
///
/// # Errors
///
/// Returns an error if the variables cannot be serialized to JSON. In practice
/// this does not happen for a map of strings, but the fallible signature mirrors
/// [`serde_json::to_string_pretty`].
pub fn render_json(env_vars: &[(String, String)]) -> Result<String> {
    let map: HashMap<&str, &str> = env_vars
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
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
    fn test_render_unknown_format_errors() {
        let vars = vec![("APP".to_string(), "myapp".to_string())];
        assert!(render(&vars, "powershell").is_err());
    }
}
