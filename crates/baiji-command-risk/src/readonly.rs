//! Read-only command detection: which `bash` calls may skip the HITL prompt.
//!
//! The inverse of [`crate::assess`]. Where `assess` biases toward recall
//! (flag anything that *might* destroy data), this biases toward precision:
//! a command is auto-approvable only when every segment is a known inspection
//! program, every path it names stays inside the allowed roots, and nothing in
//! the command line can write, background, or run code we did not see.
//!
//! Anything unrecognized returns `false`, which just means "ask the user as
//! before" — a false negative costs one keypress, a false positive could leak
//! `~/.ssh` to the model or run an unseen program.
//!
//! Caveat: `git` reads repository config, and a hostile repository's
//! `.git/config` can point `core.fsmonitor` / `core.pager` / diff textconv at a
//! program. That risk is the same as a shell prompt that runs `git status`;
//! users who work in untrusted repositories should disable auto-approval.
//! (`cargo metadata`/`tree` are deliberately absent for the same reason: a
//! repository `.cargo/config.toml` can set `build.rustc-wrapper`.)

use crate::tokenize::{self, Token};
use crate::{RiskContext, RiskLevel, assess};
use std::path::{Path, PathBuf};

/// Inputs for the read-only check.
#[derive(Debug, Clone, Default)]
pub struct ReadOnlyPolicy {
    /// Directories a path argument may point into (the bash working directory
    /// plus `policy.allowed_paths`). Empty = nothing is auto-approvable.
    pub roots: Vec<PathBuf>,
    /// The user's home directory, for `~` expansion.
    pub home_dir: Option<PathBuf>,
    /// User-trusted command prefixes (`"cargo test"`, `"npm run lint"`), as
    /// whitespace-split words. A segment whose leading words equal a prefix is
    /// approved regardless of its arguments; the global rules (no substitution,
    /// no write redirect, no background `&`) still apply.
    pub trusted_prefixes: Vec<Vec<String>>,
}

impl ReadOnlyPolicy {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            home_dir: std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from),
            trusted_prefixes: Vec::new(),
        }
    }

    pub fn with_trusted_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.trusted_prefixes = prefixes
            .into_iter()
            .map(|p| {
                p.as_ref()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|words| !words.is_empty())
            .collect();
        self
    }
}

/// Programs that only read and print. Their non-flag arguments are paths or
/// patterns, all of which must resolve inside the allowed roots.
///
/// Deliberately absent: `jq` (`jq -n env` prints every secret in the
/// environment), `xxd` (`xxd in out` / `-r` write files), `less` (`LESSOPEN`
/// preprocessors, `!cmd`), `env`/`printenv` (secrets), `tee`, `cargo`.
const READ_ONLY_PROGRAMS: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "pwd", "which", "whoami", "stat", "file", "du", "df",
    "tree", "echo", "basename", "dirname", "realpath", "readlink", "grep", "egrep", "fgrep", "rg",
    "diff", "cut", "nl", "date", "uname", "true", "false", "test", "[", "md5sum", "sha256sum",
    "shasum", "cksum", "hexdump", "od", "strings", "column", "tac", "rev", "fold", "comm", "join",
    "paste", "expand", "unexpand", "type", "sort", "uniq",
];

