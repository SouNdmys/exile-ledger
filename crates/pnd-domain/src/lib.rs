//! 纯领域类型:搜索引用(URL/id 解析)、价格与货币、挂单摘要。
//! 不依赖任何 I/O,是其余 crate 共同建立的最底层。

pub mod listing;
pub mod observe;
pub mod price;
pub mod search_ref;

pub use listing::{ListingSummary, ObservationId, WatchId};
pub use observe::{
    CHECK_RUNGS, GoneClass, MIN_OBSERVED_LIFETIME_SECS, PRICE_BUCKET_UNITS, PriceBucket,
    STALE_AFTER_SECS, SUB_DIVINE_BUCKET_MILLI, classify_gone, divine_price_bucket, is_stale,
    next_check_after, observed_lifetime_secs, price_bucket,
};
pub use price::{
    Currency, CurrencyRates, Price, PriceCap, RateOverride, RateSource, RateSources, Verdict, judge,
};
pub use search_ref::{
    Game, SearchIdError, SearchRef, StatMatch, decode_search_id, default_label_for,
    encode_league_path, encode_search_id, live_page_url, parse_search_reference, search_page_url,
    search_request_body, unique_search_page_url, with_seller_filter, with_sort, with_stat_filter,
    with_stat_group,
};
