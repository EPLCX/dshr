//! 插件权限给人看的说法：“设置 → 扩展”的卡片、授权提示、安装确认都用这里。
//!
//! JS 插件的权限就是清单里声明的能力（`fs:workspace:read`、`net:<主机>`……）；内置插件没有清单，
//! 它们实际能做的事写在 [`builtin_permissions`] 这张表里，同样显示在卡片上。

use rust_i18n::t;

/// 一项能力给人看的说法。
pub fn capability_text(name: &str) -> String {
    let key = match name {
        "fs:workspace:read" => "settings.capability_fs_read",
        "fs:workspace:write" => "settings.capability_fs_write",
        "net:*" => "settings.capability_net_any",
        "exec:*" => "settings.capability_exec_any",
        "llm" => "settings.capability_llm",
        "session:read" => "settings.capability_session_read",
        "session:control" => "settings.capability_session_control",
        "agent:run" => "settings.capability_agent_run",
        "hooks:gate" => "settings.capability_hooks_gate",
        "hooks:review" => "settings.capability_hooks_review",
        "hooks:tool-output" => "settings.capability_hooks_tool_output",
        "hooks:write" => "settings.capability_hooks_write",
        "context:compact" => "settings.capability_context_compact",
        "context:rewrite" => "settings.capability_context_rewrite",
        "provider" => "settings.capability_provider",
        "oauth" => "settings.capability_oauth",
        "model:route" => "settings.capability_model_route",
        "notify" => "settings.capability_notify",
        "background" => "settings.capability_background",
        "dshr:config:read" => "settings.capability_config_read",
        "dshr:config:write" => "settings.capability_config_write",
        "dshr:skills" => "settings.capability_skills",
        "dshr:sessions:read" => "settings.capability_sessions_read",
        "dshr:sessions:write" => "settings.capability_sessions_write",
        "dshr:mcp" => "settings.capability_mcp",
        "plugins:install" => "settings.capability_plugins_install",
        other if other.starts_with("plugins:call:") => {
            return t!("settings.capability_plugins_call", plugin = &other["plugins:call:".len()..]).to_string();
        }
        other if other.starts_with("mcp:") => {
            let server = other["mcp:".len()..].split('@').next().unwrap_or_default();
            return t!("settings.capability_mcp_server", name = server).to_string();
        }
        other => {
            return match (other.strip_prefix("net:"), other.strip_prefix("exec:")) {
                (Some(host), _) => t!("settings.capability_net", host = host).to_string(),
                (_, Some(program)) => t!("settings.capability_exec", program = program).to_string(),
                _ => other.to_owned(),
            };
        }
    };
    t!(key).to_string()
}

/// 能力有没有风险：替用户做事、改宿主数据、运行程序、直接放行的那些标成“需要留意”，卡片上用醒目的颜色。
pub fn capability_sensitive(name: &str) -> bool {
    name.starts_with("exec:")
        || name.starts_with("mcp:")
        || name == "net:*"
        || matches!(
            name,
            "fs:workspace:write"
                | "session:control"
                | "agent:run"
                | "hooks:gate"
                | "hooks:review"
                | "hooks:tool-output"
                | "hooks:write"
                | "context:rewrite"
                | "dshr:config:write"
                | "dshr:sessions:write"
                | "dshr:mcp"
                | "dshr:skills"
                | "plugins:install"
        )
}

/// 内置插件实际能做的事；空的表示不需要特殊权限（只做判断或显示）。
pub fn builtin_permissions(id: &str) -> Vec<String> {
    let keys: &[&str] = match id {
        "fs" => &["settings.capability_fs_read", "settings.capability_fs_write"],
        "search" => &["settings.capability_fs_read"],
        "shell" => &["settings.builtin_run_commands"],
        "mcp" => &["settings.builtin_mcp_processes"],
        "title" | "compaction" => &["settings.builtin_model_conversation"],
        "delegate" => &["settings.capability_agent_run"],
        "instructions" => &["settings.builtin_read_instructions"],
        "skills" => &["settings.builtin_read_skills"],
        "provider-messages" | "provider-openai" => &["settings.builtin_connect_provider"],
        "creator" => &["settings.builtin_write_plugins", "settings.builtin_creator_grant"],
        _ => &[],
    };
    keys.iter().map(|key| t!(*key).to_string()).collect()
}

/// 内置插件的显示名；文案里没有时用 id。
pub fn builtin_name(id: &str) -> String {
    let key = format!("plugin_name.{id}");
    let text = t!(&key);
    if text == key { id.to_owned() } else { text.to_string() }
}