/// Per-program flags that write files, run programs or read file lists outside
/// the path check: (short-flag characters forbidden in any bundle, long flags).
fn forbidden_flags(program: &str) -> (&'static str, &'static [&'static str]) {
    match program {
        // `-o FILE` / `-uoFILE` write; `--compress-program` runs a program
        "sort" => ("o", &["--output", "--compress-program"]),
        // `-o FILE` writes; `-R` (with -H) writes 00Tree.html into every directory
        "tree" => ("oR", &["--output"]),
        // `-C` compiles a magic file next to the input; `-m` reads one from anywhere
        "file" => ("Cm", &["--compile", "--magic-file"]),
        // `date -s` sets the clock
        "date" => ("s", &["--set"]),
        // `--pre CMD` runs a preprocessor; `--hostname-bin` runs a program
        "rg" => ("", &["--pre", "--hostname-bin"]),
        // `-O<pager>` runs a pager (with the matched files) through sh
        "git" => ("O", &["--open-files-in-pager", "--output", "--ext-diff", "--config", "--exec-path", "--upload-pack", "--receive-pack"]),
        _ => ("", &[]),
    }
}

/// `git` subcommands that never write the work tree, index, refs or remotes.
const GIT_READ_ONLY: &[&str] = &[
    "status", "diff", "log", "show", "rev-parse", "ls-files", "blame", "describe", "shortlog",
    "grep", "ls-tree", "cat-file", "rev-list", "merge-base", "name-rev", "whatchanged",
];

/// Long flags that make any otherwise read-only program write a file, run code,
/// or read a list of paths the path check never sees (`--files0-from=list`).
const WRITING_FLAGS: &[&str] = &[
    "--output",
    "--output-file",
    "--ext-diff",
    "--exec",
    "--in-place",
    "--pre",
    "--files0-from",
    "--config",
    "--compress-program",
    "--hostname-bin",
];

/// `find` actions that delete, run programs or write files.
const FIND_ACTIONS: &[&str] = &[
    "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls",
];

/// Token-level rules that apply to every word: shell expansions the path check
/// cannot see through. Brace expansion (`{.,}{.,}/x` → `../x`), `~otheruser`,
/// and globs whose component starts with a dot (`.*/` matches `..`).
fn token_is_opaque(text: &str) -> bool {
    let check = |word: &str| {
        crate::paths::has_brace_expansion(word)
            || (word.starts_with('~') && word != "~" && !word.starts_with("~/"))
            || word
                .split('/')
                .any(|part| part.starts_with('.') && part.contains(['*', '?', '[']))
    };
    check(text) || text.split_once('=').is_some_and(|(_, value)| check(value))
}

/// Whether `command` may run without asking the user.
pub fn is_read_only(command: &str, policy: &ReadOnlyPolicy) -> bool {
    if policy.roots.is_empty() || command.trim().is_empty() {
        return false;
    }
    // Anything the tokenizer flattens or cannot see: substitutions, variables,
    // heredocs, writes via `>`/`>>`/`tee`, background jobs.
    if command.contains(['$', '`'])
        || command.contains("<<")
        || !tokenize::embedded_substitutions(command).is_empty()
        || has_write_redirect(command)
    {
        return false;
    }
    let ctx = RiskContext {
        working_dir: policy.roots.first().cloned(),
        home_dir: policy.home_dir.clone(),
        scratch_dir: None,
    };
    // Belt and braces: a read-only command is never destructive by
    // definition, so any finding at all means our allowlist is wrong.
    if assess(command, &ctx).level > RiskLevel::Low {
        return false;
    }
    let segments = tokenize::split_segments(command);
    if segments.is_empty() {
        return false;
    }
    if segments.iter().flatten().any(|t| token_is_opaque(&t.text)) {
        return false;
    }
    // A lone `&` backgrounds a job; `split_segments` does not split on it.
    if segments
        .iter()
        .flatten()
        .any(|t| t.is_operator && t.text == "&")
    {
        return false;
    }
    segments
        .iter()
        .all(|segment| segment_is_read_only(segment, policy, &ctx))
}

fn segment_is_read_only(tokens: &[Token], policy: &ReadOnlyPolicy, ctx: &RiskContext) -> bool {
    let Some(program) = tokens.first() else {
        return true;
    };
    // `VAR=x cmd` changes the environment of an otherwise known program
    // (`GIT_EXTERNAL_DIFF=…`, `LD_PRELOAD=…`).
    if program.text.contains('=') {
        return false;
    }
    let words: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
    if policy
        .trusted_prefixes
        .iter()
        .any(|prefix| words.len() >= prefix.len() && words[..prefix.len()] == prefix[..])
    {
        return true;
    }
    // Only bare program names: `./ls` or `/tmp/x/cat` could be anything.
    if program.text.contains('/') {
        return false;
    }
    let args = &tokens[1..];
    match program.text.as_str() {
        "git" => git_is_read_only(args, policy, ctx),
        "find" => find_is_read_only(args, policy, ctx),
        "sed" => sed_is_print_only(args, policy, ctx),
        // `uniq in out` writes `out`.
        "uniq" if args.iter().filter(|t| !t.is_flag()).count() > 1 => false,
        // Only the version query: any other cargo command can run a
        // repository-configured `build.rustc-wrapper`.
        "cargo" => args.len() == 1 && matches!(args[0].text.as_str(), "--version" | "-V"),
        name if READ_ONLY_PROGRAMS.contains(&name) => {
            // `tail -f` never exits on its own; the bash timeout ends it,
            // which is harmless but wasteful — still read-only.
            flags_allowed(name, args) && plain_args_ok(args, policy, ctx)
        }
        _ => false,
    }
}

fn git_is_read_only(args: &[Token], policy: &ReadOnlyPolicy, ctx: &RiskContext) -> bool {
    if !flags_allowed("git", args) {
        return false;
    }
    let mut index = 0;
    // Global options before the subcommand.
    while let Some(token) = args.get(index) {
        match token.text.as_str() {
            "--no-pager" | "--no-optional-locks" | "-P" => index += 1,
            "-C" => {
                let Some(dir) = args.get(index + 1) else {
                    return false;
                };
                if !path_inside_roots(&dir.text, policy, ctx) {
                    return false;
                }
                index += 2;
            }
            // `-c key=value` can set core.pager / diff.external to a program.
            _ if token.is_flag() => return false,
            _ => break,
        }
    }
    let Some(sub) = args.get(index) else {
        return false;
    };
    let rest = &args[index + 1..];
    let read_only = match sub.text.as_str() {
        s if GIT_READ_ONLY.contains(&s) => true,
        // `reflog expire` / `reflog delete` rewrite history
        "reflog" => matches!(rest.first().map(|t| t.text.as_str()), None | Some("show")),
        // Listing only: any operand creates/renames/deletes a branch.
        "branch" => rest.iter().all(|t| {
            matches!(
                t.text.as_str(),
                "-a" | "--all"
                    | "-r"
                    | "--remotes"
                    | "-v"
                    | "-vv"
                    | "--verbose"
                    | "--list"
                    | "--show-current"
                    | "--no-color"
                    | "--merged"
                    | "--no-merged"
            )
        }),
        "remote" => rest
            .iter()
            .all(|t| matches!(t.text.as_str(), "-v" | "--verbose")),
        "stash" => matches!(rest.first().map(|t| t.text.as_str()), Some("list" | "show")),
        "tag" => rest
            .iter()
            .all(|t| matches!(t.text.as_str(), "-l" | "--list" | "-n")),
        _ => false,
    };
    read_only && plain_args_ok(rest, policy, ctx)
}

fn find_is_read_only(args: &[Token], policy: &ReadOnlyPolicy, ctx: &RiskContext) -> bool {
    let mut iter = args.iter().peekable();
    while let Some(token) = iter.next() {
        let text = token.text.as_str();
        if FIND_ACTIONS.contains(&text) || WRITING_FLAGS.contains(&text) {
            return false;
        }
        // Predicate operands are patterns, not paths.
        if matches!(
            text,
            "-name" | "-iname" | "-path" | "-ipath" | "-wholename" | "-regex" | "-iregex"
                | "-type" | "-maxdepth" | "-mindepth" | "-newer" | "-mtime" | "-mmin" | "-size"
                | "-printf" | "-user" | "-group" | "-perm"
        ) {
            iter.next();
            continue;
        }
        if !token.is_flag()
            && !matches!(text, "!" | "(" | ")" | "-o" | "-a")
            && !path_inside_roots(text, policy, ctx)
        {
            return false;
        }
    }
    true
}

/// `sed -n '10,20p' file` — the only sed form agents use to read files.
/// Any other script can write (`w`), execute (`e`) or edit in place.
fn sed_is_print_only(args: &[Token], policy: &ReadOnlyPolicy, ctx: &RiskContext) -> bool {
    let mut words = args.iter();
    if words.next().map(|t| t.text.as_str()) != Some("-n") {
        return false;
    }
    let Some(script) = words.next() else {
        return false;
    };
    let script = script.text.as_str();
    let body = script.strip_suffix('p').unwrap_or("");
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || matches!(c, ',' | '$')) {
        return false;
    }
    // GNU sed keeps parsing options after the script (`-e 'w /x'`, `-i`, `-f`):
    // everything after the script must be a file operand.
    let rest: Vec<Token> = words.cloned().collect();
    rest.iter().all(|t| !t.is_flag()) && plain_args_ok(&rest, policy, ctx)
}

