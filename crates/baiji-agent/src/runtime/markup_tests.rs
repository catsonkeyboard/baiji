use super::*;
use serde_json::json;

/// A real-world leak whose opening `invoke` line was lost upstream: the
/// parameters have no tool name, so this must not be executed.
const HEADLESS_SAMPLE: &str = r#"<parameter name="bash">cd /path/to/project && timeout 900 <build command> 2>&1 | tail -6</DSML parameter>
<parameter name="file_path">/path/to/file</DSML parameter>
<parameter name="intent">Build after rename</DSML parameter>
<parameter name="timeout">950000</DSML parameter>
</invoke>
</calls>"#;

#[test]
fn sample_without_invoke_header_is_unparseable_not_prose() {
    match scan_tool_call_markup(HEADLESS_SAMPLE) {
        MarkupScan::Unparseable { reason } => {
            assert!(reason.contains("outside of an invoke"), "{reason}")
        }
        other => panic!("expected Unparseable, got {other:?}"),
    }
}

#[test]
fn envelope_with_invoke_header_is_recovered() {
    let text = format!(
        "Building now.\n<calls>\n<invoke name=\"bash\">\n{}",
        HEADLESS_SAMPLE.replacen("parameter name=\"bash\"", "parameter name=\"command\"", 1)
    );
    let MarkupScan::Parsed {
        calls,
        sanitized_text,
    } = scan_tool_call_markup(&text)
    else {
        panic!("expected Parsed");
    };
    assert_eq!(sanitized_text, "Building now.");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "bash");
    assert_eq!(
        calls[0].arguments,
        json!({
            "command": "cd /path/to/project && timeout 900 <build command> 2>&1 | tail -6",
            "file_path": "/path/to/file",
            "intent": "Build after rename",
            "timeout": 950000,
        })
    );
}

#[test]
fn canonical_dsml_envelope_with_two_calls_is_recovered() {
    let text = "<｜DSML｜function_calls>\n<｜DSML｜invoke name=\"read\">\n<｜DSML｜parameter name=\"file_path\" string=\"true\">src/main.rs</｜DSML｜parameter>\n<｜DSML｜parameter name=\"limit\" string=\"false\">40</｜DSML｜parameter>\n</｜DSML｜invoke>\n<｜DSML｜invoke name=\"bash\">\n<｜DSML｜parameter name=\"command\" string=\"true\">echo 123</｜DSML｜parameter>\n</｜DSML｜invoke>\n</｜DSML｜function_calls>";
    let MarkupScan::Parsed {
        calls,
        sanitized_text,
    } = scan_tool_call_markup(text)
    else {
        panic!("expected Parsed");
    };
    assert!(sanitized_text.is_empty());
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "read");
    assert_eq!(
        calls[0].arguments,
        json!({"file_path": "src/main.rs", "limit": 40})
    );
    assert_eq!(calls[1].name, "bash");
    // string="true" keeps the value a string even when it looks numeric.
    assert_eq!(calls[1].arguments, json!({"command": "echo 123"}));
}

#[test]
fn string_hint_true_preserves_numeric_looking_text() {
    let text = "<invoke name=\"write\">\n<parameter name=\"content\" string=\"true\">42</parameter>\n</invoke>";
    let MarkupScan::Parsed { calls, .. } = scan_tool_call_markup(text) else {
        panic!("expected Parsed");
    };
    assert_eq!(calls[0].arguments, json!({"content": "42"}));
}

#[test]
fn parameter_body_containing_markup_is_kept_verbatim() {
    let text = "<invoke name=\"write\">\n<parameter name=\"content\" string=\"true\">see <invoke name=\"x\"> docs</parameter>\n</invoke>";
    let MarkupScan::Parsed { calls, .. } = scan_tool_call_markup(text) else {
        panic!("expected Parsed");
    };
    assert_eq!(
        calls[0].arguments,
        json!({"content": "see <invoke name=\"x\"> docs"})
    );
}

