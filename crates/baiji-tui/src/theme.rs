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
}

impl Theme {
    /// 深色主题（默认）
    pub fn dark() -> Self {
        Self {
            user: Color::Green,
            assistant: Color::White,
            tool: Color::Yellow,
            system: Color::DarkGray,
            accent: Color::Cyan,
            error: Color::Red,
        }
    }

    /// 浅色主题
    pub fn light() -> Self {
        Self {
            user: Color::Rgb(0, 128, 0), // 深绿
            assistant: Color::Black,
            tool: Color::Rgb(180, 110, 0), // 深黄
            system: Color::Rgb(120, 120, 120),
            accent: Color::Blue,
            error: Color::Rgb(200, 0, 0),
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
