//! `edit_file` 与文件改动报告（统一 diff）。

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

use crate::{Access, Tool, ToolContext, ToolOutput, parse, truncate, write_access};

/// diff 的时间上限。超过后算法退回近似结果（仍然是正确的 diff，只是不一定最短），
/// 整份重写大文件时不会卡住工具调用。
const DIFF_TIMEOUT: Duration = Duration::from_millis(500);

fn line_diff<'a>(before: &'a str, after: &'a str) -> TextDiff<'a, 'a, str> {
    TextDiff::configure().timeout(DIFF_TIMEOUT).diff_lines(before, after)
}

fn render_unified(diff: &TextDiff<'_, '_, str>, path: &str) -> String {
    diff.unified_diff().context_radius(3).header(&format!("a/{path}"), &format!("b/{path}")).to_string()
}

/// 统一 diff，前后各保留 3 行上下文。
pub fn unified_diff(path: &str, before: &str, after: &str) -> String {
    render_unified(&line_diff(before, after), path)
}

/// 改动报告：首行摘要 `Edited path (+a -r)`，空一行后是统一 diff。界面据此显示行数与着色的 diff。
/// 行数统计与 diff 文本出自同一次比较。
pub(crate) fn change_report(verb: &str, path: &str, before: &str, after: &str) -> String {
    let diff = line_diff(before, after);
    let (mut added, mut removed) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    format!("{verb} {path} (+{added} -{removed})\n\n{}", render_unified(&diff, path))
}

/// 在阻塞线程池里生成改动报告：比较大文件是纯 CPU 工作，不占用运行时的工作线程。
pub(crate) async fn change_report_blocking(verb: &'static str, path: String, before: String, after: String) -> String {
    let fallback = format!("{verb} {path}");
    tokio::task::spawn_blocking(move || change_report(verb, &path, &before, &after)).await.unwrap_or(fallback)
}

pub struct EditFile;

#[derive(Deserialize)]
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait::async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a text file. `old_string` must match the file exactly (including whitespace) \
         and be unique unless `replace_all` is true; include surrounding lines to make it unique. \
         Read the file first. Returns a unified diff of the change."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact text to replace."},
                "new_string": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "default": false}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn access(&self, input: &Value, ctx: &ToolContext) -> Access {
        write_access(input, ctx)
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let input: EditFileInput = match parse(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if input.old_string.is_empty() {
            return ToolOutput::error("old_string 不能为空；创建新文件请用 write_file");
        }
        if input.old_string == input.new_string {
            return ToolOutput::error("old_string 与 new_string 相同，没有要修改的内容");
        }
        let path = ctx.resolve(&input.path);
        let before = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) => return ToolOutput::error(format!("无法读取 {}: {error}", input.path)),
        };
        // CRLF 文件：`read_file` 给模型看的是去掉 `\r` 的文本，模型给的跨行 `old_string` 只有 `\n`，
        // 直接匹配永远找不到。按 LF 匹配、替换，写回时换回 CRLF，保持文件原来的换行。
        let crlf = crate::is_crlf(&before) && !input.old_string.contains('\r');
        let (haystack, old, new) = if crlf {
            (
                std::borrow::Cow::Owned(before.replace("\r\n", "\n")),
                input.old_string.replace("\r\n", "\n"),
                input.new_string.replace("\r\n", "\n"),
            )
        } else {
            (std::borrow::Cow::Borrowed(before.as_str()), input.old_string.clone(), input.new_string.clone())
        };
        let count = haystack.matches(&old).count();
        let after = match count {
            0 => return ToolOutput::error(format!("在 {} 中没有找到 old_string，请先读取文件确认原文", input.path)),
            1 => haystack.replacen(&old, &new, 1),
            _ if input.replace_all => haystack.replace(&old, &new),
            n => {
                return ToolOutput::error(format!(
                    "old_string 在 {} 中出现了 {n} 次；请带上更多上下文使其唯一，或设置 replace_all",
                    input.path
                ));
            }
        };
        drop(haystack);
        let after = if crlf { crate::to_crlf(&after) } else { after };
        // 插件的 `beforeWrite`：可以拒绝，也可以改写要写的内容（差异按最终内容算）。
        let (after, changed_by) = match crate::apply_write_intents(ctx, "edit_file", &input.path, Some(&before), after) {
            Ok(result) => result,
            Err(reason) => return ToolOutput::error(format!("没有写入 {}：{reason}", input.path)),
        };
        match crate::write_atomic(path, after.clone()).await {
            Ok(()) => {
                let report = change_report_blocking("Edited", input.path, before, after).await + &crate::adjusted_note(&changed_by);
                ToolOutput::ok(truncate(report))
            }
            Err(error) => ToolOutput::error(format!("无法写入 {}: {error}", input.path)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_owned());
        (dir, ctx)
    }

    #[tokio::test]
    async fn edits_unique_match_and_reports_diff() {
        let (dir, ctx) = ctx();
        std::fs::write(dir.path().join("a.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        let out = EditFile.execute(json!({"path": "a.rs", "old_string": "\"hi\"", "new_string": "\"hello\""}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("Edited a.rs (+1 -1)"));
        assert!(out.content.contains("-    println!(\"hi\");"));
        assert!(out.content.contains("+    println!(\"hello\");"));
        assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "fn main() {\n    println!(\"hello\");\n}\n");
    }

    #[tokio::test]
    async fn rejects_missing_and_ambiguous_matches() {
        let (dir, ctx) = ctx();
        std::fs::write(dir.path().join("b.txt"), "x\nx\n").unwrap();
        let missing = EditFile.execute(json!({"path": "b.txt", "old_string": "y", "new_string": "z"}), &ctx).await;
        assert!(missing.is_error);
        let ambiguous = EditFile.execute(json!({"path": "b.txt", "old_string": "x", "new_string": "z"}), &ctx).await;
        assert!(ambiguous.is_error && ambiguous.content.contains("2 次"));
        let all = EditFile.execute(json!({"path": "b.txt", "old_string": "x", "new_string": "z", "replace_all": true}), &ctx).await;
        assert!(!all.is_error);
        assert_eq!(std::fs::read_to_string(dir.path().join("b.txt")).unwrap(), "z\nz\n");
    }

    #[tokio::test]
    async fn edits_crlf_files_with_lf_strings_and_keeps_crlf() {
        let (dir, ctx) = ctx();
        let file = dir.path().join("w.cs");
        std::fs::write(&file, "class A {\r\n    int x;\r\n    int y;\r\n}\r\n").unwrap();
        // 模型从 read_file 看到的是 LF：跨行的 old_string 也要找得到。
        let out = EditFile
            .execute(json!({"path": "w.cs", "old_string": "int x;\n    int y;", "new_string": "int x;\n    int y;\n    int z;"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("Edited w.cs (+1 -0)"), "{}", out.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "class A {\r\n    int x;\r\n    int y;\r\n    int z;\r\n}\r\n");

        // 混用换行的文件不做转换，按原文精确匹配。
        std::fs::write(&file, "a\r\nb\nc\n").unwrap();
        let mixed = EditFile.execute(json!({"path": "w.cs", "old_string": "b\nc", "new_string": "B\nC"}), &ctx).await;
        assert!(!mixed.is_error, "{}", mixed.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\r\nB\nC\n");
    }
}
