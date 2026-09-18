//! UI 主题

use ratatui::style::Color;

/// 配色方案（浅色/深色终端各一套）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub user: Color,
    pub assistant: Color,
    pub tool: Color,
    pub system: Color,
    pub accent: Color,
    pub error: Color,
    /// 用户消息整行的底色条（`❯` 行高亮背景）
    pub highlight: Color,
}

impl Theme {
    /// 深色主题（默认）：近黑底上的三档灰 + 少量彩色强调（Grok/Claude Code 风格）
    pub fn dark() -> Self {
        Self {
            user: Color::White,      // `❯ ` 前缀加粗，靠符号而非颜色区分
            assistant: Color::Gray,  // 回答正文：中灰
            tool: Color::Yellow,     // ● 工具活动
            system: Color::DarkGray, // 元信息/边框/⎿ 结果
            accent: Color::Cyan,
            error: Color::Red,
            highlight: Color::Rgb(38, 38, 38), // #262626 用户消息底条
        }
    }

    /// 浅色主题
    pub fn light() -> Self {
        Self {
            user: Color::Black,
            assistant: Color::Rgb(60, 60, 60), // 深灰
            tool: Color::Rgb(180, 110, 0),     // 深黄
            system: Color::Rgb(130, 130, 130),
            accent: Color::Blue,
            error: Color::Rgb(200, 0, 0),
            highlight: Color::Rgb(228, 228, 228), // 浅灰用户消息底条
        }
    }

    /// 按配置名解析（未知值回退 dark）
    pub fn parse(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "light" => Self::light(),
            _ => Self::dark(),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_theme_parse() {
        assert_eq!(Theme::parse("dark"), Theme::dark());
        assert_eq!(Theme::parse("light"), Theme::light());
        assert_eq!(Theme::parse("Light"), Theme::light());
        assert_eq!(Theme::parse("whatever"), Theme::dark());
        assert_ne!(Theme::dark(), Theme::light());
    }
}
