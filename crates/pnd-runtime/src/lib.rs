//! actor 线程:串行交易网关、轮询调度、判定去重、live 会话管理与
//! ninja 采样管线;`src/bin/*_probe.rs` 是调用同一套生产函数的验证探针。
//!
//! 三条线程,职责分得很死:
//! - `pnd-runtime`([`actor`])—— 唯一的写者,手里是全部会动的状态。
//! - `pnd-trade-gateway`([`gateway`])—— 唯一碰交易站的线程,一条串行队列。
//! - `pnd-rates` —— 每 15 分钟去 poe.ninja 读一次换算表。
//!
//! [`poll`] 和 [`decide`] 里没有线程也没有 I/O:一个回答"什么时候该跑",
//! 一个回答"这条挂单值不值得叫人",两个都是纯函数,可以在测试里把几个小时
//! 的行为在一毫秒里跑完。

use std::time::{SystemTime, UNIX_EPOCH};

pub mod actor;
pub mod decide;
pub mod gateway;
pub mod poll;

pub use actor::{
    RuntimeCommand, RuntimeError, RuntimeEvent, RuntimeHandle, RuntimePaths, WatchRunState,
    WatchStatus,
};
pub use decide::{Decision, MatchedListing, coalesce, decide};
pub use gateway::{
    GatewayError, GatewayEvent, GatewayHandle, GatewayMessage, GatewayReply, GatewayRequest,
    Priority, ReplyKind, RequestKind, RequestTag, SearchOutcome, TradeGateway, TradeTransport,
};
pub use poll::{PollEntry, PollOutcome, PollScheduler};

/// 现在是 unix 秒。
///
/// 整个 crate 只有这一个地方读系统时钟:别的函数都把 `now` 当参数收,
/// 于是"限速退避到底退了多久""第三次失败之后隔多久再来"这些问题
/// 都能在测试里钉死时间跑出来。
#[must_use]
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
