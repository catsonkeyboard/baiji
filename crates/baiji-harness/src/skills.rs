//! Skills 加载
//!
//! Skill 是一个包含 `SKILL.md` 的目录（与常见 agent skills 约定一致）：
//! ```text
//! skills/
//!   deploy/SKILL.md
//!   review/SKILL.md
//! ```
//! `SKILL.md` 支持 YAML 风格 frontmatter（`name` / `description`）。
//!
//! 渐进式披露：系统提示里只列出技能名与描述；模型判断任务匹配时，
//! 通过 [`SkillTool`]（`skill` 工具）按名取回正文及技能目录内的附带文件。
//! 走专用工具而不是 read 工具：用户级技能目录（`~/.baiji/skills`）在工作区白名单之外，
//! 也不应为此把它加进可写的白名单。

use anyhow::Result;
use async_trait::async_trait;
use baiji_agent::{AgentTool, ToolOutput};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 一个技能
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// frontmatter 之后的正文
    pub body: String,
    /// 技能目录（附带文件相对它解析）
    pub dir: PathBuf,
}

/// 从多个技能根目录加载（后者覆盖同名前者）
pub fn load_skills(dirs: &[std::path::PathBuf]) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        // 排序:同一根目录下若多个目录声明了相同的 frontmatter name,
        // 去重结果不依赖文件系统返回顺序——路径字典序大者后处理、获胜
        // (与多根目录"后者覆盖前者"的心智一致)
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
        for entry in entries {
            let skill_file = entry.path().join("SKILL.md");
            if let Some(skill) = load_skill_file(&skill_file) {
                if let Some(existing) = skills.iter_mut().find(|s| s.name == skill.name) {
                    *existing = skill;
                } else {
                    skills.push(skill);
                }
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn load_skill_file(path: &Path) -> Option<Skill> {
    let content = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&content);

    let name = frontmatter
        .as_ref()
        .and_then(|fm| fm.lines().find_map(|l| parse_kv(l, "name")))
        .or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
        })?;

    let description = frontmatter
        .as_ref()
        .and_then(|fm| fm.lines().find_map(|l| parse_kv(l, "description")))
        .unwrap_or_default();

    Some(Skill {
        name,
        description,
        body: body.trim().to_string(),
        dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
    })
}

/// 分离 `---` 包裹的 frontmatter 与正文
pub(crate) fn split_frontmatter(content: &str) -> (Option<String>, String) {
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let frontmatter = rest[..end].trim().to_string();
            let body = rest[end + 4..]
                .trim_start_matches('-')
                .trim_start()
                .to_string();
            return (Some(frontmatter), body);
        }
    }
    (None, content.to_string())
}

pub(crate) fn parse_kv(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    line.trim()
        .strip_prefix(&prefix)
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// 按配置过滤技能:`enabled = false` 全关(返回空);`disabled` 按名剔除。
/// 返回 (保留的技能, 被禁用的名字)——调用方用于日志。
pub fn filter_skills(
    skills: Vec<Skill>,
    enabled: bool,
    disabled: &[String],
) -> (Vec<Skill>, Vec<String>) {
    if !enabled {
        let skipped = skills.iter().map(|s| s.name.clone()).collect();
        return (Vec::new(), skipped);
    }
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for skill in skills {
        if disabled.iter().any(|name| name == &skill.name) {
            skipped.push(skill.name);
        } else {
            kept.push(skill);
        }
    }
    (kept, skipped)
}

/// 生成注入系统提示的技能清单段
pub fn skills_section(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let lines: Vec<String> = skills
        .iter()
        .map(|s| {
            if s.description.is_empty() {
                format!("- {}", s.name)
            } else {
                format!("- {}: {}", s.name, s.description)
            }
        })
        .collect();
    Some(format!(
        "## Available skills\nWhen a task matches one of these skills, call the `skill` tool with its name to load the full instructions BEFORE starting the task:\n{}",
        lines.join("\n")
    ))
}

/// 附带文件的大小上限
const MAX_SKILL_FILE_BYTES: u64 = 256 * 1024;

/// `skill` 工具：按名加载技能正文 / 技能目录内的附带文件
pub struct SkillTool {
    skills: Vec<Skill>,
}

impl SkillTool {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }

    /// 读取技能目录内的附带文件；拒绝绝对路径、`..` 与逃出目录的符号链接
    fn read_bundled(skill: &Skill, file: &str) -> std::result::Result<String, String> {
        let relative = Path::new(file);
        let escapes = relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)));
        if escapes {
            return Err(format!(
                "'{file}' must be a relative path inside the skill directory"
            ));
        }
        let root = skill
            .dir
            .canonicalize()
            .map_err(|e| format!("skill directory unavailable: {e}"))?;
        let target = root
            .join(relative)
            .canonicalize()
            .map_err(|_| format!("no file '{file}' in skill '{}'", skill.name))?;
        if !target.starts_with(&root) {
            return Err(format!("'{file}' resolves outside the skill directory"));
        }
        let size = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        if size > MAX_SKILL_FILE_BYTES {
            return Err(format!("'{file}' is too large ({size} bytes)"));
        }
        std::fs::read_to_string(&target).map_err(|e| format!("cannot read '{file}': {e}"))
    }
}

