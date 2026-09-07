//! 交易站限速器:纯逻辑,不碰网络也不读系统时钟。
//!
//! 时间一律由调用方以 `i64` unix 秒传进来。这样限速器是完全确定的:同样的
//! 输入永远给同样的答案,测试可以把六个小时的历史在一毫秒里跑完,也不会
//! 因为机器时钟跳变而行为漂移。
//!
//! 官方交易站在每个响应里都带限速头:`X-Rate-Limit-<规则>` 是上限,
//! `X-Rate-Limit-<规则>-State` 是当前用量。两个头都是逗号分隔的
//! `次数:窗口秒:冷却秒` 三元组,而且**同下标同窗口**——第 i 个上限桶和第 i 个
//! 用量桶说的是同一个时间窗。
//!
//! 移植自 Path of Building 的 `TradeQueryRateLimiter.lua`,但有意做了几处
//! 改动,原因写在各自的函数上。
//!
//! 我们只用服务端预算的一半(见 [`Budget`]):同一个 IP 上用户自己还开着
//! 浏览器和别的交易工具,贴着上限跑就是在替他们踩线。

use std::collections::{BTreeMap, VecDeque};

/// 搜索接口的限速策略名(响应头 `X-Rate-Limit-Policy` 的取值)。
pub const SEARCH_POLICY: &str = "trade-search-request-limit";

/// 抓取挂单详情的限速策略名。
pub const FETCH_POLICY: &str = "trade-fetch-request-limit";

/// search 策略最长的那个桶:6 小时 600 次(2026-09-06 实测的 `600:21600:3600`)。
///
/// 短窗口(10 秒 5 次那种)靠限速器排队就能扛过去,只有这个 6 小时的桶会在
/// 跑了一整天之后把人卡死 —— 所以轮询节奏的地板按它算。
pub const SEARCH_LONG_WINDOW_REQUESTS: u32 = 600;
pub const SEARCH_LONG_WINDOW_SECS: u32 = 21_600;

/// 一条 `次数:窗口秒:冷却秒` 三元组。
///
/// 同样的写法在两个头里含义不同:在上限头里三个数是"允许次数 / 窗口 /
/// 超限后罚多久";在 State 头里前两个数是"已用次数 / 同一个窗口",第三个数
/// 是**当前还剩多少秒冷却**(平时是 0)。这是服务端定的,别在这里统一。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub requests: u32,
    pub window_secs: u32,
    pub timeout_secs: u32,
}

/// 一个响应里解析出来的全部限速信息。
///
/// `limits` 和 `state` 都按规则名(`Ip` / `Account`)分组,组内的 `Vec<Bucket>`
/// 保持头里的原始顺序,因为上限和用量是靠下标对齐的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateHeaders {
    pub policy: String,
    pub rules: Vec<String>,
    pub limits: BTreeMap<String, Vec<Bucket>>,
    pub state: BTreeMap<String, Vec<Bucket>>,
    pub retry_after_secs: Option<u32>,
}

impl RateHeaders {
    /// 规则里有没有 `Account`(不分大小写)。
    ///
    /// 这是判断 POESESSID 还活着的唯一可靠信号:带上了 cookie 却只回 `Ip`
    /// 规则,就说明服务端根本没认出这个会话。PoB 也是这么判的。
    pub fn mentions_account(&self) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.eq_ignore_ascii_case("account"))
    }
}

/// 解析响应头。头名不分大小写;没有 `X-Rate-Limit-Policy` 就返回 `None`
/// ——那说明这个响应根本不是交易站限速接口回的,没什么可学的。
pub fn parse_rate_headers<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<RateHeaders> {
    let mut by_name: BTreeMap<String, &str> = BTreeMap::new();
    for (name, value) in headers {
        by_name.insert(name.trim().to_ascii_lowercase(), value.trim());
    }

    let policy = (*by_name.get("x-rate-limit-policy")?).to_string();

    let rules: Vec<String> = by_name
        .get("x-rate-limit-rules")
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|rule| !rule.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut limits = BTreeMap::new();
    let mut state = BTreeMap::new();
    for rule in &rules {
        let key = rule.to_ascii_lowercase();
        for (header, target) in [
            (format!("x-rate-limit-{key}"), &mut limits),
            (format!("x-rate-limit-{key}-state"), &mut state),
        ] {
            if let Some(value) = by_name.get(header.as_str()) {
                let buckets = parse_buckets(value);
                if !buckets.is_empty() {
                    target.insert(rule.clone(), buckets);
                }
            }
        }
    }

    let retry_after_secs = by_name
        .get("retry-after")
        .and_then(|value| value.parse().ok());

    Some(RateHeaders {
        policy,
        rules,
        limits,
        state,
        retry_after_secs,
    })
}

