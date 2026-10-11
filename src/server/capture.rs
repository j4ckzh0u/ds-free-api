//! 浏览器采集登录 —— CDP 驱动本机 Chrome，人工登录后旁路采集凭据
//!
//! 流程：管理面板点击「浏览器采集登录」→ 后端拉起 headful Chrome（独立的
//! 持久化 profile，保留数美指纹与登录态）→ 用户在窗口里正常完成登录
//! （MFA / 数美验证码 / 微信扫码均由人完成）→ 程序通过 CDP 监听网络事件，
//! 登录成功后客户端自动发起的 `/api/v0/*` 鉴权请求一出现即采集：
//!
//! - `Authorization: Bearer <token>` → token
//! - 请求头里的 `User-Agent` / `X-Device-Id` → 账号级身份覆盖（token ↔ 设备
//!   身份与浏览器会话同源）
//! - `Network.getCookies`（含 HttpOnly）→ Cookie 串
//!
//! 采集结果按「同一浏览器（X-Device-Id 相同）= 同一账号」的规则写入
//! config.toml 并热同步进账号池；不存在则新增（默认名 `token-<前8位>`，
//! 支持备注名），已存在则更新该账号的 token/cookies/UA。
//!
//! 本模块只做旁路观察，不注入、不自动化任何页面交互。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chromiumoxide::Browser;
use chromiumoxide::browser::BrowserConfig;
use chromiumoxide::cdp::browser_protocol::network::EventRequestWillBeSent;
use chromiumoxide::detection::DetectionOptions;
use futures::StreamExt;
use log::{debug, error, info, warn};
use serde::Serialize;

/// 采集目标（真实上游）
const SIGN_IN_URL: &str = "https://chat.deepseek.com/sign_in";
const API_PREFIX: &str = "https://chat.deepseek.com/api/v0/";
/// Cookie 按域过滤：只保留 DeepSeek 自己的（排除第三方统计等）
const COOKIE_DOMAIN: &str = "deepseek.com";
/// 总超时：用户登录 + 页面发起首个鉴权请求的等待上限
const CAPTURE_TIMEOUT: Duration = Duration::from_mins(5);
/// 环境变量：覆盖 Chrome 可执行文件路径（默认自动探测标准安装路径）
pub(crate) const ENV_CHROME_PATH: &str = "DS_CHROME_PATH";

/// 采集到的凭据
#[derive(Debug, Clone)]
pub struct CapturedCredentials {
    pub token: String,
    pub cookies: String,
    pub user_agent: String,
    pub x_device_id: String,
}

/// 采集状态（管理面板轮询）
#[derive(Debug, Clone, Serialize)]
pub struct CaptureStatus {
    pub phase: CapturePhase,
    /// 展示给用户的消息（进度 / 失败原因）
    pub message: String,
    /// 成功时新增/更新的账号标识
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_label: Option<String>,
    /// 本次运行观测到的 `/api/v0/*` 请求数（诊断：页面是否正常发起请求）
    pub seen_api_requests: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturePhase {
    Idle,
    Running,
    Success,
    Failed,
}

/// 采集管理器：全局单槽（同时只允许一次采集）
pub struct CaptureManager {
    status: Mutex<CaptureStatus>,
    seen_api_requests: Arc<AtomicU32>,
}

impl CaptureManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            status: Mutex::new(CaptureStatus {
                phase: CapturePhase::Idle,
                message: String::new(),
                account_label: None,
                seen_api_requests: 0,
            }),
            seen_api_requests: Arc::new(AtomicU32::new(0)),
        }
    }

    /// 当前状态快照
    pub fn status(&self) -> CaptureStatus {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        status.seen_api_requests = self.seen_api_requests.load(Ordering::Relaxed);
        status
    }

    /// 尝试占用采集槽；已被占用时返回 `Err(当前状态)`
    fn try_start(&self) -> Result<(), CaptureStatus> {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if status.phase == CapturePhase::Running {
            return Err(status.clone());
        }
        self.seen_api_requests.store(0, Ordering::Relaxed);
        *status = CaptureStatus {
            phase: CapturePhase::Running,
            message: String::new(),
            account_label: None,
            seen_api_requests: 0,
        };
        drop(status);
        Ok(())
    }

    fn set_message(&self, message: impl Into<String>) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.message = message.into();
    }

    fn set_done(&self, phase: CapturePhase, message: impl Into<String>, label: Option<String>) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.phase = phase;
        status.message = message.into();
        status.account_label = label;
    }
}

