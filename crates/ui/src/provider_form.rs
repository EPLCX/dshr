//! 提供商表单：添加内置提供商、添加自定义 API，或编辑已有提供商。

use std::sync::Arc;

use dshr_config::credentials::KeySource;
use dshr_config::providers::{ADAPTERS, Adapter, CUSTOM_ADAPTER, adapter, custom_provider_id};
use dshr_config::{ModelEntry, PromptCache, Protocol, ProviderConfig, ResolvedProvider};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::design::ControlSize as _;

use crate::announce::announce;
use crate::backend::{Backend, KeyStatus};
use crate::design::{
    Tone, Typography as _, description, divider, field, heading, hint, in_section, motion, palette, pressable, radius, readable, size,
    space,
};
use crate::dropdown::{DropdownChanged, DropdownState};
use crate::reveal::reveal;

pub enum FormEvent {
    /// 配置已写入，列表需要刷新（随后收起表单）。
    Saved,
    /// 账号登录状态变了（登录/退出成功）：只刷新列表，不收起表单。
    AccountChanged,
    /// 退出编辑。
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Add,
    Edit(ProviderConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Catalog,
    Custom,
}

enum Fetch {
    Idle,
    Running,
    Done(usize),
    Failed(String),
}

struct ModelRow {
    id: Entity<InputState>,
    name: Entity<InputState>,
}

pub struct ProviderForm {
    backend: Arc<Backend>,
    mode: Mode,
    tab: Tab,
    /// 尚未添加的内置适配器（添加模式的下拉选项）。
    available: Vec<&'static Adapter>,
    adapter_select: Entity<DropdownState>,
    /// 添加方式（内置 / 自定义）和自定义提供商的协议：选项类设置用下拉框。
    tab_select: Entity<DropdownState>,
    protocol_select: Entity<DropdownState>,
    /// Messages 协议的提示词缓存：关闭 / 5 分钟 / 1 小时（[`PromptCache`]）。
    prompt_cache_select: Entity<DropdownState>,
    prompt_cache: PromptCache,
    /// “保存”按钮的焦点：表单里没有别的可以先聚焦的控件时，打开表单就聚焦它。
    save_focus: FocusHandle,
    /// 当前选中的适配器；自定义提供商为 `None`。
    adapter: Option<&'static Adapter>,
    name: Entity<InputState>,
    protocol: Protocol,
    api_key: Entity<InputState>,
    key_status: KeyStatus,
    base_url: Entity<InputState>,
    custom_open: bool,
    models: Vec<ModelRow>,
    fetch: Fetch,
    message: Option<(String, bool)>,
    /// 进行中的账号登录（DeepSeek / DeepSeek OAuth 表单用登录按钮代替 API 密钥输入）。
    sign_in: Option<tokio::task::JoinHandle<Result<String, String>>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FormEvent> for ProviderForm {}

impl ProviderForm {
    pub fn add(backend: Arc<Backend>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let configured = backend.file_config().providers;
        let available: Vec<_> = ADAPTERS.iter().filter(|adapter| !configured.iter().any(|p| p.adapter == adapter.id)).collect();
        Self::build(backend, Mode::Add, available, window, cx)
    }

    pub fn edit(backend: Arc<Backend>, provider: ProviderConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(backend, Mode::Edit(provider), Vec::new(), window, cx)
    }

    fn build(backend: Arc<Backend>, mode: Mode, available: Vec<&'static Adapter>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editing = match &mode {
            Mode::Edit(provider) => Some(provider.clone()),
            Mode::Add => None,
        };
        let adapter = match &editing {
            Some(provider) => adapter(&provider.adapter),
            None => available.first().copied(),
        };
        // 内置的都加过了时直接打开“自定义”：目录页上只剩一句说明，没有可填的控件。
        let custom = editing.as_ref().is_some_and(|p| p.adapter == CUSTOM_ADAPTER) || (editing.is_none() && available.is_empty());
        let tab = if custom { Tab::Custom } else { Tab::Catalog };

