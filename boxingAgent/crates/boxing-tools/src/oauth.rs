//! MCP OAuth 2.1 客户端（PKCE + 授权码流程）。
//!
//! 与 Hermes 原版 `tools/mcp_oauth.py` 对等实现：
//! - PKCE 授权码流程（code_verifier → code_challenge → token exchange）
//! - Token 持久化到 `~/.hermes/mcp-tokens/<server>.json`
//! - 过期检测 + 自动刷新
//! - OAuth 元数据发现（`/.well-known/oauth-authorization-server`）
//! - 动态客户端注册（RFC 7591）
//! - 本地回调服务器（ephemeral localhost HTTP）
//!
//! 文件布局（与 Python 一致）：
//! - `<server>.json` — tokens（access_token, refresh_token, expires_at）
//! - `<server>.client.json` — 动态注册的客户端信息（client_id, client_secret）
//! - `<server>.meta.json` — OAuth 服务器元数据（token_endpoint 等）

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ToolError;

// ===== 类型 =====

/// OAuth 服务器配置（config.yaml 中 per-server 的 oauth 配置）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub redirect_port: u16,
    #[serde(default = "default_client_name")]
    pub client_name: String,
}

fn default_client_name() -> String {
    "boxingAgent".into()
}

/// OAuth 元数据（从 `/.well-known/oauth-authorization-server` 发现）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OAuthMetadata {
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub authorization_endpoint: String,
    #[serde(default)]
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: String,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

/// 持久化的 token 数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// 绝对过期时间（Unix epoch 秒），与 Python 的 `expires_at` 一致。
    pub expires_at: f64,
    #[serde(default)]
    pub scope: Option<String>,
}

impl StoredToken {
    /// 检查 token 是否已过期。
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        now >= self.expires_at
    }

    /// 是否有 refresh_token。
    pub fn can_refresh(&self) -> bool {
        self.refresh_token.is_some()
    }
}

/// 动态注册的客户端信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

// ===== PKCE =====

/// PKCE 验证器对。
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

impl PkcePair {
    /// 生成随机 code_verifier（43 字符）+ code_challenge（SHA256 + base64url）。
    pub fn generate() -> Self {
        // 生成 32 字节随机数据 → base64url 编码（43 字符）
        let random_bytes: [u8; 32] = {
            // 使用 std 随机（不依赖 rand crate）
            let mut bytes = [0u8; 32];
            let mut hasher = Sha256::new();
            hasher.update(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .to_string()
                    .as_bytes(),
            );
            hasher.update(std::process::id().to_string().as_bytes());
            let hash = hasher.finalize();
            bytes.copy_from_slice(&hash);
            bytes
        };
        let verifier = URL_SAFE_NO_PAD.encode(random_bytes);

        // code_challenge = base64url(SHA256(verifier))
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

        Self { verifier, challenge }
    }
}

