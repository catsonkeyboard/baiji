//! 声明行判定（保守：宁可多报一行也不漏符号）。
//! 被 signatures.rs 的正则回退路径与 read 的测试引用。

pub(crate) fn is_signature_line(line: &str) -> bool {
    let t = line.trim_start();
    let indent_depth = line.len() - t.len();

    // Rust
    if t.starts_with("pub ") || t.starts_with("fn ") || t.starts_with("async fn ") {
        return t.contains("fn ")
            || t.starts_with("pub struct")
            || t.starts_with("pub enum")
            || t.starts_with("pub trait")
            || t.starts_with("pub mod")
            || t.starts_with("pub const")
            || t.starts_with("pub static");
    }
    if t.starts_with("struct ")
        || t.starts_with("enum ")
        || t.starts_with("trait ")
        || t.starts_with("impl ")
        || t.starts_with("mod ")
        || t.starts_with("macro_rules!")
    {
        return true;
    }
    // TS/JS
    if t.starts_with("export ")
        || t.starts_with("function ")
        || t.starts_with("class ")
        || t.starts_with("interface ")
        || t.starts_with("type ")
    {
        return t.contains("function")
            || t.contains("class")
            || t.contains("interface")
            || t.contains("=>")
            || t.contains("type ")
            || t.ends_with('{');
    }
    // Go
    if t.starts_with("func ")
        || t.starts_with("type ") && t.contains(" struct")
        || t.starts_with("type ") && t.contains(" interface")
    {
        return true;
    }
    // Python / Java / C 系
    if t.starts_with("def ") || t.starts_with("class ") || t.starts_with("async def ") {
        return true;
    }
    // 缩进的 impl 块内方法（Rust/Java/C++）：8 空格内的 fn/def/pubic 等
    if indent_depth > 0 && indent_depth <= 8 {
        let short = t.trim();
        if short.starts_with("pub fn ") || short.starts_with("fn ") || short.starts_with("def ") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_line_detection() {
        // Rust
        assert!(is_signature_line("pub fn foo() {}"));
        assert!(is_signature_line("struct Point {"));
        assert!(is_signature_line("impl Foo {"));
        assert!(is_signature_line("    fn private(&self) {}"));
        // TS/JS
        assert!(is_signature_line("export function bar() {"));
        assert!(is_signature_line("interface Baz {"));
        // Go
        assert!(is_signature_line("func handler(w http.ResponseWriter) {"));
        assert!(is_signature_line("type Reader interface {"));
        // Python
        assert!(is_signature_line("def main():"));
        // 非声明行
        assert!(!is_signature_line("let x = 1;"));
        assert!(!is_signature_line("return foo();"));
        assert!(!is_signature_line("// pub fn commented() {}"));
    }
}
