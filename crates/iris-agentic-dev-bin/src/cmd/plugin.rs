use anyhow::Result;
use std::ffi::OsString;

/// List all iris-agentic-dev-* binaries discovered on PATH.
pub fn list_plugins() {
    let prefix = "iris-agentic-dev-";
    let paths = std::env::var("PATH").unwrap_or_default();
    let mut plugins = vec![];
    for dir in std::env::split_paths(&paths) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(cmd) = name.strip_prefix(prefix) {
                    plugins.push((cmd.to_string(), entry.path()));
                }
            }
        }
    }
    plugins.sort();
    plugins.dedup_by_key(|(name, _)| name.clone());
    if plugins.is_empty() {
        println!("No iris-agentic-dev-* plugins found on PATH.");
    } else {
        println!("Discovered plugins:");
        for (name, path) in plugins {
            println!("  {} → {}", name, path.display());
        }
    }
}

/// #86: how close a typo is to a built-in, in single-character edits — Damerau-Levenshtein
/// (optimal string alignment), so a TRANSPOSITION costs one, not two. `mpc` for `mcp` is
/// the transposition, and it is the typo clap's own "a similar subcommand exists" tip used
/// to catch. That tip became unreachable when `allow_external_subcommands` turned every
/// unknown token into an External instead of an error, so this restores it; the built-in
/// names come from the clap command itself, so the two cannot drift apart.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().map(|c| c.to_ascii_lowercase()).collect();
    let b: Vec<char> = b.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

/// The built-in whose name is within one or two edits of `cmd`, if any. Two edits only for
/// names long enough that two is still a typo rather than a different word.
pub(crate) fn nearest_builtin<'a>(cmd: &str, builtins: &[&'a str]) -> Option<&'a str> {
    builtins
        .iter()
        .map(|b| (edit_distance(cmd, b), *b))
        .filter(|(d, b)| *d <= if b.len() >= 5 { 2 } else { 1 })
        .min_by_key(|(d, b)| (*d, b.len()))
        .map(|(_, b)| b)
}

/// If a binary named iris-agentic-dev-{cmd} exists on PATH, exec it with the remaining args.
/// Never returns on Unix (process is replaced). Returns Ok on Windows after child exits.
///
/// `builtins` comes from the clap command definition (main.rs), never a literal here: the
/// error names the built-in subcommands, and a hardcoded list would go stale the first time
/// one is added or renamed, with nothing to catch it.
pub fn try_dispatch_plugin(cmd: &str, args: &[OsString], builtins: &[&str]) -> Result<()> {
    let binary = format!("iris-agentic-dev-{}", cmd);
    match which::which(&binary) {
        Ok(path) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let err = std::process::Command::new(&path).args(args).exec();
                anyhow::bail!("failed to exec {}: {}", path.display(), err);
            }
            #[cfg(not(unix))]
            {
                let status = std::process::Command::new(&path).args(args).status()?;
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        Err(_) => {
            // #86: with allow_external_subcommands on, this replaces clap's
            // "unrecognized subcommand" for every typo — so it has to be at least as
            // informative as what it replaced, clap's near-miss tip included. Exit 1 (clap
            // used 2); both are non-zero, and nothing in the repo asserts on 2 for this path.
            let suggestion = match nearest_builtin(cmd, builtins) {
                Some(b) => format!("\n  tip: a similar subcommand exists: '{b}'"),
                None => String::new(),
            };
            eprintln!(
                "iris-interop-dev: unknown command '{}'\n  \
                 not a built-in subcommand ({}), and no plugin\n  \
                 binary '{}' was found on PATH.{}\n\
                 Run `iris-interop-dev --help` for built-in commands, or\n\
                 `iris-interop-dev --list-plugins` to see discovered plugins.",
                cmd,
                builtins.join(", "),
                binary,
                suggestion
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::nearest_builtin;

    const BUILTINS: &[&str] = &["mcp", "compile", "init", "install"];

    /// #86: `allow_external_subcommands` swallowed clap's "a similar subcommand exists"
    /// tip. These are the typos it used to catch.
    #[test]
    fn a_typo_points_at_the_builtin_it_meant() {
        for (typo, want) in [
            ("mpc", "mcp"),
            ("Mcp", "mcp"),
            ("compil", "compile"),
            ("comple", "compile"),
            ("ini", "init"),
            ("instal", "install"),
        ] {
            assert_eq!(nearest_builtin(typo, BUILTINS), Some(want), "{typo}");
        }
    }

    /// A real plugin name must not be "corrected" into a built-in — the message is printed
    /// for `foo` just as often as for `mpc`, and a wrong tip is worse than none.
    #[test]
    fn an_unrelated_command_gets_no_suggestion() {
        for name in ["hello", "totally-unknown-xyzzy", "skills", "doctor", ""] {
            assert_eq!(nearest_builtin(name, BUILTINS), None, "{name}");
        }
    }
}