/// 逗号分隔的桶列表。看不懂的桶直接跳过而不是整条报错:头是服务端随时可能
/// 加字段的东西,少认一个桶只是保守一点,认错了才危险。
fn parse_buckets(value: &str) -> Vec<Bucket> {
    value
        .split(',')
        .filter_map(|chunk| {
            let mut parts = chunk.trim().split(':');
            let requests = parts.next()?.trim().parse().ok()?;
            let window_secs = parts.next()?.trim().parse().ok()?;
            let timeout_secs = parts.next()?.trim().parse().ok()?;
            Some(Bucket {
                requests,
                window_secs,
                timeout_secs,
            })
        })
        .collect()
}

/// 我们给自己留的预算:服务端上限的 `percent` %,再减 `margin` 次。
///
/// 百分比是"温柔"那一半——同一个 IP 上用户的浏览器也在发请求;`margin` 是
/// 抄 PoB 的余量,防止在窗口边界上和外部请求撞车。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub percent: u32,
    pub margin: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            percent: 50,
            margin: 1,
        }
    }
}

impl Budget {
    /// 服务端说这个桶能发 `requests` 次,我们自己只用其中多少次。
    ///
    /// 至少留 1 次:预算再小也得能发出请求,否则整条策略永远卡死。
    #[must_use]
    pub fn effective_limit(&self, requests: u32) -> u32 {
        (requests.saturating_mul(self.percent) / 100)
            .saturating_sub(self.margin)
            .max(1)
    }
}

/// 一次在途请求的凭据。故意不给 `Clone`:它按值交给
/// [`RateLimiter::finish_request`],所以一次请求不可能被结算两遍。
#[derive(Debug)]
pub struct RequestTicket {
    policy: String,
    id: u64,
}

impl RequestTicket {
    /// 自增编号,排队日志里用来对上"这条响应是哪次请求的"。
    pub fn id(&self) -> u64 {
        self.id
    }
}

/// 单条策略的全部状态。
#[derive(Debug, Clone, Default)]
struct PolicyTracker {
    /// 服务端给的上限,按规则名分组。空 = 还没学到,这条策略算"未知"。
    limits: BTreeMap<String, Vec<Bucket>>,
    /// 服务端给的用量快照,和 `limits` 同下标对齐。
    server_state: BTreeMap<String, Vec<Bucket>>,
    /// 上面这份快照是什么时候的。快照会随时间失效,得知道它多老。
    state_at: i64,
    /// 我们自己发过的请求时刻,旧的在前。
    history: VecDeque<i64>,
    retry_after_until: Option<i64>,
    in_flight: u32,
}

impl PolicyTracker {
    fn max_window(&self) -> u32 {
        self.limits
            .values()
            .chain(self.server_state.values())
            .flatten()
            .map(|bucket| bucket.window_secs)
            .max()
            .unwrap_or(0)
    }
}

/// 给界面看的一个桶的用量,例如 "6h: 41/299"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketUsage {
    pub rule: String,
    pub window_secs: u32,
    /// 本地历史和服务端快照取大者。
    pub used: u32,
    /// 我们给自己定的上限([`Budget::effective_limit`])。
    pub allowed: u32,
    /// 服务端的真实上限,用来告诉用户我们只用了它的一半。
    pub server_limit: u32,
}

/// 一个桶的评估结果:给界面的部分 + 算下次可请求时刻要用的两个内部字段。
struct BucketEval {
    usage: BucketUsage,
    /// 窗口内最早那次请求的时刻,`None` 表示窗口里没有我们自己的请求。
    oldest_in_window: Option<i64>,
    /// State 头报的剩余冷却秒。
    server_timeout_secs: u32,
}

/// 按策略跟踪限速的纯状态机。
#[derive(Debug, Clone)]
pub struct RateLimiter {
    budget: Budget,
    policies: BTreeMap<String, PolicyTracker>,
    /// 票据编号,只为了日志能对上号。
    next_ticket_id: u64,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(Budget::default())
    }
}

