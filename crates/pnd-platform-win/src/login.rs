//! 程序内登录官网:平台无关的对外接口。
//!
//! 在这一步之前,拿到 `POESESSID` 的唯一办法是开浏览器的开发者工具、翻
//! Application → Cookies、把那一长串复制出来粘进设置页。那既难教又容易粘错,
//! 而且 cookie 是 HttpOnly 的,油猴脚本读不到,直接去读 Chrome/Edge 的 cookie
//! 库既像木马又被浏览器的新防护挡着。
//!
//! 于是换成:程序自己开一个窗口,里面跑 Windows 11 自带的 Edge 内核
//! (WebView2),你在里面**正常登录**(账号密码 / Steam / Epic 都行),
//! 登录成功后官网会把你送回 `/my-account`,程序在那一刻从 WebView2 自己的
//! cookie 管理器里读走 `POESESSID`,关窗,顺手跑一次"测试会话"。
//!
//! 三条纪律:
//!
//! - **程序不碰密码。** 输入框是 Edge 的,页面是官网的,程序只在最后读一个
//!   cookie。
//! - **cookie 值永远不进日志。** [`LoginEvent`] 的 `Debug` 把它换成长度,
//!   探针打印的也是长度。
//! - **窗口是你点出来的。** 它像任何一个普通窗口那样拿走焦点,但程序自己
//!   永远不会主动弹它。
//!
//! 真正的 Win32 + WebView2 实现在 `win32::login_window`,这里只放类型、
//! 一个把命令丢过去/把事件收回来的 [`LoginService`],以及三小块带测试的
//! 纯逻辑(判定"登录成了没"、挑出 cookie、算窗口位置)。

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::PlatformError;
use crate::alert_card::{RectI, scale_for_dpi};

/// 登录线程启动握手的上限。和卡片同一个数:线程没在这个时间里把消息队列
/// 建起来,就当它没起来。
const STARTUP_TIMEOUT: Duration = Duration::from_millis(750);

/// 关窗时最多等线程收摊多久。WebView2 退出要跟浏览器进程打个招呼,比卡片慢,
/// 但界面线程不该为它卡住 —— 等不到就放手,那条线程自己会走完。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(1_500);

/// 登录窗在 96 dpi 下的逻辑尺寸。官网登录页(以及 Steam / Epic 的授权页)
/// 在这个尺寸下不用横向滚动。
pub const LOGIN_LOGICAL_WIDTH: i32 = 1000;
pub const LOGIN_LOGICAL_HEIGHT: i32 = 760;

/// 官网"我的账号"页。登录成不成,问它最准。
///
/// **注意它不重定向。** 2026-09-07 实测:匿名访客访问它拿到的是 **HTTP 401**,
/// 地址栏还停在 `/my-account`。所以"这一跳落在 /my-account"**不能**当成
/// "已经登录了" —— 判据是那一跳的 HTTP 状态码,见 [`after_navigation`]。
pub const ACCOUNT_URL: &str = "https://www.pathofexile.com/my-account";

/// 官网登录页。`/my-account` 答 401 时把人送到这儿,否则他会盯着一页
/// "401 Unauthorized" 不知道该干嘛。
pub const LOGIN_URL: &str = "https://www.pathofexile.com/login";

/// 官网自己。用来分辨"这一跳还在官网上"(多半是刚登完被送回来了)和
/// "这一跳在 Steam / Epic / Cloudflare 那边"(那是登录流程的中间站,别打扰)。
pub const SITE_PREFIX: &str = "https://www.pathofexile.com/";

/// 读 cookie 时问的域。cookie 是按域存的,和当前页面在哪儿无关。
pub const COOKIE_ORIGIN: &str = "https://www.pathofexile.com";

/// 要找的那个 cookie 的名字。
pub const SESSION_COOKIE: &str = "POESESSID";

/// 登录窗自己最多主动跳几次。
///
/// 有上限是因为"401 就送去登录页"和"回到官网别的页面就回去核对"这两条规则
/// 加在一起,理论上能跟着用户的浏览来回弹。八次足够走完任何一条登录流程
/// (账号密码 / Steam / Epic 都是三四跳),弹够了就彻底安静下来,
/// 剩下的交给设置页那个"现在核对一次"按钮。
pub const MAX_AUTO_NAVIGATIONS: u32 = 8;

