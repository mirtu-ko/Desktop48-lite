//! 口袋48 接口地址常量。
//!
//! 正式界面链路是「渲染层 request → 通用 `netRequest` 代理 → 渲染层解析」，
//! Rust 侧不再解析业务报文；这里只留诊断 / 探针要探测的端点地址。

/// 在播 / 回放列表（渲染层 `LIVE_LIST_URL` 同源，见 `common/pocket-api-urls.ts`）。
pub const LIVE_LIST_URL: &str = "https://pocketapi.48.cn/live/api/v1/live/getLiveList";
/// 直播 / 回放详情（诊断探测用）。
pub const LIVE_ONE_URL: &str = "https://pocketapi.48.cn/live/api/v1/live/getLiveOne";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_use_pocketapi_origin() {
        assert!(LIVE_LIST_URL.starts_with("https://pocketapi.48.cn"));
        assert!(LIVE_ONE_URL.starts_with("https://pocketapi.48.cn"));
    }
}
