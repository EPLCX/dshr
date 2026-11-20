//! 分发：发布者首次信任、旧版本归档与回滚、宿主版本要求。

use super::*;

fn write_version(dir: &Path, version: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), format!("id = \"dist\"\nversion = \"{version}\"\n")).unwrap();
    std::fs::write(dir.join("index.js"), format!("export const tools = [{{ name: \"dist_v\", execute: () => \"{version}\" }}];\n"))
        .unwrap();
}

fn packed(source: &Path, version: &str, key: Option<&ed25519_dalek::SigningKey>) -> Vec<u8> {
    let dir = source.join(format!("dist-{version}"));
    write_version(&dir, version);
    let out = source.join(format!("dist-{version}.dshrplugin"));
    crate::pack::pack(&dir, Some(&out), key).unwrap();
    std::fs::read(out).unwrap()
}

#[test]
fn publishers_are_pinned_and_old_versions_archived() {
    let f = Fixture::new();
    let source = tempfile::tempdir().unwrap();
    let plugins = f.plugins_dir();
    let alice = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
    let mallory = ed25519_dalek::SigningKey::from_bytes(&[2u8; 32]);
    let stage = |bytes: Vec<u8>| crate::install::stage_bytes(&bytes, "dist.dshrplugin", "test", None, None).unwrap();

    stage(packed(source.path(), "1.0.0", Some(&alice))).commit(&plugins).unwrap();
    let recorded = crate::sources::load(&plugins)["dist"].clone();
    assert_eq!(recorded.version.as_deref(), Some("1.0.0"));
    assert!(recorded.signer.as_deref().unwrap().starts_with(crate::pack::KEY_PREFIX));

    // 换了签名的钥匙、没有签名：都不装，除非 --force。
    let error = stage(packed(source.path(), "1.1.0", Some(&mallory))).commit(&plugins).unwrap_err();
    assert!(error.contains("发布者变了"), "{error}");
    let error = stage(packed(source.path(), "1.1.0", None)).commit(&plugins).unwrap_err();
    assert!(error.contains("没有签名"), "{error}");
    assert!(std::fs::read_to_string(plugins.join("dist/plugin.toml")).unwrap().contains("1.0.0"));

    // 同一把钥匙签的升级：装上，旧版本归档。
    stage(packed(source.path(), "1.1.0", Some(&alice))).commit(&plugins).unwrap();
    stage(packed(source.path(), "1.2.0", Some(&alice))).commit(&plugins).unwrap();
    let versions: Vec<String> = crate::sources::versions(&plugins, "dist").into_iter().map(|archived| archived.version).collect();
    assert_eq!(versions, ["1.1.0", "1.0.0"]);

    // 回滚：从归档里再装一次（同一个发布者，正常检查也能过）。
    let archived = crate::sources::rollback_source(&plugins, "dist", Some("1.0.0")).unwrap();
    crate::install::stage_local(&archived.dir, Some("dist")).unwrap().commit(&plugins).unwrap();
    assert!(std::fs::read_to_string(plugins.join("dist/plugin.toml")).unwrap().contains("1.0.0"));
    assert!(crate::sources::rollback_source(&plugins, "dist", Some("9.9.9")).unwrap_err().contains("没有归档的 9.9.9"));

    // 最多留 3 个旧版本。
    for minor in 3..7 {
        stage(packed(source.path(), &format!("1.{minor}.0"), Some(&alice)))
            .commit_with(&plugins, crate::install::CommitOptions { force: true, index: None })
            .unwrap();
    }
    assert_eq!(crate::sources::versions(&plugins, "dist").len(), crate::sources::KEEP_VERSIONS);

    // 卸载时来源记录一起清掉（之后可以重新信任别的发布者）。
    crate::install::uninstall(&plugins, "dist").unwrap();
    assert!(!crate::sources::load(&plugins).contains_key("dist"));
}

#[test]
fn engines_requirement_is_checked_at_activation() {
    let f = Fixture::new();
    f.plugin("future", "[engines]\ndshr = \">=99\"\n", "export const tools = [];");
    f.plugin("current", "[engines]\ndshr = \">=0.1\"\n", "export const tools = [];");
    f.plugin("typo", "[engines]\ndshr = \"not a version\"\n", "");
    let cx = f.context();
    assert!(failure(&cx, "future").contains("需要 dshr >=99"));
    assert_eq!(cx.status("current"), Some(Status::Active));
    assert!(failure(&cx, "typo").contains("写错了"));
}
