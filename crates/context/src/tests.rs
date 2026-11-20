use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

struct Words;

impl ExtensionPoint for Words {
    type Item = &'static str;
}

struct Rewrite;

impl Hook for Rewrite {
    type Handler = dyn Fn(&mut String) + Send + Sync;
}

struct Ping(u32);

impl Event for Ping {}

trait Greeter: Send + Sync {
    fn greet(&self) -> String;
}

struct English;

impl Greeter for English {
    fn greet(&self) -> String {
        "hello".into()
    }
}

/// 用闭包写的测试插件。
struct Inline<F> {
    manifest: Manifest,
    apply: F,
}

impl<F: Fn(&mut Scope) -> anyhow::Result<()> + Send + Sync + 'static> Plugin for Inline<F> {
    fn manifest(&self) -> Manifest {
        self.manifest.clone()
    }

    fn apply(&self, cx: &mut Scope) -> anyhow::Result<()> {
        (self.apply)(cx)
    }
}

fn plugin(manifest: Manifest, apply: impl Fn(&mut Scope) -> anyhow::Result<()> + Send + Sync + 'static) -> impl Plugin {
    Inline { manifest, apply }
}

fn word(id: &'static str, word: &'static str) -> impl Plugin {
    plugin(Manifest::new(id), move |cx| {
        cx.contribute::<Words>(word);
        Ok(())
    })
}

fn greeter() -> impl Plugin {
    plugin(Manifest::new("greeter"), |cx| {
        cx.provide::<dyn Greeter>(Arc::new(English));
        Ok(())
    })
}

/// 依赖 `Greeter` 的插件：把问候语的长度记进 `seen`，并贡献一个词。
fn needs_greeter(seen: Arc<AtomicU32>) -> impl Plugin {
    plugin(Manifest::new("user").requires::<dyn Greeter>(), move |cx| {
        let greeter = cx.get::<dyn Greeter>().expect("requires 里的服务一定在");
        seen.fetch_add(greeter.greet().len() as u32, Ordering::SeqCst);
        cx.contribute::<Words>("user");
        Ok(())
    })
}

#[test]
fn contributions_follow_plugin_lifecycle() {
    let cx = Context::new();
    cx.add(word("a", "alpha")).unwrap();
    cx.add(word("b", "beta")).unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["alpha", "beta"]);

    // 已经拿到手的快照不受之后的变化影响。
    let before = cx.snapshot();
    cx.disable("a").unwrap();
    assert_eq!(before.items::<Words>(), ["alpha", "beta"]);
    assert_eq!(cx.snapshot().items::<Words>(), ["beta"]);
    assert_eq!(cx.status("a"), Some(Status::Disabled));

    cx.enable("a").unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["beta", "alpha"]);

    cx.remove("b").unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["alpha"]);
    assert_eq!(cx.plugins(), [("a", Status::Active)]);
}

#[test]
fn duplicate_and_unknown_plugins_are_errors() {
    let cx = Context::new();
    cx.add(word("a", "alpha")).unwrap();
    assert_eq!(cx.add(word("a", "again")), Err(ContextError::DuplicatePlugin("a")));
    assert_eq!(cx.disable("nope"), Err(ContextError::UnknownPlugin("nope".into())));
    assert_eq!(cx.snapshot().items::<Words>(), ["alpha"]);
}

#[test]
fn pending_until_required_service_appears() {
    let cx = Context::new();
    let seen = Arc::new(AtomicU32::new(0));
    cx.add(needs_greeter(seen.clone())).unwrap();
    assert!(matches!(cx.status("user"), Some(Status::Pending(missing)) if missing.len() == 1));
    assert!(cx.snapshot().items::<Words>().is_empty());

    cx.add(greeter()).unwrap();
    assert_eq!(cx.status("user"), Some(Status::Active));
    assert_eq!(seen.load(Ordering::SeqCst), 5);
    assert_eq!(cx.snapshot().service::<dyn Greeter>().unwrap().greet(), "hello");
}

#[test]
fn revoking_a_service_cascades() {
    let cx = Context::new();
    let seen = Arc::new(AtomicU32::new(0));
    cx.add(greeter()).unwrap();
    cx.add(needs_greeter(seen.clone())).unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["user"]);

    cx.disable("greeter").unwrap();
    assert!(matches!(cx.status("user"), Some(Status::Pending(_))));
    assert!(cx.snapshot().items::<Words>().is_empty());
    assert!(cx.snapshot().service::<dyn Greeter>().is_none());

    cx.enable("greeter").unwrap();
    assert_eq!(cx.status("user"), Some(Status::Active));
    assert_eq!(cx.snapshot().items::<Words>(), ["user"]);
    assert_eq!(seen.load(Ordering::SeqCst), 10);
}

