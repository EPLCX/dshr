//! LLM Provider 抽象。
//!
//! 各 Provider 把线上流式协议（Messages / OpenAI Chat Completions）归一成 [`StreamEvent`]，
//! 再由 [`MessageAccumulator`] 拼成完整的 assistant 消息。

pub mod accumulate;
pub mod connect;
pub mod messages;
pub mod mock;
pub mod openai;
pub mod types;

pub use accumulate::{AssembledMessage, MessageAccumulator};
pub use connect::{
    BuiltinProviders, Credentials, HttpClient, MessagesPlugin, OpenAiPlugin, PluginProviderInfo, ProviderAuth, ProviderAuths,
    ProviderFactory, Providers, connect, list_models, plugin_models, readable_model_name,
};
pub use messages::MessagesProvider;
pub use openai::OpenAiProvider;
pub use types::{BlockKind, ChatRequest, EventStream, LlmError, Provider, ReasoningEffort, StreamEvent, ToolSpec, UsageUpdate};
