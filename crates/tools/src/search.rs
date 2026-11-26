//! `grep` 与 `glob`：遍历工作区时遵守 `.gitignore`，跳过二进制和过大的文件。
//!
//! 两者都用 `ignore` 的并行遍历（每个 CPU 一个线程），`grep` 的匹配用 ripgrep 的内核（`grep-searcher`）：
//! 整块缓冲区里找匹配、只给命中的行算行号，而不是逐行跑正则。结果最后排序，输出与遍历顺序无关。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::{WalkBuilder, WalkState};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Access, Tool, ToolContext, ToolOutput, parse, truncate};

const DEFAULT_MAX_RESULTS: usize = 200;
/// `grep` 最多收集这么多行再排序截断；超过时提前结束遍历（这时取到的前几条不保证是全局最前面的）。
const COLLECT_LIMIT: usize = 20_000;
/// 超过这个大小的文件不搜索内容。
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// `max_results` 的上限：模型给多大都按这个算（不然 [`COLLECT_LIMIT`] 的提前结束形同虚设）。
const MAX_RESULTS_LIMIT: usize = 2_000;
/// 每条结果最多保留的字符数。压缩过的 JS、一行的数据文件命中一次就是几 MB，不截的话收集时占满内存，
/// 输出也只剩这一行。
const MAX_LINE_CHARS: usize = 500;

/// 一行命中的文本：去掉行尾空白，过长的只留开头。
fn clip_line(line: &str) -> String {
    let line = line.trim_end();
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => format!("{}… [这一行太长，只显示前 {MAX_LINE_CHARS} 个字符]", &line[..cut]),
        None => line.to_owned(),
    }
}

fn walker(root: &Path) -> ignore::WalkParallel {
    // 隐藏文件也搜索（如 .github），但仍遵守 .gitignore，并总是跳过 .git 目录。
    WalkBuilder::new(root).hidden(false).filter_entry(|entry| entry.file_name() != ".git").build_parallel()
}

/// 显示用的相对路径，统一用 `/` 分隔。
fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// 遍历在阻塞线程里进行，丢弃工具调用的 future（用户点了停止）并不会让它停下来。
/// 这个守卫随 future 一起析构并置位标记，遍历每处理一个文件检查一次，及时退出。
struct StopOnDrop(Arc<AtomicBool>);

impl StopOnDrop {
    fn new() -> (Self, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        (Self(flag.clone()), flag)
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn limit_note(shown: usize, truncated: bool) -> String {
    if truncated { format!("\n… 只显示前 {shown} 条结果，请缩小范围") } else { String::new() }
}

pub struct Grep;

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    /// 只搜索匹配这个模式的文件，如 `*.rs`。
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    max_results: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression. Respects .gitignore and skips binary files. \
         Returns `path:line: text` for each matching line."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression (Rust regex syntax)."},
                "path": {"type": "string", "description": "Directory or file to search, default the workspace."},
                "glob": {"type": "string", "description": "Only search files whose path matches, e.g. `*.rs`."},
                "case_insensitive": {"type": "boolean", "default": false},
                "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS_LIMIT, "default": DEFAULT_MAX_RESULTS}
            },
            "required": ["pattern"]
        })
    }

    fn access(&self, input: &Value, ctx: &ToolContext) -> Access {
        ctx.read_access(input.get("path").and_then(Value::as_str).unwrap_or("."))
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let input: GrepInput = match parse(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        // 设了行结束符：匹配不会跨行（如 `\s` 不匹配换行），和逐行匹配的语义一致。
        let matcher = match LineSearch::matcher(&input.pattern, input.case_insensitive) {
            Ok(matcher) => matcher,
            Err(error) => return ToolOutput::error(format!("正则表达式无效: {error}")),
        };
        let filter = match input.glob.as_deref().map(globset::Glob::new).transpose() {
            Ok(glob) => glob.map(|glob| glob.compile_matcher()),
            Err(error) => return ToolOutput::error(format!("glob 无效: {error}")),
        };
        let root = ctx.resolve(input.path.as_deref().unwrap_or("."));
        let workspace = ctx.workspace.clone();
        let max = input.max_results.unwrap_or(DEFAULT_MAX_RESULTS).clamp(1, MAX_RESULTS_LIMIT);

        let (_stop_guard, stopped) = StopOnDrop::new();
        let result = tokio::task::spawn_blocking(move || {
            let search = LineSearch { matcher: &matcher, hidden: true, per_file: max, stop: Some(&stopped) };
            let keep = |relative: &str, path: &Path| match &filter {
                Some(filter) => filter.is_match(relative) || path.file_name().is_some_and(|name| filter.is_match(name)),
                None => true,
            };
            let mut found = search.run(&root, &workspace, &keep, clip_line);
            found.sort();
            let truncated = found.len() > max;
            found.truncate(max);
            let lines: Vec<String> = found.into_iter().map(|(path, number, line)| format!("{path}:{number}: {line}")).collect();
            (lines, truncated)
        })
        .await;

        match result {
            Ok((lines, _)) if lines.is_empty() => ToolOutput::ok("没有匹配的结果"),
            Ok((lines, truncated)) => ToolOutput::ok(truncate(lines.join("\n") + &limit_note(lines.len(), truncated))),
            Err(error) => ToolOutput::error(format!("搜索失败: {error}")),
        }
    }
}