impl Default for CaptureManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 解析 Chrome 可执行文件：环境变量优先，否则按平台标准路径探测
fn chrome_executable() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var(ENV_CHROME_PATH) {
        let path = std::path::PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "环境变量 {ENV_CHROME_PATH} 指向的可执行文件不存在: {}",
            path.display()
        ));
    }
    chromiumoxide::detection::default_executable(DetectionOptions::default())
        .map_err(|e| format!("未找到 Chrome/Chromium，可用 {ENV_CHROME_PATH} 指定路径: {e}"))
}

/// 请求头里取值（CDP Headers 是 JSON 对象，键大小写不保证）
fn header_value(
    headers: &chromiumoxide::cdp::browser_protocol::network::Headers,
    name: &str,
) -> Option<String> {
    let obj = headers.inner().as_object()?;
    obj.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.as_str())
        .map(str::to_string)
}

/// 从鉴权请求事件提取凭据；非鉴权 API 请求返回 None
fn inspect_request(
    event: &EventRequestWillBeSent,
    api_prefix: &str,
) -> Option<CapturedCredentials> {
    if !event.request.url.starts_with(api_prefix) {
        return None;
    }
    let auth = header_value(&event.request.headers, "Authorization")?;
    let token = auth
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())?;
    let user_agent = header_value(&event.request.headers, "User-Agent").unwrap_or_default();
    let x_device_id = header_value(&event.request.headers, "X-Device-Id").unwrap_or_default();
    Some(CapturedCredentials {
        token: token.to_string(),
        cookies: String::new(),
        user_agent,
        x_device_id,
    })
}

/// CDP Cookie 列表 → Cookie 请求头串（按域过滤；非法值跳过并告警）
fn cookie_header_string(
    cookies: &[chromiumoxide::cdp::browser_protocol::network::Cookie],
    domain_filter: &str,
) -> String {
    let mut pairs = Vec::new();
    for c in cookies {
        if !c.domain.contains(domain_filter) {
            continue;
        }
        let pair = format!("{}={}", c.name, c.value);
        if axum::http::HeaderValue::from_str(&pair).is_err() {
            warn!(
                target: "http::server",
                "采集到非法 Cookie 值，已跳过: name={}", c.name
            );
            continue;
        }
        pairs.push(pair);
    }
    pairs.join("; ")
}

/// 采集核心：启动浏览器 → 打开登录页 → 监听鉴权请求 → 取 Cookies
///
/// `api_prefix` / `cookie_domain` 参数化以便本地 mock 测试。
async fn capture_with(
    config: BrowserConfig,
    page_url: &str,
    api_prefix: &str,
    cookie_domain: &str,
    timeout: Duration,
    seen: Arc<AtomicU32>,
) -> Result<CapturedCredentials, String> {
    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| format!("启动浏览器失败: {e}"))?;

    // chromiumoxide 的事件循环需要被持续驱动（Handler 是 Stream）
    let pump = tokio::spawn(async move {
        while let Some(event) = handler.next().await {
            if let Err(e) = event {
                debug!(target: "http::server", "CDP handler 事件错误: {e}");
            }
        }
    });

    let result = async {
        // 先订阅网络事件、后导航——已登录的 profile 在页面加载瞬间就会发出
        // 鉴权请求，订阅晚于导航会漏掉（mock 测试实测）
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| format!("创建标签页失败: {e}"))?;
        // 注意必须用 **Page 级**监听：chromiumoxide 的 handler 会把带 session_id
        // 的页面级事件只路由给对应 target（browser 级监听器收不到 Network 事件）
        let mut events = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|e| format!("订阅网络事件失败: {e}"))?;
        page.goto(page_url)
            .await
            .map_err(|e| format!("打开登录页失败: {e}"))?;

        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        let creds: CapturedCredentials = loop {
            tokio::select! {
                event = events.next() => match event {
                    Some(event) => {
                        if event.request.url.starts_with(api_prefix) {
                            seen.fetch_add(1, Ordering::Relaxed);
                        }
                        if let Some(mut creds) = inspect_request(&event, api_prefix) {
                            let cookies = page
                                .get_cookies()
                                .await
                                .map_err(|e| format!("读取 Cookies 失败: {e}"))?;
                            creds.cookies = cookie_header_string(&cookies, cookie_domain);
                            break creds;
                        }
                    }
                    None => return Err("浏览器已退出（窗口被关闭？）".to_string()),
                },
                () = &mut deadline => {
                    return Err(format!(
                        "超时（{} 秒）：未检测到登录后的鉴权请求；请在窗口内完成登录，\
                         或确认登录后页面已正常打开",
                        timeout.as_secs()
                    ));
                }
            }
        };
        Ok(creds)
    }
    .await;

    if let Err(e) = browser.close().await {
        debug!(target: "http::server", "关闭浏览器失败（进程将随句柄释放退出）: {e}");
    }
    pump.abort();
    result
}

