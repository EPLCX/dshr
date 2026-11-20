//! Skills：一个文件夹加一份 `SKILL.md`，教 agent 怎么做某一类事情。
//!
//! 每个 Skill 是 `<skills 目录>/<名字>/SKILL.md`，开头用 front matter 写名字和说明：
//!
//! ```text
//! ---
//! name: release-notes
//! description: 根据 git 历史写发布说明
//! ---
//! 正文：步骤、约定、可以引用同目录下的脚本和模板……
//! ```
//!
//! 系统提示词里只放各个 Skill 的名字、说明和路径；任务对得上时模型自己用 `read_file` 读正文，
//! 所以装很多 Skill 也不占上下文。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dshr_agent::{PromptContext, PromptContributor, PromptParts};
use dshr_context::{ExtensionPoint, Manifest, Plugin, Scope};

/// 工作区里放 Skills 的位置。后两个是其他 agent 工具的约定，已有的 Skill 可以直接用。
const WORKSPACE_DIRS: [&str; 3] = [".dshr/skills", ".agents/skills", ".claude/skills"];

/// 提示词里最多列这么多个 Skill，每条说明最多这么多字符。
const MAX_SKILLS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 300;
/// front matter 一定在文件开头，只读这么多字节就够了。
const HEADER_BYTES: usize = 4096;

const INTRO: &str = "Skills are folders of instructions for specific kinds of task. When the task matches a skill \
below, read its SKILL.md with read_file before you start and follow it; files it mentions are relative to its folder.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// `SKILL.md` 的路径。
    pub path: PathBuf,
}

/// 扩展点：插件自带的 Skills 文件夹（插件包的 `skills/`）。里面的 Skill 在提示词里叫 `<插件>:<名字>`。
pub struct SkillRoots;

impl ExtensionPoint for SkillRoots {
    type Item = Arc<SkillRoot>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoot {
    pub plugin: String,
    pub dir: PathBuf,
}

/// 把可用的 Skills 列进系统提示词：数据目录下的对所有工作区生效，工作区里的只对该工作区生效。
pub struct SkillsPlugin {
    home: PathBuf,
}

impl SkillsPlugin {
    /// `home`：dshr 的数据目录（`~/.dshr`），全局 Skills 在它的 `skills/` 下。
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }
}

impl Plugin for SkillsPlugin {
    fn manifest(&self) -> Manifest {
        Manifest::new("skills")
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        cx.contribute::<PromptParts>(Arc::new(Catalog { global: self.home.join("skills") }));
        // 全局 Skills 在数据目录里、不在工作区里：模型按提示词去读 `SKILL.md` 时不用询问。
        cx.contribute::<dshr_tools::ReadableRoots>(self.home.join("skills"));
        Ok(())
    }
}

struct Catalog {
    global: PathBuf,
}

impl PromptContributor for Catalog {
    fn contribute(&self, cx: &PromptContext<'_>) -> Option<String> {
        let mut skills = discover(&self.global, cx.workspace);
        let roots = cx.plugins.map(|plugins| plugins.items::<SkillRoots>()).unwrap_or_default();
        skills.extend(discover_plugin_skills(roots));
        if skills.is_empty() {
            return None;
        }
        let mut text = String::from(INTRO);
        for skill in skills.iter().take(MAX_SKILLS) {
            text.push_str(&format!("\n- {}: {} ({})", skill.name, skill.description, skill.path.display()));
        }
        Some(text)
    }
}

/// 全局与工作区里的 Skills，按名字排序；同名时工作区里的优先。
pub fn discover(global: &Path, workspace: &Path) -> Vec<Skill> {
    let mut skills = BTreeMap::new();
    let workspace_dirs = WORKSPACE_DIRS.iter().rev().map(|dir| workspace.join(dir));
    for dir in std::iter::once(global.to_owned()).chain(workspace_dirs) {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if let Some(skill) = load(&entry.path()) {
                skills.insert(skill.name.clone(), skill);
            }
        }
    }
    skills.into_values().collect()
}

