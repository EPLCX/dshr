//! TLS 的加密后端。
//!
//! 整个程序只用 rustls 的 aws-lc-rs 后端（reqwest 默认就是它）。rustls 只开一个后端时能自己选，
//! 但 `ureq`（`rustls-no-provider`）只认进程级的默认值，所以启动时先装上；用到 TLS 的地方再各调一次，
//! 不依赖调用顺序（重复调用无副作用）。
//!
//! 以前 `ureq` 打开了 `ring`、reqwest 打开了 `aws-lc-rs`：两个后端同时在时 rustls 选不出默认值，
//! tokio-tungstenite 连 `wss://` 时直接 panic（发布版 `panic = "abort"`，整个程序退出）。

/// 把 aws-lc-rs 装成进程级的默认 CryptoProvider；已经有了就什么也不做。
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn client_config_builds_with_the_default_provider() {
        super::install_crypto_provider();
        super::install_crypto_provider();
        // 两个后端同时打开时这一步会 panic。
        let _ = rustls::ClientConfig::builder().with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth();
    }
}
