//! DeepSeek HTTP 客户端 —— 原始 API 调用层
//!
//! 无状态管理：无缓存、无重试、无会话状态（唯一的状态是 HIF 风控令牌句柄，
//! 缓存逻辑在 `hif.rs`，本模块只负责把它拼进 SSE 请求头）。
//! 每个方法对应一个 REST 端点（详见 docs/ds-api-reference.md）。
//! 流方法（completion/edit_message）返回原始字节流，由上层解析 SSE。
//!
//! 仅包含最小业务逻辑：HTTP 错误码和业务错误码解析（into_result）。

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use thiserror::Error;
use wreq::multipart::{Form, Part};
use wreq_util::Emulation;

// API 端点常量
const ENDPOINT_USERS_LOGIN: &str = "/users/login";
#[allow(dead_code)]
const ENDPOINT_USERS_CURRENT: &str = "/users/current";
const ENDPOINT_USERS_AUTH_TOKEN_CHECK_DEVICE: &str = "/users/auth_token/check_device";
const ENDPOINT_CHAT_SESSION_CREATE: &str = "/chat_session/create";
const ENDPOINT_CHAT_SESSION_DELETE: &str = "/chat_session/delete";
const ENDPOINT_CHAT_SESSION_FETCH_PAGE: &str = "/chat_session/fetch_page";
#[allow(dead_code)]
const ENDPOINT_CHAT_SESSION_UPDATE_TITLE: &str = "/chat_session/update_title";
const ENDPOINT_CHAT_CREATE_POW_CHALLENGE: &str = "/chat/create_pow_challenge";
const ENDPOINT_CHAT_COMPLETION: &str = "/chat/completion";
#[allow(dead_code)]
const ENDPOINT_CHAT_EDIT_MESSAGE: &str = "/chat/edit_message";
const ENDPOINT_CHAT_STOP_STREAM: &str = "/chat/stop_stream";
const ENDPOINT_FILE_UPLOAD: &str = "/file/upload_file";
const ENDPOINT_FILE_FETCH: &str = "/file/fetch_files";

#[derive(Debug, Error)]
pub enum ClientError {
    /// HTTP 层错误（网络、超时、DNS 等）
    #[error("HTTP error: {0}")]
    Http(#[from] wreq::Error),

    /// HTTP 状态码非 2xx
    #[error("HTTP status {status}: {body}")]
    Status { status: u16, body: String },

    /// 业务错误：API 返回 HTTP 200 但 biz_code 非 0
    #[error("Business error: code={code}, msg={msg}")]
    Business { code: i64, msg: String },

    /// JSON 解析失败
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// Header 值包含非法字符
    #[error("Invalid header value: {0}")]
    InvalidHeader(String),
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    code: i64,
    msg: String,
    data: Option<EnvelopeData<T>>,
}

#[derive(Debug, Deserialize)]
struct EnvelopeData<T> {
    biz_code: i64,
    biz_msg: String,
    biz_data: Option<T>,
}

impl<T: serde::de::DeserializeOwned> Envelope<T> {
    fn into_result(self) -> Result<T, ClientError> {
        if self.code != 0 {
            return Err(ClientError::Business {
                code: self.code,
                msg: self.msg,
            });
        }
        let data = self.data.ok_or_else(|| ClientError::Business {
            code: -1,
            msg: "missing data".into(),
        })?;
        if data.biz_code != 0 {
            return Err(ClientError::Business {
                code: data.biz_code,
                msg: data.biz_msg,
            });
        }
        data.biz_data.map_or_else(
            || {
                // 允许 biz_data 为 null，尝试从 null 构造 T（仅当 T 是 Option 时成功）
                serde_json::from_value(serde_json::Value::Null).map_err(|_| ClientError::Business {
                    code: -1,
                    msg: "missing biz_data".into(),
                })
            },
            Ok,
        )
    }
}

/// 登录 payload
///
/// 字段与真实客户端一一对应：`email` / `mobile` 二选一，另一个传空串
/// （**不是省略也不是 null** —— 真实客户端固定发 6 个 key）。
#[derive(Debug, Serialize)]
pub struct LoginPayload {
    pub email: String,
    pub mobile: String,
    pub password: String,
    pub area_code: String,
    pub device_id: String,
    pub os: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginData {
    pub code: i64,
    pub msg: String,
    pub user: UserInfo,
}

/// 账号禁言状态（登录响应与 /users/current 均返回）
#[derive(Debug, Clone, Deserialize)]
pub struct ChatStatus {
    /// 1 = 被禁言；接受 int / bool 两种形态
    #[serde(default, deserialize_with = "de_muted_flag")]
    pub is_muted: i64,
    /// 解封时间戳（Unix 秒，浮点）；None = 未设置
    #[serde(default)]
    pub mute_until: Option<f64>,
}

/// 兼容 int(0/1) 与 bool 两种 is_muted 形态
fn de_muted_flag<'de, D>(d: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Bool(b) => i64::from(b),
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
        _ => 0,
    })
}

#[derive(Debug, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub token: String,
    pub email: Option<String>,
    pub mobile_number: Option<String>,
    /// 禁言状态（缺失视为未禁言）
    #[serde(default)]
    pub chat: Option<ChatStatus>,
}

/// POST /users/auth_token/check_device 响应
#[derive(Debug, Deserialize)]
pub struct CheckDeviceData {
    /// 服务端下发的令牌轮换指令（通常为 null）
    pub rotate: Option<serde_json::Value>,
}