        let names: Vec<SharedString> = available.iter().map(|adapter| SharedString::from(adapter.name)).collect();
        let adapter_select = cx.new(|cx| DropdownState::new(names, (!available.is_empty()).then_some(0), cx).width(320.));
        let input = |value: String, placeholder: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).default_value(value).placeholder(placeholder))
        };
        let name =
            input(editing.as_ref().map(|p| p.name.clone()).unwrap_or_default(), t!("settings.name_placeholder").to_string(), window, cx);
        let base_url = input(
            editing.as_ref().map(|p| p.base_url.clone()).unwrap_or_default(),
            t!("settings.api_url_placeholder").to_string(),
            window,
            cx,
        );
        let api_key = cx.new(|cx| InputState::new(window, cx).masked(true));

        let subscription = cx.subscribe_in(&adapter_select, window, |this, _, DropdownChanged(index): &DropdownChanged, window, cx| {
            if let Some(adapter) = this.available.get(*index).copied() {
                this.adapter = Some(adapter);
                this.prompt_cache = PromptCache::default_for(adapter.id);
                this.models.clear();
                this.fetch = Fetch::Idle;
                this.base_url.update(cx, |input, cx| input.set_value(String::new(), window, cx));
                this.refresh_key_status(window, cx);
                cx.notify();
            }
        });

        let tab_select = cx.new(|cx| DropdownState::new(tab_labels(), TABS.iter().position(|value| *value == tab), cx).width(200.));
        let tab_subscription = cx.subscribe_in(&tab_select, window, |this, _, DropdownChanged(index): &DropdownChanged, window, cx| {
            if let Some(tab) = TABS.get(*index) {
                this.set_tab(*tab, window, cx);
            }
        });
        let protocol = editing.as_ref().and_then(|p| p.protocol).unwrap_or(Protocol::OpenAi);
        let protocol_select =
            cx.new(|cx| DropdownState::new(protocol_labels(), PROTOCOLS.iter().position(|value| *value == protocol), cx).width(260.));
        let protocol_subscription =
            cx.subscribe_in(&protocol_select, window, |this, _, DropdownChanged(index): &DropdownChanged, window, cx| {
                if let Some(protocol) = PROTOCOLS.get(*index) {
                    this.protocol = *protocol;
                    this.refresh_key_status(window, cx);
                    cx.notify();
                }
            });
        // 没写过时显示这个提供商的默认值（内置 Anthropic 默认 5 分钟，其余关闭）。
        let prompt_cache = match &editing {
            Some(provider) => provider.prompt_cache.unwrap_or_else(|| PromptCache::default_for(&provider.adapter)),
            None => PromptCache::default_for(adapter.map_or(CUSTOM_ADAPTER, |adapter| adapter.id)),
        };
        let prompt_cache_select = cx.new(|cx| {
            DropdownState::new(prompt_cache_labels(), PromptCache::ALL.iter().position(|value| *value == prompt_cache), cx).width(260.)
        });
        let prompt_cache_subscription =
            cx.subscribe_in(&prompt_cache_select, window, |this, _, DropdownChanged(index): &DropdownChanged, _, cx| {
                if let Some(value) = PromptCache::ALL.get(*index) {
                    this.prompt_cache = *value;
                    cx.notify();
                }
            });
        let mut form = Self {
            backend,
            tab,
            available,
            adapter_select,
            tab_select,
            protocol_select,
            prompt_cache_select,
            prompt_cache,
            save_focus: cx.focus_handle(),
            adapter,
            name,
            protocol: editing.as_ref().and_then(|p| p.protocol).unwrap_or(Protocol::OpenAi),
            api_key,
            key_status: KeyStatus { active: None, env_present: false },
            base_url,
            // 编辑时如果已有自定义内容，直接展开。
            custom_open: editing.as_ref().is_some_and(|p| !p.base_url.is_empty() || !p.models.is_empty()),
            models: Vec::new(),
            fetch: Fetch::Idle,
            message: None,
            sign_in: None,
            mode,
            _subscriptions: vec![subscription, tab_subscription, protocol_subscription, prompt_cache_subscription],
        };
        if let Some(provider) = editing {
            for model in &provider.models {
                form.push_model_row(&model.id, &model.name, window, cx);
            }
        }
        form.refresh_key_status(window, cx);
        form
    }

    /// 由当前表单内容拼出的提供商配置（尚未保存）。
    fn draft(&self, cx: &App) -> ProviderConfig {
        let models = self
            .models
            .iter()
            .filter_map(|row| {
                let id = row.id.read(cx).value().trim().to_owned();
                (!id.is_empty()).then(|| ModelEntry::new(id, row.name.read(cx).value().trim()))
            })
            .fold(Vec::<ModelEntry>::new(), |mut models, model| {
                if !models.iter().any(|existing| existing.id == model.id) {
                    models.push(model);
                }
                models
            });
        let base_url = self.base_url.read(cx).value().trim().to_owned();
        // 和默认值一样时不写进配置文件：以后默认值变了也跟着变。
        let prompt_cache = |adapter: &str| (self.prompt_cache != PromptCache::default_for(adapter)).then_some(self.prompt_cache);
        match (&self.mode, self.tab) {
            (Mode::Edit(existing), _) => ProviderConfig {
                name: if existing.adapter == CUSTOM_ADAPTER { self.name.read(cx).value().trim().to_owned() } else { String::new() },
                protocol: (existing.adapter == CUSTOM_ADAPTER).then_some(self.protocol),
                base_url,
                models,
                prompt_cache: prompt_cache(&existing.adapter),
                ..existing.clone()
            },
            (Mode::Add, Tab::Catalog) => {
                let adapter = self.adapter.expect("添加内置提供商时总有选中的适配器");
                ProviderConfig { base_url, models, prompt_cache: prompt_cache(adapter.id), ..ProviderConfig::from_adapter(adapter) }
            }
            (Mode::Add, Tab::Custom) => {
                let name = self.name.read(cx).value().trim().to_owned();
                ProviderConfig {
                    id: custom_provider_id(&name, &self.backend.file_config().providers),
                    adapter: CUSTOM_ADAPTER.into(),
                    name,
                    protocol: Some(self.protocol),
                    base_url,
                    models,
                    plugin: None,
                    prompt_cache: prompt_cache(CUSTOM_ADAPTER),
                }
            }
        }
    }

    /// 要不要显示提示词缓存的设置：只有 Messages 协议用得上；DeepSeek 是自动缓存，不显示。
    fn shows_prompt_cache(&self, cx: &App) -> bool {
        let resolved = self.resolved(cx);
        resolved.protocol == Protocol::Messages
            && (self.is_custom() || self.adapter.is_some_and(|adapter| !adapter.id.starts_with("deepseek")))
    }

    fn render_prompt_cache(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let p = palette(cx);
        field(t!("settings.prompt_cache").to_string(), self.prompt_cache_select.clone()).child(hint(
            t!("settings.prompt_cache_hint").to_string(),
            Tone::Neutral,
            &p,
        ))
    }

    fn is_custom(&self) -> bool {
        match &self.mode {
            Mode::Edit(provider) => provider.adapter == CUSTOM_ADAPTER,
            Mode::Add => self.tab == Tab::Custom,
        }
    }

    fn refresh_key_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let resolved = self.resolved(cx);
        self.key_status = match &self.mode {
            Mode::Edit(_) => self.backend.key_status(&resolved),
            // 添加时还没有保存过的 Key，只看环境变量。
            Mode::Add => {
                KeyStatus { active: None, env_present: resolved.env_key.is_some() && self.backend.key_status(&resolved).env_present }
            }
        };
        let placeholder = match (&self.key_status.active, resolved.env_key) {
            (Some((_, KeySource::Keyring)), _) => t!("settings.api_key_saved_placeholder"),
            (_, Some(env)) => t!("settings.api_key_env_placeholder", env = env),
            (_, None) => t!("settings.api_key_placeholder"),
        };
        self.api_key.update(cx, |input, cx| input.set_placeholder(placeholder.to_string(), window, cx));
        // 自定义提供商的地址必填，占位给出示例；内置提供商留空即用默认地址。
        let url_placeholder = if self.is_custom() { t!("settings.api_url_custom_placeholder") } else { t!("settings.api_url_placeholder") };
        self.base_url.update(cx, |input, cx| input.set_placeholder(url_placeholder.to_string(), window, cx));
    }

    fn resolved(&self, cx: &App) -> ResolvedProvider {
        match (&self.mode, self.tab, self.adapter) {
            (Mode::Add, Tab::Catalog, None) => ProviderConfig::from_adapter(&ADAPTERS[0]).resolve(),
            _ => self.draft(cx).resolve(),
        }
    }

    fn push_model_row(&mut self, id: &str, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let id = cx.new(|cx| {
            InputState::new(window, cx).default_value(id.to_owned()).placeholder(t!("settings.model_id_placeholder").to_string())
        });
        let name = cx.new(|cx| {
            InputState::new(window, cx).default_value(name.to_owned()).placeholder(t!("settings.model_name_placeholder").to_string())
        });
        self.models.push(ModelRow { id, name });
    }

    fn adapter_defaults(&self) -> &'static [(&'static str, &'static str)] {
        if self.is_custom() { &[] } else { self.adapter.map_or(&[], |adapter| adapter.default_models) }
    }

    /// 添加模型：如果还在用默认目录，先把默认模型复制出来，在此基础上编辑。
    fn add_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.models.is_empty() {
            for (id, name) in self.adapter_defaults() {
                self.push_model_row(id, name, window, cx);
            }
        }
        self.push_model_row("", "", window, cx);
        cx.notify();
    }

    fn fetch_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed_key = self.api_key.read(cx).value().trim().to_owned();
        let task = self.backend.fetch_models(self.resolved(cx), Some(typed_key));
        self.fetch = Fetch::Running;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.map_err(|error| error.to_string()).and_then(|result| result);
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(models) => {
                        // 显示名的优先级：用户已经填过的 > 接口给的 > 适配器自带的 > 从 ID 推出来的。空的不算填过，
                        // 否则一行空的显示名会挡住后面的默认值。很多接口（OpenAI、兼容网关）只给 ID，最后一项兜底。
                        let typed: Vec<(String, String)> = this
                            .models
                            .iter()
                            .map(|row| (row.id.read(cx).value().trim().to_owned(), row.name.read(cx).value().trim().to_owned()))
                            .filter(|(_, name)| !name.is_empty())
                            .collect();
                        let defaults = this.adapter_defaults();
                        this.models.clear();
                        for model in &models {
                            let name = typed
                                .iter()
                                .find(|(known, _)| *known == model.id)
                                .map(|(_, name)| name.clone())
                                .or_else(|| (!model.name.is_empty()).then(|| model.name.clone()))
                                .or_else(|| defaults.iter().find(|(id, _)| *id == model.id).map(|(_, name)| (*name).to_owned()))
                                .or_else(|| dshr_llm::readable_model_name(&model.id))
                                .unwrap_or_default();
                            this.push_model_row(&model.id, &name, window, cx);
                        }
                        this.fetch = Fetch::Done(models.len());
                    }
                    Err(error) => this.fetch = Fetch::Failed(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.mode == Mode::Add && self.tab == Tab::Catalog && self.adapter.is_none() {
            return;
        }
        let draft = self.draft(cx);
        if self.is_custom() {
            if draft.name.is_empty() {
                return self.fail(t!("settings.name_required").to_string(), cx);
            }
            if draft.base_url.is_empty() {
                return self.fail(t!("settings.url_required").to_string(), cx);
            }
        }
        let id = draft.id.clone();
        let saved = self.backend.update_config(|config| match config.providers.iter_mut().find(|p| p.id == draft.id) {
            Some(existing) => *existing = draft,
            None => config.providers.push(draft),
        });
        if let Err(error) = saved {
            return self.fail(error, cx);
        }
        let key = self.api_key.read(cx).value().trim().to_owned();
        if !key.is_empty()
            && let Err(error) = self.backend.save_api_key(&id, &key)
        {
            return self.fail(error, cx);
        }
        cx.emit(FormEvent::Saved);
    }

    fn remove_provider(&mut self, cx: &mut Context<Self>) {
        let Mode::Edit(provider) = &self.mode else {
            return;
        };
        let id = provider.id.clone();
        match self.backend.update_config(|config| config.providers.retain(|p| p.id != id)) {
            Ok(()) => cx.emit(FormEvent::Saved),
            Err(error) => self.fail(error, cx),
        }
    }

    fn clear_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Mode::Edit(provider) = &self.mode
            && let Err(error) = self.backend.delete_api_key(&provider.id)
        {
            return self.fail(error, cx);
        }
        self.refresh_key_status(window, cx);
        cx.emit(FormEvent::Saved);
        cx.notify();
    }

    fn fail(&mut self, error: String, cx: &mut Context<Self>) {
        announce(error.clone(), cx);
        self.message = Some((error, true));
        cx.notify();
    }

    /// 这个表单是不是 DeepSeek OAuth 提供商（非自定义、接口地址未改）：
    /// 它的凭据区用「登录账号」按钮代替 API 密钥输入（账号令牌就是推理凭据）。
    /// `deepseek`（API 密钥提供商）与账号登录互不影响，保留密钥输入框。
    fn is_deepseek_official(&self, cx: &App) -> bool {
        if self.is_custom() {
            return false;
        }
        let resolved = self.resolved(cx);
        dshr_config::providers::adapter(dshr_config::providers::DEEPSEEK_OAUTH)
            .is_some_and(|adapter| resolved.id == adapter.id && adapter.base_url == resolved.base_url)
    }

    /// 点「使用 DeepSeek 账号登录」：打开浏览器走 PKCE 授权，成功后令牌进钥匙串。
    fn sign_in_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sign_in.is_some() {
            return;
        }
        self.message = None;
        // 登录线程拿到授权地址后让界面打开系统浏览器。
        let (url_tx, url_rx) = futures::channel::oneshot::channel::<String>();
        let handle = self.backend.sign_in_deepseek(move |url| {
            let _ = url_tx.send(url);
        });
        self.sign_in = Some(handle);
        cx.notify();

        cx.spawn_in(window, async move |_, cx| {
            if let Ok(url) = url_rx.await {
                cx.update(|_window, cx| cx.open_url(&url)).ok();
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            let Some(handle) = this.update(cx, |this, _| this.sign_in.take()).ok().flatten() else {
                return;
            };
            let result = handle.await.map_err(|error| error.to_string()).and_then(|inner| inner);
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_account) => {
                        this.backend.forget_deepseek_key();
                        // 让默认模型改用 OAuth 提供商（配置里没有时补上）。
                        if let Err(error) = this.backend.prefer_oauth_provider() {
                            return this.fail(error, cx);
                        }
                        this.message = Some((t!("settings.signed_in").to_string(), false));
                        this.refresh_key_status(window, cx);
                        // 只刷新列表，不收起表单（用户可能还在编辑）。
                        cx.emit(FormEvent::AccountChanged);
                        cx.notify();
                    }
                    Err(error) => this.fail(error, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// 退出 DeepSeek 账号：删除钥匙串里的登录令牌。
    fn sign_out_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.backend.sign_out_deepseek();
        self.message = Some((t!("settings.signed_out").to_string(), false));
        self.refresh_key_status(window, cx);
        // 只刷新列表，不收起表单（用户可能还在编辑）。
        cx.emit(FormEvent::AccountChanged);
        cx.notify();
    }

    /// 表单刚展开时放焦点的地方：第一个要填的控件。按下的“添加 / 编辑”按钮已经被表单替换掉，
    /// 不主动放的话焦点会落到别处（读屏用户会觉得焦点飘走了）。
    pub fn focus_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editing = matches!(self.mode, Mode::Edit(_));
        if !editing && self.tab == Tab::Catalog && !self.available.is_empty() {
            self.adapter_select.read(cx).focus_handle().focus(window, cx);
        } else if self.is_custom() {
            self.name.read(cx).focus_handle(cx).focus(window, cx);
        } else if self.resolved(cx).requires_key {
            self.api_key.read(cx).focus_handle(cx).focus(window, cx);
        } else {
            self.save_focus.focus(window, cx);
        }
    }

    /// 读屏软件读到的控件名称带上区块：“添加提供商，API 密钥”“编辑 DeepSeek，API 密钥”。
    fn label(&self, field: impl AsRef<str>) -> String {
        let section = match &self.mode {
            Mode::Add => t!("a11y.add_provider_form").to_string(),
            Mode::Edit(provider) => t!("settings.editing", name = provider.resolve().name).to_string(),
        };
        in_section(section, field)
    }

    fn set_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.models.clear();
        self.fetch = Fetch::Idle;
        self.message = None;
        self.refresh_key_status(window, cx);
        cx.notify();
    }

    /// 选项文字、名称随语言变，选中项随当前状态变。可以在渲染时每次调用。
    fn sync_selects(&self, cx: &mut Context<Self>) {
        let kind_label = self.label(t!("a11y.provider_kind"));
        let tab_index = TABS.iter().position(|tab| *tab == self.tab);
        self.tab_select.update(cx, |select, cx| {
            select.set_label(kind_label, cx);
            select.set_item_labels(tab_labels(), cx);
            select.set_selected(tab_index, cx);
        });
        let protocol_label = self.label(t!("settings.protocol"));
        let protocol_index = PROTOCOLS.iter().position(|protocol| *protocol == self.protocol);
        self.protocol_select.update(cx, |select, cx| {
            select.set_label(protocol_label, cx);
            select.set_item_labels(protocol_labels(), cx);
            select.set_selected(protocol_index, cx);
        });
        let cache_label = self.label(t!("settings.prompt_cache"));
        let cache_index = PromptCache::ALL.iter().position(|value| *value == self.prompt_cache);
        self.prompt_cache_select.update(cx, |select, cx| {
            select.set_label(cache_label, cx);
            select.set_item_labels(prompt_cache_labels(), cx);
            select.set_selected(cache_index, cx);
        });
    }

    /// 显示 API Key 前的验证；已存进钥匙串的 Key 在验证后读出回显。
    fn key_guard(&self) -> crate::secret_reveal::Guard {
        let saved = matches!(self.key_status.active, Some((_, KeySource::Keyring)));
        let loader = match &self.mode {
            Mode::Edit(provider) if saved => {
                let id = provider.id.clone();
                Some(Arc::new(move || dshr_config::credentials::load_api_key(&id).ok().flatten()) as crate::secret_reveal::Loader)
            }
            _ => None,
        };
        crate::secret_reveal::Guard::Verify(loader)
    }

    fn render_key(&self, cx: &mut Context<Self>) -> AnyElement {
        let resolved = self.resolved(cx);
        let p = palette(cx);
        // DeepSeek 官方提供商：凭据区是账号登录按钮，不是 API 密钥输入框。
        if self.is_deepseek_official(cx) {
            let label = div().text_label().child(readable(t!("settings.account").to_string()));
            let signing_in = self.sign_in.is_some();
            let body: AnyElement = if self.backend.is_signed_in() {
                h_flex()
                    .gap(px(space::S))
                    .child(hint(t!("settings.signed_in_hint").to_string(), Tone::Success, &p))
                    .child(reveal(
                        "reveal-sign-out",
                        crate::design::framed(
                            "sign-out",
                            Button::new("sign-out")
                                .subtle(cx)
                                .control_xs()
                                .label(t!("settings.sign_out").to_string())
                                .on_click(cx.listener(|this, _, window, cx| this.sign_out_account(window, cx))),
                        ),
                    ))
                    .into_any_element()
            } else {
                v_flex()
                    .gap(px(space::S))
                    .child(reveal(
                        "reveal-sign-in",
                        crate::design::framed(
                            "sign-in",
                            Button::new("sign-in")
                                .control_sm()
                                .label(t!("settings.sign_in_with_account").to_string())
                                .disabled(signing_in)
                                .on_click(cx.listener(|this, _, window, cx| this.sign_in_account(window, cx))),
                        )
                        .inert(signing_in),
                    ))
                    .children(signing_in.then(|| hint(t!("settings.signing_in").to_string(), Tone::Neutral, &p)))
                    .into_any_element()
            };
            return v_flex().gap(px(space::S)).child(label).child(body).into_any_element();
        }
        let label = h_flex().gap(px(space::XS)).child(div().text_label().child(readable(t!("settings.api_key").to_string()))).child(
            // 可聚焦，屏幕阅读器能读出说明；鼠标悬停显示提示。
            reveal(
                "reveal-api-key-help",
                pressable("api-key-help", &p)
                    .accessibility_label(t!("settings.api_key_help").to_string())
                    .rounded_full()
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(t!("settings.api_key_help").to_string()).build(window, cx)
                    })
                    .child(Icon::new(IconName::Info).size(px(size::ICON_SM)).text_color(p.text_secondary)),
            ),
        );
        if !resolved.requires_key {
            return v_flex()
                .gap(px(space::S))
                .child(label)
                .child(hint(t!("settings.api_key_not_needed").to_string(), Tone::Neutral, &p))
                .into_any_element();
        }
        let status: AnyElement = match (&self.key_status.active, resolved.env_key) {
            (Some((key, KeySource::Keyring)), _) => h_flex()
                .gap(px(space::S))
                .child(hint(t!("settings.key_saved", key = key).to_string(), Tone::Neutral, &p))
                .child(reveal(
                    "reveal-clear-key",
                    crate::design::framed(
                        "clear-key",
                        Button::new("clear-key")
                            .subtle(cx)
                            .control_xs()
                            .label(t!("settings.clear_key").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.clear_key(window, cx))),
                    ),
                ))
                .into_any_element(),
            (_, Some(env)) if self.key_status.env_present => {
                hint(t!("settings.env_found", env = env).to_string(), Tone::Success, &p).into_any_element()
            }
            (_, Some(env)) => hint(t!("settings.env_missing", env = env).to_string(), Tone::Warning, &p).into_any_element(),
            (_, None) => div().into_any_element(),
        };
        v_flex()
            .gap(px(space::S))
            .child(label)
            .child(reveal(
                "reveal-api-key",
                crate::design::framed_handle(
                    gpui_kit::Focusable::focus_handle(self.api_key.read(cx), cx),
                    Input::new(&self.api_key).aria_label(self.label(t!("settings.api_key"))).suffix(crate::secret_reveal::toggle(
                        "toggle-api-key",
                        &self.api_key,
                        self.key_guard(),
                        cx,
                    )),
                ),
            ))
            .child(status)
            .children(crate::secret_reveal::error(&self.api_key).map(|error| hint(error.to_string(), Tone::Danger, &p)))
            .into_any_element()
    }

    fn render_models(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let p = palette(cx);
        let defaults = self.adapter_defaults();
        let subtitle = if self.models.is_empty() && !defaults.is_empty() {
            Some(t!("settings.using_default_models"))
        } else if self.models.is_empty() {
            None
        } else {
            Some(t!("settings.using_custom_models"))
        };
        let fetch_label = match &self.fetch {
            Fetch::Running => t!("settings.fetching"),
            _ => t!("settings.fetch_models"),
        };
        let fetch_status = match &self.fetch {
            Fetch::Done(count) => Some(hint(t!("settings.fetched", count = count).to_string(), Tone::Success, &p)),
            Fetch::Failed(error) => Some(hint(t!("settings.fetch_failed", error = error).to_string(), Tone::Danger, &p)),
            _ => None,
        };

        let body: AnyElement = if self.models.is_empty() {
            let content: AnyElement = if defaults.is_empty() {
                hint(t!("settings.no_models_box").to_string(), Tone::Neutral, &p).into_any_element()
            } else {
                v_flex()
                    .w_full()
                    .gap(px(space::XS))
                    .children(defaults.iter().map(|(id, name)| {
                        h_flex().gap(px(space::S)).child(div().text_body().child(*name)).child(hint(*id, Tone::Neutral, &p))
                    }))
                    .into_any_element()
            };
            div()
                .p(px(space::L))
                .rounded(px(radius::LG))
                .border_1()
                .border_dashed()
                .border_color(p.border)
                .flex()
                .justify_center()
                .child(content)
                .into_any_element()
        } else {
            v_flex()
                .gap(px(space::S))
                .children(self.models.iter().enumerate().map(|(index, row)| {
                    // 每行两个输入框，名称里带上“第几个模型”，不然读出来全是“模型 ID”“显示名”。
                    let row_label = |field: String| t!("a11y.model_row", index = index + 1, field = field).to_string();
                    reveal(
                        ("reveal-model", index),
                        h_flex()
                            .gap(px(space::S))
                            .child(div().flex_1().child(crate::design::framed_handle(
                                gpui_kit::Focusable::focus_handle(row.id.read(cx), cx),
                                Input::new(&row.id).small().aria_label(row_label(t!("settings.model_id_placeholder").to_string())),
                            )))
                            .child(div().flex_1().child(crate::design::framed_handle(
                                gpui_kit::Focusable::focus_handle(row.name.read(cx), cx),
                                Input::new(&row.name).small().aria_label(row_label(t!("settings.model_name_placeholder").to_string())),
                            )))
                            .child(crate::design::framed(
                                ("remove-model", index),
                                Button::new(("remove-model", index))
                                    .subtle(cx)
                                    .control_sm()
                                    .icon(IconName::Close)
                                    .accessibility_label(row_label(t!("settings.remove_model").to_string()))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if index < this.models.len() {
                                            this.models.remove(index);
                                            cx.notify();
                                        }
                                    })),
                            )),
                    )
                }))
                .into_any_element()
        };

        v_flex()
            .gap(px(space::M))
            .child(
                h_flex()
                    .gap(px(space::S))
                    .child(
                        v_flex()
                            .flex_1()
                            .child(div().text_label().child(readable(t!("settings.model_catalog").to_string())))
                            .children(subtitle.map(|subtitle| hint(subtitle.to_string(), Tone::Neutral, &p))),
                    )
                    .child(reveal(
                        "reveal-fetch-models",
                        crate::design::framed(
                            "fetch-models",
                            Button::new("fetch-models")
                                .subtle(cx)
                                .control_sm()
                                .icon(IconName::RefreshCw)
                                .label(fetch_label.to_string())
                                .disabled(matches!(self.fetch, Fetch::Running))
                                .on_click(cx.listener(|this, _, window, cx| this.fetch_models(window, cx))),
                        )
                        .inert(matches!(self.fetch, Fetch::Running)),
                    )),
            )
            .children(fetch_status)
            .child(body)
            .child(
                h_flex().child(reveal(
                    "reveal-add-model",
                    crate::design::framed(
                        "add-model",
                        Button::new("add-model")
                            .control_sm()
                            .icon(IconName::Plus)
                            .label(t!("settings.add_model").to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.add_model(window, cx))),
                    ),
                )),
            )
    }

    fn render_endpoint(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let p = palette(cx);
        let default_url = (!self.is_custom()).then(|| self.adapter.map(|adapter| adapter.base_url)).flatten();
        field(
            t!("settings.api_url").to_string(),
            crate::design::framed_handle(
                gpui_kit::Focusable::focus_handle(self.base_url.read(cx), cx),
                Input::new(&self.base_url).aria_label(self.label(t!("settings.api_url"))),
            ),
        )
        .children(default_url.map(|url| hint(t!("settings.api_url_default", url = url).to_string(), Tone::Neutral, &p)))
    }
}