/// 采集成功后的持久化 + 账号池同步
///
/// 同一浏览器（X-Device-Id 相同）视为同一账号：已存在则更新其
/// token/cookies/UA，否则新增。返回（账号标识, 是否为更新）。
async fn persist_account(
    ctx: &CaptureContext,
    creds: CapturedCredentials,
    remark: &str,
) -> Result<(String, bool), String> {
    let label = ds_core::account_display_label(remark, "", "", &creds.token);
    let account = crate::config::Account {
        email: String::new(),
        mobile: String::new(),
        area_code: String::new(),
        password: String::new(),
        device_id: String::new(),
        token: creds.token,
        cookies: creds.cookies,
        remark: remark.trim().to_string(),
        x_device_id: creds.x_device_id,
        user_agent: creds.user_agent,
    };

    let is_update = {
        let mut config = ctx.config.write().await;
        let existing = config
            .ds_core
            .accounts
            .iter_mut()
            .find(|a| !a.x_device_id.is_empty() && a.x_device_id == account.x_device_id);
        if let Some(existing) = existing {
            existing.token.clone_from(&account.token);
            existing.cookies.clone_from(&account.cookies);
            existing.user_agent.clone_from(&account.user_agent);
            if !account.remark.is_empty() {
                existing.remark.clone_from(&account.remark);
            }
            true
        } else {
            if config
                .ds_core
                .accounts
                .iter()
                .any(|a| a.token == account.token)
            {
                return Err("该浏览器会话的 token 已存在于账号列表".to_string());
            }
            config.ds_core.accounts.push(account.clone());
            false
        }
    };

    {
        let config = ctx.config.read().await;
        config
            .validate()
            .map_err(|e| format!("配置校验失败: {e}"))?;
        config.save(&ctx.config_path).map_err(|e| {
            error!(target: "http::server", "采集结果写回 config.toml 失败: {e}");
            format!("配置保存失败: {e}")
        })?;
    }

    // 更新场景：池内同名账号持有旧 token，先移除再由 sync 重新加载；
    // 账号标识变化（备注修改）时 sync 自会删旧加新，remove 未命中属正常
    if is_update
        && let Err(ds_core::PoolError::AccountBusy(id)) = ctx.adapter.remove_account(&label)
    {
        return Err(format!("账号 {id} 正在使用中，请稍后再采集更新"));
    }
    {
        let config = ctx.config.read().await;
        ctx.adapter.sync_accounts(&config.ds_core.accounts).await;
        drop(config);
    }
    Ok((label, is_update))
}

/// 采集任务的上下文（从 AppState 拆出的 'static 句柄）
pub(crate) struct CaptureContext {
    pub(crate) config: Arc<tokio::sync::RwLock<crate::config::Config>>,
    pub(crate) config_path: std::path::PathBuf,
    pub(crate) adapter: Arc<crate::openai_adapter::OpenAIAdapter>,
    /// 持久化浏览器 profile 目录（保留登录态与数美指纹，二次采集免登录）
    pub(crate) profile_dir: std::path::PathBuf,
}

