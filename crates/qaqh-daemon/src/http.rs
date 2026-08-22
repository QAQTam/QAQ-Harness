//! daemon 内部共享 HTTP 管线（手写 TCP 实现，无框架依赖）。
//!
//! Ringing HTTP/SSE 与 webUI 静态托管（debug_http）共用同一套请求解析与
//! 响应写出，避免两份各自漂移的 HTTP 实现。SSE 长连接的写出不在此层
//! （ringing_http 自行管理流式响应）。

use std::collections::HashMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 请求 body 上限。
pub(crate) const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// 已解析的 HTTP 请求。
pub(crate) struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|v| v.as_str())
    }
}

/// 读取并解析一个 HTTP 请求（请求行 + headers + Content-Length body）。
///
/// 总是消费完整 body：socket 中不留未读数据，后续 `Connection: close`
/// 的正常关闭不会触发客户端 RST（Windows 上带未读数据 close 会丢响应）。
pub(crate) async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0_u8; 2048];
    // 先找 header 结束标记（\r\n\r\n）
    let header_end = loop {
        let text = String::from_utf8_lossy(&buf);
        if let Some(pos) = text.find("\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp).await.map_err(stringify)?;
        if n == 0 {
            return Err("connection closed before headers".into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 {
            return Err("request headers too large".into());
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "missing request line".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing method".to_string())?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| "missing path".to_string())?
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err("body too large".into());
    }
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp).await.map_err(stringify)?;
        if n == 0 {
            return Err("connection closed during body".into());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[header_end..header_end + content_length].to_vec();
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

pub(crate) fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// 写出完整响应（Ringing 端点的原始语义：不半关闭、不排空）。
///
/// Ringing 路径经 [`read_request`] 已消费完整 body，socket 无未读数据，
/// 正常 close 即可；且 SSE 长连接绝不能在此层关闭。
/// webUI 静态响应用 [`write_response_no_cache`]（带优雅关闭）。
pub(crate) async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.map_err(stringify)?;
    stream.write_all(body).await.map_err(stringify)?;
    stream.flush().await.map_err(stringify)?;
    Ok(())
}

/// 静态资源响应：`Cache-Control: no-cache` + 优雅关闭。
///
/// - no-cache：入口页注入桥脚本（含 daemon token），禁止缓存旧页面；
/// - 半关闭后短暂排空接收缓冲：静态处理器可能未读客户端后续字节，
///   Windows 上带未读数据直接 close 会以 RST 收场，客户端收不全 body。
pub(crate) async fn write_response_no_cache(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.map_err(stringify)?;
    stream.write_all(body).await.map_err(stringify)?;
    stream.flush().await.map_err(stringify)?;
    graceful_close(stream).await;
    Ok(())
}

/// 半关闭 + 短超时排空残余入站数据（见 [`write_response_no_cache`] 注释）。
async fn graceful_close(stream: &mut TcpStream) {
    if stream.shutdown().await.is_err() {
        return;
    }
    let mut sink = [0_u8; 4096];
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        stream.read(&mut sink),
    )
    .await;
}
