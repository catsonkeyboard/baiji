//! Read-only detection tests.
//!
//! Precision matters more than recall here: every `true` skips a human, so the
//! negative sweeps (writes, code execution, paths outside the workspace) are
//! the important half.

use super::*;

fn policy() -> ReadOnlyPolicy {
    ReadOnlyPolicy {
        roots: vec![PathBuf::from("/home/u/proj")],
        home_dir: Some(PathBuf::from("/home/u")),
        trusted_prefixes: Vec::new(),
    }
}

#[test]
fn common_inspection_commands_are_read_only() {
    let policy = policy();
    for cmd in [
        "ls",
        "ls -la src",
        "pwd",
        "cat Cargo.toml",
        "head -n 50 src/main.rs",
        "tail -20 logs/app.log",
        "wc -l src/*.rs",
        "grep -rn TODO src",
        "rg -n 'fn main' crates",
        "git status",
        "git status --short",
        "git diff",
        "git diff --stat HEAD~1",
        "git log --oneline -20",
        "git show HEAD:src/lib.rs",
        "git branch",
        "git branch -a",
        "git remote -v",
        "git -C crates/foo status",
        "git --no-pager log -5",
        "git stash list",
        "find . -name '*.rs' -type f",
        "find src -maxdepth 2",
        "sed -n '10,40p' src/lib.rs",
        "sed -n '5p' README.md",
        "cat a.txt | grep foo | wc -l",
        "ls src && git status",
        "git log -3 2>/dev/null",
        "cargo --version",
        "grep -i todo src",
        "grep -o 'fn [a-z_]*' src/lib.rs",
        "sort -n data.txt",
        "sort -rk2 data.txt | uniq -c",
        "git reflog",
        "git reflog show -5",
        "tree -L 2 src",
        "du -sh target",
        "diff a.txt b.txt",
        "sort names.txt | uniq -c",
        "cat /home/u/proj/README.md",
        "ls ./crates/../src",
    ] {
        assert!(is_read_only(cmd, &policy), "should be read-only: {cmd}");
    }
}

#[test]
fn writes_and_side_effects_are_not_read_only() {
    let policy = policy();
    for cmd in [
        "echo hi > out.txt",
        "echo hi >> out.txt",
        "cat a | tee b",
        "ls > /tmp/listing",
        "git commit -m wip",
        "git push",
        "git checkout main",
        "git branch -D old",
        "git branch new-branch",
        "git stash",
        "git stash drop",
        "git tag v1.0",
        "git -c core.pager=evil log",
        "git diff --output=patch.diff",
        "git diff --ext-diff",
        "find . -delete",
        "find . -name x -exec rm {} ;",
        "find . -fprint out.txt",
        "sed -i 's/a/b/' file",
        "sed 's/a/b/w out' file",
        "sed -n '1e rm -rf ~' file",
        "sort -o out.txt in.txt",
        "uniq in.txt out.txt",
        "rm -rf build",
        "touch new.txt",
        "mkdir x",
        "cargo build",
        "cargo test",
        "npm install",
        "make",
        "python3 script.py",
        "sh -c 'ls'",
        "bash script.sh",
        "./ls",
        "/tmp/evil/cat x",
        "sleep 100 &",
        "ls & rm -rf build",
        "env",
        "printenv",
        "curl https://example.com",
        "xargs cat",
        "sudo ls",
        "LD_PRELOAD=/tmp/x.so ls",
        "GIT_EXTERNAL_DIFF=evil git diff",
        "",
        "   ",
    ] {
        assert!(!is_read_only(cmd, &policy), "must not be read-only: {cmd:?}");
    }
}

#[test]
fn substitutions_and_variables_are_not_read_only() {
    let policy = policy();
    for cmd in [
        "cat $(echo /etc/passwd)",
        "cat `which ls`",
        "cat $HOME/.ssh/id_rsa",
        "echo $OPENAI_API_KEY",
        "ls ${DIR}",
        "cat <<EOF\nhi\nEOF",
    ] {
        assert!(!is_read_only(cmd, &policy), "must not be read-only: {cmd}");
    }
}

#[test]
fn paths_outside_the_workspace_are_not_auto_approved() {
    let policy = policy();
    for cmd in [
        "cat ~/.ssh/id_rsa",
        "cat /etc/passwd",
        "ls /",
        "ls ~",
        "cat ../other-project/secrets.env",
        "head ../../.aws/credentials",
        "grep -r token /home/u",
        "find / -name '*.pem'",
        "git -C /etc status",
        "rg --file=/etc/shadow x",
        "cat src/../../outside.txt",
    ] {
        assert!(!is_read_only(cmd, &policy), "must not be read-only: {cmd}");
    }
}

#[test]
fn extra_roots_extend_the_allowed_area() {
    let mut policy = policy();
    policy.roots.push(PathBuf::from("/home/u/shared-docs"));
    assert!(is_read_only("cat /home/u/shared-docs/notes.md", &policy));
    assert!(!is_read_only("cat /home/u/other/notes.md", &policy));
}

#[test]
fn trusted_prefixes_approve_matching_segments_only() {
    let policy = policy().with_trusted_prefixes(["cargo test", "cargo check", "npm run lint"]);
    for cmd in [
        "cargo test",
        "cargo test -p baiji-agent -- --nocapture",
        "cargo check --workspace",
        "npm run lint",
        "cargo check && git status",
    ] {
        assert!(is_read_only(cmd, &policy), "trusted: {cmd}");
    }
    for cmd in [
        "cargo build",
        "cargo test > out.txt",
        "cargo test && rm -rf src",
        "cargo testx",
        "npm run lint:fix",
        "cargo test $(rm -rf ~)",
    ] {
        assert!(!is_read_only(cmd, &policy), "not trusted: {cmd}");
    }
}

#[test]
fn empty_roots_never_auto_approve() {
    let policy = ReadOnlyPolicy::default();
    assert!(!is_read_only("ls", &policy));
}

#[test]
fn reviewer_found_bypasses_are_not_read_only() {
    let policy = policy();
    for cmd in [
        // 经由 pager / 配置执行任意程序
        "git grep '-Otouch PWNED; true' -l TODO",
        "git grep -Orm -l TODO",
        "git grep --open-files-in-pager=sh -l TODO",
        "git log --config=core.pager=sh",
        "cargo metadata",
        "cargo tree",
        "cargo metadata --config 'build.rustc-wrapper=\"./w.sh\"'",
        "rg --hostname-bin=./x --hyperlink-format=default foo",
        "rg --pre ./x foo",
        // 写文件
        "xxd a.txt Cargo.toml",
        "echo 41 | xxd -r -p - x.sh",
        "sort -o/home/u/.zshrc a.txt",
        "sort -uo Cargo.toml x",
        "sort --compress-program=./x big.txt",
        "git reflog expire --expire=now --all",
        "git reflog delete HEAD@{1}",
        "tree -R -H . src",
        "tree -o out.html",
        "file -C -m x",
        "date -s 2020-01-01",
        "sed -n 1p -e 'w /abs' file",
        "sed -n 1p -i.bak file",
        "sed -n 1p -f script.sed file",
        // 读白名单外 / 泄漏密钥
        "cat ~u/.ssh/id_rsa",
        "cat ~root/.bash_history",
        "cat .*/.*/etc/passwd",
        "cat {.,}{.,}/secret",
        "cat .?/x",
        "jq -n env",
        "wc --files0-from=list.txt",
        "less README.md",
    ] {
        assert!(!is_read_only(cmd, &policy), "must not be read-only: {cmd}");
    }
}