#[async_trait]
impl AgentTool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load a skill's full instructions by name (see 'Available skills' in the system prompt). \
         Pass 'file' to read a supporting file that the skill's instructions reference."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name"},
                "file": {"type": "string", "description": "Optional path of a supporting file, relative to the skill directory"}
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolOutput> {
        let Some(name) = args["name"].as_str() else {
            return Ok(ToolOutput::err("[Error] missing required argument 'name'"));
        };
        let Some(skill) = self.skills.iter().find(|s| s.name == name) else {
            let available: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
            return Ok(ToolOutput::err(format!(
                "[Error] unknown skill '{name}'. Available: {}",
                available.join(", ")
            )));
        };

        match args["file"].as_str().filter(|f| !f.is_empty()) {
            Some(file) => Ok(match Self::read_bundled(skill, file) {
                Ok(content) => ToolOutput::ok(content),
                Err(e) => ToolOutput::err(format!("[Error] {e}")),
            }),
            None => {
                // 列出附带文件，模型才知道有什么可读
                let mut extras: Vec<String> = std::fs::read_dir(&skill.dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    // 只列文件:目录经 'file' 参数读不出来,列了只会引来失败调用
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|f| f != "SKILL.md" && !f.starts_with('.'))
                    .collect();
                extras.sort();
                let mut out = format!("# Skill: {}\n\n{}", skill.name, skill.body);
                if !extras.is_empty() {
                    out.push_str(&format!(
                        "\n\n[Supporting files — read with the skill tool's 'file' argument: {}]",
                        extras.join(", ")
                    ));
                }
                Ok(ToolOutput::ok(out))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, frontmatter: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn test_load_skills_with_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "deploy",
            "name: deploy\ndescription: How to deploy the service\n",
            "Run ./scripts/deploy.sh after tests pass.",
        );
        write_skill(dir.path(), "plain", "", "No frontmatter here.");

        let skills = load_skills(&[dir.path().to_path_buf()]);
        assert_eq!(skills.len(), 2);

        let deploy = skills.iter().find(|s| s.name == "deploy").unwrap();
        assert_eq!(deploy.description, "How to deploy the service");
        assert!(deploy.body.contains("deploy.sh"));

        // 无 frontmatter 时以目录名为技能名
        let plain = skills.iter().find(|s| s.name == "plain").unwrap();
        assert!(plain.description.is_empty());
    }

    #[test]
    fn test_later_dir_overrides_same_name() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_skill(
            a.path(),
            "deploy",
            "name: deploy\ndescription: old\n",
            "old body",
        );
        write_skill(
            b.path(),
            "deploy",
            "name: deploy\ndescription: new\n",
            "new body",
        );

        let skills = load_skills(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "new");
    }

    #[test]
    fn test_skills_section() {
        assert!(skills_section(&[]).is_none());
        let section = skills_section(&[Skill {
            name: "deploy".into(),
            description: "deploys".into(),
            body: String::new(),
            dir: PathBuf::new(),
        }])
        .unwrap();
        assert!(section.contains("- deploy: deploys"));
        assert!(section.contains("`skill` tool"));
    }

    #[tokio::test]
    async fn test_skill_tool_loads_body_and_bundled_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        write_skill(
            dir.path(),
            "deploy",
            "name: deploy\ndescription: d\n",
            "Run the checklist.",
        );
        std::fs::write(dir.path().join("deploy").join("checklist.md"), "1. test").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("deploy").join("link.txt"),
        )
        .unwrap();

        let tool = SkillTool::new(load_skills(&[dir.path().to_path_buf()]));

        let out = tool
            .execute(serde_json::json!({"name": "deploy"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Run the checklist."));
        assert!(out.content.contains("checklist.md"));

        let out = tool
            .execute(serde_json::json!({"name": "deploy", "file": "checklist.md"}))
            .await
            .unwrap();
        assert_eq!(out.content, "1. test");

        // 逃逸：.. / 绝对路径 / 指向目录外的符号链接
        for file in ["../deploy/SKILL.md", "/etc/passwd", "link.txt"] {
            let out = tool
                .execute(serde_json::json!({"name": "deploy", "file": file}))
                .await
                .unwrap();
            assert!(out.is_error, "should reject {file}: {}", out.content);
            assert!(!out.content.contains("secret"));
        }

        let out = tool
            .execute(serde_json::json!({"name": "nope"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("deploy"));
    }

    #[test]
    fn test_same_root_duplicate_name_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        // 两个目录声明同一个 frontmatter name——路径字典序,后者(bar)稳定覆盖前者(foo)
        write_skill(
            dir.path(),
            "foo",
            "name: shared\ndescription: from foo\n",
            "foo body",
        );
        write_skill(
            dir.path(),
            "bar",
            "name: shared\ndescription: from bar\n",
            "bar body",
        );

        for _ in 0..5 {
            let skills = load_skills(&[dir.path().to_path_buf()]);
            assert_eq!(skills.len(), 1, "same-name dedup");
            assert_eq!(
                skills[0].description, "from bar",
                "path-sorted later entry wins"
            );
        }
    }

    #[test]
    fn test_filter_skills_config() {
        let skills = vec![
            Skill {
                name: "a".into(),
                description: String::new(),
                body: String::new(),
                dir: PathBuf::new(),
            },
            Skill {
                name: "b".into(),
                description: String::new(),
                body: String::new(),
                dir: PathBuf::new(),
            },
        ];

        // 全开
        let (kept, skipped) = filter_skills(skills.clone(), true, &[]);
        assert_eq!(kept.len(), 2);
        assert!(skipped.is_empty());

        // 按名禁用
        let (kept, skipped) = filter_skills(skills.clone(), true, &["b".to_string()]);
        assert_eq!(
            kept.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(skipped, vec!["b".to_string()]);

        // 总开关关闭
        let (kept, skipped) = filter_skills(skills, false, &[]);
        assert!(kept.is_empty());
        assert_eq!(skipped.len(), 2);
    }

    #[tokio::test]
    async fn test_skill_listing_excludes_directories() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "deploy",
            "name: deploy\ndescription: d\n",
            "body",
        );
        std::fs::write(dir.path().join("deploy").join("notes.md"), "n").unwrap();
        std::fs::create_dir(dir.path().join("deploy").join("assets")).unwrap();

        let tool = SkillTool::new(load_skills(&[dir.path().to_path_buf()]));
        let out = tool
            .execute(serde_json::json!({"name": "deploy"}))
            .await
            .unwrap();
        assert!(out.content.contains("notes.md"), "{}", out.content);
        assert!(
            !out.content.contains("assets"),
            "directories must not be listed: {}",
            out.content
        );
    }

    #[test]
    fn test_split_frontmatter_no_marker() {
        let (fm, body) = split_frontmatter("just body text");
        assert!(fm.is_none());
        assert_eq!(body, "just body text");
    }
}