#[test]
fn failed_apply_registers_nothing() {
    let cx = Context::new();
    let attempts = Arc::new(AtomicU32::new(0));
    let counter = attempts.clone();
    cx.add(plugin(Manifest::new("flaky"), move |cx| {
        cx.contribute::<Words>("half");
        cx.provide::<dyn Greeter>(Arc::new(English));
        // 第一次失败，第二次成功。
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            anyhow::bail!("boom");
        }
        Ok(())
    }))
    .unwrap();
    assert_eq!(cx.status("flaky"), Some(Status::Failed("boom".into())));
    assert!(cx.snapshot().items::<Words>().is_empty());
    assert!(cx.snapshot().service::<dyn Greeter>().is_none());

    cx.enable("flaky").unwrap();
    assert_eq!(cx.status("flaky"), Some(Status::Active));
    assert_eq!(cx.snapshot().items::<Words>(), ["half"]);
}

#[test]
fn a_service_has_one_provider() {
    let cx = Context::new();
    cx.add(greeter()).unwrap();
    cx.add(plugin(Manifest::new("second"), |cx| {
        cx.contribute::<Words>("second");
        cx.provide::<dyn Greeter>(Arc::new(English));
        Ok(())
    }))
    .unwrap();
    assert!(matches!(cx.status("second"), Some(Status::Failed(error)) if error.contains("greeter")));
    assert!(cx.snapshot().items::<Words>().is_empty());
}

#[test]
fn hooks_run_in_priority_order() {
    let cx = Context::new();
    let append = |id, priority: i32, text: &'static str| {
        plugin(Manifest::new(id), move |cx| {
            cx.hook::<Rewrite>(priority, Arc::new(move |value: &mut String| value.push_str(text)));
            Ok(())
        })
    };
    cx.add(append("late", 10, "c")).unwrap();
    cx.add(append("early", -10, "a")).unwrap();
    cx.add(append("middle", 0, "b")).unwrap();

    let run = |cx: &Context| {
        let mut value = String::new();
        for hook in cx.snapshot().hooks::<Rewrite>() {
            hook(&mut value);
        }
        value
    };
    assert_eq!(run(&cx), "abc");
    cx.disable("middle").unwrap();
    assert_eq!(run(&cx), "ac");
}

#[test]
fn events_reach_listeners_until_disabled() {
    let cx = Context::new();
    let total = Arc::new(AtomicU32::new(0));
    let counter = total.clone();
    cx.add(plugin(Manifest::new("listener"), move |cx| {
        let counter = counter.clone();
        cx.on(move |ping: &Ping| {
            counter.fetch_add(ping.0, Ordering::SeqCst);
        });
        Ok(())
    }))
    .unwrap();

    cx.emit(&Ping(2));
    cx.emit(&Ping(3));
    assert_eq!(total.load(Ordering::SeqCst), 5);
    cx.disable("listener").unwrap();
    cx.emit(&Ping(100));
    assert_eq!(total.load(Ordering::SeqCst), 5);
}

#[test]
fn optional_service_reactivates_the_plugin() {
    let cx = Context::new();
    cx.add(plugin(Manifest::new("polite").optional::<dyn Greeter>(), |cx| {
        cx.contribute::<Words>(if cx.get::<dyn Greeter>().is_some() { "with" } else { "without" });
        Ok(())
    }))
    .unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["without"]);

    cx.add(greeter()).unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["with"]);
    cx.remove("greeter").unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["without"]);
}

#[test]
fn config_falls_back_to_default() {
    /// 用量上限。
    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize, JsonSchema)]
    #[serde(default)]
    struct Limits {
        /// 最多几个。
        #[schemars(range(min = 1, max = 100))]
        max: u32,
        verbose: bool,
    }

    impl Default for Limits {
        fn default() -> Self {
            Self { max: 3, verbose: false }
        }
    }

    let read = Arc::new(Mutex::new(Vec::new()));
    let sink = read.clone();
    let cx = Context::new();
    cx.set_config("limits", serde_json::json!({"max": 7}));
    cx.add(plugin(Manifest::new("limits").config::<Limits>(), move |cx| {
        sink.lock().unwrap().push(cx.config::<Limits>());
        Ok(())
    }))
    .unwrap();

    // 格式不对时用默认值；没有配置时也是。
    cx.set_config("limits", serde_json::json!({"max": "many"}));
    cx.disable("limits").unwrap();
    cx.enable("limits").unwrap();
    assert_eq!(*read.lock().unwrap(), [Limits { max: 7, verbose: false }, Limits { max: 3, verbose: false }]);

    // 声明过的配置类型连同默认值、范围和说明一起出现在插件信息里，设置页据此生成表单。
    let info = cx.describe().into_iter().find(|info| info.id == "limits").unwrap();
    assert_eq!(info.status, Status::Active);
    let schema = info.config.unwrap();
    let max = &schema["properties"]["max"];
    assert_eq!(max["type"], "integer");
    assert_eq!((max["default"].as_u64(), max["minimum"].as_u64(), max["maximum"].as_u64()), (Some(3), Some(1), Some(100)));
    assert_eq!(max["description"], "最多几个。");
    assert_eq!(schema["properties"]["verbose"]["type"], "boolean");
    assert_eq!(schema["properties"]["verbose"]["default"], false);
}