/// 按正则逐行搜索一个目录（`grep` 工具和插件的 `host.grep` 共用）。
///
/// 并行遍历（遵守 `.gitignore`，总是跳过 `.git`），每个文件用 ripgrep 的内核（`grep-searcher`）整块搜索，
/// 跳过二进制文件和超过 2 MB 的文件。返回的结果没有排序：调用方按自己的规则排好再截断。
pub struct LineSearch<'a> {
    pub matcher: &'a RegexMatcher,
    /// 也搜隐藏文件（`.github` 之类）。
    pub hidden: bool,
    /// 一个文件里最多收集多少行。
    pub per_file: usize,
    /// 置位后尽快停下（工具调用被取消）。
    pub stop: Option<&'a AtomicBool>,
}

pub use grep_regex::RegexMatcher;

impl LineSearch<'_> {
    /// 按 `grep` 工具的规则建正则：不区分大小写可选，匹配不跨行。
    pub fn matcher(pattern: &str, case_insensitive: bool) -> Result<RegexMatcher, String> {
        RegexMatcherBuilder::new()
            .case_insensitive(case_insensitive)
            .line_terminator(Some(b'\n'))
            .build(pattern)
            .map_err(|error| error.to_string())
    }

    /// 在 `root` 下搜索：（相对 `base` 的路径（`/` 分隔），行号，`clip` 处理过的行）。`keep(相对路径, 完整路径)`
    /// 为 `false` 的文件不搜。最多收集约 [`COLLECT_LIMIT`] 条就提前结束（这时不保证是排序后最前面的那些）。
    pub fn run(
        &self,
        root: &Path,
        base: &Path,
        keep: &(dyn Fn(&str, &Path) -> bool + Sync),
        clip: fn(&str) -> String,
    ) -> Vec<(String, u64, String)> {
        // 不在凑够条数时就停：并行遍历的先后不固定，要全部找完、排序后取前面的，输出才稳定。
        let found: Mutex<Vec<(String, u64, String)>> = Mutex::default();
        let count = AtomicUsize::new(0);
        let limit = self.per_file.max(COLLECT_LIMIT);
        let (matcher, per_file, stop) = (self.matcher, self.per_file, self.stop);
        WalkBuilder::new(root).hidden(!self.hidden).filter_entry(|entry| entry.file_name() != ".git").build_parallel().run(|| {
            let (found, count) = (&found, &count);
            let mut searcher = SearcherBuilder::new().binary_detection(BinaryDetection::quit(0)).line_number(true).build();
            Box::new(move |entry| {
                if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) || count.load(Ordering::Relaxed) > limit {
                    return WalkState::Quit;
                }
                let Ok(entry) = entry else { return WalkState::Continue };
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    return WalkState::Continue;
                }
                let path = entry.path();
                let relative = display_path(path, base);
                if !keep(&relative, path) || entry.metadata().is_ok_and(|meta| meta.len() > MAX_FILE_BYTES) {
                    return WalkState::Continue;
                }
                let mut local = Vec::new();
                // 读不了的文件、半路发现是二进制的文件都跳过（已经找到的行保留）。
                let _ = searcher.search_path(
                    matcher,
                    path,
                    Lossy(|number, line| {
                        local.push((relative.clone(), number, clip(line)));
                        Ok(local.len() <= per_file)
                    }),
                );
                if !local.is_empty() {
                    count.fetch_add(local.len(), Ordering::Relaxed);
                    found.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).extend(local);
                }
                WalkState::Continue
            })
        });
        found.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct Glob;

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    path: Option<String>,
}