/// 起一条登录线程需要的全部配置。
#[derive(Clone, Debug)]
pub struct LoginConfig {
    /// WebView2 的用户数据目录(cookie、缓存都落在这里)。
    ///
    /// 单独给一个目录、而不是让它用默认位置:这样"登录状态"是本程序自己的,
    /// 既不动你浏览器里的登录,也能在出问题时整个删掉重来。
    pub user_data_dir: PathBuf,
    /// 窗口标题。
    pub title: String,
    /// 窗口顶上那条提示。突然弹出来一个浏览器窗口而不说自己是干嘛的,
    /// 第一反应会是关掉它。
    pub hint_text: String,
}

/// 发给登录线程的命令。外部只通过 [`LoginService`] 的方法产生它们。
#[derive(Clone, Debug)]
pub(crate) enum LoginCommand {
    Open,
    Capture,
    Close,
    Shutdown,
}

/// 登录窗为什么没能用起来。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginFailure {
    /// 这台机器上没有 WebView2 运行时。Windows 11 默认带,所以基本只会出现在
    /// 被精简过的系统上;调用方该退回"手动粘 cookie"那条路。
    RuntimeMissing,
    /// 建 WebView2 环境失败(用户数据目录写不了之类)。
    EnvironmentFailed(String),
    /// 环境有了,但控件挂不到窗口上。
    ControllerFailed(String),
    /// cookie 管理器读不出来。
    CookieReadFailed(String),
}

impl fmt::Display for LoginFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeMissing => formatter.write_str("the WebView2 runtime is not installed"),
            Self::EnvironmentFailed(detail) => {
                write!(formatter, "the WebView2 environment failed: {detail}")
            }
            Self::ControllerFailed(detail) => {
                write!(formatter, "the WebView2 controller failed: {detail}")
            }
            Self::CookieReadFailed(detail) => {
                write!(
                    formatter,
                    "the WebView2 cookies could not be read: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for LoginFailure {}

/// 登录线程报回来的事情。调用方每帧 `try_next_event` 抽干即可。
///
/// `Debug` 是手写的,唯一的理由是 [`Self::SessionCaptured`]:那个字段就是会话
/// 本身,`derive` 出来的 `Debug` 会把它原样印进任何一句 `{:?}` —— 日志、
/// panic 消息、探针输出。手写的这份只印长度。
#[derive(Clone, Eq, PartialEq)]
pub enum LoginEvent {
    /// 窗口建出来了,WebView2 已经挂上去,正在往官网走。
    Opened,
    /// 一次页面跳转走完了。Cloudflare 的"稍等一下"、Steam / Epic 的授权页,
    /// 在这里都只是一条普通的一跳,不做任何特判。
    ///
    /// 状态码跟着一起报,因为它就是 [`after_navigation`] 的判据:出问题时
    /// 只看 URL 说不清"为什么没抓到"(`/my-account` 答 401 和答 200 的地址
    /// 一模一样)。`None` = 这台机器上的 WebView2 太老、报不出状态码。
    Navigated {
        url: String,
        http_status: Option<i32>,
    },
    /// 这次读到的域下有哪些 cookie。**只有名字,永远没有值。**
    ///
    /// 它存在的理由是可验证性:不打开这一眼,就没法回答"匿名状态下官网到底
    /// 发不发 POESESSID"这个问题,而那个答案决定了自动抓取该怎么设防。
    CookieNames(Vec<String>),
    /// 读到会话了。调用方存下来就该关窗。
    SessionCaptured {
        poesessid: String,
    },
    /// 读了,但域下没有会话 —— 还没登录,或者登录没走完。
    NotLoggedIn,
    /// 窗口没了(用户关的,或者抓到会话之后程序关的)。
    Closed,
    Failed(LoginFailure),
}

impl fmt::Debug for LoginEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Opened => formatter.write_str("Opened"),
            Self::Navigated { url, http_status } => formatter
                .debug_struct("Navigated")
                .field("url", url)
                .field("http_status", http_status)
                .finish(),
            Self::CookieNames(names) => formatter.debug_tuple("CookieNames").field(names).finish(),
            Self::SessionCaptured { poesessid } => formatter
                .debug_struct("SessionCaptured")
                .field("poesessid", &Redacted(poesessid.chars().count()))
                .finish(),
            Self::NotLoggedIn => formatter.write_str("NotLoggedIn"),
            Self::Closed => formatter.write_str("Closed"),
            Self::Failed(failure) => formatter.debug_tuple("Failed").field(failure).finish(),
        }
    }
}