impl RateLimiter {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            policies: BTreeMap::new(),
            next_ticket_id: 0,
        }
    }

    /// 记下"我现在要发一条请求"。必须在真正发出去之前调用:在途请求也要占
    /// 预算,否则并发的两条请求会各自以为自己是第一条。
    pub fn insert_request(&mut self, policy: &str, now: i64) -> RequestTicket {
        let id = self.next_ticket_id;
        self.next_ticket_id += 1;
        let tracker = self.policies.entry(policy.to_string()).or_default();
        tracker.history.push_back(now);
        tracker.in_flight += 1;
        RequestTicket {
            policy: policy.to_string(),
            id,
        }
    }

    /// 请求回来了。`headers` 是 `None` 表示这次连头都没拿到(网络错误),
    /// 那就只销掉在途计数,已知的限速信息原样保留。
    ///
    /// 服务端快照的采纳规则(照搬 PoB `UpdateFromHeader`):**只有在没有别的
    /// 请求在途时才整份采纳**。还有请求在飞的时候,这份快照可能是在它们出发
    /// 之前生成的,直接采纳会把用量算少;所以逐桶取大者,宁可保守。
    pub fn finish_request(
        &mut self,
        ticket: RequestTicket,
        headers: Option<&RateHeaders>,
        now: i64,
    ) {
        let tracker = self.policies.entry(ticket.policy.clone()).or_default();
        tracker.in_flight = tracker.in_flight.saturating_sub(1);

        if let Some(headers) = headers {
            let tracker = self.policies.entry(headers.policy.clone()).or_default();
            if let Some(secs) = headers.retry_after_secs {
                tracker.retry_after_until = Some(now + i64::from(secs));
            }
            tracker.limits = headers.limits.clone();
            if tracker.in_flight == 0 || tracker.server_state.is_empty() {
                tracker.server_state = headers.state.clone();
            } else {
                merge_state(&mut tracker.server_state, &headers.state);
            }
            tracker.state_at = now;
        }

        // 修剪历史。正常情况下头里的策略名就是票据上的那个,两次调用里的
        // 第二次不会发生;真不一样时两边都得修剪。
        self.age_out(&ticket.policy, now);
        if let Some(other) = headers
            .map(|headers| headers.policy.as_str())
            .filter(|policy| *policy != ticket.policy)
        {
            self.age_out(other, now);
        }
    }

    /// 这条策略下一次可以发请求的时刻(unix 秒)。`<= now` 就是现在可以发。
    ///
    /// 优先级:未知策略 → Retry-After → 各个桶。所有"等到某时刻"都多加 1 秒
    /// ——服务端和我们都只精确到秒,踩在整秒边界上开跑就是在赌。
    pub fn next_request_time(&mut self, policy: &str, now: i64) -> i64 {
        self.age_out(policy, now);
        let Some(tracker) = self.policies.get(policy) else {
            // 从没发过这条策略的请求:放行。第一条请求存在的意义就是把限速头
            // 学回来。
            return now;
        };
        if tracker.limits.is_empty() {
            // 还没学到限速头。有请求在途就死等它回来(PoB 用一个远未来的
            // 常数干同样的事),否则说明上一条请求失败了,允许再试一次。
            return if tracker.in_flight > 0 {
                i64::MAX / 2
            } else {
                now
            };
        }
        if let Some(until) = tracker.retry_after_until
            && until > now
        {
            // 服务端明说了等多久,别再自作聪明。
            return until;
        }

        let mut next = now;
        for eval in evaluate(&self.budget, tracker, now) {
            if eval.server_timeout_secs > 0 {
                // State 里的第三个数是"还要冷却多少秒",从快照那一刻算起。
                next = next.max(tracker.state_at + i64::from(eval.server_timeout_secs) + 1);
            }
            if eval.usage.used >= eval.usage.allowed {
                // 到顶了,等窗口里最早那次请求滚出窗口。窗口里没有我们自己的
                // 请求(用量全是别处来的),就从快照时刻起等满一个窗口。
                let anchor = eval.oldest_in_window.unwrap_or(tracker.state_at);
                next = next.max(anchor + i64::from(eval.usage.window_secs) + 1);
            }
        }
        next
    }

    /// 给界面的预算用量。未知策略返回空表。
    pub fn budget_view(&mut self, policy: &str, now: i64) -> Vec<BucketUsage> {
        self.age_out(policy, now);
        let Some(tracker) = self.policies.get(policy) else {
            return Vec::new();
        };
        evaluate(&self.budget, tracker, now)
            .into_iter()
            .map(|eval| eval.usage)
            .collect()
    }

    /// 丢掉比最大窗口还老的历史。留着也不影响计数(每次都按窗口过滤),只是
    /// 会无限长下去。
    fn age_out(&mut self, policy: &str, now: i64) {
        let Some(tracker) = self.policies.get_mut(policy) else {
            return;
        };
        let max_window = tracker.max_window();
        if max_window == 0 {
            return;
        }
        let cutoff = now - i64::from(max_window);
        while tracker.history.front().is_some_and(|ts| *ts <= cutoff) {
            tracker.history.pop_front();
        }
    }
}