impl Render for ProviderForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_selects(cx);
        let p = palette(cx);
        // 标题（添加时是两个标签页）不动，下面的内容在切换标签页时按方向滑进来。
        // 标签按钮留在动画层外面，用键盘切换时焦点不丢。
        let header: AnyElement = match &self.mode {
            Mode::Add => {
                let desc = if self.tab == Tab::Catalog { t!("settings.catalog_desc") } else { t!("settings.custom_desc") };
                v_flex()
                    .gap(px(space::M))
                    .child(heading(t!("a11y.add_provider_form").to_string(), 3).text_heading())
                    .child(field(t!("a11y.provider_kind").to_string(), self.tab_select.clone()))
                    .child(description(desc.to_string(), &p))
                    .into_any_element()
            }
            Mode::Edit(provider) => {
                let name = provider.resolve().name;
                heading(t!("settings.editing", name = name).to_string(), 3).text_heading().into_any_element()
            }
        };
        // 切换“添加方式”即时显示（换页不做动画）。
        let wrap = |body: Div| v_flex().gap(px(space::XL)).child(header).child(body).into_any_element();
        let mut form = v_flex().gap(px(space::XL));

        if self.mode == Mode::Add && self.tab == Tab::Catalog && self.available.is_empty() {
            return wrap(form.child(description(t!("settings.all_added").to_string(), &p)));
        }

        if self.mode == Mode::Add && self.tab == Tab::Catalog {
            let label = self.label(t!("settings.provider"));
            self.adapter_select.update(cx, |select, cx| select.set_label(label, cx));
            form = form.child(field(t!("settings.provider").to_string(), self.adapter_select.clone()));
        }
        if self.is_custom() {
            form = form
                .child(field(
                    t!("settings.name").to_string(),
                    crate::design::framed_handle(
                        gpui_kit::Focusable::focus_handle(self.name.read(cx), cx),
                        Input::new(&self.name).aria_label(self.label(t!("settings.name"))),
                    ),
                ))
                .child(field(t!("settings.protocol").to_string(), self.protocol_select.clone()))
                .child(self.render_endpoint(cx));
        }

        form = form.child(self.render_key(cx));
        if self.shows_prompt_cache(cx) {
            form = form.child(self.render_prompt_cache(cx));
        }
        form = form.child(divider(&p));

        if self.is_custom() {
            form = form.child(self.render_models(cx));
        } else {
            let open = self.custom_open;
            let toggle = reveal(
                "reveal-toggle-custom-settings",
                pressable("toggle-custom-settings", &p)
                    .accessibility_label(self.label(t!("settings.custom_settings")))
                    .aria_expanded(open)
                    .flex()
                    .items_center()
                    .rounded(px(radius::SM))
                    .gap(px(space::XS))
                    .cursor_pointer()
                    .text_label()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.custom_open = !this.custom_open;
                        cx.notify();
                    }))
                    .child(motion::turn(
                        Icon::new(IconName::ChevronRight).size(px(size::ICON_SM)),
                        "custom-settings-turn",
                        open,
                        motion::QUARTER_TURN,
                        window,
                        cx,
                    ))
                    .child(t!("settings.custom_settings").to_string()),
            );
            // 展开、收起即时完成。
            let section = open.then(|| {
                v_flex()
                    .pt(px(space::XL))
                    .gap(px(space::XL))
                    .child(self.render_endpoint(cx))
                    .child(divider(&p))
                    .child(self.render_models(cx))
            });
            form = form.child(v_flex().child(toggle).children(section));
        }

        let editing = matches!(self.mode, Mode::Edit(_));
        let footer = h_flex()
            .gap(px(space::S))
            .when(editing, |footer| {
                footer.child(crate::design::framed(
                    "remove-provider",
                    Button::new("remove-provider")
                        .subtle(cx)
                        .icon(IconName::Delete)
                        .label(t!("settings.delete_provider").to_string())
                        .on_click(cx.listener(|this, _, _, cx| this.remove_provider(cx))),
                ))
            })
            .child(div().flex_1())
            .children(self.message.clone().map(|(text, error)| {
                motion::fade_in(hint(text, if error { Tone::Danger } else { Tone::Success }, &p), "provider-message-enter")
            }))
            .when(editing || self.tab == Tab::Custom, |footer| {
                footer.child(crate::design::framed(
                    "cancel-provider",
                    Button::new("cancel-provider")
                        .label(t!("settings.cancel").to_string())
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(FormEvent::Cancelled))),
                ))
            })
            .child(
                crate::design::framed_handle(
                    (self.save_focus).clone(),
                    Button::new("save-provider")
                        .primary()
                        .label(t!("settings.save").to_string())
                        .accessibility_label(self.label(t!("settings.save")))
                        .track_focus(&self.save_focus)
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                )
                .tint("save-provider"),
            );
        // 表单里按 Esc 收起表单（编辑时、自定义时有“取消”），而不是关掉整个设置弹窗。
        div()
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    cx.emit(FormEvent::Cancelled);
                }
            }))
            .child(wrap(form.child(reveal("reveal-provider-footer", footer))))
            .into_any_element()
    }
}

const TABS: [Tab; 2] = [Tab::Catalog, Tab::Custom];
const PROTOCOLS: [Protocol; 2] = [Protocol::OpenAi, Protocol::Messages];

fn tab_labels() -> Vec<SharedString> {
    vec![t!("settings.tab_catalog").to_string().into(), t!("settings.tab_custom").to_string().into()]
}

fn prompt_cache_labels() -> Vec<SharedString> {
    vec![
        t!("settings.prompt_cache_off").to_string().into(),
        t!("settings.prompt_cache_5m").to_string().into(),
        t!("settings.prompt_cache_1h").to_string().into(),
    ]
}

fn protocol_labels() -> Vec<SharedString> {
    vec![t!("settings.protocol_openai").to_string().into(), t!("settings.protocol_messages").to_string().into()]
}