/// 运行一次完整采集并更新管理器状态（由启动端点 spawn）
async fn run_capture(manager: Arc<CaptureManager>, ctx: CaptureContext, remark: String) {
    manager.set_message("正在启动浏览器…");

    let result = async {
        let exe = chrome_executable()?;
        manager.set_message("已启动浏览器，请在窗口中登录 chat.deepseek.com…");

        let browser_config = BrowserConfig::builder()
            .with_head()
            .window_size(1280, 900)
            .user_data_dir(&ctx.profile_dir)
            .chrome_executable(exe)
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--lang=zh-CN")
            .build()
            .map_err(|e| format!("浏览器配置无效: {e}"))?;

        capture_with(
            browser_config,
            SIGN_IN_URL,
            API_PREFIX,
            COOKIE_DOMAIN,
            CAPTURE_TIMEOUT,
            Arc::clone(&manager.seen_api_requests),
        )
        .await
    }
    .await;

    match result {
        Ok(creds) => match persist_account(&ctx, creds, &remark).await {
            Ok((label, is_update)) => {
                let action = if is_update {
                    "已更新账号"
                } else {
                    "已添加账号"
                };
                info!(target: "http::server", "浏览器采集成功: {action} {label}");
                manager.set_done(
                    CapturePhase::Success,
                    format!("{action} {label}"),
                    Some(label),
                );
            }
            Err(e) => {
                warn!(target: "http::server", "浏览器采集持久化失败: {e}");
                manager.set_done(CapturePhase::Failed, e, None);
            }
        },
        Err(e) => {
            warn!(target: "http::server", "浏览器采集失败: {e}");
            manager.set_done(CapturePhase::Failed, e, None);
        }
    }
}