/// 逐桶把新快照并进旧快照:次数取大者,窗口和剩余冷却用新的。
///
/// 冷却用新值而不是取大者:冷却是"还剩多少",它就该随时间变小;而 `state_at`
/// 同时被推到了 now,新值配新锚点才对得上。
fn merge_state(
    existing: &mut BTreeMap<String, Vec<Bucket>>,
    fresh: &BTreeMap<String, Vec<Bucket>>,
) {
    for (rule, fresh_buckets) in fresh {
        match existing.get_mut(rule) {
            Some(old_buckets) if old_buckets.len() == fresh_buckets.len() => {
                for (old, new) in old_buckets.iter_mut().zip(fresh_buckets) {
                    *old = if old.window_secs == new.window_secs {
                        Bucket {
                            requests: old.requests.max(new.requests),
                            ..*new
                        }
                    } else {
                        // 桶的形状变了,旧值没有可比性,整个换掉。
                        *new
                    };
                }
            }
            _ => {
                existing.insert(rule.clone(), fresh_buckets.clone());
            }
        }
    }
}

/// 把每个桶的"已用多少 / 允许多少"算出来。
///
/// 两个来源取大者:本地历史知道我们自己发了什么,服务端快照还包含用户浏览器
/// 等外部请求。快照会变旧,所以要把"从快照到现在这段时间里已经滚出窗口的
/// 那些自己的请求"减掉,否则同一次请求会被算两次、越算越满。
fn evaluate(budget: &Budget, tracker: &PolicyTracker, now: i64) -> Vec<BucketEval> {
    let mut out = Vec::new();
    for (rule, limits) in &tracker.limits {
        let state = tracker.server_state.get(rule);
        for (index, limit) in limits.iter().enumerate() {
            let window = i64::from(limit.window_secs);
            let cutoff = now - window;
            let state_cutoff = tracker.state_at - window;

            let mut local_used = 0u32;
            let mut oldest_in_window: Option<i64> = None;
            let mut aged_out_since_state = 0u32;
            for &ts in &tracker.history {
                if ts > cutoff {
                    local_used += 1;
                    oldest_in_window = Some(oldest_in_window.map_or(ts, |old: i64| old.min(ts)));
                } else if ts > state_cutoff {
                    // 拍快照那会儿还在窗口里,现在已经滚出去了。
                    aged_out_since_state += 1;
                }
            }

            let server = state.and_then(|buckets| buckets.get(index)).copied();
            let server_used = server.map_or(0, |bucket| {
                bucket.requests.saturating_sub(aged_out_since_state)
            });

            out.push(BucketEval {
                usage: BucketUsage {
                    rule: rule.clone(),
                    window_secs: limit.window_secs,
                    used: local_used.max(server_used),
                    allowed: budget.effective_limit(limit.requests),
                    server_limit: limit.requests,
                },
                oldest_in_window,
                server_timeout_secs: server.map_or(0, |bucket| bucket.timeout_secs),
            });
        }
    }
    out
}

