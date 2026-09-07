//! 纯领域类型:搜索引用(URL/id 解析)、价格与货币、挂单摘要。
//! 不依赖任何 I/O,是其余 crate 共同建立的最底层。

pub mod listing;
pub mod price;
pub mod search_ref;

pub use listing::{ListingSummary, WatchId};
pub use price::{Currency, CurrencyRates, Price, PriceCap, Verdict, judge, to_divine_milli};
pub use search_ref::{
    SearchIdError, SearchRef, decode_search_id, default_label_for, encode_league_path,
    live_page_url, parse_search_reference, search_page_url, search_request_body,
    with_seller_filter, with_sort,
};