/// 启动采集（占用单槽；重复调用返回当前状态）
pub(crate) fn start_capture(
    manager: &Arc<CaptureManager>,
    ctx: CaptureContext,
    remark: String,
) -> Result<(), CaptureStatus> {
    manager.try_start()?;
    let manager = Arc::clone(manager);
    tokio::spawn(run_capture(manager, ctx, remark));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 通过 serde 构造 CDP 事件（生成类型无 new 构造器）
    fn auth_request_event(url: &str, headers: &serde_json::Value) -> EventRequestWillBeSent {
        serde_json::from_value(serde_json::json!({
            "requestId": "req-1",
            "loaderId": "loader-1",
            "documentURL": "https://chat.deepseek.com/",
            "request": {
                "url": url,
                "method": "GET",
                "headers": headers,
                "initialPriority": "High",
                "referrerPolicy": "strict-origin-when-cross-origin",
            },
            "timestamp": 0.0,
            "wallTime": 0.0,
            "initiator": { "type": "other" },
            "redirectHasExtraInfo": false,
        }))
        .expect("构造 CDP 事件")
    }

    fn credentials_from_headers(
        url: &str,
        headers: &serde_json::Value,
        api_prefix: &str,
    ) -> Option<CapturedCredentials> {
        inspect_request(&auth_request_event(url, headers), api_prefix)
    }

    /// 带 Bearer 的 API 请求 → 提取 token + UA + X-Device-Id（键大小写不敏感）
    #[test]
    fn inspect_request_expects_bearer_api_request() {
        let creds = credentials_from_headers(
            "https://chat.deepseek.com/api/v0/users/current",
            &serde_json::json!({
                "authorization": "Bearer M5qbJMIqU2CSqG0j",
                "user-agent": "Mozilla/5.0 Chrome/101.0.0.0",
                "X-Device-Id": "5512ebd8-9df7-4418-9534-578bd772f5e3",
            }),
            "https://chat.deepseek.com/api/v0/",
        )
        .expect("鉴权请求应被采集");
        assert_eq!(creds.token, "M5qbJMIqU2CSqG0j");
        assert_eq!(creds.user_agent, "Mozilla/5.0 Chrome/101.0.0.0");
        assert_eq!(creds.x_device_id, "5512ebd8-9df7-4418-9534-578bd772f5e3");

        // 非 API 请求 → 不采集
        assert!(
            credentials_from_headers(
                "https://fe-static.deepseek.com/chat/static/main.js",
                &serde_json::json!({}),
                "https://chat.deepseek.com/api/v0/",
            )
            .is_none()
        );
        // API 请求但空 Bearer → 不采集
        assert!(
            credentials_from_headers(
                "https://chat.deepseek.com/api/v0/users/current",
                &serde_json::json!({"Authorization": "Bearer  "}),
                "https://chat.deepseek.com/api/v0/",
            )
            .is_none()
        );
    }

    /// Cookie 列表按域过滤并序列化为请求头串；非法值跳过
    #[test]
    fn cookie_header_string_filters_by_domain_and_skips_illegal() {
        use chromiumoxide::cdp::browser_protocol::network::Cookie;

        let cookie = |name: &str, value: &str, domain: &str| -> Cookie {
            serde_json::from_value(serde_json::json!({
                "name": name, "value": value, "domain": domain,
                "path": "/", "expires": 0.0, "size": 1,
                "httpOnly": false, "secure": false, "session": true,
                "priority": "Medium", "sourceScheme": "Secure", "sourcePort": 443,
            }))
            .expect("构造 CDP Cookie")
        };

        let cookies = vec![
            cookie("ds_session_id", "abc123", ".deepseek.com"),
            cookie("smidV2", "fp-value", ".deepseek.com"),
            cookie("track", "1", ".volces.com"),
            cookie("bad", "x\ny", ".deepseek.com"),
        ];
        let header = cookie_header_string(&cookies, "deepseek.com");
        assert_eq!(header, "ds_session_id=abc123; smidV2=fp-value");
    }

    /// 端到端：真实（headless）Chrome 打开 mock 页面 → 页面自动发起带
    /// Authorization 的 API 请求 → 采集 token + cookies。
    /// 需要本机 Chrome：`cargo test --lib capture -- --ignored`
    #[tokio::test]
    #[ignore = "需要本机 Chrome"]
    async fn capture_pipeline_works_with_mock_page() {
        use axum::routing::get;

        const TOKEN: &str = "mocktoken1234567890";
        // mock 登录页：设置 Cookie 后立即发起带鉴权头的 API 请求
        let page = format!(
            "<!doctype html><html><body><script>\
             document.cookie='ds_session_id=mock-session-abc; path=/';\
             document.cookie='smidV2=mock-fp-xyz; path=/';\
             fetch('/api/v0/ping', {{headers: {{'Authorization': 'Bearer {TOKEN}', \
             'X-Device-Id': 'mock-device-uuid-1234'}}}});\
             </script></body></html>"
        );

        let app = axum::Router::new().route(
            "/sign_in",
            get(move || {
                let page = page.clone();
                async move { axum::response::Html(page) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let profile = std::env::temp_dir().join(format!(
            "ds-capture-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let config = BrowserConfig::builder()
            .user_data_dir(&profile)
            .build()
            .expect("浏览器配置");

        let seen = Arc::new(AtomicU32::new(0));
        let creds = capture_with(
            config,
            &format!("http://{addr}/sign_in"),
            &format!("http://{addr}/api/v0/"),
            "127.0.0.1",
            Duration::from_mins(2),
            Arc::clone(&seen),
        )
        .await
        .expect("采集应成功");

        assert_eq!(creds.token, TOKEN);
        assert_eq!(creds.x_device_id, "mock-device-uuid-1234");
        assert!(
            creds.cookies.contains("ds_session_id=mock-session-abc"),
            "cookies 应包含页面设置的会话 Cookie: {:?}",
            creds.cookies
        );
        assert!(
            creds.cookies.contains("smidV2=mock-fp-xyz"),
            "cookies 应包含指纹 Cookie: {:?}",
            creds.cookies
        );
        assert!(seen.load(Ordering::Relaxed) >= 1, "应观测到 API 请求");

        if let Err(e) = std::fs::remove_dir_all(&profile) {
            debug!(target: "http::server", "清理 mock profile 失败: {e}");
        }
    }

    /// 采集槽互斥：Running 期间再次启动被拒绝
    #[test]
    fn capture_slot_is_exclusive() {
        let manager = CaptureManager::new();
        assert!(manager.try_start().is_ok());
        assert!(manager.try_start().is_err(), "运行中不允许重复启动");
        manager.set_done(CapturePhase::Failed, "boom", None);
        assert!(manager.try_start().is_ok(), "失败后释放采集槽");
        let status = manager.status();
        assert_eq!(status.phase, CapturePhase::Running);
    }
}
