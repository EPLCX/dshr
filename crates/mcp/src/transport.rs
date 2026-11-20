//! 传输：把 JSON-RPC 消息送到服务器，把服务器发来的消息交回给 [`crate::Client`]。
//!
//! 有两种：stdio（子进程的标准输入输出，每行一条消息）和 streamable HTTP（见 `http.rs`）。
//! 服务器发来的消息（响应、请求、通知）都放进同一个接收端，由客户端的读循环分发。

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::McpError;

/// 服务器发来的消息。发送端全部释放时，客户端认为连接已经断开。
pub type Incoming = mpsc::UnboundedReceiver<Value>;

#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// 发一条消息。连接已经不能用（进程退出、HTTP 会话失效、服务器连不上）时返回
    /// [`McpError::Closed`] 或 [`McpError::Unreachable`]，客户端据此认为连接断了。
    async fn send(&self, message: &Value) -> Result<(), McpError>;

    /// 握手协商出的协议版本（HTTP 传输之后的每个请求都带上它）。在发 `notifications/initialized` 之前调用。
    fn set_protocol_version(&self, _version: &str) {}

    /// 握手完成：HTTP 传输开始收服务器主动发来的消息。
    async fn initialized(&self) {}

    /// 主动断开（HTTP 传输告诉服务器结束会话）。
    async fn close(&self) {}
}

/// 子进程的 stdio：每行一条 JSON。
pub struct StdioTransport {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    reader: JoinHandle<()>,
}

/// 在一对读写流上建立 stdio 传输（需要在 tokio 运行时里调用）。
pub fn stdio(
    reader: impl AsyncRead + Send + Unpin + 'static,
    writer: impl AsyncWrite + Send + Unpin + 'static,
) -> (Arc<dyn Transport>, Incoming) {
    let (sender, incoming) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        // 按字节读行：`lines()` 遇到一行不是 UTF-8（服务器往 stdout 打的日志里带了本地编码的文字）就报错，
        // 读循环随之结束，整个连接被当成断开。这里只跳过那一行。
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // 有的服务器会往 stdout 打日志，不是 JSON 的行跳过。
            let Ok(message) = serde_json::from_slice::<Value>(&line) else { continue };
            if sender.send(message).is_err() {
                break;
            }
        }
    });
    let transport = StdioTransport { writer: tokio::sync::Mutex::new(Box::new(writer)), reader };
    (Arc::new(transport), incoming)
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    async fn send(&self, message: &Value) -> Result<(), McpError> {
        let mut line = message.to_string();
        line.push('\n');
        let mut writer = self.writer.lock().await;
        let written = async {
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await
        };
        written.await.map_err(|error| match error.kind() {
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof => McpError::Closed,
            _ => McpError::Io(error),
        })
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn non_utf8_lines_are_skipped_not_fatal() {
        let input: &[u8] = b"\xff\xfe log in GBK \xd6\xd0\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\nplain log\n{\"id\":2}";
        let (_transport, mut incoming) = stdio(input, tokio::io::sink());
        assert_eq!(incoming.recv().await.unwrap()["id"], 1);
        // 最后一行没有换行也照收。
        assert_eq!(incoming.recv().await.unwrap()["id"], 2);
        assert!(incoming.recv().await.is_none());
    }
}