const MAX_GLOB_RESULTS: usize = 500;

#[async_trait::async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern relative to the workspace, e.g. `src/**/*.rs` or `**/Cargo.toml`. \
         Respects .gitignore."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Directory to search, default the workspace."}
            },
            "required": ["pattern"]
        })
    }

    fn access(&self, input: &Value, ctx: &ToolContext) -> Access {
        ctx.read_access(input.get("path").and_then(Value::as_str).unwrap_or("."))
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let input: GlobInput = match parse(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let matcher = match globset::GlobBuilder::new(&input.pattern).literal_separator(true).build() {
            Ok(glob) => glob.compile_matcher(),
            Err(error) => return ToolOutput::error(format!("glob 无效: {error}")),
        };
        let root: PathBuf = ctx.resolve(input.path.as_deref().unwrap_or("."));
        let (_stop_guard, stopped) = StopOnDrop::new();
        let result = tokio::task::spawn_blocking(move || {
            let found: Mutex<Vec<String>> = Mutex::default();
            walker(&root).run(|| {
                let (matcher, root, stopped, found) = (&matcher, &root, &stopped, &found);
                Box::new(move |entry| {
                    if stopped.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    }
                    let Ok(entry) = entry else { return WalkState::Continue };
                    if entry.file_type().is_some_and(|kind| kind.is_file()) {
                        let relative = display_path(entry.path(), root);
                        if matcher.is_match(&relative) {
                            found.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(relative);
                        }
                    }
                    WalkState::Continue
                })
            });
            let mut paths = found.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
            paths.sort();
            paths
        })
        .await;
        match result {
            Ok(paths) if paths.is_empty() => ToolOutput::ok("没有匹配的文件"),
            Ok(paths) => {
                let truncated = paths.len() > MAX_GLOB_RESULTS;
                let shown: Vec<_> = paths.into_iter().take(MAX_GLOB_RESULTS).collect();
                ToolOutput::ok(truncate(shown.join("\n") + &limit_note(shown.len(), truncated)))
            }
            Err(error) => ToolOutput::error(format!("查找失败: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    hello();\n}\n").unwrap();
        std::fs::write(root.join("src/nested/lib.rs"), "pub fn hello() {}\n").unwrap();
        std::fs::write(root.join("target/out.rs"), "fn hello() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "Hello world\n").unwrap();
        // ignore 只在 git 仓库里读取 .gitignore。
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let ctx = ToolContext::new(root.to_owned());
        (dir, ctx)
    }

    #[tokio::test]
    async fn grep_clips_long_lines_and_caps_max_results() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(dir.path().to_owned());
        // 压缩过的一行：命中后只留开头。
        std::fs::write(dir.path().join("min.js"), format!("{}needle{}\n", "a".repeat(10), "b".repeat(1_000_000))).unwrap();
        let out = Grep.execute(json!({"pattern": "needle"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.len() < 2_000 && out.content.contains("这一行太长"), "{}", out.content.len());

        // max_results 给得再大也按上限算。
        let lines: String = (0..3_000).map(|n| format!("hit {n}\n")).collect();
        std::fs::write(dir.path().join("many.txt"), lines).unwrap();
        let out = Grep.execute(json!({"pattern": "^hit", "max_results": 1_000_000_000u64}), &ctx).await;
        let shown = out.content.matches("many.txt:").count();
        assert!(shown > 0 && shown <= MAX_RESULTS_LIMIT, "{shown}");
    }

    #[tokio::test]
    async fn grep_respects_gitignore_and_filters() {
        let (_dir, ctx) = workspace();
        let out = Grep.execute(json!({"pattern": "hello"}), &ctx).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/main.rs:2:     hello();"));
        assert!(out.content.contains("src/nested/lib.rs:1: pub fn hello() {}"));
        assert!(!out.content.contains("target/"));
        assert!(!out.content.contains("README"));

        let insensitive = Grep.execute(json!({"pattern": "hello", "case_insensitive": true, "glob": "*.md"}), &ctx).await;
        assert_eq!(insensitive.content, "README.md:1: Hello world");

        let limited = Grep.execute(json!({"pattern": "hello", "max_results": 1}), &ctx).await;
        assert!(limited.content.contains("只显示前 1 条"));

        assert!(Grep.execute(json!({"pattern": "("}), &ctx).await.is_error);
    }

    #[tokio::test]
    async fn grep_matches_line_by_line_and_sorts_results() {
        let (dir, ctx) = workspace();
        std::fs::write(dir.path().join("src/split.txt"), "hello\nworld\nhello world\n").unwrap();
        // 不跨行匹配：只有第 3 行。
        let out = Grep.execute(json!({"pattern": r"hello\s+world"}), &ctx).await;
        assert_eq!(out.content, "src/split.txt:3: hello world");
        // 并行遍历，但结果按路径、行号排好，截断时取的也总是前面那几条。
        let all = Grep.execute(json!({"pattern": "hello", "case_insensitive": true}), &ctx).await;
        let lines: Vec<&str> = all.content.lines().collect();
        let mut sorted = lines.clone();
        sorted.sort();
        assert_eq!(lines, sorted);
        let first_two = Grep.execute(json!({"pattern": "hello", "case_insensitive": true, "max_results": 2}), &ctx).await;
        assert!(first_two.content.starts_with(&format!("{}\n{}\n", lines[0], lines[1])), "{}", first_two.content);
        assert!(first_two.content.contains("只显示前 2 条"));
    }

    #[tokio::test]
    async fn glob_matches_relative_paths() {
        let (_dir, ctx) = workspace();
        let out = Glob.execute(json!({"pattern": "src/**/*.rs"}), &ctx).await;
        assert_eq!(out.content, "src/main.rs\nsrc/nested/lib.rs");
        let none = Glob.execute(json!({"pattern": "*.py"}), &ctx).await;
        assert_eq!(none.content, "没有匹配的文件");
    }

    #[test]
    fn line_search_respects_hidden_filter_and_per_file_limit() {
        let (dir, _ctx) = workspace();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".github")).unwrap();
        std::fs::write(root.join(".github/ci.yml"), "hello: ci\n").unwrap();
        std::fs::write(root.join("src/many.txt"), "hello 1\nhello 2\nhello 3\n").unwrap();
        let matcher = LineSearch::matcher("HELLO", true).unwrap();
        let run = |hidden: bool, per_file: usize, keep: &(dyn Fn(&str, &Path) -> bool + Sync)| {
            let mut found = LineSearch { matcher: &matcher, hidden, per_file, stop: None }.run(root, root, keep, str::to_owned);
            found.sort();
            found.into_iter().map(|(path, line, _)| format!("{path}:{line}")).collect::<Vec<_>>()
        };
        let all = |_: &str, _: &Path| true;
        // 隐藏文件按选项决定；.gitignore 忽略的 target/ 总不搜。
        assert_eq!(
            run(true, 10, &all),
            [
                ".github/ci.yml:1",
                "README.md:1",
                "src/main.rs:2",
                "src/many.txt:1",
                "src/many.txt:2",
                "src/many.txt:3",
                "src/nested/lib.rs:1"
            ]
        );
        assert!(!run(false, 10, &all).iter().any(|hit| hit.starts_with(".github")));
        // 过滤按相对路径；一个文件里收集到上限多一条就停。
        let txt = |relative: &str, _: &Path| relative.ends_with(".txt");
        assert_eq!(run(true, 1, &txt), ["src/many.txt:1", "src/many.txt:2"]);
        assert!(LineSearch::matcher("(", false).is_err());
    }
}

/// 输入框 `@` 提及里找工作区文件：遵守 `.gitignore`，跳过 `.git`。`query` 按不区分大小写的子序列匹配路径，
/// 文件名里连续出现的排在前面，其次是路径里连续出现的，再其次是按顺序出现的；返回前 `limit` 个。
///
/// 每打一个字都要找一次：工作区的文件列表遍历一次后缓存 [`FILE_INDEX_TTL`]，这期间的查询只在内存里过滤，
/// 不再每次把整个工作区走一遍。
pub fn find_files(root: &Path, query: &str, limit: usize) -> Vec<String> {
    let index = FileIndex::cached(root);
    let query = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &str)> = Vec::new();
    for (path, lower) in &index.files {
        let name = lower.rsplit('/').next().unwrap_or(lower);
        let rank = if query.is_empty() || name.contains(&query) {
            0
        } else if lower.contains(&query) {
            1
        } else if is_subsequence(&query, lower) {
            2
        } else {
            continue;
        };
        scored.push((rank, path.len(), path));
    }
    scored.sort_unstable();
    scored.into_iter().take(limit).map(|(_, _, path)| path.to_owned()).collect()
}