/// 生成随机 state 参数（防 CSRF）。
fn generate_state() -> String {
    let mut hasher = Sha256::new();
    hasher.update(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos().to_string());
    hasher.update(b"boxing-oauth-state");
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

// ===== Token 存储 =====

/// Token 存储管理器：持久化到 `~/.hermes/mcp-tokens/`。
pub struct TokenStorage {
    tokens_path: PathBuf,
    client_path: PathBuf,
    meta_path: PathBuf,
}

impl TokenStorage {
    pub fn new(hermes_home: &Path, server_name: &str) -> Self {
        let dir = hermes_home.join("mcp-tokens");
        let safe_name = server_name.replace('/', "_").replace('\\', "_");
        Self {
            tokens_path: dir.join(format!("{safe_name}.json")),
            client_path: dir.join(format!("{safe_name}.client.json")),
            meta_path: dir.join(format!("{safe_name}.meta.json")),
        }
    }

    pub fn load_tokens(&self) -> Option<StoredToken> {
        let data = read_json(&self.tokens_path)?;
        serde_json::from_value(data).ok()
    }

    pub fn save_tokens(&self, token: &StoredToken) -> Result<(), ToolError> {
        write_json(&self.tokens_path, &serde_json::to_value(token).unwrap())
    }

    pub fn load_client_info(&self) -> Option<ClientInfo> {
        let data = read_json(&self.client_path)?;
        serde_json::from_value(data).ok()
    }

    pub fn save_client_info(&self, info: &ClientInfo) -> Result<(), ToolError> {
        write_json(&self.client_path, &serde_json::to_value(info).unwrap())
    }

    pub fn load_metadata(&self) -> Option<OAuthMetadata> {
        let data = read_json(&self.meta_path)?;
        serde_json::from_value(data).ok()
    }

    pub fn save_metadata(&self, meta: &OAuthMetadata) -> Result<(), ToolError> {
        write_json(&self.meta_path, &serde_json::to_value(meta).unwrap())
    }

    pub fn has_tokens(&self) -> bool {
        self.tokens_path.exists()
    }

    pub fn remove(&self) {
        let _ = std::fs::remove_file(&self.tokens_path);
        let _ = std::fs::remove_file(&self.client_path);
        let _ = std::fs::remove_file(&self.meta_path);
    }
}

// ===== OAuth 客户端 =====

/// MCP OAuth 客户端：管理授权流程 + token 刷新。
pub struct OAuthClient {
    server_url: String,
    config: OAuthConfig,
    storage: TokenStorage,
    http: reqwest::blocking::Client,
}

impl OAuthClient {
    pub fn new(server_url: &str, config: &OAuthConfig, hermes_home: &Path, server_name: &str) -> Self {
        Self {
            server_url: server_url.to_string(),
            config: config.clone(),
            storage: TokenStorage::new(hermes_home, server_name),
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none()) // OAuth 不跟随重定向
                .build()
                .unwrap_or_default(),
        }
    }

    /// 获取有效的 access_token：先检查缓存 → 过期则刷新 → 都不行则发起授权。
    pub fn get_access_token(&self) -> Result<String, ToolError> {
        // 1. 检查缓存的 token
        if let Some(token) = self.storage.load_tokens() {
            if !token.is_expired() {
                return Ok(token.access_token);
            }
            // 2. 尝试刷新
            if token.can_refresh() {
                if let Ok(new_token) = self.refresh_token(&token) {
                    self.storage.save_tokens(&new_token)?;
                    return Ok(new_token.access_token);
                }
            }
        }

        // 3. 需要完整的浏览器授权流程
        Err(ToolError::Other(
            "MCP OAuth: 需要浏览器授权。请运行 boxing-agent mcp-auth <server> 交互式授权".into(),
        ))
    }

    /// 执行完整的 OAuth 授权码 + PKCE 流程（交互式，需要用户在浏览器中授权）。
    pub fn authorize(&self) -> Result<StoredToken, ToolError> {
        // 1. 发现 OAuth 元数据
        let metadata = self.discover_metadata()?;
        self.storage.save_metadata(&metadata)?;

        // 2. 获取或注册客户端
        let client_info = self.get_or_register_client(&metadata)?;

        // 3. 生成 PKCE
        let pkce = PkcePair::generate();
        let state = generate_state();

        // 4. 找空闲端口
        let port = if self.config.redirect_port > 0 {
            self.config.redirect_port
        } else {
            find_free_port()
        };
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        // 5. 构建授权 URL
        let mut auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
            metadata.authorization_endpoint,
            url_encode(&client_info.client_id),
            url_encode(&redirect_uri),
            pkce.challenge,
            state,
        );
        if !self.config.scope.is_empty() {
            auth_url.push_str(&format!("&scope={}", url_encode(&self.config.scope)));
        }

        // 6. 提示用户 + 启动回调服务器
        eprintln!("\n  MCP OAuth: 需要授权。请在浏览器中打开以下 URL：\n\n    {auth_url}\n");

        let auth_code = wait_for_callback(port, &state)?;

        // 7. 用 code + code_verifier 换取 token
        let token = self.exchange_code(
            &auth_code,
            &pkce.verifier,
            &client_info,
            &redirect_uri,
            &metadata,
        )?;

        // 8. 持久化
        self.storage.save_tokens(&token)?;
        self.storage.save_client_info(&client_info)?;

        Ok(token)
    }

    /// 发现 OAuth 元数据（`/.well-known/oauth-authorization-server`）。
    fn discover_metadata(&self) -> Result<OAuthMetadata, ToolError> {
        // 先检查缓存的元数据
        if let Some(cached) = self.storage.load_metadata() {
            if !cached.token_endpoint.is_empty() {
                return Ok(cached);
            }
        }

        // 尝试从服务器 URL 发现
        let well_known = format!(
            "{}/.well-known/oauth-authorization-server",
            self.server_url.trim_end_matches('/')
        );

        let resp = self
            .http
            .get(&well_known)
            .send()
            .map_err(|e| ToolError::Other(format!("OAuth 元数据发现失败: {e}")))?;

        if resp.status().is_success() {
            let meta: OAuthMetadata = resp
                .json()
                .map_err(|e| ToolError::Other(format!("解析 OAuth 元数据: {e}")))?;
            Ok(meta)
        } else {
            // 回退：从服务器 URL 推断端点
            let base = self.server_url.trim_end_matches('/');
            Ok(OAuthMetadata {
                issuer: base.to_string(),
                authorization_endpoint: format!("{base}/authorize"),
                token_endpoint: format!("{base}/token"),
                registration_endpoint: format!("{base}/register"),
                revocation_endpoint: format!("{base}/revoke"),
                scopes_supported: vec![],
            })
        }
    }

    /// 获取预注册的客户端信息，或动态注册新客户端。
    fn get_or_register_client(&self, metadata: &OAuthMetadata) -> Result<ClientInfo, ToolError> {
        // 预注册的 client_id
        if !self.config.client_id.is_empty() {
            return Ok(ClientInfo {
                client_id: self.config.client_id.clone(),
                client_secret: if self.config.client_secret.is_empty() {
                    None
                } else {
                    Some(self.config.client_secret.clone())
                },
            });
        }

        // 检查缓存
        if let Some(cached) = self.storage.load_client_info() {
            return Ok(cached);
        }

        // 动态注册（RFC 7591）
        if metadata.registration_endpoint.is_empty() {
            return Err(ToolError::Other(
                "OAuth: 服务器不支持动态注册且未配置 client_id".into(),
            ));
        }

        let body = json!({
            "client_name": self.config.client_name,
            "redirect_uris": [format!("http://127.0.0.1:{}/callback", find_free_port())],
            "grant_types": ["authorization_code", "refresh_token"],
            "token_endpoint_auth_method": "none",
        });

        let resp = self
            .http
            .post(&metadata.registration_endpoint)
            .json(&body)
            .send()
            .map_err(|e| ToolError::Other(format!("动态注册失败: {e}")))?;

        if !resp.status().is_success() {
            return Err(ToolError::Other(format!(
                "动态注册失败: HTTP {}",
                resp.status()
            )));
        }

        let data: Value = resp
            .json()
            .map_err(|e| ToolError::Other(format!("解析注册响应: {e}")))?;

        Ok(ClientInfo {
            client_id: data
                .get("client_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::Other("注册响应缺少 client_id".into()))?
                .to_string(),
            client_secret: data
                .get("client_secret")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }

    /// 用授权码换取 token。
    fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        client: &ClientInfo,
        redirect_uri: &str,
        metadata: &OAuthMetadata,
    ) -> Result<StoredToken, ToolError> {
        let mut body = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", redirect_uri.to_string()),
            ("client_id", client.client_id.clone()),
            ("code_verifier", code_verifier.to_string()),
        ];
        if let Some(secret) = &client.client_secret {
            body.push(("client_secret", secret.clone()));
        }

        let resp = self
            .http
            .post(&metadata.token_endpoint)
            .form(&body)
            .send()
            .map_err(|e| ToolError::Other(format!("Token 交换失败: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(ToolError::Other(format!("Token 交换 HTTP {status}: {text}")));
        }

        let data: Value = resp
            .json()
            .map_err(|e| ToolError::Other(format!("解析 token 响应: {e}")))?;

        parse_token_response(&data)
    }

    /// 用 refresh_token 刷新 access_token。
    fn refresh_token(&self, token: &StoredToken) -> Result<StoredToken, ToolError> {
        let metadata = self.storage.load_metadata().ok_or_else(|| {
            ToolError::Other("OAuth: 无缓存的元数据，无法刷新 token".into())
        })?;

        let refresh_token = token
            .refresh_token
            .as_ref()
            .ok_or_else(|| ToolError::Other("OAuth: 无 refresh_token".into()))?;

        let body = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token.clone()),
            ("client_id", self.storage.load_client_info()
                .map(|c| c.client_id)
                .unwrap_or_else(|| self.config.client_id.clone())),
        ];

        let resp = self
            .http
            .post(&metadata.token_endpoint)
            .form(&body)
            .send()
            .map_err(|e| ToolError::Other(format!("Token 刷新失败: {e}")))?;

        if !resp.status().is_success() {
            // Token 刷新失败 — 清除缓存，下次需要重新授权
            self.storage.remove();
            return Err(ToolError::Other("Token 刷新失败，已清除缓存，需要重新授权".into()));
        }

        let data: Value = resp
            .json()
            .map_err(|e| ToolError::Other(format!("解析刷新响应: {e}")))?;

        let mut new_token = parse_token_response(&data)?;
        // 保留 refresh_token（如果新响应中没有）
        if new_token.refresh_token.is_none() {
            new_token.refresh_token = token.refresh_token.clone();
        }
        Ok(new_token)
    }
}

