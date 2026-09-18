//! CLI 参数解析（无外部依赖的手写解析）

/// 解析后的 CLI 选项
#[derive(Debug, Default, PartialEq)]
pub struct CliOptions {
    /// `-e/--exec <msg>`：headless 一次性执行
    pub exec: Option<String>,
    /// `--session <id>`：恢复指定会话（配合 --exec；TUI 下忽略）
    pub session: Option<String>,
    /// `--yes/-y`：headless 模式自动放行确认名单内的工具（默认拒绝）
    pub yes: bool,
    /// `--sessions`：列出全部会话后退出
    pub list_sessions: bool,
    /// `--continue-until-done`：headless 自动接力（todo 未完成则继续，直到完成或轮次上限）
    pub continue_until_done: bool,
    /// `-h/--help`
    pub help: bool,
}

pub const USAGE: &str = "\
baiji — 终端 AI coding agent

用法:
  baiji                        交互式 TUI
  baiji -e \"<消息>\" [--yes]    headless 一次性执行（回答打印到 stdout）
  baiji -e \"<消息>\" --session <id>
                               在指定历史会话上继续
  baiji --sessions             列出全部会话
  baiji -h | --help            本帮助

选项:
  -e, --exec <msg>     headless 执行的消息
  --session <id>       恢复的会话 ID（见 --sessions）
  -y, --yes            headless 模式自动放行需确认的工具（默认拒绝）
      --continue-until-done
                       自动接力：todo 未完成则继续，直到完成或轮次上限
      --sessions       列出会话
  环境变量 BAIJI_TELEMETRY=file 时，把 span/event 追加写入 ~/.baiji/traces/";

/// 解析参数（未知参数忽略，保持前向兼容）
pub fn parse(args: &[String]) -> CliOptions {
    let mut opts = CliOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => opts.help = true,
            "--sessions" | "--list-sessions" => opts.list_sessions = true,
            "-y" | "--yes" => opts.yes = true,
            "--continue-until-done" => opts.continue_until_done = true,
            "-e" | "--exec" => {
                if let Some(msg) = args.get(i + 1) {
                    opts.exec = Some(msg.clone());
                    i += 1;
                } else {
                    opts.help = true; // -e 缺参数 → 显示用法
                }
            }
            "--session" => {
                // 缺值 / 后面跟的是另一个选项 → 显示用法，而不是静默开一个新会话
                match args.get(i + 1).filter(|v| !v.starts_with('-')) {
                    Some(id) => {
                        opts.session = Some(id.clone());
                        i += 1;
                    }
                    None => opts.help = true,
                }
            }
            _ => {}
        }
        i += 1;
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| String::from(*s)).collect()
    }

    #[test]
    fn test_parse_defaults_to_tui() {
        assert_eq!(parse(&[]), CliOptions::default());
    }

    #[test]
    fn test_parse_exec_and_flags() {
        let opts = parse(&args(&["-e", "帮我修这个 bug"]));
        assert_eq!(opts.exec.as_deref(), Some("帮我修这个 bug"));
        assert!(!opts.yes);

        let opts = parse(&args(&["--exec", "hello", "--yes"]));
        assert_eq!(opts.exec.as_deref(), Some("hello"));
        assert!(opts.yes);

        let opts = parse(&args(&["--exec", "继续", "--session", "sess_1", "-y"]));
        assert_eq!(opts.session.as_deref(), Some("sess_1"));
        assert!(opts.yes);
    }

    #[test]
    fn test_parse_exec_without_value_shows_help() {
        let opts = parse(&args(&["-e"]));
        assert!(opts.help);
        assert_eq!(opts.exec, None);
    }

    #[test]
    fn test_parse_list_and_help() {
        assert!(parse(&args(&["-e", "x", "--session"])).help);
        assert!(parse(&args(&["--session", "-y", "-e", "x"])).help);
        assert!(parse(&args(&["--sessions"])).list_sessions);
        assert!(parse(&args(&["--list-sessions"])).list_sessions);
        assert!(parse(&args(&["-h"])).help);
        assert!(parse(&args(&["--help"])).help);
    }

    #[test]
    fn test_parse_continue_until_done() {
        let opts = parse(&args(&["-e", "做个大任务", "--continue-until-done"]));
        assert!(opts.continue_until_done);
        assert_eq!(opts.exec.as_deref(), Some("做个大任务"));
        assert!(!parse(&args(&["-e", "x"])).continue_until_done);
    }

    #[test]
    fn test_parse_ignores_unknown() {
        let opts = parse(&args(&["--future-flag", "positional"]));
        assert_eq!(opts, CliOptions::default());
    }
}