/// 文件列表缓存多久。新建的文件最多晚这么久出现在 `@` 菜单里。
const FILE_INDEX_TTL: std::time::Duration = std::time::Duration::from_secs(10);
/// 一个工作区最多记这么多个文件（只数文件，不数目录）。
const MAX_INDEXED_FILES: usize = 200_000;
/// 最多缓存几个工作区的文件列表。
const MAX_CACHED_INDEXES: usize = 8;

/// 一个工作区的文件：（显示用的相对路径，它的小写形式）。
struct FileIndex {
    built: std::time::Instant,
    files: Vec<(String, String)>,
}

impl FileIndex {
    /// 这个工作区的文件列表：没有或过期了就重新遍历（不持锁遍历，同时来的查询各自等自己的结果）。
    fn cached(root: &Path) -> Arc<Self> {
        static INDEXES: std::sync::LazyLock<Mutex<std::collections::HashMap<PathBuf, Arc<FileIndex>>>> =
            std::sync::LazyLock::new(Default::default);
        let lock = || INDEXES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(index) = lock().get(root).filter(|index| index.built.elapsed() < FILE_INDEX_TTL) {
            return index.clone();
        }
        let index = Arc::new(Self::build(root));
        let mut indexes = lock();
        indexes.retain(|_, index| index.built.elapsed() < FILE_INDEX_TTL);
        if indexes.len() >= MAX_CACHED_INDEXES {
            indexes.clear();
        }
        indexes.insert(root.to_owned(), index.clone());
        index
    }

