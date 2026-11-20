//! API Key 存取：Windows 凭据管理器 / macOS 钥匙串，每个提供商一条。

use std::env;

use crate::ResolvedProvider;

const SERVICE: &str = "dshr";

#[derive(Debug, thiserror::Error)]
#[error("系统钥匙串不可用: {0}")]
pub struct CredentialError(#[from] keyring::Error);

/// API Key 的来源，按优先级从高到低。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// 系统钥匙串（设置页保存的）。
    Keyring,
    /// 适配器约定的环境变量（如 `DEEPSEEK_API_KEY`），设置页里 Key 留空时使用。
    Env,
}

/// 钥匙串条目名。DeepSeek 的条目名沿用早期版本的 `deepseek-api-key`。
fn account(provider_id: &str) -> String {
    format!("{provider_id}-api-key")
}

pub fn load_api_key(provider_id: &str) -> Result<Option<String>, CredentialError> {
    match keyring::Entry::new(SERVICE, &account(provider_id))?.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn save_api_key(provider_id: &str, key: &str) -> Result<(), CredentialError> {
    Ok(keyring::Entry::new(SERVICE, &account(provider_id))?.set_password(key.trim())?)
}

pub fn delete_api_key(provider_id: &str) -> Result<(), CredentialError> {
    match keyring::Entry::new(SERVICE, &account(provider_id))?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// 按 钥匙串 → 环境变量 的顺序查找 API Key。钥匙串读取失败按“没有”处理。
pub fn resolve_api_key(provider: &ResolvedProvider) -> Option<(String, KeySource)> {
    let non_empty = |key: String| Some(key).filter(|key| !key.trim().is_empty());
    if let Some(key) = load_api_key(&provider.id).ok().flatten().and_then(non_empty) {
        return Some((key, KeySource::Keyring));
    }
    env_api_key(provider).map(|key| (key, KeySource::Env))
}

pub fn env_api_key(provider: &ResolvedProvider) -> Option<String> {
    provider.env_key.and_then(|name| env::var(name).ok()).filter(|key| !key.trim().is_empty())
}

/// 提供商是否可用：不需要 Key，或能找到 Key。
/// 账号登录的提供商（`deepseek-oauth`）不吃 API Key，只认登录令牌，这里始终不算就绪：
/// 它的就绪状态由界面按「是否已登录」单独判断。
pub fn is_ready(provider: &ResolvedProvider) -> bool {
    if provider.id == crate::providers::DEEPSEEK_OAUTH {
        return false;
    }
    !provider.requires_key || resolve_api_key(provider).is_some()
}

/// 设置页里展示的掩码形式，如 `sk-a…f9c2`。
pub fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "•".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// 任意一条钥匙串条目（插件密钥等）。`account` 是条目名，服务名与 API Key 相同。
pub fn load_secret(account: &str) -> Result<Option<String>, CredentialError> {
    match keyring::Entry::new(SERVICE, account)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn save_secret(account: &str, value: &str) -> Result<(), CredentialError> {
    Ok(keyring::Entry::new(SERVICE, account)?.set_password(value)?)
}

pub fn delete_secret(account: &str) -> Result<(), CredentialError> {
    match keyring::Entry::new(SERVICE, account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_keys() {
        assert_eq!(mask("sk-abcdef123456"), "sk-a…3456");
        assert_eq!(mask("short"), "•••••");
    }

    #[test]
    fn deepseek_keeps_legacy_account_name() {
        assert_eq!(account("deepseek"), "deepseek-api-key");
    }
}