#[test]
fn config_can_keep_a_plugin_disabled() {
    let cx = Context::new();
    cx.configure([("a".to_owned(), serde_json::json!({"enabled": false}))]);
    cx.add(word("a", "alpha")).unwrap();
    cx.add(word("b", "beta")).unwrap();
    assert_eq!(cx.status("a"), Some(Status::Disabled));
    assert_eq!(cx.snapshot().items::<Words>(), ["beta"]);

    cx.enable("a").unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["beta", "alpha"]);
}

#[tokio::test]
async fn spawned_tasks_stop_with_the_plugin() {
    struct SetOnDrop(Arc<AtomicU32>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(1, Ordering::SeqCst);
        }
    }

    let dropped = Arc::new(AtomicU32::new(0));
    let flag = dropped.clone();
    let cx = Context::new();
    cx.add(plugin(Manifest::new("worker"), move |cx| {
        let guard = SetOnDrop(flag.clone());
        cx.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        Ok(())
    }))
    .unwrap();

    tokio::task::yield_now().await;
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    cx.disable("worker").unwrap();
    for _ in 0..100 {
        if dropped.load(Ordering::SeqCst) == 1 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("插件停用后后台任务还在运行");
}

#[test]
fn replace_swaps_a_plugin_in_place() {
    let cx = Context::new();
    cx.set_config("b", serde_json::json!({"enabled": false, "level": 3}));
    cx.add(word("a", "a1")).unwrap();
    cx.add(word("b", "b1")).unwrap();
    cx.add(word("c", "c1")).unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["a1", "c1"]);
    assert_eq!(cx.config("b"), Some(serde_json::json!({"enabled": false, "level": 3})));
    assert_eq!(cx.config("a"), None);

    // 旧快照不受影响；新快照里旧的注册撤掉了、新的在，插件还在原来的位置。
    let before = cx.snapshot();
    cx.replace(word("a", "a2")).unwrap();
    assert_eq!(before.items::<Words>(), ["a1", "c1"]);
    assert_eq!(cx.snapshot().items::<Words>(), ["c1", "a2"]);
    assert_eq!(cx.plugins().iter().map(|(id, _)| *id).collect::<Vec<_>>(), ["a", "b", "c"]);

    // 被停用的换了也保持停用；启用后用的是新的那份。
    cx.replace(word("b", "b2")).unwrap();
    assert_eq!(cx.status("b"), Some(Status::Disabled));
    cx.enable("b").unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["c1", "a2", "b2"]);

    // 换上一个激活失败的：它的注册没了，状态是失败；再换回好的就恢复。没有这个 id 时等同于 add。
    cx.replace(plugin(Manifest::new("a"), |_| anyhow::bail!("坏了"))).unwrap();
    assert!(matches!(cx.status("a"), Some(Status::Failed(_))));
    assert_eq!(cx.snapshot().items::<Words>(), ["c1", "b2"]);
    cx.replace(word("a", "a3")).unwrap();
    cx.replace(word("d", "d1")).unwrap();
    assert_eq!(cx.snapshot().items::<Words>(), ["c1", "b2", "a3", "d1"]);
}

#[tokio::test]
async fn subscribers_hear_about_new_snapshots() {
    struct Nothing;
    impl Plugin for Nothing {
        fn manifest(&self) -> Manifest {
            Manifest::new("nothing")
        }
        fn apply(&self, _cx: &mut Scope) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let cx = Context::new();
    let mut changes = cx.subscribe();
    assert!(!changes.has_changed().unwrap());
    cx.add(Nothing).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), changes.changed()).await.unwrap().unwrap();
    cx.disable("nothing").unwrap();
    assert!(changes.has_changed().unwrap());
}
