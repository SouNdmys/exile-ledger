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
pub mod live_worker;
pub mod ninja_sampler;
pub mod poll;

pub use actor::{
    HideoutOutcome, RuntimeCommand, RuntimeError, RuntimeEvent, RuntimeHandle, RuntimePaths,
    WatchRunState, WatchStatus,
};
pub use decide::{Decision, MatchedListing, coalesce, decide};
pub use gateway::{
    GatewayError, GatewayEvent, GatewayHandle, GatewayMessage, GatewayReply, GatewayRequest,
    Priority, ReplyKind, RequestKind, RequestTag, SearchOutcome, SessionCheckOutcome, TradeGateway,
    TradeTransport,
};
pub use live_worker::{
    LiveConnector, LiveEvent, LiveOffReason, LiveRunState, LiveStream, LiveWorkerConfig,
    LiveWorkerHandle, TungsteniteConnector, backoff_delay, jitter_for, next_attempt,
    run_live_worker, spawn_live_worker,
};
pub use ninja_sampler::{
    HourlyBudget, SamplerConfig, SamplerError, SamplerEvent, SamplerHandle, SamplerPlan,
    SamplerStage, SamplerStep, budget_delay, budget_eta_secs, character_limit, highest_stage,
    plan_partitions, rate_limit_delay, refresh_prices, run_sampler, should_skip, stage_order,
    stage_plan,
};
pub use poll::{PollEntry, PollOutcome, PollScheduler, budget_floor_interval};

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

/// 一张短命 token 的"体检报告":有没有、多长、什么时候作废、还剩多久。
///
/// **永远不印 token 本身。** 它是能替你私聊、替你传送的凭证,而这个字符串
/// 会进日志、进探针的屏幕输出。长度和 `exp` 已经够回答唯一要紧的那个问题:
/// "我按下按钮的这一刻,它还活着吗"。
///
/// 运行时决定"要不要先换一张"时记的就是这一句,两个探针印的也是这一句 ——
/// 于是屏幕上看到的和程序判断的是同一件事。
#[must_use]
pub fn describe_token(token: Option<&str>, now: i64) -> String {
    let Some(token) = token else {
        return "absent".to_string();
    };
    let chars = token.chars().count();
    let Some(exp) = pnd_trade::jwt_expiry(token) else {
        // 不是"坏了" —— 只是问不出作废时刻,那时只能退回"拿到多久了"去猜。
        return format!("present ({chars} chars, no readable exp)");
    };
    let left = exp - now;
    let when = chrono::DateTime::from_timestamp(exp, 0).map_or_else(
        || exp.to_string(),
        |utc| {
            utc.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        },
    );
    let remaining = if left > 0 {
        format!("{}m{}s left", left / 60, left % 60)
    } else {
        format!("expired {}s ago", -left)
    };
    format!("present ({chars} chars, exp {when} local, {remaining})")
}
