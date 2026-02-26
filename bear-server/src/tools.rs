// ---------------------------------------------------------------------------
// Thin delegation layer: bear-server tool execution delegates to bear-core.
// ServerState implements bear_core::tools::ToolContext (see tool_bridge.rs).
// BusSender implements bear_core::tools::ToolBus (see tool_bridge.rs).
// ---------------------------------------------------------------------------

use uuid::Uuid;

use crate::state::{BusSender, PendingToolCall, ServerState};

// Re-export from bear-core so existing callers (ws.rs) keep working.
pub use bear_core::tools::parse_tool_calls;

// ---------------------------------------------------------------------------
// Thin wrappers that delegate to bear_core::tools via trait implementations
// ---------------------------------------------------------------------------

/// Execute a tool call, delegating to bear_core::tools::execute_tool.
pub async fn execute_tool(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    ptc: &PendingToolCall,
) -> String {
    bear_core::tools::execute_tool(state, bus, session_id, ptc).await
}

/// Execute a run_command tool call directly (used by ShellExec handler).
pub async fn execute_run_command(
    state: &ServerState,
    session_id: Uuid,
    bus: &BusSender,
    cmd_str: &str,
    cwd: &str,
) -> String {
    bear_core::tools::execute_run_command(state, bus, session_id, cmd_str, cwd).await
}