    fn build(root: &Path) -> Self {
        let found: Mutex<Vec<(String, String)>> = Mutex::default();
        let count = AtomicUsize::new(0);
        walker(root).run(|| {
            let (found, count) = (&found, &count);
            Box::new(move |entry| {
                if count.load(Ordering::Relaxed) >= MAX_INDEXED_FILES {
                    return WalkState::Quit;
                }
                let Ok(entry) = entry else { return WalkState::Continue };
                if entry.file_type().is_some_and(|kind| kind.is_file()) {
                    let path = display_path(entry.path(), root);
                    let lower = path.to_lowercase();
                    count.fetch_add(1, Ordering::Relaxed);
                    found.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((path, lower));
                }
                WalkState::Continue
            })
        });
        let mut files = found.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
        files.truncate(MAX_INDEXED_FILES);
        Self { built: std::time::Instant::now(), files }
    }
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|wanted| chars.any(|c| c == wanted))
}

#[cfg(test)]
mod find_files_tests {
    use super::*;

    #[test]
    fn finds_files_by_name_then_path_then_subsequence() {
        let dir = tempfile::tempdir().unwrap();
        for path in ["src/main.rs", "src/lib.rs", "docs/maintenance.md", "target/x/main.o", ".git/HEAD"] {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, "x").unwrap();
        }
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        // `.gitignore` 只在 git 仓库里生效：建一个空的 `.git`（上面已经有了）。
        let found = find_files(dir.path(), "main", 10);
        assert_eq!(found, ["src/main.rs", "docs/maintenance.md"]);
        assert_eq!(find_files(dir.path(), "slb", 10), ["src/lib.rs"]);
        assert!(find_files(dir.path(), "", 2).len() == 2);
    }
}
