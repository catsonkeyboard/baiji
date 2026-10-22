//! Path classification: the safety-critical core.
//!
//! Correctness here matters more than anywhere else in the crate, because the
//! catastrophic tier is the one protection that cannot be talked around. It is
//! therefore written to be **absolute and simple**: a small set of paths that
//! may never be recursively destroyed, compared after normalization, with no
//! dependence on correctly understanding the surrounding command.

use super::{RiskContext, RiskFinding, RiskLevel};
use std::path::{Component, Path, PathBuf};

/// Credential stores. Protected *recursively*: destroying a single private key
/// inside `~/.ssh` is as damaging as destroying the directory, so containment
/// matters here and exact-match alone would leave an obvious hole.
const PROTECTED_CREDENTIAL_SUBPATHS: &[&str] = &[".ssh", ".gnupg", ".aws", ".kube", ".docker"];

/// Directories whose *wholesale* destruction is unacceptable, but whose
/// individual files are legitimately edited and removed all the time. Matched
/// exactly, so `~/.config` is protected while `~/.config/app/stale.toml` is not.
const PROTECTED_HOME_SUBPATHS: &[&str] = &[
    ".config",
    ".baiji",
    ".claude",
    ".local",
    ".local/share",
    "Documents",
    "Desktop",
];

/// Absolute system paths that must never be recursively destroyed.
const PROTECTED_SYSTEM_PATHS: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib64",
    "/opt",
    "/proc",
    "/root",
    "/sbin",
    "/srv",
    "/sys",
    "/usr",
    "/var",
    "/Applications",
    "/System",
    "/Library",
    "/Users",
    "/home",
];

/// System paths where the *contents* are as critical as the directory itself,
/// so deleting a single file inside them is also unacceptable.
const SYSTEM_PATHS_PROTECTED_RECURSIVELY: &[&str] = &[
    "/bin", "/boot", "/dev", "/etc", "/lib", "/lib64", "/proc", "/sbin", "/sys", "/usr",
    "/var/lib", "/System", "/Library",
];

/// APFS (the macOS default) and NTFS ignore case: `/users/u` is the home
/// directory and `~/.SSH` the credential store. Linux file systems do not.
const CASE_INSENSITIVE_FS: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// `path` in the form protected paths are compared in: lowercased where the
/// file system ignores case. Used only where a looser match gives the stricter
/// verdict (protected and home paths); working-directory and temp-directory
/// matching stay byte-wise, since folding there could only relax a verdict and
/// would be wrong on a case-sensitive volume.
fn fold_case(path: &Path) -> PathBuf {
    if CASE_INSENSITIVE_FS {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

/// The set of paths this policy protects, exposed for testing and docs.
pub struct ProtectedPaths;

impl ProtectedPaths {
    pub fn home_subpaths() -> &'static [&'static str] {
        PROTECTED_HOME_SUBPATHS
    }
    pub fn credential_subpaths() -> &'static [&'static str] {
        PROTECTED_CREDENTIAL_SUBPATHS
    }
    pub fn system_paths() -> &'static [&'static str] {
        PROTECTED_SYSTEM_PATHS
    }
    pub fn recursive_system_paths() -> &'static [&'static str] {
        SYSTEM_PATHS_PROTECTED_RECURSIVELY
    }
}

/// Resolve `~`, `$HOME`, and relative paths into something comparable.
///
/// This is lexical only: it never touches the filesystem, so it is safe to call
/// on paths that do not exist and cannot be slowed down by a hostile argument.
pub fn expand(raw: &str, ctx: &RiskContext) -> PathBuf {
    let text = expand_known_prefix(raw, ctx);
    // Never normalize unresolved substitutions away (e.g. `$UNKNOWN/..`).
    if text.contains(['$', '`']) {
        return PathBuf::from(text);
    }

    let path = PathBuf::from(&text);
    if path.is_absolute() {
        return normalize(&path);
    }
    match &ctx.working_dir {
        Some(cwd) => normalize(&cwd.join(path)),
        None => path,
    }
}