/// `<redacted 32 chars>`。长度留着是因为它能回答"是不是读到了个空串",
/// 而值本身一个字符都不该出现。
struct Redacted(usize);

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<redacted {} chars>", self.0)
    }
}

/// 调用线程和登录线程之间的全部共享状态。和卡片同一套:命令进
/// `Mutex<VecDeque>`,再用 `PostThreadMessageW` 把线程从 `GetMessageW` 里叫醒。
#[derive(Debug, Default)]
pub(crate) struct LoginShared {
    commands: Mutex<VecDeque<LoginCommand>>,
    native_thread_id: AtomicU64,
}

impl LoginShared {
    pub(crate) fn push(&self, command: LoginCommand) {
        self.commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push_back(command);
    }

    pub(crate) fn pop(&self) -> Option<LoginCommand> {
        self.commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .pop_front()
    }

    /// 队列里还有没有命令。登录线程用它补一次被嵌套消息泵吃掉的唤醒。
    pub(crate) fn has_commands(&self) -> bool {
        !self
            .commands
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_empty()
    }

    pub(crate) fn native_thread_id(&self) -> u32 {
        self.native_thread_id.load(Ordering::Acquire) as u32
    }

    pub(crate) fn set_native_thread_id(&self, id: u32) {
        self.native_thread_id
            .store(u64::from(id), Ordering::Release);
    }
}

/// 拥有一条登录线程。所有方法都不等待窗口,界面线程不会被卡住。
pub struct LoginService {
    shared: Arc<LoginShared>,
    events: mpsc::Receiver<LoginEvent>,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for LoginService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoginService")
            .field("native_thread_id", &self.shared.native_thread_id())
            .finish_non_exhaustive()
    }
}

impl LoginService {
    /// 起一条名为 `pnd-login` 的 STA 线程,等它把消息队列建好再返回。
    ///
    /// 这一步**不建窗口**:窗口是 [`open`](Self::open) 的事。等的只是那条线程
    /// 的消息队列 —— 队列还没有的时候 `PostThreadMessageW` 会把命令丢掉。
    pub fn start(config: LoginConfig) -> Result<Self, PlatformError> {
        let shared = Arc::new(LoginShared::default());
        let (event_sender, event_receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread =
            platform_spawn_worker(config, Arc::clone(&shared), event_sender, ready_sender)?;
        match ready_receiver.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                shared,
                events: event_receiver,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                // 线程还活着但没就绪:先请它退出,再把超时报上去 —— 不然它会
                // 一直挂在那儿,而调用方手上已经没有能关掉它的东西了。
                shared.push(LoginCommand::Shutdown);
                let _ = platform_wake(shared.native_thread_id());
                Err(PlatformError::Thread {
                    operation: "LoginService::start",
                    detail: format!(
                        "the login thread was not ready within {} ms: {error}",
                        STARTUP_TIMEOUT.as_millis()
                    ),
                })
            }
        }
    }

    /// 开窗并走到官网。窗口已经开着就什么都不做。
    pub fn open(&self) -> Result<(), PlatformError> {
        self.dispatch(LoginCommand::Open)
    }

    /// 现在就读一次 cookie(设置页上那个"我已经登录了,现在读"的备选按钮)。
    pub fn capture(&self) -> Result<(), PlatformError> {
        self.dispatch(LoginCommand::Capture)
    }

    /// 关窗。线程留着,下次点登录还用它。
    pub fn close(&self) -> Result<(), PlatformError> {
        self.dispatch(LoginCommand::Close)
    }

    /// 取一条已经发生的事件,没有就返回 `None`,永不阻塞。
    #[must_use]
    pub fn try_next_event(&self) -> Option<LoginEvent> {
        self.events.try_recv().ok()
    }

    fn dispatch(&self, command: LoginCommand) -> Result<(), PlatformError> {
        let thread_id = self.shared.native_thread_id();
        if thread_id == 0 {
            return Err(PlatformError::Thread {
                operation: "LoginService::dispatch",
                detail: "the login thread is gone".to_owned(),
            });
        }
        self.shared.push(command);
        platform_wake(thread_id)
    }
}