/// 插件自带的 Skills：名字前面加上 `<插件>:`，不会和全局、工作区里的撞名。
pub fn discover_plugin_skills(roots: &[Arc<SkillRoot>]) -> Vec<Skill> {
    let mut skills = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root.dir) else { continue };
        let mut found: Vec<Skill> = entries.flatten().filter_map(|entry| load(&entry.path())).collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        skills.extend(found.into_iter().map(|skill| Skill { name: format!("{}:{}", root.plugin, skill.name), ..skill }));
    }
    skills
}

/// 读一个 Skill 文件夹；没有 `SKILL.md` 时返回 `None`。
fn load(dir: &Path) -> Option<Skill> {
    use std::io::Read as _;

    let path = dir.join("SKILL.md");
    let mut header = Vec::new();
    std::fs::File::open(&path).ok()?.take(HEADER_BYTES as u64).read_to_end(&mut header).ok()?;
    let header = String::from_utf8_lossy(&header);
    let (name, description) = parse_front_matter(&header);
    let folder = dir.file_name()?.to_string_lossy().into_owned();
    Some(Skill {
        name: name.unwrap_or(folder),
        description: description.unwrap_or_default().chars().take(MAX_DESCRIPTION_CHARS).collect(),
        path,
    })
}

/// 从开头的 `---` 块里取 `name` 和 `description`。只认单行的 `键: 值`，值两边的引号去掉；
/// 没有 front matter 时两个都是 `None`。
fn parse_front_matter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    for line in lines.take_while(|line| line.trim_end() != "---") {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim().trim_matches(['"', '\'']).trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "name" => name = Some(value.to_owned()),
            "description" => description = Some(value.to_owned()),
            _ => {}
        }
    }
    (name, description)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, folder: &str, content: &str) {
        let dir = root.join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parses_front_matter() {
        let text =
            "---\nname: \"release-notes\"\ndescription: 根据 git 历史写发布说明: 含破坏性变更\nlicense: MIT\n---\n# 正文\nname: 不算\n";
        assert_eq!(parse_front_matter(text), (Some("release-notes".into()), Some("根据 git 历史写发布说明: 含破坏性变更".into())));
        assert_eq!(parse_front_matter("# 只有正文\nname: x\n"), (None, None));
    }

    #[test]
    fn discovers_global_and_workspace_skills() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let global = home.path().join("skills");
        write_skill(&global, "notes", "---\nname: notes\ndescription: 全局的\n---\n");
        write_skill(&global, "plain", "没有 front matter，用文件夹名。\n");
        write_skill(&workspace.path().join(".claude/skills"), "notes", "---\nname: notes\ndescription: 工作区的\n---\n");
        write_skill(&workspace.path().join(".dshr/skills"), "deploy", "---\ndescription: 部署到测试环境\n---\n");
        // 不是 Skill 的文件夹跳过。
        std::fs::create_dir_all(global.join("empty")).unwrap();

        let skills = discover(&global, workspace.path());
        let summary: Vec<_> = skills.iter().map(|skill| (skill.name.as_str(), skill.description.as_str())).collect();
        assert_eq!(summary, [("deploy", "部署到测试环境"), ("notes", "工作区的"), ("plain", "")]);
        assert!(skills[1].path.starts_with(workspace.path()));
    }

    #[test]
    fn catalog_lists_skills_in_the_prompt() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let catalog = Catalog { global: home.path().join("skills") };
        let options = dshr_context::Options::new();
        let cx = PromptContext { workspace: workspace.path(), options: &options, plugins: None };
        assert_eq!(catalog.contribute(&cx), None);

        write_skill(&home.path().join("skills"), "notes", "---\nname: notes\ndescription: 写发布说明\n---\n");
        let text = catalog.contribute(&cx).unwrap();
        assert!(text.starts_with(INTRO));
        assert!(text.contains("\n- notes: 写发布说明 ("));
        assert!(text.contains("SKILL.md)"));
    }
}