/// Expand only complete, known leading variables. Prefix lookalikes such as
/// `$HOME_BACKUP` and shell parameter operators remain unresolved.
fn expand_known_prefix(raw: &str, ctx: &RiskContext) -> String {
    for (name, value) in [
        ("HOME", &ctx.home_dir),
        ("BAIJI_SCRATCH_DIR", &ctx.scratch_dir),
    ] {
        let Some(value) = value else { continue };
        for prefix in [format!("${name}"), format!("${{{name}}}")] {
            if let Some(rest) = raw.strip_prefix(&prefix)
                && (rest.is_empty() || rest.starts_with('/'))
            {
                return format!("{}{rest}", value.display());
            }
        }
    }
    if let Some(home) = &ctx.home_dir {
        if raw == "~" {
            return home.display().to_string();
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return format!("{}/{rest}", home.display());
        }
        // `~name` naming the current user is the home directory too:
        // `rm -rf ~alice` must not slip past the home check as a relative path.
        // macOS resolves the name case-insensitively (`~ALICE`).
        if let Some(user) = home.file_name().and_then(|n| n.to_str())
            && let Some(named) = raw.strip_prefix('~')
        {
            let (name, rest) = named.split_at(named.find('/').unwrap_or(named.len()));
            if name == user || (CASE_INSENSITIVE_FS && name.to_lowercase() == user.to_lowercase()) {
                return format!("{}{rest}", home.display());
            }
        }
    }
    raw.to_string()
}

/// Lexically remove `.` and `..` so `/home/u/../..` is seen as `/`.
///
/// Without this, `rm -rf ~/../..` would walk straight past the protected-path
/// check. Note this is intentionally *not* symlink-aware; see the crate docs on
/// defense in depth.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        return PathBuf::from("/");
    }
    out
}

/// Whether destroying this path is categorically unacceptable.
///
/// Exposed separately because this is the single most important predicate in
/// the crate and deserves to be testable in isolation.
pub fn is_catastrophic_target(path: &Path, ctx: &RiskContext) -> bool {
    let path = fold_case(&normalize(path));
    let protected = |p: &str| fold_case(Path::new(p));

    // Exact system roots, plus anything inside the ones whose contents are as
    // unrecoverable as the directory itself (`/etc/passwd`). `/home` and
    // `/Users` are deliberately not recursive: a user's own project lives
    // under them, and the home directory itself is handled below.
    if PROTECTED_SYSTEM_PATHS.iter().any(|p| path == protected(p)) {
        return true;
    }
    if SYSTEM_PATHS_PROTECTED_RECURSIVELY
        .iter()
        .any(|p| path.starts_with(protected(p)))
    {
        return true;
    }

    let Some(home) = &ctx.home_dir else {
        return false;
    };
    let home = fold_case(&normalize(home));

    // The home directory itself.
    if path == home {
        return true;
    }
    // Credential stores, including anything inside them.
    if PROTECTED_CREDENTIAL_SUBPATHS
        .iter()
        .any(|sub| path.starts_with(home.join(protected(sub))))
    {
        return true;
    }
    // Config and document roots, but not their individual files.
    PROTECTED_HOME_SUBPATHS
        .iter()
        .any(|sub| path == home.join(protected(sub)))
}