impl Drop for LoginService {
    fn drop(&mut self) {
        self.shared.push(LoginCommand::Close);
        self.shared.push(LoginCommand::Shutdown);
        let _ = platform_wake(self.shared.native_thread_id());
        let Some(thread) = self.thread.take() else {
            return;
        };
        // 只等一小会儿。WebView2 收摊要和浏览器进程打招呼,偶尔会拖;拖过头就
        // 放手让那条线程自己走完 —— 关个登录窗不值得把界面冻住。
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
}

fn platform_spawn_worker(
    config: LoginConfig,
    shared: Arc<LoginShared>,
    events: mpsc::Sender<LoginEvent>,
    ready: mpsc::SyncSender<Result<(), PlatformError>>,
) -> Result<JoinHandle<()>, PlatformError> {
    #[cfg(windows)]
    {
        crate::win32::spawn_login_worker(config, shared, events, ready)
    }
    #[cfg(not(windows))]
    {
        crate::non_windows::spawn_login_worker(config, shared, events, ready)
    }
}

fn platform_wake(thread_id: u32) -> Result<(), PlatformError> {
    #[cfg(windows)]
    {
        crate::win32::wake_login(thread_id)
    }
    #[cfg(not(windows))]
    {
        crate::non_windows::wake_login(thread_id)
    }
}

/// 一次页面跳转走完之后,登录窗该做什么。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AfterNavigation {
    /// 什么都别做。登录页本身、Steam / Epic 的授权页、Cloudflare 的
    /// "稍等一下",都是登录流程的中间站,插手只会把人打断。
    Wait,
    /// 已经登录了 —— 读 cookie。
    Capture,
    /// 还没登录 —— 把人送到登录页。
    GoToLogin,
    /// 人回到了官网上别的页面。多半是刚登完被送回去了,回"我的账号"核对一次。
    RecheckAccount,
}

/// 这条 URL 是不是"我的账号"页。
///
/// 前缀比完还要看下一个字符:`/my-account-recovery` 之类的路径开头一样,
/// 但它不是同一个页面。
#[must_use]
pub fn is_account_url(url: &str) -> bool {
    let Some(rest) = url.trim().strip_prefix(ACCOUNT_URL) else {
        return false;
    };
    matches!(rest.chars().next(), None | Some('/' | '?' | '#'))
}

/// 一次跳转走完之后该做什么。整套自动判定就这一个函数,所以它是纯的、带测试的。
///
/// **为什么要带 HTTP 状态码。** 一开始的设计是"落在 `/my-account` 就等于登录成功",
/// 依据是"匿名访客会被 302 到 `/login`"。2026-09-07 实测:官网**不重定向**,
/// 它对匿名访客直接答 **401**,地址栏原地不动。与此同时 Cloudflare 给每个访客
/// 都发一个 32 位的 `POESESSID` —— 于是只看 URL 的话,程序会把一个匿名会话
/// 当成登录成功存进设置,盖掉你上一次真正登出来的那个。
///
/// 所以判据是**两条一起**:URL 是"我的账号"页,**并且**那一跳答了 200。
///
/// `http_status` 为 `None` = 这台机器上的 WebView2 太老、报不出状态码。那就当
/// 没登录:宁可让人手动粘一次 cookie,也不能存一个假的进去。
#[must_use]
pub fn after_navigation(url: &str, http_status: Option<i32>) -> AfterNavigation {
    let url = url.trim();
    if is_account_url(url) {
        return if http_status == Some(200) {
            AfterNavigation::Capture
        } else {
            AfterNavigation::GoToLogin
        };
    }
    if url.starts_with(LOGIN_URL) || !url.starts_with(SITE_PREFIX) {
        return AfterNavigation::Wait;
    }
    AfterNavigation::RecheckAccount
}