/// “提供：…”里的一项：种类和数量 → 文字。
pub fn provides_text(kind: &str, count: usize) -> String {
    match kind {
        "tools" => t!("settings.provides_tools", count = count).to_string(),
        "sidebar" => t!("settings.provides_sidebar").to_string(),
        "settings" => t!("settings.provides_settings").to_string(),
        "panels" => t!("settings.provides_panels").to_string(),
        "status" => t!("settings.provides_status").to_string(),
        "hooks" => t!("settings.provides_hooks").to_string(),
        "cards" => t!("settings.provides_cards").to_string(),
        "commands" => t!("settings.provides_commands", count = count).to_string(),
        "slash" => t!("settings.provides_slash", count = count).to_string(),
        "mcp" => t!("settings.provides_mcp", count = count).to_string(),
        "skills" => t!("settings.provides_skills", count = count).to_string(),
        "presets" => t!("settings.provides_presets", count = count).to_string(),
        "providers" => t!("settings.provides_providers").to_string(),
        "themes" => t!("settings.provides_themes").to_string(),
        "schedules" => t!("settings.provides_schedules").to_string(),
        "services" => t!("settings.provides_services").to_string(),
        "prompt" => t!("settings.provides_prompt").to_string(),
        "events" => t!("settings.provides_events").to_string(),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_has_words() {
        rust_i18n::set_locale("zh-CN");
        for name in [
            "fs:workspace:read",
            "fs:workspace:write",
            "net:*",
            "exec:*",
            "llm",
            "session:read",
            "session:control",
            "agent:run",
            "hooks:gate",
            "hooks:review",
            "hooks:tool-output",
            "context:compact",
            "context:rewrite",
            "provider",
            "oauth",
            "model:route",
            "notify",
            "background",
            "dshr:config:read",
            "dshr:config:write",
            "dshr:skills",
            "dshr:sessions:read",
            "dshr:sessions:write",
            "dshr:mcp",
            "plugins:install",
            "hooks:write",
            "mcp:github@1a2b3c4d",
            "plugins:call:helper",
        ] {
            let text = capability_text(name);
            assert!(!text.starts_with("settings."), "{name} 没有文案：{text}");
        }
        assert_eq!(capability_text("session:read"), "读取当前对话事件");
        assert!(capability_text("net:api.example.com").contains("api.example.com"));
        assert!(capability_text("exec:git").contains("git"));
        assert!(capability_sensitive("plugins:install") && !capability_sensitive("session:read"));
        assert_eq!(builtin_permissions("shell"), ["运行命令"]);
        assert!(builtin_permissions("permission").is_empty());
        assert_eq!(builtin_name("mcp"), "MCP 服务器");
        assert_eq!(provides_text("tools", 3), "3 个工具");
    }

    #[test]
    fn chinese_wording_stays_formal() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/app.yml")).unwrap();
        // 文件能不能解析由 rust_i18n 在编译时检查；这里只看措辞。
        assert_eq!(t!("title_bar.close", locale = "zh-CN"), "关闭");
        for line in text.lines().filter(|line| line.trim_start().starts_with("zh-CN:")) {
            for phrase in ["还没", "再点一次", "没能", "出错了", "不在了", "一下", "空着", "不认识"] {
                assert!(!line.contains(phrase), "口语化表述：{line}");
            }
        }
    }

    #[test]
    fn palettes_are_checked() {
        use std::collections::BTreeMap;
        let set =
            |pairs: &[(&str, &str)]| pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect::<BTreeMap<_, _>>();
        let ok =
            crate::design::PaletteOverride::parse(&set(&[("accent", "#cc785c"), ("text", "#1f1f1f"), ("surface", "#ffffff")]), &set(&[]));
        assert!(ok.is_ok());
        let faint = crate::design::PaletteOverride::parse(&set(&[("text", "#dddddd"), ("surface", "#ffffff")]), &set(&[]));
        assert!(faint.unwrap_err().contains("contrast"));
        assert!(crate::design::PaletteOverride::parse(&set(&[("glow", "#ffffff")]), &set(&[])).unwrap_err().contains("unknown color"));
        assert!(crate::design::PaletteOverride::parse(&set(&[("accent", "red")]), &set(&[])).is_err());

        // 内置的高对比度配色要让设置页能用：颜色都认得，浅色、深色两套都满足对比度要求。
        let high = crate::backend::builtin_high_contrast_theme();
        assert!(crate::design::PaletteOverride::parse(&high.light, &high.dark).is_ok());
    }

    #[test]
    fn palette_labels_exist_in_every_language() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/locales/app.yml")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for key in ["palette", "palette_default", "palette_high_contrast", "palette_hint"] {
            let header = format!("  {key}:");
            let start = lines.iter().position(|line| *line == header).unwrap_or_else(|| panic!("没有 {key}"));
            let locales: Vec<&str> = lines[start + 1..]
                .iter()
                .take_while(|line| line.starts_with("    "))
                .filter_map(|line| line.trim_start().split_once(": ").map(|(locale, _)| locale))
                .collect();
            for (code, name) in crate::LANGUAGES {
                assert!(locales.contains(code), "settings.{key} 缺少 {name}（{code}）");
            }
        }
    }
}