/// Classify one resolved target, returning a finding when it is notable.
pub fn classify_target(
    expanded: &Path,
    raw: &str,
    recursive: bool,
    ctx: &RiskContext,
) -> Option<RiskFinding> {
    // Brace expansion and `~otheruser` are shell expansions we do not perform:
    // `rm -rf {~,x}` or `rm -rf ~bob` would otherwise look
    // like a harmless relative path inside the working directory.
    let expanded_raw = expand_known_prefix(raw, ctx);
    if has_brace_expansion(raw) || expanded_raw.starts_with('~') {
        return Some(RiskFinding {
            level: RiskLevel::Confirm,
            reason: "target uses brace or `~user` expansion, so the affected paths cannot be \
                     checked before execution"
                .to_string(),
            target: Some(raw.to_string()),
        });
    }

    // Glob and variable expansion we did not perform: we cannot know the
    // footprint, so escalate rather than guess.
    if (raw.contains('*') || raw.contains('?') || raw.contains('['))
        && !expand_known_prefix(raw, ctx).contains(['$', '`'])
    {
        // A bare `/*` or `~/*` is catastrophic in effect even though no single
        // resolved path is protected.
        if let Some(parent_of_glob) = expanded.parent()
            && is_catastrophic_target(parent_of_glob, ctx)
            && expanded.file_name().is_some_and(|n| n == "*")
        {
            return Some(RiskFinding {
                level: RiskLevel::Catastrophic,
                reason: "would destroy the entire contents of a protected directory".to_string(),
                target: Some(raw.to_string()),
            });
        }
        // A glob in a filename cannot escape its concrete parent directory.
        // Keep wildcard directory components conservative because their parent
        // is not the actual expansion boundary (`/home/u/*/node_modules`).
        if let Some(parent) = expanded.parent()
            && !parent.to_string_lossy().contains(['*', '?'])
        {
            let inside_cwd = ctx
                .working_dir
                .as_ref()
                .is_some_and(|cwd| parent.starts_with(normalize(cwd)));
            if inside_cwd || is_temp_path(parent) {
                return Some(RiskFinding {
                    level: RiskLevel::Low,
                    reason: "glob is bounded to the working or temporary directory".to_string(),
                    target: Some(raw.to_string()),
                });
            }
        }
        return Some(RiskFinding {
            level: RiskLevel::Confirm,
            reason: "target contains a glob, so the exact set of affected files \
                     is not known before execution"
                .to_string(),
            target: Some(raw.to_string()),
        });
    }

    // Check the resolved path first: `$HOME` expands to a protected path and
    // must be denied outright, not merely queried. Only genuinely unresolvable
    // substitutions fall through to the Confirm tier below.
    if is_catastrophic_target(expanded, ctx) {
        return Some(RiskFinding {
            level: RiskLevel::Catastrophic,
            reason: "targets a protected system or home path that must never be \
                     destroyed"
                .to_string(),
            target: Some(expanded.display().to_string()),
        });
    }

    if expand_known_prefix(raw, ctx).contains(['$', '`']) {
        return Some(RiskFinding {
            level: RiskLevel::Confirm,
            reason: "target is computed at runtime (variable or command \
                     substitution), so its value cannot be checked in advance"
                .to_string(),
            target: Some(raw.to_string()),
        });
    }

    // Raw device nodes are never a safe write target.
    if expanded.starts_with("/dev")
        && !expanded.starts_with("/dev/null")
        && !expanded.starts_with("/dev/stdout")
        && !expanded.starts_with("/dev/stderr")
    {
        return Some(RiskFinding {
            level: RiskLevel::Catastrophic,
            reason: "writes directly to a device node, which can destroy a \
                     filesystem or disk"
                .to_string(),
            target: Some(expanded.display().to_string()),
        });
    }

    let inside_cwd = ctx
        .working_dir
        .as_ref()
        .is_some_and(|cwd| expanded.starts_with(normalize(cwd)));

    if inside_cwd {
        // The routine case: an agent tidying up its own workspace.
        return recursive.then(|| RiskFinding {
            level: RiskLevel::Low,
            reason: "recursive delete inside the working directory".to_string(),
            target: Some(expanded.display().to_string()),
        });
    }

    if is_temp_path(expanded) {
        return None;
    }

    // A concrete path being outside the workspace is not, by itself, a reason
    // to interrupt the agent. Protected home/system paths and unknown targets
    // have already returned above. Keep this as Low so the operation remains
    // visible in risk logs without requiring a justification retry.
    Some(RiskFinding {
        level: RiskLevel::Low,
        reason: "destructive operation targets a concrete path outside the working directory"
            .to_string(),
        target: Some(expanded.display().to_string()),
    })
}

/// Shell brace expansion (`{a,b}`, `{1..3}`), as opposed to `${VAR}`.
pub(crate) fn has_brace_expansion(raw: &str) -> bool {
    let chars: Vec<char> = raw.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c != '{' || (i > 0 && chars[i - 1] == '$') {
            continue;
        }
        let Some(close) = chars[i + 1..].iter().position(|c| *c == '}') else {
            continue;
        };
        let inner: String = chars[i + 1..i + 1 + close].iter().collect();
        if inner.contains(',') || inner.contains("..") {
            return true;
        }
    }
    false
}

/// Temp directories are conventionally disposable, so deleting inside them is
/// not worth a reflection turn.
fn is_temp_path(path: &Path) -> bool {
    ["/tmp", "/var/tmp", "/private/tmp"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod paths_tests;