/// 收到 429 之后等多久再重试:服务端说的 `Retry-After` 和指数退避取大者,
/// 退避封顶 60 秒(照 PoB 的 `ProcessQueue`)。
pub fn backoff_after_429(attempts: u32, retry_after: Option<u32>) -> u32 {
    let exponential = 2u32.checked_pow(attempts).unwrap_or(u32::MAX).min(60);
    retry_after.unwrap_or(0).max(exponential)
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;

    // 2026-09-06 从 pathofexile.com/api/trade2 实测到的两组头,原样照抄。
    const SEARCH_LIMITS: &str = "5:10:60,15:60:300,30:300:1800,600:21600:3600";
    const SEARCH_STATE: &str = "1:10:0,1:60:0,1:300:0,191:21600:0";
    const FETCH_LIMITS: &str = "12:4:10,16:12:300,50:300:300,1000:21600:1800";
    const FETCH_STATE: &str = "1:4:0,1:12:0,1:300:0,215:21600:0";

    fn search_headers() -> RateHeaders {
        search_headers_with(SEARCH_STATE, None)
    }

    fn search_headers_with(state: &str, retry_after: Option<&str>) -> RateHeaders {
        let mut raw = vec![
            ("X-Rate-Limit-Policy", SEARCH_POLICY),
            ("X-Rate-Limit-Rules", "Ip"),
            ("X-Rate-Limit-Ip", SEARCH_LIMITS),
            ("X-Rate-Limit-Ip-State", state),
        ];
        if let Some(secs) = retry_after {
            raw.push(("Retry-After", secs));
        }
        parse_rate_headers(raw).expect("policy header present")
    }

    fn fetch_headers() -> RateHeaders {
        parse_rate_headers([
            ("X-Rate-Limit-Policy", FETCH_POLICY),
            ("X-Rate-Limit-Rules", "Ip"),
            ("X-Rate-Limit-Ip", FETCH_LIMITS),
            ("X-Rate-Limit-Ip-State", FETCH_STATE),
        ])
        .expect("policy header present")
    }

    fn bucket(requests: u32, window_secs: u32, timeout_secs: u32) -> Bucket {
        Bucket {
            requests,
            window_secs,
            timeout_secs,
        }
    }

    #[test]
    fn parses_measured_search_headers() {
        let headers = search_headers();
        assert_eq!(headers.policy, "trade-search-request-limit");
        assert_eq!(headers.rules, vec!["Ip".to_string()]);
        assert_eq!(headers.retry_after_secs, None);

        let limits = &headers.limits["Ip"];
        assert_eq!(limits.len(), 4);
        assert_eq!(limits[0], bucket(5, 10, 60));
        assert_eq!(limits[1], bucket(15, 60, 300));
        assert_eq!(limits[2], bucket(30, 300, 1800));
        assert_eq!(limits[3], bucket(600, 21600, 3600));

        let state = &headers.state["Ip"];
        assert_eq!(state.len(), 4);
        assert_eq!(state[0], bucket(1, 10, 0));
        assert_eq!(state[3], bucket(191, 21600, 0));

        assert!(!headers.mentions_account());

        // 头名不分大小写:同样的内容换个写法必须解析成同一份东西。
        let lowercased = parse_rate_headers([
            ("x-rate-limit-policy", SEARCH_POLICY),
            ("x-rate-limit-rules", "Ip"),
            ("x-rate-limit-ip", SEARCH_LIMITS),
            ("x-rate-limit-ip-state", SEARCH_STATE),
        ])
        .expect("policy header present");
        assert_eq!(lowercased, headers);
    }

    #[test]
    fn no_policy_header_means_no_rate_limit_info() {
        assert!(parse_rate_headers([("Content-Type", "application/json")]).is_none());
    }

    #[test]
    fn rules_with_account_are_detected() {
        assert!(!search_headers().mentions_account());

        let with_session = parse_rate_headers([
            ("X-Rate-Limit-Policy", SEARCH_POLICY),
            ("X-Rate-Limit-Rules", "Ip,Account"),
            ("X-Rate-Limit-Ip", SEARCH_LIMITS),
            ("X-Rate-Limit-Ip-State", SEARCH_STATE),
            ("X-Rate-Limit-Account", "3:5:60"),
            ("X-Rate-Limit-Account-State", "1:5:0"),
        ])
        .expect("policy header present");
        assert_eq!(
            with_session.rules,
            vec!["Ip".to_string(), "Account".to_string()]
        );
        assert!(with_session.mentions_account());
        assert_eq!(with_session.limits["Account"], vec![bucket(3, 5, 60)]);
    }

    #[test]
    fn effective_limits_match_the_plan() {
        let budget = Budget::default();
        assert_eq!(budget.percent, 50);
        assert_eq!(budget.margin, 1);

        let search: Vec<u32> = search_headers().limits["Ip"]
            .iter()
            .map(|b| budget.effective_limit(b.requests))
            .collect();
        assert_eq!(search, vec![1, 6, 14, 299]);

        let fetch: Vec<u32> = fetch_headers().limits["Ip"]
            .iter()
            .map(|b| budget.effective_limit(b.requests))
            .collect();
        assert_eq!(fetch, vec![5, 7, 24, 499]);
    }

    /// 轮询节奏的地板照这两个常数算,所以它们必须还是实测的那一对。
    #[test]
    fn the_long_search_window_matches_the_measured_header() {
        let longest = search_headers().limits["Ip"].last().copied().expect("桶");
        assert_eq!(longest.requests, SEARCH_LONG_WINDOW_REQUESTS);
        assert_eq!(longest.window_secs, SEARCH_LONG_WINDOW_SECS);
        assert_eq!(
            Budget::default().effective_limit(SEARCH_LONG_WINDOW_REQUESTS),
            299
        );
    }

    #[test]
    fn first_request_is_not_blocked() {
        let mut limiter = RateLimiter::default();
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, 1_000), 1_000);
        assert!(limiter.budget_view(SEARCH_POLICY, 1_000).is_empty());
    }

    #[test]
    fn unknown_policy_with_request_in_flight_blocks() {
        let mut limiter = RateLimiter::default();
        let ticket = limiter.insert_request(SEARCH_POLICY, 1_000);
        assert_eq!(ticket.id(), 0);
        // 限速头还没回来,不知道能发几次,只能等。
        assert_eq!(
            limiter.next_request_time(SEARCH_POLICY, 1_000),
            i64::MAX / 2
        );

        limiter.finish_request(ticket, Some(&search_headers()), 1_000);
        assert!(limiter.next_request_time(SEARCH_POLICY, 1_000) < i64::MAX / 2);
    }

    #[test]
    fn half_budget_of_search_policy_allows_one_per_ten_seconds() {
        let t0 = 1_000_000;
        let mut limiter = RateLimiter::default();
        let ticket = limiter.insert_request(SEARCH_POLICY, t0);
        limiter.finish_request(ticket, Some(&search_headers()), t0);

        // 10 秒桶的一半预算是 1 次,刚用掉,得等它滚出窗口。
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 1), t0 + 11);
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 5), t0 + 11);
        // 窗口是半开区间:满 10 秒那一刻它已经出去了,晚点再问就不用等了。
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 10), t0 + 10);
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 11), t0 + 11);

        let view = limiter.budget_view(SEARCH_POLICY, t0 + 1);
        assert_eq!(view[0].window_secs, 10);
        assert_eq!(
            (view[0].used, view[0].allowed, view[0].server_limit),
            (1, 1, 5)
        );
        assert_eq!(view[3].window_secs, 21600);
        assert_eq!(
            (view[3].used, view[3].allowed, view[3].server_limit),
            (191, 299, 600)
        );
    }

    #[test]
    fn six_hour_bucket_caps_at_299() {
        let t0 = 2_000_000;
        let mut limiter = RateLimiter::default();

        // 第一条请求学到限速头,后面 298 条每 60 秒一条,小窗口都不会满。
        let ticket = limiter.insert_request(SEARCH_POLICY, t0);
        limiter.finish_request(
            ticket,
            Some(&search_headers_with(
                "1:10:0,1:60:0,1:300:0,1:21600:0",
                None,
            )),
            t0,
        );
        for i in 1..299 {
            let ts = t0 + i * 60;
            let ticket = limiter.insert_request(SEARCH_POLICY, ts);
            limiter.finish_request(ticket, None, ts);
        }

        let now = t0 + 298 * 60 + 11;
        let view = limiter.budget_view(SEARCH_POLICY, now);
        assert_eq!((view[3].used, view[3].allowed), (299, 299));
        // 六小时桶到顶:只能等最早那次请求滚出 6 小时窗口。
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, now), t0 + 21_601);
    }

    #[test]
    fn retry_after_wins_over_everything() {
        let t0 = 3_000_000;
        let mut limiter = RateLimiter::default();
        let ticket = limiter.insert_request(SEARCH_POLICY, t0);
        limiter.finish_request(
            ticket,
            Some(&search_headers_with(
                "5:10:37,1:60:0,1:300:0,1:21600:0",
                Some("45"),
            )),
            t0,
        );

        // 桶自己算出来是 t0+38(冷却)和 t0+11(到顶),都被 Retry-After 压过。
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 1), t0 + 45);
        // 过了就不再拦。
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 46), t0 + 46);
    }

    #[test]
    fn server_state_higher_than_local_history_is_believed() {
        let t0 = 4_000_000;
        let mut limiter = RateLimiter::default();
        let ticket = limiter.insert_request(SEARCH_POLICY, t0);
        limiter.finish_request(
            ticket,
            Some(&search_headers_with(
                "5:10:0,1:60:0,1:300:0,1:21600:0",
                None,
            )),
            t0,
        );

        // t0+10 时我们自己那一次已经出了 10 秒窗口,本地历史是空的;服务端却说
        // 这个窗口用掉了 5 次(用户的浏览器)。信服务端,从快照时刻等满一个窗口。
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 10), t0 + 11);

        // 换成 60 秒桶:本地只有 1 次(远没到 6 次),服务端说 9 次,照样得等。
        let mut limiter = RateLimiter::default();
        let ticket = limiter.insert_request(SEARCH_POLICY, t0);
        limiter.finish_request(
            ticket,
            Some(&search_headers_with(
                "1:10:0,9:60:0,1:300:0,1:21600:0",
                None,
            )),
            t0,
        );
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 20), t0 + 61);
    }

    #[test]
    fn timeout_in_state_blocks_until_it_lapses() {
        let t0 = 5_000_000;
        let mut limiter = RateLimiter::default();
        let ticket = limiter.insert_request(SEARCH_POLICY, t0);
        limiter.finish_request(
            ticket,
            Some(&search_headers_with(
                "5:10:37,1:60:0,1:300:0,1:21600:0",
                None,
            )),
            t0,
        );

        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 1), t0 + 38);
        assert_eq!(limiter.next_request_time(SEARCH_POLICY, t0 + 38), t0 + 38);
    }

    #[test]
    fn in_flight_merge_keeps_larger_count() {
        let t0 = 6_000_000;
        let mut limiter = RateLimiter::default();
        let first = limiter.insert_request(SEARCH_POLICY, t0);
        let second = limiter.insert_request(SEARCH_POLICY, t0);
        let third = limiter.insert_request(SEARCH_POLICY, t0);

        // 还有两条在途:整份采纳(之前什么都不知道)。
        limiter.finish_request(
            first,
            Some(&search_headers_with(
                "1:10:0,9:60:0,1:300:0,1:21600:0",
                None,
            )),
            t0,
        );
        assert_eq!(used_for_window(&mut limiter, t0, 60), 9);

        // 还有一条在途:这份快照可能是在它出发前拍的,取大者,不能信小的。
        limiter.finish_request(
            second,
            Some(&search_headers_with(
                "1:10:0,4:60:0,1:300:0,1:21600:0",
                None,
            )),
            t0,
        );
        assert_eq!(used_for_window(&mut limiter, t0, 60), 9);

        // 最后一条回来了,没有在途请求,这份快照是可信的同步点:整份采纳。
        limiter.finish_request(
            third,
            Some(&search_headers_with(
                "1:10:0,4:60:0,1:300:0,1:21600:0",
                None,
            )),
            t0,
        );
        assert_eq!(used_for_window(&mut limiter, t0, 60), 4);
    }

    fn used_for_window(limiter: &mut RateLimiter, now: i64, window_secs: u32) -> u32 {
        limiter
            .budget_view(SEARCH_POLICY, now)
            .into_iter()
            .find(|usage| usage.window_secs == window_secs)
            .expect("window present")
            .used
    }

    #[test]
    fn backoff_after_429_grows_and_caps() {
        assert_eq!(backoff_after_429(1, None), 2);
        assert_eq!(backoff_after_429(2, None), 4);
        assert_eq!(backoff_after_429(3, None), 8);
        assert_eq!(backoff_after_429(10, None), 60);

        assert_eq!(backoff_after_429(1, Some(30)), 30);
        assert_eq!(backoff_after_429(3, Some(2)), 8);
        assert_eq!(backoff_after_429(10, Some(30)), 60);
        assert_eq!(backoff_after_429(3, Some(120)), 120);
    }
}