/// No forbidden short-flag character in any bundle and no forbidden long flag.
fn flags_allowed(program: &str, args: &[Token]) -> bool {
    let (short, long) = forbidden_flags(program);
    args.iter().filter(|t| t.is_flag()).all(|t| {
        let text = t.text.as_str();
        if let Some(name) = text.strip_prefix("--") {
            let name = name.split_once('=').map(|(k, _)| k).unwrap_or(name);
            !long.iter().any(|l| l.trim_start_matches('-') == name)
        } else {
            // `-n` style bundle; for git, `-O<pager>` puts the value right after
            !text[1..].chars().any(|c| short.contains(c))
        }
    })
}

/// Every flag is non-writing, and every non-flag argument that could be a
/// path resolves inside the allowed roots.
fn plain_args_ok(args: &[Token], policy: &ReadOnlyPolicy, ctx: &RiskContext) -> bool {
    args.iter().all(|token| {
        let text = token.text.as_str();
        if token.is_operator {
            return true;
        }
        if token.is_flag() {
            let name = text.split_once('=').map(|(k, _)| k).unwrap_or(text);
            if WRITING_FLAGS.contains(&name) {
                return false;
            }
            // `--file=/etc/x`, `--git-dir=…`: check the value like a path.
            return match text.split_once('=') {
                Some((_, value)) if looks_like_path(value) => {
                    path_inside_roots(value, policy, ctx)
                }
                _ => true,
            };
        }
        !looks_like_path(text) || path_inside_roots(text, policy, ctx)
    })
}