#[test]
fn markup_inside_code_fence_or_inline_code_is_ignored() {
    let fenced = "Example of the format:\n```xml\n<invoke name=\"bash\">\n<parameter name=\"command\">ls</parameter>\n</invoke>\n```\nThat is all.";
    assert_eq!(scan_tool_call_markup(fenced), MarkupScan::None);
    let inline = "Use `<invoke>` and `<parameter>` tags.";
    assert_eq!(scan_tool_call_markup(inline), MarkupScan::None);
}

#[test]
fn ordinary_prose_is_not_markup() {
    assert_eq!(
        scan_tool_call_markup("The build passed. Next I will invoke the tests."),
        MarkupScan::None
    );
    assert_eq!(scan_tool_call_markup("</invoke>"), MarkupScan::None);
}

#[test]
fn invoke_without_name_is_unparseable() {
    let text = "<invoke>\n<parameter name=\"command\">ls</parameter>\n</invoke>";
    assert!(matches!(
        scan_tool_call_markup(text),
        MarkupScan::Unparseable { .. }
    ));
}

#[test]
fn unterminated_parameter_is_unparseable() {
    let text = "<invoke name=\"bash\">\n<parameter name=\"command\">ls";
    assert!(matches!(
        scan_tool_call_markup(text),
        MarkupScan::Unparseable { .. }
    ));
}

#[test]
fn harmony_call_prefers_object_that_ends_the_text() {
    let (prefix, name, args, suffix) =
        parse_text_wrapped_tool_call("ok to=functions.bash {\"command\": \"ls\"}").unwrap();
    assert_eq!(prefix, "ok");
    assert_eq!(name, "bash");
    assert_eq!(args, json!({"command": "ls"}));
    assert!(suffix.is_empty());
    assert!(parse_text_wrapped_tool_call("to=functions. {}").is_none());
    assert!(parse_text_wrapped_tool_call("no marker here {\"a\":1}").is_none());
}

#[test]
fn unparsed_markup_notice_names_the_reason() {
    let notice = unparsed_markup_notice("unterminated invoke");
    assert!(notice.contains("unterminated invoke"));
    assert!(notice.contains("nothing was executed"));
}

#[test]
fn prose_explaining_the_formats_is_never_a_call() {
    // 行内代码里的 marker + 代码块里的 JSON：说明文字，不执行
    let explained = "Models sometimes leak `to=functions.bash` followed by JSON:\n```json\n{\"command\": \"rm -rf target\"}\n```\nThat is a bug.";
    assert!(parse_text_wrapped_tool_call(explained).is_none());
    // JSON 之后还有正文：不是泄漏的调用信封
    assert!(
        parse_text_wrapped_tool_call("Call to=functions.bash {\"command\": \"ls\"} and then read it.")
            .is_none()
    );
    // JSON 不紧跟工具名
    assert!(parse_text_wrapped_tool_call("to=functions.bash then {\"command\": \"ls\"}").is_none());
    // 只是提到标签名：不触发重发
    assert_eq!(
        scan_tool_call_markup("Use <parameter> tags inside an <invoke> block."),
        MarkupScan::None
    );
    // 行内代码整段排除（不只是紧邻反引号）
    assert_eq!(
        scan_tool_call_markup(
            "Write `<invoke name=\"bash\">` then `<parameter name=\"command\">ls</parameter>` and close it."
        ),
        MarkupScan::None
    );
}

#[test]
fn inline_code_ranges_pair_equal_backtick_runs() {
    let text = "a `x` b ``y ` z`` c `unclosed";
    let ranges = inline_code_ranges(text);
    assert_eq!(ranges.len(), 2);
    assert_eq!(&text[ranges[0].0..ranges[0].1], "`x`");
    assert_eq!(&text[ranges[1].0..ranges[1].1], "``y ` z``");
}