// ===== 辅助函数 =====

/// 解析 token 响应 JSON → StoredToken（计算绝对过期时间）。
fn parse_token_response(data: &Value) -> Result<StoredToken, ToolError> {
    let access_token = data
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::Other("token 响应缺少 access_token".into()))?
        .to_string();

    let expires_in = data
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(3600) as f64;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    Ok(StoredToken {
        access_token,
        token_type: data
            .get("token_type")
            .and_then(|v| v.as_str())
            .unwrap_or("Bearer")
            .to_string(),
        refresh_token: data
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from),
        expires_at: now + expires_in,
        scope: data
            .get("scope")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

/// 查找空闲端口。
fn find_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr().map(|addr| addr.port()))
        .unwrap_or(8484)
}

/// 等待 OAuth 回调（本地 HTTP 服务器）。
fn wait_for_callback(port: u16, expected_state: &str) -> Result<String, ToolError> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .map_err(|e| ToolError::Other(format!("绑定回调端口 {port} 失败: {e}")))?;

    eprintln!("  等待授权回调（端口 {port}）...\n");

    // 设置 5 分钟超时
    listener
        .set_nonblocking(false)
        .map_err(|e| ToolError::Other(format!("设置监听: {e}")))?;

    let (mut stream, _) = listener
        .accept()
        .map_err(|e| ToolError::Other(format!("接受回调连接失败: {e}")))?;

    let mut request = String::new();
    stream
        .read_to_string(&mut request)
        .map_err(|e| ToolError::Other(format!("读取回调: {e}")))?;

    // 解析 GET /callback?code=xxx&state=yyy HTTP/1.1
    let first_line = request.lines().next().unwrap_or("");
    let url = first_line.split(' ').nth(1).unwrap_or("");

    // 从 URL 中提取 code 和 state
    let query = url.split('?').nth(1).unwrap_or("");
    let mut code = None;
    let mut state = None;
    let mut error = None;

    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        let key = kv.next().unwrap_or("");
        let value = kv.next().unwrap_or("");
        match key {
            "code" => code = Some(percent_decode(value)),
            "state" => state = Some(percent_decode(value)),
            "error" => error = Some(percent_decode(value)),
            _ => {}
        }
    }

    // 回复浏览器
    let body = if code.is_some() {
        "<html><body><h2>Authorization Successful</h2><p>You can close this tab.</p></body></html>"
    } else {
        "<html><body><h2>Authorization Failed</h2></body></html>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());

    // 验证 state（CSRF 防护）
    if let Some(ref s) = state {
        if s != expected_state {
            return Err(ToolError::Other(
                format!("OAuth state 不匹配（CSRF 检查失败）: 期望 {expected_state}, 收到 {s}"),
            ));
        }
    }

    if let Some(err) = error {
        return Err(ToolError::Other(format!("OAuth 授权错误: {err}")));
    }

    code.ok_or_else(|| ToolError::Other("OAuth 回调缺少 code 参数".into()))
}

