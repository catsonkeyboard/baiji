/// Agent 提示词
pub struct AgentPrompt;

impl AgentPrompt {
    /// 获取系统提示词
    pub fn system_prompt() -> String {
        r#"You are a helpful AI assistant with access to tools.

Guidelines:
1. Use tools only when the question genuinely requires external information or actions.
2. Call each tool at most once per question unless a follow-up search is clearly necessary.
3. After receiving tool results, synthesize the information and give a final answer directly — do NOT keep calling more tools unless the result was empty or an error.
4. If you already have enough information to answer, respond immediately without using tools.
5. Keep answers concise and focused on what was asked."#.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_prompt() {
        let prompt = AgentPrompt::system_prompt();
        assert!(prompt.contains("tools"));
        assert!(prompt.contains("final answer"));
    }
}