/// Words that a program could open as a file. Bare words (`main.rs`, `TODO`)
/// resolve relative to the working directory and are therefore inside it.
fn looks_like_path(text: &str) -> bool {
    text.starts_with('/') || text.starts_with('~') || text.contains("..")
}

fn path_inside_roots(raw: &str, policy: &ReadOnlyPolicy, ctx: &RiskContext) -> bool {
    if raw == "/dev/null" {
        return true;
    }
    let expanded = crate::paths::expand(raw, ctx);
    if expanded.as_os_str().is_empty() || expanded.to_string_lossy().contains(['$', '`']) {
        return false;
    }
    policy.roots.iter().any(|root| {
        let root = crate::paths::expand(&root.display().to_string(), ctx);
        Path::new(&expanded).starts_with(&root)
    })
}

/// Any `>` that is not a harmless sink (`2>&1`, `>/dev/null`). `tee` writes
/// too, so it is rejected by name in [`READ_ONLY_PROGRAMS`] absence.
fn has_write_redirect(command: &str) -> bool {
    let mut rest = command.to_string();
    for sink in [
        "2>&1",
        "1>&2",
        ">&2",
        "&>/dev/null",
        "&> /dev/null",
        "2>/dev/null",
        "2> /dev/null",
        ">/dev/null",
        "> /dev/null",
    ] {
        rest = rest.replace(sink, " ");
    }
    rest.contains('>')
}

#[cfg(test)]
#[path = "readonly_tests.rs"]
mod readonly_tests;
