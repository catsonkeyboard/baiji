use std::env;
use std::path::Path;
use tokio::fs;
use tracing::{debug, warn};

/// 加载 AGENTS.md 文件内容
///
/// 从当前工作目录查找 AGENTS.md 文件，如果存在则返回其内容。
/// 用于将项目记忆注入到 system prompt 中。
pub async fn load_agents_md() -> Option<String> {
    let cwd = match env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            warn!("Failed to get current directory: {}", e);
            return None;
        }
    };
    load_agents_md_from_path(&cwd).await
}

/// 从指定路径加载 AGENTS.md（内部函数，用于测试）
async fn load_agents_md_from_path(dir: &Path) -> Option<String> {
    let path = dir.join("AGENTS.md");
    debug!("Looking for AGENTS.md at: {:?}", path);

    if !path.exists() {
        debug!("AGENTS.md not found in current directory");
        return None;
    }

    if !path.is_file() {
        warn!("AGENTS.md exists but is not a file");
        return None;
    }

    match fs::read_to_string(&path).await {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                debug!("AGENTS.md is empty");
                return None;
            }
            debug!("Successfully loaded AGENTS.md ({} bytes)", trimmed.len());
            Some(trimmed.to_string())
        }
        Err(e) => {
            warn!("Failed to read AGENTS.md: {}", e);
            None
        }
    }
}

/// 构建包含项目记忆的 system prompt
///
/// 将原始 system prompt 与当前日期、AGENTS.md 内容合并
pub fn build_system_prompt(base_prompt: &str, agents_md: Option<String>) -> String {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut parts = vec![
        base_prompt.to_string(),
        format!("[Current Date]\nToday is {}.", today),
    ];
    if let Some(memory) = agents_md {
        parts.push(format!("[Project Context]\n{}", memory));
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::fs as tokio_fs;

    #[tokio::test]
    async fn test_load_agents_md_not_found() {
        // 在临时目录中运行，AGENTS.md 不存在
        let temp_dir = TempDir::new().unwrap();

        let result = load_agents_md_from_path(temp_dir.path()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_load_agents_md_exists() {
        let temp_dir = TempDir::new().unwrap();

        // 创建测试用的 AGENTS.md
        let content = "# Project Memory\n\nThis is a test project.";
        tokio_fs::write(temp_dir.path().join("AGENTS.md"), content)
            .await
            .unwrap();

        let result = load_agents_md_from_path(temp_dir.path()).await;
        assert_eq!(result, Some(content.to_string()));
    }

    #[tokio::test]
    async fn test_load_agents_md_empty() {
        let temp_dir = TempDir::new().unwrap();

        // 创建空的 AGENTS.md
        tokio_fs::write(temp_dir.path().join("AGENTS.md"), "   ").await.unwrap();

        let result = load_agents_md_from_path(temp_dir.path()).await;
        assert!(result.is_none());
    }

    #[test]
    fn test_build_system_prompt_with_memory() {
        let base = "You are a helpful assistant.";
        let memory = Some("Project: baiji\nTech: Rust".to_string());

        let result = build_system_prompt(base, memory);
        assert!(result.contains(base));
        assert!(result.contains("[Current Date]"));
        assert!(result.contains("[Project Context]"));
        assert!(result.contains("Project: baiji"));
    }

    #[test]
    fn test_build_system_prompt_without_memory() {
        let base = "You are a helpful assistant.";

        let result = build_system_prompt(base, None);
        assert!(result.contains(base));
        assert!(result.contains("[Current Date]"));
        assert!(!result.contains("[Project Context]"));
    }
}