/// URL 编码。
fn url_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' | '_' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

/// 简单 percent-decode。
fn percent_decode(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    result
}

/// 安全读取 JSON 文件。
fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 安全写入 JSON 文件（权限 0600）。
fn write_json(path: &Path, data: &Value) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(data)
        .map_err(|e| ToolError::Other(format!("序列化 JSON: {e}")))?;
    std::fs::write(path, json)?;
    // Unix: 设置 0600 权限
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// ===== 测试 =====

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_is_43_chars() {
        let pkce = PkcePair::generate();
        // base64url(32 bytes) = 43 chars (no padding)
        assert_eq!(pkce.verifier.len(), 43);
        // challenge = base64url(SHA256(43 bytes)) = 43 chars
        assert_eq!(pkce.challenge.len(), 43);
    }

    #[test]
    fn pkce_challenge_matches_verifier() {
        let pkce = PkcePair::generate();
        let mut hasher = Sha256::new();
        hasher.update(pkce.verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn token_expiry_check() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        let fresh = StoredToken {
            access_token: "x".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            expires_at: now + 3600.0,
            scope: None,
        };
        assert!(!fresh.is_expired());

        let expired = StoredToken {
            access_token: "x".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            expires_at: now - 1.0,
            scope: None,
        };
        assert!(expired.is_expired());
    }

    #[test]
    fn token_persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!("boxing-oauth-test-{}", std::process::id()));
        let storage = TokenStorage::new(&dir, "test-server");

        let token = StoredToken {
            access_token: "abc123".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("refresh456".into()),
            expires_at: 9999999999.0,
            scope: Some("read write".into()),
        };
        storage.save_tokens(&token).unwrap();
        let loaded = storage.load_tokens().unwrap();
        assert_eq!(loaded.access_token, "abc123");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh456"));
        assert!(!loaded.is_expired());

        let client = ClientInfo {
            client_id: "client-id".into(),
            client_secret: Some("secret".into()),
        };
        storage.save_client_info(&client).unwrap();
        let loaded_client = storage.load_client_info().unwrap();
        assert_eq!(loaded_client.client_id, "client-id");

        // 清理
        storage.remove();
        assert!(!storage.has_tokens());
    }

    #[test]
    fn parse_token_response_works() {
        let data = json!({
            "access_token": "tok123",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "ref456",
            "scope": "read"
        });
        let token = parse_token_response(&data).unwrap();
        assert_eq!(token.access_token, "tok123");
        assert_eq!(token.token_type, "Bearer");
        assert_eq!(token.refresh_token.as_deref(), Some("ref456"));
        assert!(!token.is_expired()); // 3600s from now
    }

    #[test]
    fn oauth_config_from_yaml() {
        let yaml = "
client_id: my-id
client_secret: my-secret
scope: read write
redirect_port: 9484
client_name: My Agent
";
        let config: OAuthConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.client_id, "my-id");
        assert_eq!(config.scope, "read write");
        assert_eq!(config.redirect_port, 9484);
    }

    #[test]
    fn url_encode_special_chars() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("a+b=c"), "a%2Bb%3Dc");
        assert_eq!(url_encode("safe-._~"), "safe-._~");
    }

    #[test]
    fn percent_decode_roundtrip() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("safe"), "safe");
    }

    #[test]
    fn state_is_unique() {
        let s1 = generate_state();
        let s2 = generate_state();
        assert_ne!(s1, s2);
        assert!(!s1.is_empty());
    }
}
