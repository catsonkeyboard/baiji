//! Prompt 模板：`{{var}}` / `{{ var }}` 占位符替换
//!
//! 两种用途：
//! - **系统提示模板**：基础 prompt（内置或用户的 `system.md`）里的
//!   `{{cwd}}` `{{date}}` `{{os}}` `{{model}}` 在每次运行时渲染
//! - **用户 prompt 模板**：`prompts/<name>.md`，在输入框里用 `/<name> 参数…` 调用；
//!   正文可用 `{{args}}`（全部参数）、`{{1}}` `{{2}}`…（按空白切分的位置参数）及上述环境变量
//!
//! ```text
//! ~/.baiji/prompts/review.md          （用户级）
//! ./.baiji/prompts/review.md          （项目级，同名覆盖用户级）
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

/// 一个用户 prompt 模板
#[derive(Debug, Clone, PartialEq)]
pub struct PromptTemplate {
    /// 调用名（文件名去掉 `.md`）
    pub name: String,
    /// frontmatter 的 `description`（可选，用于 /help 列表）
    pub description: String,
    pub body: String,
}

/// 从多个目录加载 `*.md` 模板（后者覆盖同名前者）
pub fn load_templates(dirs: &[PathBuf]) -> Vec<PromptTemplate> {
    let mut templates: Vec<PromptTemplate> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };
            // 调用名要能在输入框里敲出来：`/name`
            if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (frontmatter, body) = crate::skills::split_frontmatter(&content);
            let description = frontmatter
                .as_ref()
                .and_then(|fm| fm.lines().find_map(|l| crate::skills::parse_kv(l, "description")))
                .unwrap_or_default();
            let template = PromptTemplate {
                name: name.to_string(),
                description,
                body: body.trim().to_string(),
            };
            match templates.iter_mut().find(|t| t.name == template.name) {
                Some(existing) => *existing = template,
                None => templates.push(template),
            }
        }
    }
    templates.sort_by(|a, b| a.name.cmp(&b.name));
    templates
}

/// 输入形如 `/name 参数…` 且 name 是已加载的模板时，返回渲染后的 prompt；否则 None。
pub fn expand_invocation(
    input: &str,
    templates: &[PromptTemplate],
    env_vars: &HashMap<&str, String>,
) -> Option<String> {
    let rest = input.trim().strip_prefix('/')?;
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    let template = templates.iter().find(|t| t.name == name)?;

    let mut vars = env_vars.clone();
    vars.insert("args", args.to_string());
    let positional: Vec<String> = args.split_whitespace().map(String::from).collect();
    // 位置参数的 key 需要 'static 之外的生命周期：先物化成 String，再借用
    let keys: Vec<String> = (1..=positional.len()).map(|i| i.to_string()).collect();
    for (key, value) in keys.iter().zip(&positional) {
        vars.insert(key.as_str(), value.clone());
    }
    let rendered = render(&template.body, &vars);
    // 模板没用到任何参数占位符时，把参数附在末尾（常见写法：/review src/main.rs）
    let uses_args = template.body.contains("{{args}}")
        || template.body.contains("{{ args }}")
        || template.body.contains("{{1}}")
        || template.body.contains("{{ 1 }}");
    Some(if args.is_empty() || uses_args {
        rendered
    } else {
        format!("{rendered}\n\n{args}")
    })
}

/// 渲染模板。未提供的变量替换为空串。
///
/// 替换值**不会**被再次扫描：用户参数里的 `{{…}}` 原样保留，不会触发二次展开。
pub fn render(template: &str, vars: &HashMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = after[..end].trim();
                out.push_str(vars.get(key).map(String::as_str).unwrap_or(""));
                rest = &after[end + 2..];
            }
            None => {
                // 未闭合：原样保留
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_basic() {
        let mut vars = HashMap::new();
        vars.insert("name", "baiji".to_string());
        vars.insert("lang", "Rust".to_string());

        assert_eq!(
            render("Hello {{name}}, written in {{ lang }}!", &vars),
            "Hello baiji, written in Rust!"
        );
    }

    #[test]
    fn test_render_missing_and_unclosed() {
        let vars = HashMap::new();
        assert_eq!(render("value={{missing}}", &vars), "value=");
        assert_eq!(render("unclosed {{ oops", &vars), "unclosed {{ oops");
        assert_eq!(render("no vars", &vars), "no vars");
    }

    #[test]
    fn test_render_adjacent_placeholders() {
        let mut vars = HashMap::new();
        vars.insert("a", "1".to_string());
        vars.insert("b", "2".to_string());
        assert_eq!(render("{{a}}{{b}}", &vars), "12");
    }

    #[test]
    fn test_load_and_expand_templates() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join("review.md"),
            "---\ndescription: user-level review\n---\nOLD {{args}}",
        )
        .unwrap();
        std::fs::write(
            project.path().join("review.md"),
            "---\ndescription: Review a file\n---\nReview {{1}} in {{cwd}}. Focus: {{args}}",
        )
        .unwrap();
        std::fs::write(project.path().join("plain.md"), "Summarize the repo.").unwrap();
        std::fs::write(project.path().join("bad name.md"), "x").unwrap();
        std::fs::write(project.path().join("notes.txt"), "x").unwrap();

        let templates =
            load_templates(&[user.path().to_path_buf(), project.path().to_path_buf()]);
        let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["plain", "review"]);
        assert_eq!(templates[1].description, "Review a file"); // 项目级覆盖用户级

        let mut env = HashMap::new();
        env.insert("cwd", "/work".to_string());
        assert_eq!(
            expand_invocation("/review src/a.rs security", &templates, &env).unwrap(),
            "Review src/a.rs in /work. Focus: src/a.rs security"
        );
        // 模板不含参数占位符 → 参数附在末尾
        assert_eq!(
            expand_invocation("/plain focus on tests", &templates, &env).unwrap(),
            "Summarize the repo.\n\nfocus on tests"
        );
        assert_eq!(expand_invocation("/plain", &templates, &env).unwrap(), "Summarize the repo.");
        // 非模板 / 普通文本
        assert!(expand_invocation("/unknown x", &templates, &env).is_none());
        assert!(expand_invocation("just text /review", &templates, &env).is_none());
        // 参数里的占位符不会二次展开
        assert_eq!(
            expand_invocation("/review {{cwd}}", &templates, &env).unwrap(),
            "Review {{cwd}} in /work. Focus: {{cwd}}"
        );
    }
}
