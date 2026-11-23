//! B 站官方直播间信息（对照 Electron `src/main/bilibili/room-info.ts`）。
//!
//! 公演条目缺团体信息时，渲染层以在播房间标题做公演名兜底匹配。
//! `get_info` 匿名可访问；结果缓存 5 分钟，单房间失败以空标题占位。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

/// 公演团体 → B 站官方直播间号（与 `common/bilibili-rooms.ts` 同源）。
pub const GROUP_ROOMS: &[(&str, u64)] = &[
    ("SNH48", 63727),
    ("BEJ48", 383045),
    ("GNZ48", 391199),
    ("CKG48", 6015846),
    ("CGT48", 27848865),
];

const ROOM_INFO_URL: &str = "https://api.live.bilibili.com/room/v1/Room/get_info";
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// B 站请求的浏览器标识，与弹幕握手同一个接口，标识必须一致（见 Electron `browser-headers.ts`）。
const BILIBILI_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const BILIBILI_REFERER: &str = "https://live.bilibili.com/";

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BilibiliRoomInfo {
    pub room_id: u64,
    /// 直播间标题；开播时官方会改为当前公演名
    pub title: String,
    /// 0=未开播 1=开播 2=轮播
    pub live_status: i64,
}

#[derive(Default)]
pub struct BilibiliRoomCache {
    cached: Mutex<Option<(Instant, Vec<BilibiliRoomInfo>)>>,
}

impl BilibiliRoomCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 拉取全部团体的房间信息（带缓存）；失败的房间以空标题占位。
    pub async fn room_infos(&self, http: &reqwest::Client) -> Vec<BilibiliRoomInfo> {
        if let Some((at, infos)) = self.read_cache()
            && at.elapsed() < CACHE_TTL
        {
            return infos;
        }
        let mut infos = Vec::with_capacity(GROUP_ROOMS.len());
        for (label, room_id) in GROUP_ROOMS {
            match fetch_room_info(http, *room_id).await {
                Ok(info) => infos.push(info),
                Err(err) => {
                    eprintln!("[bilibili] 房间信息获取失败 room={room_id}({label}): {err}");
                    infos.push(BilibiliRoomInfo {
                        room_id: *room_id,
                        title: String::new(),
                        live_status: 0,
                    });
                }
            }
        }
        if let Ok(mut guard) = self.cached.lock() {
            *guard = Some((Instant::now(), infos.clone()));
        }
        infos
    }

    fn read_cache(&self) -> Option<(Instant, Vec<BilibiliRoomInfo>)> {
        self.cached
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|(at, infos)| (*at, infos.clone())))
    }

    #[cfg(test)]
    pub fn seed(&self, infos: Vec<BilibiliRoomInfo>) {
        if let Ok(mut guard) = self.cached.lock() {
            *guard = Some((Instant::now(), infos));
        }
    }
}

/// 拉取单房间信息；接口风控 / 超时时报错（由调用方降级为空标题）。
async fn fetch_room_info(http: &reqwest::Client, room_id: u64) -> Result<BilibiliRoomInfo, String> {
    let response = http
        .get(ROOM_INFO_URL)
        .query(&[("room_id", room_id.to_string())])
        .timeout(REQUEST_TIMEOUT)
        .header(reqwest::header::USER_AGENT, BILIBILI_USER_AGENT)
        .header(reqwest::header::REFERER, BILIBILI_REFERER)
        .send()
        .await
        .map_err(|err| format!("请求失败: {err}"))?;
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|err| format!("响应解析失败: {err}"))?;
    let code = body.get("code").and_then(serde_json::Value::as_i64);
    let data = body.get("data").filter(|data| !data.is_null());
    if code != Some(0) || data.is_none() {
        return Err(format!(
            "code={}",
            code.map(|code| code.to_string())
                .unwrap_or_else(|| body["code"].to_string())
        ));
    }
    let data = data.unwrap_or(&serde_json::Value::Null);
    Ok(BilibiliRoomInfo {
        room_id,
        title: data
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        live_status: data
            .get("live_status")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_rooms_match_electron() {
        assert_eq!(GROUP_ROOMS.len(), 5);
        assert!(
            GROUP_ROOMS
                .iter()
                .any(|(label, room)| *label == "SNH48" && *room == 63727)
        );
    }

    #[tokio::test]
    async fn cache_short_circuits_network() {
        let cache = BilibiliRoomCache::new();
        cache.seed(vec![BilibiliRoomInfo {
            room_id: 63727,
            title: "缓存命中".to_string(),
            live_status: 1,
        }]);
        // 没有任何出口客户端参与：命中缓存就不会发请求。
        let client = reqwest::Client::new();
        let infos = cache.room_infos(&client).await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].title, "缓存命中");
    }
}