/// 从一域 cookie 里挑出会话。
///
/// 名字按大小写不敏感比:cookie 名在 HTTP 里是区分大小写的,但这里宁可宽一点
/// 也不要因为官网哪天写成 `PoeSessId` 就整条路走不通。空值当没有 —— 存一个
/// 空串进设置,后面每一次请求都会带着一个空 Cookie 头出去。
#[must_use]
pub fn pick_session_cookie(cookies: &[(String, String)]) -> Option<String> {
    cookies
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(SESSION_COOKIE))
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// 登录窗摆在工作区正中间时的屏幕矩形。
///
/// 纯函数的理由和卡片那个一样:窗口还没建出来就得知道摆哪儿,而"居中"在高
/// dpi 下和小屏幕上最容易算错(1000×760 在一块 1366×768 的屏上放不下)。
#[must_use]
pub fn login_geometry(work_area: RectI, dpi: u32) -> RectI {
    let width = scale_for_dpi(LOGIN_LOGICAL_WIDTH, dpi).min(work_area.w.max(1));
    let height = scale_for_dpi(LOGIN_LOGICAL_HEIGHT, dpi).min(work_area.h.max(1));
    let x = work_area.x + (work_area.w - width) / 2;
    let y = work_area.y + (work_area.h - height) / 2;
    RectI::new(
        x.clamp(work_area.x, (work_area.right() - width).max(work_area.x)),
        y.clamp(work_area.y, (work_area.bottom() - height).max(work_area.y)),
        width,
        height,
    )
}

#[cfg(test)]
mod login_tests {
    use super::*;