/// GET /chat_session/fetch_page 响应中的单个会话
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatSessionInfo {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub title_type: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub model_type: Option<String>,
    /// 更新时间戳（Unix 秒，浮点）—— 分页游标
    #[serde(default)]
    pub updated_at: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct FetchSessionsData {
    #[serde(default)]
    pub chat_sessions: Vec<ChatSessionInfo>,
    #[serde(default)]
    pub has_more: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CreateSessionData {
    pub id: String,
}

// 包装类型：biz_data 里面嵌套了 chat_session 对象
#[derive(Debug, Deserialize)]
struct CreateSessionWrapper {
    chat_session: CreateSessionData,
}

#[derive(Debug, Deserialize)]
pub struct UploadFileData {
    pub id: String,
    #[allow(dead_code)]
    pub status: String,
    #[allow(dead_code)]
    pub file_name: String,
    #[allow(dead_code)]
    pub file_size: i64,
}

#[derive(Debug, Deserialize)]
pub struct FetchFilesData {
    pub files: Vec<FileInfo>,
}

#[derive(Debug, Deserialize)]
pub struct FileInfo {
    #[allow(dead_code)]
    pub id: String,
    pub status: String,
    pub file_name: String,
    #[allow(dead_code)]
    pub file_size: i64,
    #[serde(default)]
    pub token_usage: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ChallengeData {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub signature: String,
    pub difficulty: i64,
    #[allow(dead_code)]
    pub expire_after: i64,
    pub expire_at: i64,
    pub target_path: String,
}

// 包装类型：biz_data 里面嵌套了 challenge 对象
#[derive(Debug, Deserialize)]
struct ChallengeWrapper {
    challenge: ChallengeData,
}

#[derive(Debug, Serialize)]
pub struct CompletionPayload {
    pub chat_session_id: String,
    /// 上一条响应的消息 id；会话首条为 None（抓包中**显式序列化为 null**，不省略）
    pub parent_message_id: Option<i64>,
    /// 模型类型；会话首条消息携带（如 `"default"`），后续消息为 `null`
    /// （服务端沿用首条的设定，抓包 2026-10-10）
    pub model_type: Option<String>,
    pub prompt: String,
    pub ref_file_ids: Vec<String>,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
    /// 真实客户端固定发送该字段（会话首条为 null）
    pub action: Option<serde_json::Value>,
    pub preempt: bool,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct EditMessagePayload {
    pub chat_session_id: String,
    pub message_id: i64,
    pub prompt: String,
    pub search_enabled: bool,
    pub thinking_enabled: bool,
    pub model_type: String,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct UpdateTitlePayload {
    pub chat_session_id: String,
    pub title: String,
}

#[derive(Debug, Serialize)]
pub struct StopStreamPayload {
    pub chat_session_id: String,
    pub message_id: i64,
}

/// Check if a response is an AWS WAF Challenge (US IP restriction)
fn is_waf_challenge(resp: &wreq::Response) -> bool {
    resp.status().as_u16() == 202 && resp.headers().get("x-amzn-waf-action").is_some()
}

/// Print a hint when WAF challenge is detected
fn print_waf_hint() {
    warn!(target: "ds_core::client", "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    warn!(target: "ds_core::client", "  AWS WAF Challenge detected.");
    warn!(target: "ds_core::client", "  DeepSeek CloudFront WAF blocks US-based IPs.");
    warn!(target: "ds_core::client", "  Rust HTTP clients can't execute the JS challenge.");
    warn!(target: "ds_core::client", "");
    warn!(target: "ds_core::client", "  To fix this, configure a non-US proxy in config.toml:");
    warn!(target: "ds_core::client", "    [proxy]");
    warn!(target: "ds_core::client", "    url = \"http://127.0.0.1:7890\"");
    warn!(target: "ds_core::client", "");
    warn!(target: "ds_core::client", "  https://github.com/niyue/ds-free-api");
    warn!(target: "ds_core::client", "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}

/// 默认 HIF（风控令牌）取值端点
const DEFAULT_HIF_LEIM_URL: &str = "https://hif-leim.deepseek.com/query";
/// 第二个风控令牌端点（前端 `dliqPoller`；本网络常为 NXDOMAIN，取不到就跳过）
const DEFAULT_HIF_DLIQ_URL: &str = "https://hif-dliq.deepseek.com/query";

/// 传输层拟态档位（TLS/HTTP2 指纹 + 该档位自带的默认请求头）
///
/// 关键在于**自洽**：安卓 App UA 配 Chrome/macOS 的 client hints 与
/// 「地址栏导航」头（`sec-fetch-dest: document`、`accept: text/html,…`）是
/// 任何真实客户端都不会有的组合（2026-10-07 本地 echo server 取证）。
///
/// **默认档位是浏览器**（`chrome136`）：官方 Web 客户端就是一套 Chrome 身份，
/// 而「对齐真实客户端」是降低风控暴露面的唯一可操作原则 —— 我们并不知道上游
/// 具体依据哪些信号判定违规，因此每一项都向真实客户端靠拢（见 `docs/development.md`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EmulationProfile {
    /// 原生安卓 App（OkHttp）：默认头只有 `accept: */*` / `accept-language` / UA，
    /// 与 `user_agent = "DeepSeek/… Android/…"` + `client_platform = android` 自洽
    OkHttp4_12,
    /// 桌面 Chrome（默认）：带 `sec-ch-ua*` / `sec-fetch-*` / 文档 accept，
    /// 需要同时把 UA / `client_platform` / `client_os` 切成 web 才自洽
    #[default]
    Chrome136,
}

impl EmulationProfile {
    /// 解析配置值（未知值返回 None，由调用方告警并回退默认档位）
    #[must_use]
    pub fn from_config(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "okhttp4_12" | "okhttp_4_12" | "okhttp" | "app" => Some(Self::OkHttp4_12),
            "chrome136" | "chrome_136" | "chrome" | "web" => Some(Self::Chrome136),
            _ => None,
        }
    }

    const fn to_wreq(self) -> Emulation {
        match self {
            Self::OkHttp4_12 => Emulation::OkHttp4_12,
            Self::Chrome136 => Emulation::Chrome136,
        }
    }
}

/// 请求的发起语境（决定 `sec-fetch-site`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchSite {
    /// 同源 XHR（`chat.deepseek.com` 的业务 API）
    SameOrigin,
    /// 跨源 XHR（`hif-leim.deepseek.com` 之类的风控令牌端点）
    CrossSite,
}

impl FetchSite {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SameOrigin => "same-origin",
            Self::CrossSite => "cross-site",
        }
    }
}

/// 客户端拟态身份（UA + x-client-* 请求头 + 登录 payload 的 os）
#[derive(Debug, Clone, Default)]
pub struct ClientIdentity {
    pub user_agent: String,
    pub client_version: String,
    pub client_platform: String,
    pub client_locale: String,
    pub client_bundle_id: String,
    /// X-Device-Id（设备级 UUID）；空 = 按 api_base 确定性派生（兜底，
    /// 根 crate 会在首启生成随机 UUID 并写入配置）
    pub device_id: String,
    pub device_model: String,
    pub timezone_offset: String,
    /// 登录 payload 的 os 字段
    pub client_os: String,
    /// Cookie 种子（手动令牌模式下由账号配置提供；空 = 无种子）
    ///
    /// 真实客户端的每个 API 请求都带浏览器会话 Cookie（`ds_session_id`、
    /// 数美指纹 `smidV2`、WAF 会话 `HWWAFSESID` 等，抓包 2026-10-10）。
    /// 该值在客户端构造时**灌入 Cookie jar**，此后 jar 自动捕获每个响应的
    /// `Set-Cookie`（WAF 会话续期等）并按域名作用域回传 —— 与浏览器行为一致。
    /// 静态 `Cookie` 请求头由此废弃：jar 与静态头并存会出现重复 Cookie。
    pub cookies: String,
}

impl ClientIdentity {
    /// 客户端通用请求头（登录、鉴权请求与 HIF 取令牌共用）
    ///
    /// 对齐真实客户端（2026-10-07 真实浏览器抓包）：除 UA 外还带 7 个 x-* 头，
    /// 缺这些头会明显拉低「像真实客户端」的拟态保真度。
    ///
    /// **同时显式覆盖传输层档位自带的「地址栏导航」头**：wreq 的 `chrome136`
    /// 档默认发 `sec-fetch-dest: document` / `sec-fetch-mode: navigate` /
    /// `sec-fetch-site: none` / `accept: text/html,…` / `priority`，
    /// 且 `sec-ch-ua-platform` 固定 `"macOS"`（与 Windows UA 矛盾）——
    /// 而真实客户端的业务 API 是页面内的 **XHR/fetch**
    /// （抓包：`accept: */*`、`sec-ch-ua-platform: "Windows"`、无 `priority`）。
    /// 本地 echo server 逐头比对见 `docs/development.md`。
    fn headers(&self, site: FetchSite) -> Result<wreq::header::HeaderMap, ClientError> {
        let mut h = wreq::header::HeaderMap::new();
        let mut put = |name: &'static str, value: &str| -> Result<(), ClientError> {
            h.insert(
                name,
                wreq::header::HeaderValue::from_str(value)
                    .map_err(|e| ClientError::InvalidHeader(format!("{name}: {e}")))?,
            );
            Ok(())
        };
        put(wreq::header::USER_AGENT.as_str(), &self.user_agent)?;
        put("X-Client-Version", &self.client_version)?;
        put("X-Client-Platform", &self.client_platform)?;
        put("X-Client-Locale", &self.client_locale)?;
        put("X-Client-Bundle-Id", &self.client_bundle_id)?;
        put("X-Device-Id", &self.device_id)?;
        put("X-Device-Model", &self.device_model)?;
        put("X-Client-Timezone-Offset", &self.timezone_offset)?;
        // 默认头里的 accept-language 固定 en-US，与 client_locale（默认 zh_CN）不符
        if !self.client_locale.is_empty() {
            put(
                wreq::header::ACCEPT_LANGUAGE.as_str(),
                &self.client_locale.replace('_', "-"),
            )?;
        }
        // 官方 Web 客户端对每个 API 请求都带 Referer（抓包：https://chat.deepseek.com/）
        put(wreq::header::REFERER.as_str(), "https://chat.deepseek.com/")?;
        // —— 覆盖档位默认的「导航头」，改成页面内 fetch/XHR 的真实形态 ——
        put(wreq::header::ACCEPT.as_str(), "*/*")?;
        put("Sec-Fetch-Dest", "empty")?;
        put("Sec-Fetch-Mode", "cors")?;
        put("Sec-Fetch-Site", site.as_str())?;
        // client hints 必须与 UA 自洽：版本号从 UA 的 Chrome/<maj> 提取
        // （手动令牌模式下把 UA 改成登录浏览器时，client hints 自动跟随）
        let major = chrome_major_from_ua(&self.user_agent);
        put(
            "Sec-Ch-Ua",
            &format!(
                "\"Chromium\";v=\"{major}\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"{major}\""
            ),
        )?;
        put("Sec-Ch-Ua-Mobile", "?0")?;
        put("Sec-Ch-Ua-Platform", "\"Windows\"")?;
        Ok(h)
    }
}

/// 从 UA 提取 Chrome 主版本号；UA 不含 `Chrome/<digits>` 时回退 140
///
/// 用于派生 `sec-ch-ua` 的版本号，保证手动改 UA（如对齐登录浏览器）时
/// client hints 不与 UA 矛盾。
fn chrome_major_from_ua(ua: &str) -> &str {
    ua.split("Chrome/")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or("140")
}

/// HIF 风控令牌配置
#[derive(Debug, Clone)]
pub struct HifConfig {
    /// 是否在 completion 请求上回传 `x-hif-leim` / `x-hif-dliq`（关闭仅用于对照实验）
    pub enabled: bool,
    /// `x-hif-leim` 取令牌端点
    pub leim_url: String,
    /// `x-hif-dliq` 取令牌端点（真实客户端的第二个轮询器）
    pub dliq_url: String,
}

impl Default for HifConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            leim_url: DEFAULT_HIF_LEIM_URL.to_string(),
            dliq_url: DEFAULT_HIF_DLIQ_URL.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct DsClient {
    http: wreq::Client,
    api_base: String,
    wasm_url: String,
    identity: ClientIdentity,
    /// HIF 风控令牌注册表（`x-hif-leim` + `x-hif-dliq`，按设备身份分桶）；None = 已关闭
    hif: Option<std::sync::Arc<super::hif::HifRegistry>>,
    /// 传输层拟态档位（账号视图重建客户端时复用）
    emulation: EmulationProfile,
    /// 全局代理（账号视图重建客户端时复用，保持出口一致）
    proxy_url: Option<String>,
}

/// 把 Cookie 头串（`k1=v1; k2=v2`）拆成可灌入 jar 的 Set-Cookie 行
///
/// 浏览器复制的 Cookie 请求头没有属性；按 api_base 的主机补 `Path=/; Secure`，
/// 变成 host-only 会话 Cookie（与浏览器为 ds_session_id 记录的形态一致）。
/// 无法解析的片段跳过并告警。
fn cookie_seed_lines(api_base: &str, cookies: &str) -> Vec<String> {
    let Ok(url) = wreq::Url::parse(api_base) else {
        return Vec::new();
    };
    let Some(host) = url.host_str().map(str::to_string) else {
        return Vec::new();
    };
    // Secure 属性只在 https 下加：cookie_store 会拒收非安全来源的 Secure Cookie
    let secure = if url.scheme() == "https" {
        "; Secure"
    } else {
        ""
    };
    cookies
        .split(';')
        .map(str::trim)
        .filter(|pair| {
            let ok = !pair.is_empty() && pair.contains('=');
            if !ok && !pair.is_empty() {
                warn!(target: "ds_core::client", "忽略无法解析的 Cookie 片段（缺少 '='）");
            }
            ok
        })
        .map(|pair| format!("{pair}; Path=/{secure}; Domain={host}"))
        .collect()
}

impl DsClient {
    #[must_use]
    pub fn new(
        api_base: String,
        wasm_url: String,
        mut identity: ClientIdentity,
        hif: &HifConfig,
        emulation: EmulationProfile,
        proxy_url: Option<&str>,
    ) -> Self {
        let mut builder = wreq::Client::builder().emulation(emulation.to_wreq());
        if let Some(url) = proxy_url.and_then(|u| wreq::Proxy::all(u).ok()) {
            builder = builder.proxy(url);
        }
        // 空 = 按 api_base 确定性派生，保证重启后设备身份不变
        if identity.device_id.trim().is_empty() {
            identity.device_id = derive_device_uuid(api_base.as_bytes());
        } else {
            identity.device_id = identity.device_id.trim().to_string();
        }

        // Cookie jar：种子 = 配置的 Cookie 串；之后所有响应的 Set-Cookie 自动入 jar
        let jar = std::sync::Arc::new(wreq::cookie::Jar::default());
        for line in cookie_seed_lines(&api_base, &identity.cookies) {
            if let Ok(url) = wreq::Url::parse(&api_base) {
                jar.add_cookie_str(&line, &url);
            }
        }
        let http = builder
            .cookie_provider(jar)
            .build()
            .expect("构建 HTTP 客户端失败");
        let hif = hif.enabled.then(|| {
            let headers = identity.headers(FetchSite::CrossSite).unwrap_or_else(|e| {
                warn!(target: "ds_core::client", "客户端头构造失败，HIF 取令牌将不带拟态头: {e}");
                wreq::header::HeaderMap::new()
            });
            super::hif::HifRegistry::new(
                http.clone(),
                hif.leim_url.clone(),
                hif.dliq_url.clone(),
                headers,
            )
        });

        Self {
            http,
            api_base,
            wasm_url,
            identity,
            hif,
            emulation,
            proxy_url: proxy_url.map(str::to_string),
        }
    }

    /// 登录 payload 使用的 os 值
    pub(crate) fn client_os(&self) -> &str {
        &self.identity.client_os
    }

    /// 返回绑定到指定设备身份（`X-Device-Id`）的客户端视图
    ///
    /// 真实 Web 客户端是「一个浏览器 profile = 一个数美 `device_id` + 一个
    /// `X-Device-Id`」，HIF 风控令牌也按设备下发。因此代理侧同样**按账号**
    /// 派生设备身份：多账号共用同一个 `X-Device-Id` / 同一个令牌，等于告诉
    /// 上游「这些账号来自同一台设备」。
    ///
    /// 返回的客户端与原客户端共享 HTTP 连接池与令牌注册表，仅设备身份不同。
    #[must_use]
    pub fn scoped_to(&self, x_device_id: &str) -> Self {
        let mut scoped = self.clone();
        if !x_device_id.trim().is_empty() {
            scoped.identity.device_id = x_device_id.trim().to_string();
        }
        scoped
    }

    /// 构造**账号专属视图**：独立的 HTTP 连接池 + 独立 Cookie jar
    ///
    /// jar 用账号配置的 Cookie 串播种（浏览器采集/手动复制的会话），此后
    /// 每个响应的 `Set-Cookie` 由 jar 自动捕获续期 —— 每个账号一个 jar，
    /// 等价于「一个浏览器 profile 一份 Cookie」。identity 同时覆盖设备身份
    /// （`X-Device-Id`）与可选的账号级 User-Agent；HIF 令牌注册表与基础
    /// 客户端共享（令牌按设备身份分桶，语义不变）。
    ///
    /// 与 [`DsClient::scoped_to`]（共享连接池与 jar 的轻量视图，用于测试）
    /// 不同，账号视图是完整独立的客户端。
    #[must_use]
    pub fn account_view(&self, x_device_id: &str, cookies: &str, user_agent: &str) -> Self {
        let mut identity = self.identity.clone();
        if !x_device_id.trim().is_empty() {
            identity.device_id = x_device_id.trim().to_string();
        }
        let ua = user_agent.trim();
        if !ua.is_empty() {
            identity.user_agent = ua.to_string();
        }
        identity.cookies = cookies.trim().to_string();

        let mut builder = wreq::Client::builder().emulation(self.emulation.to_wreq());
        if let Some(url) = self
            .proxy_url
            .as_deref()
            .and_then(|u| wreq::Proxy::all(u).ok())
        {
            builder = builder.proxy(url);
        }
        let jar = std::sync::Arc::new(wreq::cookie::Jar::default());
        for line in cookie_seed_lines(&self.api_base, &identity.cookies) {
            if let Ok(url) = wreq::Url::parse(&self.api_base) {
                jar.add_cookie_str(&line, &url);
            }
        }
        let http = builder
            .cookie_provider(jar)
            .build()
            .expect("构建 HTTP 客户端失败");

        Self {
            http,
            api_base: self.api_base.clone(),
            wasm_url: self.wasm_url.clone(),
            identity,
            hif: self.hif.clone(),
            emulation: self.emulation,
            proxy_url: self.proxy_url.clone(),
        }
    }

    /// 当前视图的设备身份（X-Device-Id）
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.identity.device_id
    }

    /// 预热 HIF 风控令牌（真实客户端在应用启动时即开始轮询两个端点）
    pub async fn warm_up_hif(&self) {
        if let Some(hif) = &self.hif {
            hif.warm_up(&self.identity.device_id).await;
        }
    }

    /// 读取当前设备身份的 `x-hif-leim`（诊断用；缺失时触发一次取令牌）
    pub async fn hif_token(&self) -> Option<String> {
        let hif = self.hif.as_ref()?;
        let tokens = hif.tokens_for(&self.identity.device_id);
        tokens
            .values()
            .await
            .into_iter()
            .find(|(kind, _)| *kind == super::hif::HifKind::Leim)
            .map(|(_, value)| value)
    }

    /// 客户端通用请求头（登录与鉴权请求共用）
    fn client_headers(&self) -> Result<wreq::header::HeaderMap, ClientError> {
        self.identity.headers(FetchSite::SameOrigin)
    }

    /// POST 请求的通用头 = 通用头 + `Origin`
    ///
    /// 浏览器对同源 XHR **POST** 固定携带 `Origin: https://chat.deepseek.com`，
    /// GET 不带（抓包 2026-10-10）；缺失会让上游看到「非浏览器形态的 POST」。
    fn client_post_headers(&self) -> Result<wreq::header::HeaderMap, ClientError> {
        let mut h = self.client_headers()?;
        h.insert(
            wreq::header::ORIGIN,
            wreq::header::HeaderValue::from_static("https://chat.deepseek.com"),
        );
        Ok(h)
    }

    /// 在 completion（SSE）请求上追加风控令牌头（`x-hif-leim` + `x-hif-dliq`）
    ///
    /// 真实客户端只在 SSE 请求上带这两个头（前端 `addSSEHeader`），
    /// 且两个令牌由同一个头提供者一起附加；缺失时上游可直接判定为非官方客户端。
    async fn attach_hif(&self, headers: &mut wreq::header::HeaderMap) -> Result<(), ClientError> {
        if let Some(hif) = &self.hif {
            let values = hif.tokens_for(&self.identity.device_id).values().await;
            super::hif::insert_tokens(headers, &values)?;
        }
        Ok(())
    }

    fn auth_headers(&self, token: &str) -> Result<wreq::header::HeaderMap, ClientError> {
        let mut h = self.client_headers()?;
        h.insert(
            wreq::header::AUTHORIZATION,
            wreq::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| ClientError::InvalidHeader(format!("Authorization: {e}")))?,
        );
        Ok(h)
    }

    /// POST 请求头 = 通用头 + `Origin` + `Authorization`
    fn auth_post_headers(&self, token: &str) -> Result<wreq::header::HeaderMap, ClientError> {
        let mut h = self.client_post_headers()?;
        h.insert(
            wreq::header::AUTHORIZATION,
            wreq::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| ClientError::InvalidHeader(format!("Authorization: {e}")))?,
        );
        Ok(h)
    }

    /// POST（SSE）请求头 = 通用头 + `Origin` + `Authorization` + PoW
    fn auth_post_headers_with_pow(
        &self,
        token: &str,
        pow_response: &str,
    ) -> Result<wreq::header::HeaderMap, ClientError> {
        let mut h = self.auth_post_headers(token)?;
        h.insert(
            "X-Ds-Pow-Response",
            wreq::header::HeaderValue::from_str(pow_response)
                .map_err(|e| ClientError::InvalidHeader(format!("X-Ds-Pow-Response: {e}")))?,
        );
        Ok(h)
    }

    async fn parse_envelope<T: serde::de::DeserializeOwned>(
        resp: wreq::Response,
    ) -> Result<T, ClientError> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let envelope: Envelope<T> = resp.json().await?;
        envelope.into_result()
    }

    pub async fn login(&self, payload: &LoginPayload) -> Result<LoginData, ClientError> {
        let mut h = self.client_post_headers()?;
        h.insert(
            wreq::header::REFERER,
            wreq::header::HeaderValue::from_static("https://chat.deepseek.com/sign_in"),
        );
        let resp = self
            .http
            .post(format!("{}{}", self.api_base, ENDPOINT_USERS_LOGIN))
            .headers(h)
            .json(payload)
            .send()
            .await?;

        if is_waf_challenge(&resp) {
            print_waf_hint();
            return Err(ClientError::Status {
                status: 202,
                body: "WAF Challenge: use a non-US proxy".into(),
            });
        }

        Self::parse_envelope::<LoginData>(resp).await
    }

    /// 当前登录用户信息（含禁言状态）
    ///
    /// 与登录响应的 user 字段同构；探测禁言优先用登录响应（零额外请求），
    /// 此方法保留作原始客户端 API 对称面（同 `update_title` / `edit_message`）。
    #[allow(dead_code)]
    pub async fn current_user(&self, token: &str) -> Result<UserInfo, ClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.api_base, ENDPOINT_USERS_CURRENT))
            .headers(self.auth_headers(token)?)
            .send()
            .await?;
        Self::parse_envelope::<UserInfo>(resp).await
    }

    /// 设备校验 / 令牌轮换检查
    ///
    /// 真实客户端登录成功后立即调用；`rotate` 非 null 时服务端要求轮换令牌
    /// （可能是字符串或含 token 字段的对象，由调用方解析）。
    pub async fn check_device(&self, token: &str) -> Result<CheckDeviceData, ClientError> {
        let resp = self
            .http
            .post(format!(
                "{}{}",
                self.api_base, ENDPOINT_USERS_AUTH_TOKEN_CHECK_DEVICE
            ))
            .headers(self.auth_post_headers(token)?)
            .json(&serde_json::json!({
                "device_id": self.identity.device_id,
                "device_model": self.identity.device_model,
            }))
            .send()
            .await?;
        Self::parse_envelope::<CheckDeviceData>(resp).await
    }

    /// 拉取账号的会话列表（分页）
    ///
    /// `updated_at` 为上一页末尾会话的更新时间戳（Unix 秒），None = 第一页。
    /// 真实客户端游标序列化在 `lte_cursor` 对象下（`lte_cursor.pinned=false`）。
    pub async fn fetch_sessions(
        &self,
        token: &str,
        updated_at: Option<f64>,
    ) -> Result<FetchSessionsData, ClientError> {
        let mut query: Vec<(&str, String)> = vec![("lte_cursor.pinned", "false".to_string())];
        if let Some(ts) = updated_at {
            query.push(("lte_cursor.updated_at", ts.to_string()));
        }
        let resp = self
            .http
            .get(format!(
                "{}{}",
                self.api_base, ENDPOINT_CHAT_SESSION_FETCH_PAGE
            ))
            .headers(self.auth_headers(token)?)
            .query(&query)
            .send()
            .await?;
        Self::parse_envelope::<FetchSessionsData>(resp).await
    }

    pub async fn create_session(&self, token: &str) -> Result<String, ClientError> {
        let resp = self
            .http
            .post(format!("{}{}", self.api_base, ENDPOINT_CHAT_SESSION_CREATE))
            .headers(self.auth_post_headers(token)?)
            .json(&serde_json::json!({}))
            .send()
            .await?;
        let wrapper: CreateSessionWrapper = Self::parse_envelope(resp).await?;
        let data = wrapper.chat_session;
        Ok(data.id)
    }

    pub async fn delete_session(&self, token: &str, session_id: &str) -> Result<(), ClientError> {
        let resp = self
            .http
            .post(format!("{}{}", self.api_base, ENDPOINT_CHAT_SESSION_DELETE))
            .headers(self.auth_post_headers(token)?)
            .json(&serde_json::json!({ "chat_session_id": session_id }))
            .send()
            .await?;
        Self::parse_envelope::<Option<()>>(resp).await?;
        Ok(())
    }

    pub async fn create_pow_challenge(
        &self,
        token: &str,
        target_path: &str,
    ) -> Result<ChallengeData, ClientError> {
        let resp = self
            .http
            .post(format!(
                "{}{}",
                self.api_base, ENDPOINT_CHAT_CREATE_POW_CHALLENGE
            ))
            .headers(self.auth_post_headers(token)?)
            .json(&serde_json::json!({ "target_path": target_path }))
            .send()
            .await?;
        let wrapper: ChallengeWrapper = Self::parse_envelope(resp).await?;
        let challenge = wrapper.challenge;
        Ok(challenge)
    }

    pub async fn completion(
        &self,
        token: &str,
        pow_response: &str,
        payload: &CompletionPayload,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, ClientError>> + Send>>, ClientError> {
        let mut headers = self.auth_post_headers_with_pow(token, pow_response)?;
        // 真实客户端的 completion Referer 指向会话页（抓包 2026-10-10：
        // https://chat.deepseek.com/a/chat/s/<session_id>）
        let referer = format!(
            "https://chat.deepseek.com/a/chat/s/{}",
            payload.chat_session_id
        );
        headers.insert(
            wreq::header::REFERER,
            wreq::header::HeaderValue::from_str(&referer)
                .map_err(|e| ClientError::InvalidHeader(format!("Referer: {e}")))?,
        );
        self.attach_hif(&mut headers).await?;
        let resp = self
            .http
            .post(format!("{}{}", self.api_base, ENDPOINT_CHAT_COMPLETION))
            .headers(headers.clone())
            .json(payload)
            .send()
            .await?;

        // WAF 反应式刷新：会话 Cookie（HWWAFSESID）过期时上游返回 202 challenge，
        // 此时 GET 一次站点根让 WAF 重新下发 Cookie（jar 自动捕获），再重试一次
        let resp = if is_waf_challenge(&resp) {
            warn!(
                target: "ds_core::client",
                "completion 遇到 WAF challenge，正在刷新站点 Cookie 后重试一次"
            );
            self.refresh_site_cookies().await;
            let resp = self
                .http
                .post(format!("{}{}", self.api_base, ENDPOINT_CHAT_COMPLETION))
                .headers(headers)
                .json(payload)
                .send()
                .await?;
            if is_waf_challenge(&resp) {
                print_waf_hint();
                return Err(ClientError::Status {
                    status: 202,
                    body: "WAF Challenge persists after cookie refresh".into(),
                });
            }
            resp
        } else {
            resp
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Status {
                status: status.as_u16(),
                body,
            });
        }

        Ok(Box::pin(resp.bytes_stream().map_err(ClientError::Http)))
    }

    /// 访问站点根，让 WAF 重新下发会话 Cookie（jar 自动捕获 `Set-Cookie`）
    async fn refresh_site_cookies(&self) {
        let origin = wreq::Url::parse(&self.api_base).ok().map_or_else(
            || "https://chat.deepseek.com".to_string(),
            |u| u.origin().ascii_serialization(),
        );
        match self.http.get(&origin).send().await {
            Ok(resp) => {
                debug!(
                    target: "ds_core::client",
                    "站点 Cookie 刷新完成: {} {}",
                    resp.status(),
                    origin
                );
            }
            Err(e) => {
                warn!(target: "ds_core::client", "站点 Cookie 刷新请求失败: {e}");
            }
        }
    }

    #[allow(dead_code)]
    pub async fn edit_message(
        &self,
        token: &str,
        pow_response: &str,
        payload: &EditMessagePayload,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, ClientError>> + Send>>, ClientError> {
        let mut headers = self.auth_post_headers_with_pow(token, pow_response)?;
        self.attach_hif(&mut headers).await?;
        let resp = self
            .http
            .post(format!("{}{}", self.api_base, ENDPOINT_CHAT_EDIT_MESSAGE))
            .headers(headers)
            .json(payload)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Status {
                status: status.as_u16(),
                body,
            });
        }

        Ok(Box::pin(resp.bytes_stream().map_err(ClientError::Http)))
    }

    #[allow(dead_code)]
    pub async fn update_title(
        &self,
        token: &str,
        payload: &UpdateTitlePayload,
    ) -> Result<(), ClientError> {
        let resp = self
            .http
            .post(format!(
                "{}{}",
                self.api_base, ENDPOINT_CHAT_SESSION_UPDATE_TITLE
            ))
            .headers(self.auth_post_headers(token)?)
            .json(payload)
            .send()
            .await?;
        Self::parse_envelope::<serde::de::IgnoredAny>(resp).await?;
        Ok(())
    }

    /// 取消正在进行的流式输出，不需要 PoW
    pub async fn stop_stream(
        &self,
        token: &str,
        payload: &StopStreamPayload,
    ) -> Result<(), ClientError> {
        let resp = self
            .http
            .post(format!("{}{}", self.api_base, ENDPOINT_CHAT_STOP_STREAM))
            .headers(self.auth_post_headers(token)?)
            .json(payload)
            .send()
            .await?;
        Self::parse_envelope::<Option<()>>(resp).await?;
        Ok(())
    }

    /// 上传文件，返回文件元数据（id, status 等）
    pub async fn upload_file(
        &self,
        token: &str,
        pow_response: &str,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
    ) -> Result<UploadFileData, ClientError> {
        let part = Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str(content_type)?;
        let form = Form::new().part("file", part);

        let resp = self
            .http
            .post(format!("{}{}", self.api_base, ENDPOINT_FILE_UPLOAD))
            .headers(self.auth_post_headers_with_pow(token, pow_response)?)
            .multipart(form)
            .send()
            .await?;
        Self::parse_envelope::<UploadFileData>(resp).await
    }

    /// 查询文件状态，返回文件列表（含 status: PENDING/SUCCESS/FAILED）
    pub async fn fetch_files(
        &self,
        token: &str,
        file_ids: &[String],
    ) -> Result<FetchFilesData, ClientError> {
        let ids = file_ids.join(",");
        let resp = self
            .http
            .get(format!("{}{}", self.api_base, ENDPOINT_FILE_FETCH))
            .headers(self.auth_headers(token)?)
            .query(&[("file_ids", &ids)])
            .send()
            .await?;
        Self::parse_envelope::<FetchFilesData>(resp).await
    }

    pub async fn get_wasm(&self) -> Result<Bytes, ClientError> {
        let resp = self.http.get(&self.wasm_url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.bytes().await?)
    }
}