// ---------------------------------------------------------------------------
// Tests — exercise bear_core::tools functions via re-exports
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bear_core::tools::{apply_unified_diff, strip_html_tags, validate_tool_path};

    // -- parse_tool_calls --------------------------------------------------

    #[test]
    fn parse_single_tool_call() {
        let text = r#"Let me read that file.
[TOOL_CALL]{"name": "read_file", "arguments": {"path": "src/main.rs"}}[/TOOL_CALL]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "src/main.rs");
    }

    #[test]
    fn parse_multiple_tool_calls() {
        let text = r#"I'll read both files.
[TOOL_CALL]{"name": "read_file", "arguments": {"path": "a.rs"}}[/TOOL_CALL]
Then the second:
[TOOL_CALL]{"name": "read_file", "arguments": {"path": "b.rs"}}[/TOOL_CALL]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments["path"], "a.rs");
        assert_eq!(calls[1].arguments["path"], "b.rs");
    }

    #[test]
    fn parse_no_tool_calls() {
        let text = "Just a normal response with no tools.";
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_malformed_json_skipped() {
        let text = "[TOOL_CALL]{not valid json}[/TOOL_CALL]";
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_missing_end_tag() {
        let text = r#"[TOOL_CALL]{"name": "read_file", "arguments": {"path": "a.rs"}}"#;
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    // -- parse_tool_calls format 2: [tool_name]{args}[/tool_name] ----------

    #[test]
    fn parse_tool_name_tag_single() {
        let text = r#"Let me list files. [list_files]{"path": "."}[/list_files]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_files");
        assert_eq!(calls[0].arguments["path"], ".");
    }

    #[test]
    fn parse_tool_name_tag_multiple() {
        let text = r#"[read_file]{"path": "a.rs"}[/read_file] then [read_file]{"path": "b.rs"}[/read_file]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments["path"], "a.rs");
        assert_eq!(calls[1].arguments["path"], "b.rs");
    }

    #[test]
    fn parse_tool_name_tag_malformed_json() {
        let text = "[list_files]{bad json}[/list_files]";
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_name_tag_missing_close() {
        let text = r#"[list_files]{"path": "."}"#;
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_format1_takes_priority() {
        // If both formats are present, format 1 wins (format 2 only tried when format 1 finds nothing)
        let text = r#"[TOOL_CALL]{"name": "read_file", "arguments": {"path": "x"}}[/TOOL_CALL] [list_files]{"path": "."}[/list_files]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn parse_tool_name_tag_no_underscore_ignored() {
        // Tags without underscore should NOT be parsed as tool calls
        let text = r#"[bold]some text[/bold]"#;
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_name_tag_array_body_ignored() {
        // Body must be a JSON object, not an array
        let text = r#"[list_files][1,2,3][/list_files]"#;
        let calls = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_name_tag_run_command() {
        let text = r#"[run_command]{"command": "ls -la"}[/run_command]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_command");
        assert_eq!(calls[0].arguments["command"], "ls -la");
    }

    #[test]
    fn parse_tool_name_tag_with_surrounding_text() {
        let text = "Let me check.\n[read_file]{\"path\": \"src/main.rs\"}[/read_file]\nDone.";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn parse_tool_name_tag_nested_json() {
        let text =
            r#"[write_file]{"path": "a.json", "content": "{\"key\": \"val\"}"}[/write_file]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].arguments["path"], "a.json");
    }

    // -- parse_tool_calls malformed [TOOL_CALL{ (missing ]) ----------------

    #[test]
    fn parse_malformed_tool_call_missing_bracket() {
        // LLM omits the ] after TOOL_CALL — should still parse
        let text = r#"[TOOL_CALL{"name": "js_eval", "arguments": {"code": "2+3+5"}}[/TOOL_CALL]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "js_eval");
        assert_eq!(calls[0].arguments["code"], "2+3+5");
    }

    #[test]
    fn parse_malformed_tool_call_with_space() {
        // LLM puts a space: [TOOL_CALL {
        let text = r#"[TOOL_CALL {"name": "read_file", "arguments": {"path": "a.rs"}}[/TOOL_CALL]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
    }

    #[test]
    fn parse_well_formed_still_works() {
        // Ensure the well-formed variant still works after the fix
        let text = r#"[TOOL_CALL]{"name": "js_eval", "arguments": {"code": "1+1"}}[/TOOL_CALL]"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "js_eval");
    }

    // -- strip_html_tags ----------------------------------------------------

    #[test]
    fn strip_html_basic() {
        assert_eq!(strip_html_tags("<p>hello</p>"), "\nhello");
    }

    #[test]
    fn strip_html_entities() {
        assert_eq!(strip_html_tags("a &lt; b &amp; c &gt; d"), "a < b & c > d");
    }

    #[test]
    fn strip_html_script_removed() {
        assert_eq!(
            strip_html_tags("before<script>var x=1;</script>after"),
            "beforeafter"
        );
    }

    #[test]
    fn strip_html_multibyte_utf8() {
        // Regression: '·' is 2 bytes (0xC2 0xB7). The old code used char
        // indices as byte indices, panicking on multi-byte characters.
        let html = "<a>tao/examples/drag_window.rs at dev · tauri-apps/tao · github</a>";
        let result = strip_html_tags(html);
        assert!(result.contains("·"));
        assert!(result.contains("tauri-apps"));
    }

    #[test]
    fn strip_html_multibyte_in_text() {
        // Various multi-byte chars: '€' (3 bytes), '日' (3 bytes), emoji (4 bytes)
        let html = "<div>Price: €100</div><p>日本語</p>";
        let result = strip_html_tags(html);
        assert!(result.contains("€100"));
        assert!(result.contains("日本語"));
    }

    #[test]
    fn strip_html_multibyte_near_tags() {
        let html = "café<br>résumé";
        let result = strip_html_tags(html);
        assert!(result.contains("café"));
        assert!(result.contains("résumé"));
    }

    // -- resolve_path / validate_tool_path ---------------------------------

    #[test]
    fn resolve_absolute_path() {
        let result = validate_tool_path("/tmp/foo.txt", "/home/user");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/tmp/foo.txt");
    }

    #[test]
    fn resolve_relative_path() {
        let result = validate_tool_path("src/main.rs", "/home/user/project");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/home/user/project/src/main.rs");
    }

    #[test]
    fn resolve_parent_dir_references() {
        let result = validate_tool_path("../sibling/file.rs", "/home/user/project");
        // Relative path escaping cwd should be blocked
        assert!(result.is_err());
    }

    #[test]
    fn resolve_dot_references() {
        let result = validate_tool_path("./src/../src/main.rs", "/home/user/project");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/home/user/project/src/main.rs");
    }

    #[test]
    fn resolve_multiple_parent_refs() {
        let result = validate_tool_path("../../file.txt", "/a/b/c");
        assert!(result.is_err());
    }

    #[test]
    fn validate_relative_within_cwd() {
        let result = validate_tool_path("src/main.rs", "/home/user/project");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/home/user/project/src/main.rs");
    }

    #[test]
    fn validate_relative_escaping_cwd_blocked() {
        let result = validate_tool_path("../../etc/passwd", "/home/user/project");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("outside the working directory"));
    }

    #[test]
    fn validate_absolute_path_allowed() {
        // Absolute paths are allowed (user/LLM may reference /tmp, etc.)
        let result = validate_tool_path("/tmp/test.txt", "/home/user/project");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/tmp/test.txt");
    }

    #[test]
    fn validate_empty_path_rejected() {
        let result = validate_tool_path("", "/home/user/project");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must not be empty"));
    }

    #[test]
    fn validate_dot_path_is_cwd() {
        let result = validate_tool_path(".", "/home/user/project");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/home/user/project");
    }

    // -- apply_unified_diff ------------------------------------------------

    #[test]
    fn diff_add_line() {
        let original = "line1\nline2\nline3\n";
        let diff = "@@ -2,1 +2,2 @@\n line2\n+inserted\n line3\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "line1\nline2\ninserted\nline3\n");
    }

    #[test]
    fn diff_remove_line() {
        let original = "line1\nline2\nline3\n";
        let diff = "@@ -1,3 +1,2 @@\n line1\n-line2\n line3\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "line1\nline3\n");
    }

    #[test]
    fn diff_replace_line() {
        let original = "aaa\nbbb\nccc\n";
        let diff = "@@ -1,3 +1,3 @@\n aaa\n-bbb\n+BBB\n ccc\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "aaa\nBBB\nccc\n");
    }

    #[test]
    fn diff_with_header_lines() {
        let original = "hello\nworld\n";
        let diff = "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n hello\n-world\n+universe\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "hello\nuniverse\n");
    }

    #[test]
    fn diff_off_by_few_lines() {
        // LLM claims hunk starts at line 5, but the matching context is at line 8.
        // Fuzzy matching should find it.
        let original = "a\nb\nc\nd\ne\nf\ng\ntarget_line\nh\ni\n";
        let diff = "@@ -5,3 +5,3 @@\n g\n-target_line\n+replaced_line\n h\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "a\nb\nc\nd\ne\nf\ng\nreplaced_line\nh\ni\n");
    }

    #[test]
    fn diff_multi_hunk() {
        let original = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let diff = "\
@@ -1,3 +1,3 @@
 a
-b
+B
 c
@@ -6,3 +6,3 @@
 f
-g
+G
 h
";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "a\nB\nc\nd\ne\nf\nG\nh\n");
    }

    #[test]
    fn diff_multi_hunk_off_by_one() {
        // Second hunk line number is off by 1 (claims 7, actual match at 6)
        let original = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let diff = "\
@@ -1,3 +1,3 @@
 a
-b
+B
 c
@@ -7,3 +7,3 @@
 f
-g
+G
 h
";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "a\nB\nc\nd\ne\nf\nG\nh\n");
    }

    #[test]
    fn diff_context_mismatch_returns_error() {
        // Context lines don't match anything in the file
        let original = "aaa\nbbb\nccc\n";
        let diff = "@@ -1,3 +1,3 @@\n xxx\n-yyy\n+zzz\n ccc\n";
        let result = apply_unified_diff(original, diff);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("could not find matching lines"));
    }

    #[test]
    fn diff_no_hunks_returns_error() {
        let original = "hello\n";
        let diff = "just some random text with no @@ headers";
        let result = apply_unified_diff(original, diff);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No hunks found"));
    }

    #[test]
    fn diff_large_file_off_by_many() {
        // Simulate a 100-line file where LLM is off by 10 lines
        let mut lines: Vec<String> = (1..=100).map(|i| format!("line_{i}")).collect();
        let original = lines.join("\n") + "\n";
        // Target is at line 50, but LLM claims line 40
        let diff = "@@ -40,3 +40,3 @@\n line_49\n-line_50\n+line_50_modified\n line_51\n";
        let result = apply_unified_diff(&original, diff).unwrap();
        lines[49] = "line_50_modified".to_string();
        let expected = lines.join("\n") + "\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn diff_bare_empty_lines_in_context() {
        // LLMs sometimes emit empty lines without the leading space
        let original = "fn main() {\n    println!(\"hello\");\n\n    println!(\"world\");\n}\n";
        let diff = "@@ -1,5 +1,5 @@\n fn main() {\n-    println!(\"hello\");\n+    println!(\"hi\");\n\n     println!(\"world\");\n }\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(
            result,
            "fn main() {\n    println!(\"hi\");\n\n    println!(\"world\");\n}\n"
        );
    }

    #[test]
    fn diff_pure_insertion_hunk() {
        let original = "line1\nline2\nline3\n";
        let diff = "@@ -2,0 +3,1 @@\n+inserted\n";
        let result = apply_unified_diff(original, diff).unwrap();
        // Pure insertion at line 2 — should insert after line 2
        assert!(result.contains("inserted"));
    }

    #[test]
    fn diff_trailing_whitespace_tolerant() {
        // LLM emits context lines without trailing whitespace that exists in the file.
        // The fuzzy matcher should handle this via trailing-whitespace-trimmed fallback.
        let original = "fn foo() {  \n    bar();\n}\n";
        let diff = "@@ -1,3 +1,3 @@\n fn foo() {\n-    bar();\n+    baz();\n }\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "fn foo() {  \n    baz();\n}\n");
    }

    #[test]
    fn diff_tab_vs_spaces_mismatch() {
        // File uses tabs, LLM emits spaces in context
        let original = "fn foo() {\n\tbar();\n\tbaz();\n}\n";
        let diff = "@@ -1,4 +1,4 @@\n fn foo() {\n-    bar();\n+    qux();\n     baz();\n }\n";
        // Context uses spaces but file has tabs — should fail
        let result = apply_unified_diff(original, diff);
        assert!(
            result.is_err(),
            "Should fail on tab/space mismatch, got: {:?}",
            result
        );
    }

    #[test]
    fn diff_header_lines_not_at_start() {
        // LLM emits some text before --- / +++ headers
        let original = "aaa\nbbb\nccc\n";
        let diff = "Here is the diff:\n--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n aaa\n-bbb\n+BBB\n ccc\n";
        let result = apply_unified_diff(original, diff);
        // The "Here is the diff:" line is not --- or +++, so the initial skip loop
        // breaks immediately. Then the main loop skips it (not @@). Should still work.
        assert!(
            result.is_ok(),
            "Header not at start failed: {:?}",
            result.unwrap_err()
        );
        assert_eq!(result.unwrap(), "aaa\nBBB\nccc\n");
    }

    #[test]
    fn diff_removal_line_starts_with_triple_dash() {
        // What if the LLM's diff has a line being removed that starts with "---"?
        // The initial skip loop would consume it as a header line!
        let original = "first\n--- old separator ---\nlast\n";
        let diff =
            "@@ -1,3 +1,3 @@\n first\n---- old separator ---\n+--- new separator ---\n last\n";
        let result = apply_unified_diff(original, diff);
        assert!(
            result.is_ok(),
            "Triple-dash removal failed: {:?}",
            result.unwrap_err()
        );
        assert_eq!(result.unwrap(), "first\n--- new separator ---\nlast\n");
    }

    #[test]
    fn diff_large_hunk_exceeds_search_window() {
        // Reproduce: 120-line file, hunk at line 1 with all lines as context/remove.
        let lines: Vec<String> = (1..=120).map(|i| format!("line_{i}")).collect();
        let original = lines.join("\n") + "\n";

        // Build a diff that replaces line_60 with line_60_new, with ALL other lines as context
        let mut diff = String::from("@@ -1,120 +1,120 @@\n");
        for i in 1..=120 {
            if i == 60 {
                diff.push_str(&format!("-line_{i}\n"));
                diff.push_str("+line_60_new\n");
            } else {
                diff.push_str(&format!(" line_{i}\n"));
            }
        }

        let result = apply_unified_diff(&original, &diff);
        assert!(
            result.is_ok(),
            "Large hunk failed: {:?}",
            result.unwrap_err()
        );
        let patched = result.unwrap();
        assert!(patched.contains("line_60_new"));
        assert!(!patched.contains("\nline_60\n"));
    }

    #[test]
    fn diff_full_file_replacement() {
        // LLM replaces entire file content via a single hunk — all old lines are
        // removed and all new lines are added.
        let original = "\
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use log::info;
use browser::{html, css, renderer, js};
use std::fs;
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args: Vec<String> = env::args().collect();
    let url = args.get(1).map(|s| s.as_str()).unwrap_or(\"about:blank\");
    info!(\"Loading {url}\");
    Ok(())
}
";
        let diff = "\
@@ -1,18 +1,25 @@
 use tao::{
     dpi::LogicalSize,
     event::{Event, WindowEvent},
     event_loop::{ControlFlow, EventLoop},
     window::WindowBuilder,
 };
-use log::info;
-use browser::{html, css, renderer, js};
+use log::{info, debug};
+use browser::{html, css, renderer, js, layout};
 use std::fs;
 use std::env;
+use std::path::PathBuf;
 
 fn main() -> Result<(), Box<dyn std::error::Error>> {
     env_logger::init();
     let args: Vec<String> = env::args().collect();
     let url = args.get(1).map(|s| s.as_str()).unwrap_or(\"about:blank\");
     info!(\"Loading {url}\");
+    debug!(\"Starting render\");
+    let doc = html::parse(\"<html></html>\");
+    let style = css::parse(\"\");
+    let tree = layout::build(&doc, &style);
+    renderer::paint(&tree);
+    debug!(\"Render complete\");
     Ok(())
 }
";
        let result = apply_unified_diff(original, diff);
        assert!(
            result.is_ok(),
            "Full file replacement failed: {:?}",
            result.unwrap_err()
        );
        let patched = result.unwrap();
        assert!(patched.contains("use log::{info, debug};"));
        assert!(patched.contains("layout::build"));
        assert!(patched.contains("debug!(\"Render complete\");"));
        assert!(!patched.contains("use log::info;"));
    }

    #[test]
    fn diff_line_content_starts_with_minus() {
        // A line in the file starts with '-', which could confuse the parser
        let original = "header\n- item one\n- item two\nfooter\n";
        let diff = "@@ -1,4 +1,4 @@\n header\n-- item one\n+- item ONE\n - item two\n footer\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "header\n- item ONE\n- item two\nfooter\n");
    }

    #[test]
    fn diff_line_content_starts_with_plus() {
        // A line in the file starts with '+', which could confuse the parser
        let original = "header\n+ item one\n+ item two\nfooter\n";
        let diff = "@@ -1,4 +1,4 @@\n header\n-+ item one\n++ item ONE\n + item two\n footer\n";
        let result = apply_unified_diff(original, diff).unwrap();
        assert_eq!(result, "header\n+ item ONE\n+ item two\nfooter\n");
    }
}