    fn cookies(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn the_account_page_is_recognised_with_any_trailing_bit() {
        assert!(is_account_url("https://www.pathofexile.com/my-account"));
        assert!(is_account_url("https://www.pathofexile.com/my-account/"));
        assert!(is_account_url(
            "https://www.pathofexile.com/my-account?tab=profile"
        ));
        assert!(is_account_url(
            "  https://www.pathofexile.com/my-account#top  "
        ));
    }

    #[test]
    fn look_alike_urls_are_not_the_account_page() {
        for other in [
            "https://www.pathofexile.com/login",
            "https://www.pathofexile.com/login/steam",
            // 前缀一样但不是同一个页面 —— 只比前缀会把它当成"我的账号"。
            "https://www.pathofexile.com/my-account-recovery",
            // 别人家的域名,哪怕路径一模一样。
            "https://evil.example.com/my-account",
            "https://www.pathofexile.com.evil.example/my-account",
            // 明文 http 不算:会话 cookie 不该在明文页上被认。
            "http://www.pathofexile.com/my-account",
            "",
        ] {
            assert!(!is_account_url(other), "{other} 不该被当成我的账号页");
        }
    }

    /// 这条测试就是那个 bug 的墓碑:2026-09-07 之前的判据只看 URL,而官网对
    /// 匿名访客答的是 401 而不是重定向 —— 于是一个匿名会话会被当成登录成功。
    #[test]
    fn the_account_page_only_counts_when_it_answered_200() {
        assert_eq!(
            after_navigation(ACCOUNT_URL, Some(200)),
            AfterNavigation::Capture
        );
        // 401 = 匿名访客。这是实测到的那一种。
        assert_eq!(
            after_navigation(ACCOUNT_URL, Some(401)),
            AfterNavigation::GoToLogin
        );
        // 报不出状态码的老 WebView2:当没登录,宁可让人手动粘一次。
        assert_eq!(
            after_navigation(ACCOUNT_URL, None),
            AfterNavigation::GoToLogin
        );
        for status in [403, 404, 500, 0] {
            assert_eq!(
                after_navigation(ACCOUNT_URL, Some(status)),
                AfterNavigation::GoToLogin,
                "status {status}"
            );
        }
    }

    #[test]
    fn the_login_flow_is_left_alone_and_a_detour_gets_rechecked() {
        // 登录页本身:人正在里面打字,别动。
        for staying in [
            LOGIN_URL,
            "https://www.pathofexile.com/login/steam",
            // Steam / Epic 的授权页、Cloudflare 的"稍等一下"都不是官网的域。
            "https://steamcommunity.com/openid/login",
            "https://www.epicgames.com/id/login",
            "https://challenges.cloudflare.com/turnstile",
        ] {
            assert_eq!(
                after_navigation(staying, Some(200)),
                AfterNavigation::Wait,
                "{staying}"
            );
        }
        // 官网上别的页面 = 多半刚登完被送回来了,回去核对一次。
        for detour in [
            "https://www.pathofexile.com/",
            "https://www.pathofexile.com/trade2",
            "https://www.pathofexile.com/account/view-profile/Exile",
        ] {
            assert_eq!(
                after_navigation(detour, Some(200)),
                AfterNavigation::RecheckAccount,
                "{detour}"
            );
        }
    }

    #[test]
    fn the_session_cookie_is_found_by_name_whatever_the_case() {
        let found = pick_session_cookie(&cookies(&[
            ("cf_clearance", "abc"),
            ("POESESSID", "0123456789abcdef"),
        ]));
        assert_eq!(found.as_deref(), Some("0123456789abcdef"));

        let odd_case = pick_session_cookie(&cookies(&[("PoeSessId", "zz")]));
        assert_eq!(odd_case.as_deref(), Some("zz"));
    }

    #[test]
    fn a_missing_or_empty_session_cookie_is_none() {
        assert_eq!(pick_session_cookie(&cookies(&[])), None);
        assert_eq!(
            pick_session_cookie(&cookies(&[("cf_clearance", "abc")])),
            None
        );
        // 空值存下来只会让后面每一次请求都带一个空 Cookie 头。
        assert_eq!(pick_session_cookie(&cookies(&[("POESESSID", "   ")])), None);
    }

    #[test]
    fn the_window_sits_in_the_middle_of_the_work_area() {
        let work = RectI::new(0, 0, 1920, 1040);
        let window = login_geometry(work, 96);
        assert_eq!(window.w, LOGIN_LOGICAL_WIDTH);
        assert_eq!(window.h, LOGIN_LOGICAL_HEIGHT);
        assert_eq!(window.x, (1920 - LOGIN_LOGICAL_WIDTH) / 2);
        assert_eq!(window.y, (1040 - LOGIN_LOGICAL_HEIGHT) / 2);
    }

    #[test]
    fn the_window_scales_with_dpi_and_still_fits_a_small_screen() {
        // 150% 缩放 = 144 dpi。
        let big = login_geometry(RectI::new(0, 0, 2560, 1400), 144);
        assert_eq!(big.w, 1500);
        assert_eq!(big.h, 1140);

        // 1366×768 放不下 1000×760 的 96 dpi 窗口的高度余量,更放不下 144 dpi 的。
        let small = login_geometry(RectI::new(0, 0, 1366, 728), 144);
        assert_eq!(small, RectI::new(0, 0, 1366, 728));
    }

    #[test]
    fn a_shifted_work_area_still_centres_the_window() {
        // 副屏在主屏左边:x 是负的,居中不能从 0 起算。
        let work = RectI::new(-1920, 100, 1920, 1000);
        let window = login_geometry(work, 96);
        assert_eq!(window.x + window.w / 2, work.x + work.w / 2);
        assert!(window.x >= work.x && window.right() <= work.right());
    }

    /// 会话值绝不能出现在任何一句 `{:?}` 里 —— 那是日志、panic 消息和探针
    /// 输出的共同出口。
    #[test]
    fn debug_never_prints_the_session_value() {
        let event = LoginEvent::SessionCaptured {
            poesessid: "super-secret-session-value".to_owned(),
        };
        let printed = format!("{event:?}");
        assert!(!printed.contains("super-secret"), "{printed}");
        assert!(printed.contains("<redacted 26 chars>"), "{printed}");
    }

    #[test]
    fn the_other_events_print_something_useful() {
        let navigated = format!(
            "{:?}",
            LoginEvent::Navigated {
                url: ACCOUNT_URL.to_owned(),
                http_status: Some(401),
            }
        );
        assert!(navigated.contains("my-account"), "{navigated}");
        assert!(navigated.contains("401"), "{navigated}");
        assert_eq!(format!("{:?}", LoginEvent::NotLoggedIn), "NotLoggedIn");
        assert_eq!(format!("{:?}", LoginEvent::Closed), "Closed");
        let failed = format!("{:?}", LoginEvent::Failed(LoginFailure::RuntimeMissing));
        assert!(failed.contains("RuntimeMissing"), "{failed}");
        let names = format!(
            "{:?}",
            LoginEvent::CookieNames(vec![SESSION_COOKIE.to_owned()])
        );
        assert!(names.contains(SESSION_COOKIE), "{names}");
    }
}