/// 由种子字节确定性派生 UUID（RFC 4122 v4 格式）
///
/// 用途：
/// - **按账号派生 X-Device-Id**（`ds-free-api:x-device-id:{数美 device_id}`）：
///   稳定且每账号唯一，对齐真实客户端「一个浏览器 profile 一个设备身份」；
/// - 全局 `client_device_id` 未配置、且账号也没有 `device_id` 时的兜底。
pub(crate) fn derive_device_uuid(seed: &[u8]) -> String {
    fn fnv1a(data: &[u8], offset: u64) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ offset;
        for &b in data {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
    let hi = fnv1a(seed, 0);
    let lo = fnv1a(seed, 0x9e37_79b9_7f4a_7c15);
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&hi.to_be_bytes());
    b[8..].copy_from_slice(&lo.to_be_bytes());
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        hex[0],
        hex[1],
        hex[2],
        hex[3],
        hex[4],
        hex[5],
        hex[6],
        hex[7],
        hex[8],
        hex[9],
        hex[10],
        hex[11],
        hex[12],
        hex[13],
        hex[14],
        hex[15]
    )
}

/// 解析 check_device 的 rotate 指令，提取新令牌
///
/// 形态未知（未观测到非 null 值）：兼容字符串与 `{"token": "..."}` 对象，
/// 其余形态返回 None（记录日志由调用方处理）。
pub(crate) fn extract_rotate_token(rotate: &serde_json::Value) -> Option<String> {
    match rotate {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Object(map) => map
            .get("token")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity() -> ClientIdentity {
        ClientIdentity {
            user_agent: "DeepSeek/2.5.0 Android/35".to_string(),
            client_version: "2.5.0".to_string(),
            client_platform: "android".to_string(),
            client_locale: "zh_CN".to_string(),
            client_bundle_id: "com.deepseek.chat".to_string(),
            device_id: String::new(),
            device_model: String::new(),
            timezone_offset: "28800".to_string(),
            client_os: "android".to_string(),
            cookies: String::new(),
        }
    }

    fn test_client() -> DsClient {
        DsClient::new(
            "https://chat.deepseek.com/api/v0".to_string(),
            "https://example.com/x.wasm".to_string(),
            test_identity(),
            &HifConfig::default(),
            EmulationProfile::default(),
            None,
        )
    }

    #[test]
    fn device_uuid_is_deterministic_and_well_formed() {
        let a = derive_device_uuid(b"https://chat.deepseek.com/api/v0");
        let b = derive_device_uuid(b"https://chat.deepseek.com/api/v0");
        assert_eq!(a, b, "同一 seed 必须派生同一 UUID");
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
        assert_eq!(&a[14..15], "4", "version 4");
        assert!(
            matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant"
        );
        assert_ne!(
            derive_device_uuid(b"https://other.example/api"),
            a,
            "不同 seed 应派生不同 UUID"
        );
    }

    #[test]
    fn client_headers_match_real_client_shape() {
        let c = test_client();
        let h = c.client_headers().expect("headers");
        for name in [
            "user-agent",
            "x-client-version",
            "x-client-platform",
            "x-client-locale",
            "x-client-bundle-id",
            "x-device-id",
            "x-device-model",
            "x-client-timezone-offset",
        ] {
            assert!(h.contains_key(name), "缺少请求头 {name}");
        }
        assert_eq!(h["x-client-version"], "2.5.0");
        assert_eq!(h["x-device-model"], "");
        assert_eq!(h["x-client-timezone-offset"], "28800");
        // X-Device-Id 必须是 UUID 形态（api_base 派生）
        assert_eq!(h["x-device-id"].len(), 36);

        // 业务 API 必须是**页面内 XHR** 形态，而不是地址栏导航：
        // 真实浏览器抓包为 accept: */*、sec-ch-ua-platform 与 UA 一致、
        // 不带 sec-fetch-dest: document / priority（2026-10-07 echo server 比对）
        assert_eq!(h["accept"], "*/*");
        assert_eq!(h["sec-fetch-dest"], "empty");
        assert_eq!(h["sec-fetch-mode"], "cors");
        assert_eq!(h["sec-fetch-site"], "same-origin");
        assert_eq!(h["sec-ch-ua-platform"], "\"Windows\"");
        assert_eq!(h["referer"], "https://chat.deepseek.com/");
        assert!(
            !h.contains_key("priority"),
            "不应带地址栏导航才有的 priority 头"
        );
    }

    #[test]
    fn device_id_config_override_wins() {
        let mut identity = test_identity();
        identity.device_id = "11111111-2222-4333-8444-555555555555".to_string();
        let c = DsClient::new(
            "https://chat.deepseek.com/api/v0".to_string(),
            "https://example.com/x.wasm".to_string(),
            identity,
            &HifConfig::default(),
            EmulationProfile::default(),
            None,
        );
        assert_eq!(
            c.client_headers().unwrap()["x-device-id"],
            "11111111-2222-4333-8444-555555555555"
        );
    }

    #[test]
    fn scoped_client_carries_its_own_device_identity() {
        let base = test_client();
        let a = base.scoped_to("11111111-1111-4111-8111-111111111111");
        let b = base.scoped_to("22222222-2222-4222-8222-222222222222");
        assert_eq!(
            a.client_headers().unwrap()["x-device-id"],
            "11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(
            b.client_headers().unwrap()["x-device-id"],
            "22222222-2222-4222-8222-222222222222"
        );
        // 空值不改变原设备身份
        let base_id = base.client_headers().unwrap()["x-device-id"].clone();
        assert_eq!(
            base.scoped_to("").client_headers().unwrap()["x-device-id"],
            base_id
        );
        // 设备视图与原客户端共享同一份 HIF 注册表（同一设备 = 同一份令牌）
        let base_hif = base.hif.as_ref().expect("hif enabled");
        assert!(std::sync::Arc::ptr_eq(
            &base_hif.tokens_for("device-x"),
            &a.hif.as_ref().unwrap().tokens_for("device-x")
        ));
        assert!(!std::sync::Arc::ptr_eq(
            &base_hif.tokens_for("device-x"),
            &base_hif.tokens_for("device-y")
        ));
    }

    #[test]
    fn emulation_profile_parsing() {
        assert_eq!(
            EmulationProfile::from_config("okhttp4_12"),
            Some(EmulationProfile::OkHttp4_12)
        );
        assert_eq!(
            EmulationProfile::from_config(" Chrome136 "),
            Some(EmulationProfile::Chrome136)
        );
        assert_eq!(EmulationProfile::from_config("nope"), None);
        assert_eq!(
            EmulationProfile::default(),
            EmulationProfile::Chrome136,
            "默认档位对齐浏览器（chrome136）"
        );
    }

    #[test]
    fn hif_can_be_disabled() {
        let c = DsClient::new(
            "https://chat.deepseek.com/api/v0".to_string(),
            "https://example.com/x.wasm".to_string(),
            test_identity(),
            &HifConfig {
                enabled: false,
                ..HifConfig::default()
            },
            EmulationProfile::default(),
            None,
        );
        assert!(c.hif.is_none(), "关闭后不应构造 HIF 令牌提供者");
    }

    /// jar 往返集成测试：种子的 Cookie 随请求发出；响应的 Set-Cookie 被 jar
    /// 捕获并在后续请求回传（WAF 会话自动续期的核心链路）。
    /// 用极简 HTTP/1.1 服务器记录请求 Cookie 头并下发 Set-Cookie。
    #[tokio::test]
    async fn cookie_jar_roundtrip_with_seed_and_set_cookie() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let observed: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let observed_srv = observed.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let observed = observed_srv.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut got = Vec::new();
                    loop {
                        let Ok(n) = sock.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        got.extend_from_slice(&buf[..n]);
                        if got.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&got);
                    let cookie = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("cookie:"))
                        .and_then(|l| l.split_once(':'))
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default();
                    observed.lock().unwrap().push(cookie);
                    let body = r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":{"rotate":null}}}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Set-Cookie: HWWAFSESID=fresh123; Path=/; HttpOnly\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    drop(sock.write_all(resp.as_bytes()).await);
                    drop(sock.shutdown().await);
                });
            }
        });

        let mut identity = test_identity();
        identity.cookies = "ds_session_id=old-value".to_string();
        let client = DsClient::new(
            format!("http://{addr}/api/v0"),
            "https://example.invalid/x.wasm".to_string(),
            identity,
            &HifConfig {
                enabled: false,
                ..HifConfig::default()
            },
            EmulationProfile::default(),
            None,
        );

        client.check_device("t").await.unwrap();
        client.check_device("t").await.unwrap();

        let cookies = observed.lock().unwrap();
        assert_eq!(cookies.len(), 2, "两次请求都应到达服务器");
        assert_eq!(cookies[0], "ds_session_id=old-value", "首请求带种子 Cookie");
        assert!(
            cookies[1].contains("ds_session_id=old-value"),
            "第二次请求仍带种子 Cookie: {:?}",
            cookies[1]
        );
        assert!(
            cookies[1].contains("HWWAFSESID=fresh123"),
            "Set-Cookie 下发的 WAF Cookie 应被 jar 捕获并回传: {:?}",
            cookies[1]
        );
        drop(cookies);
    }

    /// Cookie 种子串解析：拆成带主机属性的 Set-Cookie 行；坏片段跳过
    #[test]
    fn cookie_seed_lines_parse_and_skip_malformed() {
        let lines = cookie_seed_lines(
            "https://chat.deepseek.com/api/v0",
            "ds_session_id=abc; smidV2=xyz",
        );
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("ds_session_id=abc; Path=/; Secure; Domain=chat.deepseek.com")
        );
        assert!(lines[1].starts_with("smidV2=xyz; Path=/; Secure; Domain=chat.deepseek.com"));

        // http 目标不带 Secure
        let http_lines = cookie_seed_lines("http://127.0.0.1:1234/api/v0", "a=1");
        assert_eq!(http_lines[0], "a=1; Path=/; Domain=127.0.0.1");

        // 无 '=' 的片段被跳过；空串 → 无种子
        assert!(cookie_seed_lines("https://chat.deepseek.com/api/v0", "flag; a=1").len() == 1);
        assert!(cookie_seed_lines("https://chat.deepseek.com/api/v0", "   ").is_empty());
    }

    #[test]
    fn sec_ch_ua_version_follows_user_agent() {
        assert_eq!(
            chrome_major_from_ua("Mozilla/5.0 (Windows NT 10.0) Chrome/140.0.0.0"),
            "140"
        );
        assert_eq!(
            chrome_major_from_ua("Mozilla/5.0 Chrome/101.0.4951.54 Safari/537.36"),
            "101"
        );
        // 无 Chrome 字样 / 非数字版本 → 回退
        assert_eq!(chrome_major_from_ua("DeepSeek/2.5.0 Android/35"), "140");
        assert_eq!(chrome_major_from_ua("Mozilla/5.0 Chrome/abc.0"), "140");

        // UA 改成 Chrome/101 时 sec-ch-ua 版本自动跟随（手动令牌模式对齐登录浏览器）
        let mut identity = test_identity();
        identity.user_agent =
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/101.0.4951.54 Safari/537.36".to_string();
        let c = DsClient::new(
            "https://chat.deepseek.com/api/v0".to_string(),
            "https://example.com/x.wasm".to_string(),
            identity,
            &HifConfig {
                enabled: false,
                ..HifConfig::default()
            },
            EmulationProfile::default(),
            None,
        );
        let h = c.client_headers().unwrap();
        assert!(h["sec-ch-ua"].to_str().unwrap().contains("v=\"101\""));
    }

    /// completion payload 必须与真实抓包（2026-10-10）**逐字节**一致：
    /// 无 `source`；首条消息 `parent_message_id: null`（显式序列化）、
    /// `model_type` 有值；后续消息两者相反。对序列化字符串断言可同时覆盖
    /// 字段集合、字段顺序与 null 形态（`to_value` 的 Map 会按字母序丢掉顺序）。
    #[test]
    fn completion_payload_matches_capture() {
        let first = CompletionPayload {
            chat_session_id: "1b1907d5".to_string(),
            parent_message_id: None,
            model_type: Some("default".to_string()),
            prompt: "你好".to_string(),
            ref_file_ids: vec![],
            thinking_enabled: true,
            search_enabled: true,
            action: None,
            preempt: false,
        };
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            "{\"chat_session_id\":\"1b1907d5\",\"parent_message_id\":null,\
              \"model_type\":\"default\",\"prompt\":\"你好\",\"ref_file_ids\":[],\
              \"thinking_enabled\":true,\"search_enabled\":true,\"action\":null,\
              \"preempt\":false}"
        );

        let follow = CompletionPayload {
            parent_message_id: Some(2),
            model_type: None,
            ..first
        };
        assert_eq!(
            serde_json::to_string(&follow).unwrap(),
            "{\"chat_session_id\":\"1b1907d5\",\"parent_message_id\":2,\
              \"model_type\":null,\"prompt\":\"你好\",\"ref_file_ids\":[],\
              \"thinking_enabled\":true,\"search_enabled\":true,\"action\":null,\
              \"preempt\":false}"
        );
    }

    #[test]
    fn chat_status_accepts_int_and_bool_mute_flags() {
        let int_form: ChatStatus =
            serde_json::from_str(r#"{"is_muted":1,"mute_until":1790397239.972}"#).unwrap();
        assert_eq!(int_form.is_muted, 1);
        assert_eq!(int_form.mute_until, Some(1_790_397_239.972));

        let bool_form: ChatStatus = serde_json::from_str(r#"{"is_muted":false}"#).unwrap();
        assert_eq!(bool_form.is_muted, 0);
        assert_eq!(bool_form.mute_until, None);

        // 缺失 chat 字段 = 未禁言
        let user: UserInfo = serde_json::from_str(r#"{"id":"u","token":"t"}"#).unwrap();
        assert!(user.chat.is_none());
    }

    #[test]
    fn check_device_rotate_parsing() {
        let null_rotate: CheckDeviceData = serde_json::from_str(r#"{"rotate":null}"#).unwrap();
        assert!(null_rotate.rotate.is_none());

        let str_rotate: CheckDeviceData =
            serde_json::from_str(r#"{"rotate":"new-token"}"#).unwrap();
        assert_eq!(
            extract_rotate_token(str_rotate.rotate.as_ref().unwrap()).as_deref(),
            Some("new-token")
        );

        let obj_rotate: CheckDeviceData =
            serde_json::from_str(r#"{"rotate":{"token":"obj-token"}}"#).unwrap();
        assert_eq!(
            extract_rotate_token(obj_rotate.rotate.as_ref().unwrap()).as_deref(),
            Some("obj-token")
        );

        let junk: serde_json::Value = serde_json::from_str(r#"{"foo":1}"#).unwrap();
        assert!(extract_rotate_token(&junk).is_none());
    }

    #[test]
    fn fetch_sessions_response_parsing() {
        let data: FetchSessionsData = serde_json::from_str(
            r#"{"chat_sessions":[
                {"id":"97952d9b","title":"命令行命令翻译","title_type":"SYSTEM",
                 "pinned":false,"model_type":"default","updated_at":1778039195.997},
                {"id":"5be62e10","title":null,"pinned":true}
            ],"has_more":true}"#,
        )
        .unwrap();
        assert_eq!(data.chat_sessions.len(), 2);
        assert_eq!(data.has_more, Some(true));
        assert_eq!(
            data.chat_sessions[0].title.as_deref(),
            Some("命令行命令翻译")
        );
        assert_eq!(data.chat_sessions[0].updated_at, Some(1_778_039_195.997));
        assert!(data.chat_sessions[1].title.is_none());
        assert!(data.chat_sessions[1].model_type.is_none());
        assert!(data.chat_sessions[1].updated_at.is_none());

        // 空响应（无会话账号）
        let empty: FetchSessionsData = serde_json::from_str(r#"{"chat_sessions":[]}"#).unwrap();
        assert!(empty.chat_sessions.is_empty());
        assert_eq!(empty.has_more, None);
    }
}
