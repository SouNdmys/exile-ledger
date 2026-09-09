//! poe.ninja 客户端:快照状态、build 搜索的最小 protobuf 解析、NDIC 字典、
//! 角色详情、经济汇率与暗金参考价、采样分区计划、词缀聚合。

pub mod aggregate;
pub mod character;
pub mod client;
pub mod economy;
pub mod index_state;
pub mod ndic;
pub mod plan;
pub mod search;
pub mod wire;

/// poe.ninja 的 API 文档明确要求带一个能认出调用方、并留有联系方式的
/// User-Agent。带上它,对面出问题时能直接找到人,而不是把我们当匿名爬虫掐掉。
pub const USER_AGENT: &str = concat!(
    "ExileLedger/",
    env!("CARGO_PKG_VERSION"),
    " (contact: https://github.com/SouNdmys/exile-ledger)"
);
