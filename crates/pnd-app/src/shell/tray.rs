//! 外壳和通知区("托盘")图标之间的那根线。
//!
//! 单独一个文件的理由和 [`super::link`] 一样:方向不同。这里只有三件事 ——
//! 起图标、把托盘线程报回来的事件接住、回答"点叉到底关不关窗"。
//!
//! 为什么要这个功能:程序一开就是几个小时(蹲价在轮询、市场观察在攒数据、
//! ninja 在采样),而窗口右上角那个叉今天等于"退出",手滑一下就把它们全掐了。
//! 缩到托盘之后,叉只是把窗口藏起来,后台照跑。

use gpui::{Context, Window};
use pnd_platform_win::{TrayConfig, TrayEvent, TrayHandle, TrayService};

use super::AppShell;
use crate::i18n::Text;

/// 起托盘图标。
///
/// 失败不致命(和 actor、卡片一个待遇):程序照常开窗,只是点叉会像以前一样
/// 直接退出 —— 见 [`close_should_quit`]。
pub(crate) fn start_tray(
    text: &'static Text,
    window: &Window,
    log: &mut Vec<String>,
) -> Option<TrayHandle> {
    let main_hwnd = main_window_hwnd(window);
    if main_hwnd == 0 {
        // 没有句柄就没法藏窗口。图标照样加(右键还能退出),但那说明有事不对劲。
        log.push("tray: could not read the main window handle".to_owned());
    }
    let config = TrayConfig {
        tooltip: text.app_title.to_owned(),
        main_hwnd,
        menu_open: text.tray_menu_open.to_owned(),
        menu_quit: text.tray_menu_quit.to_owned(),
    };
    match TrayService::start(config) {
        Ok(tray) => {
            // 图标从哪儿取的要记一行:退回系统默认图标时通知区上什么都没坏,
            // 只是那张图不再是本程序的 —— 不说一声就只会以为"本来就长这样"。
            log.push(format!("tray: icon added, source = {}", tray.icon_source()));
            Some(tray)
        }
        Err(error) => {
            log.push(format!("tray icon failed to start: {error}"));
            None
        }
    }
}

/// gpui 的窗口 → Win32 的 `HWND`。取不到就是 0,平台层认这个数字是"没有主窗口"。
///
/// 要写成 `HasWindowHandle::window_handle(window)` 而不是 `window.window_handle()`:
/// gpui 自己在 `Window` 上也有一个同名的固有方法,而固有方法优先 —— 点出来的
/// 那个返回的是 gpui 内部的窗口 id,不是系统句柄。
fn main_window_hwnd(window: &Window) -> isize {
    let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window) else {
        return 0;
    };
    match handle.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(win32) => win32.hwnd.get(),
        _ => 0,
    }
}

/// 点叉之后该不该真的把窗口关掉(关掉 = 程序退出)。
///
/// 两个"照旧退出"要分清楚:设置里关掉这个功能当然照旧退出;而**托盘图标
/// 没起来**时也必须照旧退出 —— 否则窗口会藏进一个再也点不回来的地方,
/// 只能去任务管理器杀进程。
#[must_use]
pub(crate) fn close_should_quit(close_to_tray: bool, tray_ready: bool) -> bool {
    !(close_to_tray && tray_ready)
}

impl AppShell {
    /// 窗口的关闭按钮按下了。返回 `true` = 真的关。
    pub(crate) fn on_close_requested(&mut self) -> bool {
        if close_should_quit(self.settings.close_to_tray, self.tray.is_some()) {
            return true;
        }
        let Some(result) = self.tray.as_ref().map(TrayHandle::hide_main) else {
            return true;
        };
        if let Err(error) = result {
            // 藏不起来就老老实实关掉,别留一个"点了叉什么都没发生"的窗口。
            self.push_log(format!("tray: could not hide the main window: {error}"));
            return true;
        }
        self.push_log("tray: main window hidden".to_owned());
        false
    }

    /// 把托盘线程报回来的事件抽干。返回"有没有东西变了"。
    pub(crate) fn drain_tray_events(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        loop {
            let Some(event) = self.tray.as_ref().and_then(TrayHandle::try_next_event) else {
                break;
            };
            changed = true;
            match event {
                TrayEvent::Restore => self.restore_from_tray(),
                TrayEvent::Quit => self.quit_from_tray(cx),
            }
        }
        changed
    }

    /// 点了托盘图标:把窗口拿回来。
    ///
    /// **这里故意不调 gpui 的 `Window::activate_window()`。** 那个方法为了绕过
    /// Windows"没收到过输入的进程不许抢前台"那条规矩,会用 `SendInput` 伪造
    /// 一次 Alt 按下抬起 —— 而这个程序旁边通常开着游戏,那一下 Alt 会实打实地
    /// 按进游戏里。本项目的底线是**永远不向游戏发输入**,所以宁可少一点
    /// "一定抢到前台"的把握。
    ///
    /// 平台层的 `show_main` 做的是同样的三步(显示、必要时取消最小化、
    /// `SetForegroundWindow`),只是不伪造那一下按键:绝大多数时候够用,
    /// 偶尔抢不到前台的话任务栏上那一格会闪,点一下就是了。
    fn restore_from_tray(&mut self) {
        let Some(result) = self.tray.as_ref().map(TrayHandle::show_main) else {
            return;
        };
        if let Err(error) = result {
            self.push_log(format!("tray: could not show the main window: {error}"));
            return;
        }
        self.push_log("tray: main window shown".to_owned());
    }

    /// 托盘菜单里选了"退出程序"。开着"点叉缩起来"的时候,这就是关掉它的那条路。
    fn quit_from_tray(&mut self, cx: &mut Context<Self>) {
        self.push_log("tray: quit".to_owned());
        // 先摘图标再退出:进程直接结束的话,通知区里会留下一个点了没反应的
        // 僵尸图标,要鼠标划过去才消失。
        if let Some(tray) = &mut self.tray {
            tray.stop();
        }
        self.tray = None;
        cx.quit();
    }
}

#[cfg(test)]
mod tray_tests {
    use super::close_should_quit;

    /// 点叉的四种情形。
    ///
    /// 最要紧的是"开着这个功能、但托盘图标没起来"那一条:那时候必须照旧退出。
    /// 藏起来的话窗口就进了一个点不回来的地方,只能去任务管理器杀进程 ——
    /// 而这个程序的后台正攒着几个小时的观察数据。
    #[test]
    fn the_close_button_only_hides_when_there_is_somewhere_to_hide() {
        assert!(!close_should_quit(true, true), "开着而且有托盘 = 藏起来");
        assert!(close_should_quit(false, true), "设置里关掉 = 照旧退出");
        assert!(close_should_quit(true, false), "托盘没起来 = 照旧退出");
        assert!(close_should_quit(false, false));
    }
}
