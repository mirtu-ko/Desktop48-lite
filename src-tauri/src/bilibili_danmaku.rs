//! B 站直播弹幕（对照 Electron `main/bilibili/danmaku-*.ts` + `wbi.ts` + `buvid.ts`）。
//!
//! 链路：房间号 → `get_info` 拿真实房间号 → `nav` 拿 WBI 口令 → `getDanmuInfo`（带 `w_rid` 签名）
//! 拿弹幕服务器与 token → WSS `/sub` 鉴权 → 收 `op=5` 消息 → 拆包（可能的 brotli/deflate 压缩）
//! → 取 `DANMU_MSG` 的正文与用户名 → 攒批下发。
//!
//! 必须放 Rust：渲染层直接建 WS 会绕过出口白名单，而且没有 brotli 解不了 `protover=3`。
//! 失败一律静默降级——弹幕拿不到不能影响播放，除首次握手外不向渲染层抛错。
//!
//! 本文件只做「一个房间一条连接 + 订阅者 fan-out」，与会话生命周期无关的部分
//! （谁订阅了哪个房间）在 `bridge.rs` 的通道里。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{Mutex, broadcast};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::app_log::log_line;

const ROOM_INFO_URL: &str = "https://api.live.bilibili.com/room/v1/Room/get_info";
const WBI_NAV_URL: &str = "https://api.bilibili.com/x/web-interface/nav";
const DANMU_INFO_URL: &str = "https://api.live.bilibili.com/xlive/web-room/v1/index/getDanmuInfo";
const BUVID_SPI_URL: &str = "https://api.bilibili.com/x/frontend/finger/spi";

/// 与 B 站浏览器标识同源（弹幕握手与 room-info 打同一批接口，标识必须一致）
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const REFERER: &str = "https://live.bilibili.com/";

const API_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// 攒批下发间隔：逐条 IPC 在热门房间会打爆渲染层
const BATCH_INTERVAL: Duration = Duration::from_millis(150);
/// 待发队列上限：渲染层异常慢时丢最旧的，弹幕过期即无意义
const MAX_PENDING: usize = 200;
/// 重连退避 1s → 2s … 上限 30s
const RECONNECT_BASE_MS: u64 = 1_000;
const RECONNECT_MAX_MS: u64 = 30_000;
/// 连续失败到这个次数就重新握手（token 与服务器列表可能已失效）
const REINIT_AFTER_ATTEMPTS: u32 = 5;

const PACKET_HEADER_SIZE: usize = 16;
const OP_HEARTBEAT: u32 = 2;
const OP_MESSAGE: u32 = 5;
const OP_AUTH: u32 = 7;
const HEARTBEAT_BODY: &str = "[object Object]";
/// 鉴权时请求的协议版本：3 = 正文为 brotli 压缩
const PROTOVER: u32 = 3;
const DEFLATE_PROTOVER: u16 = 2;
const BROTLI_PROTOVER: u16 = 3;

/// 单条弹幕（与 `common/live-danmaku.ts` 的 `LiveDanmaku` 同形）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LiveDanmaku {
    pub text: String,
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emots: Option<BTreeMap<String, DanmakuEmote>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DanmakuEmote {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

/// 批次（与 `IpcEventMap` 的 `danmakuBatch` 同形）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DanmakuBatch {
    pub room_id: u64,
    pub items: Vec<LiveDanmaku>,
}

// ===== 二进制包协议（纯函数，便于单测） =====

/// 组装一个发往服务端的包（正文恒为未压缩的 JSON 文本）。
pub fn pack_packet(body: &str, op: u32) -> Vec<u8> {
    let payload = body.as_bytes();
    let mut out = Vec::with_capacity(PACKET_HEADER_SIZE + payload.len());
    out.extend_from_slice(&((PACKET_HEADER_SIZE + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(&(PACKET_HEADER_SIZE as u16).to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&op.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// 鉴权包正文。`uid` 恒为 0（未登录）；`buvid` 必须非空，留空会被降级到稀疏通道。
pub fn build_auth_body(room_id: u64, token: &str, buvid: &str) -> String {
    serde_json::json!({
        "uid": 0,
        "roomid": room_id,
        "protover": PROTOVER,
        "buvid": buvid,
        "platform": "web",
        "type": 2,
        "key": token,
    })
    .to_string()
}

struct Packet {
    op: u32,
    body: Option<serde_json::Value>,
}

/// 拆出缓冲区里的全部包（含压缩包展开后的内层包）。
///
/// 一个 TCP 帧里可能拼着多个包，压缩包的正文里还嵌着一层包，故按包头长度循环 + 递归展开。
/// 长度字段不自洽（半包 / 脏数据）时立即停止：继续按错误偏移读会切出一串垃圾包。
pub fn unpack_packets(buffer: &[u8]) -> Vec<(u32, Option<serde_json::Value>)> {
    let mut out = Vec::new();
    append_packets(buffer, &mut out, 0);
    out.into_iter()
        .map(|packet| (packet.op, packet.body))
        .collect()
}

fn append_packets(buffer: &[u8], out: &mut Vec<Packet>, depth: usize) {
    // 递归深度上限：防止恶意/损坏数据构造出无限嵌套
    if depth > 4 {
        return;
    }
    let mut offset = 0usize;
    while offset + PACKET_HEADER_SIZE <= buffer.len() {
        let total = u32::from_be_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ]) as usize;
        let header_len = u16::from_be_bytes([buffer[offset + 4], buffer[offset + 5]]) as usize;
        let protover = u16::from_be_bytes([buffer[offset + 6], buffer[offset + 7]]);
        let op = u32::from_be_bytes([
            buffer[offset + 8],
            buffer[offset + 9],
            buffer[offset + 10],
            buffer[offset + 11],
        ]);
        if header_len < PACKET_HEADER_SIZE || total < header_len || offset + total > buffer.len() {
            return;
        }
        let body = &buffer[offset + header_len..offset + total];
        offset += total;

        if protover == DEFLATE_PROTOVER || protover == BROTLI_PROTOVER {
            if let Some(inflated) = inflate_body(body, protover) {
                append_packets(&inflated, out, depth + 1);
            }
            continue;
        }
        out.push(Packet {
            op,
            body: if op == OP_MESSAGE {
                serde_json::from_slice(body).ok()
            } else {
                None
            },
        });
    }
}

fn inflate_body(body: &[u8], protover: u16) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    if protover == BROTLI_PROTOVER {
        brotli::Decompressor::new(body, 4096)
            .read_to_end(&mut out)
            .ok()?;
    } else {
        flate2::read::ZlibDecoder::new(body)
            .read_to_end(&mut out)
            .ok()?;
    }
    Some(out)
}

/// 从消息正文里取出弹幕。非 `DANMU_MSG` 一律丢弃：
/// 进场 / 人气 / 榜单格式各异，部分还是 base64 protobuf，没有通用文本可显示。
pub fn extract_danmaku(payload: &serde_json::Value) -> Vec<LiveDanmaku> {
    let items: Vec<&serde_json::Value> = match payload {
        serde_json::Value::Array(list) => list.iter().collect(),
        other => vec![other],
    };
    items.into_iter().filter_map(extract_one).collect()
}

fn extract_one(item: &serde_json::Value) -> Option<LiveDanmaku> {
    let cmd = item.get("cmd")?.as_str()?;
    // cmd 常带后缀（如 `DANMU_MSG:4:0:2:2:2:0`），比对冒号前的主指令名
    if cmd.split(':').next()? != "DANMU_MSG" {
        return None;
    }
    let info = item.get("info")?.as_array()?;
    let text = info.get(1)?.as_str()?.to_string();
    if text.is_empty() {
        return None;
    }
    // info[2] = [uid, 用户名, ...]
    let username = info
        .get(2)
        .and_then(|sender| sender.as_array())
        .and_then(|sender| sender.get(1))
        .and_then(|name| name.as_str())
        .unwrap_or_default()
        .to_string();
    let emots = extract_emotes(info.first(), &text);
    Some(LiveDanmaku {
        text,
        username,
        emots,
    })
}

/// 取正文里各条消息的主指令名（诊断用）：弹幕数对不上网页时，靠它分清「没发」与「没认」。
pub fn list_commands(payload: &serde_json::Value) -> Vec<String> {
    let items: Vec<&serde_json::Value> = match payload {
        serde_json::Value::Array(list) => list.iter().collect(),
        other => vec![other],
    };
    items
        .into_iter()
        .filter_map(|item| {
            item.get("cmd")
                .and_then(|cmd| cmd.as_str())
                .filter(|cmd| !cmd.is_empty())
                .map(|cmd| cmd.split(':').next().unwrap_or(cmd).to_string())
        })
        .collect()
}

const EMOTE_FALLBACK_SIZE: u32 = 20;

/// 表情：普通表情在 `info[0][15].extra.emots`（键即占位符），
/// 粉丝装扮 / 大表情在 `info[0][13]`（单条，名字藏在 `emoticon_unique` 的方括号里）。
fn extract_emotes(
    meta: Option<&serde_json::Value>,
    content: &str,
) -> Option<BTreeMap<String, DanmakuEmote>> {
    let meta = meta?.as_array()?;
    let mut emots: BTreeMap<String, DanmakuEmote> = BTreeMap::new();

    if let Some(extra) = meta
        .get(15)
        .and_then(|slot| slot.get("extra"))
        .and_then(parse_json_object)
        && let Some(raw) = extra.get("emots").and_then(|value| value.as_object())
    {
        for (name, value) in raw {
            if let Some(emote) = to_emote(value)
                && !name.is_empty()
            {
                emots.insert(name.clone(), emote);
            }
        }
    }

    if let Some(bulge) = meta.get(13).and_then(parse_json_object)
        && let Some(emote) = to_emote(&serde_json::Value::Object(bulge.clone()))
    {
        let key = bulge
            .get("emoticon_unique")
            .and_then(|value| value.as_str())
            .and_then(|unique| {
                let start = unique.find('[')?;
                let end = unique[start..].find(']')? + start;
                Some(unique[start..=end].to_string())
            })
            .unwrap_or_else(|| content.to_string());
        if !key.is_empty() {
            emots.insert(key, emote);
        }
    }

    (!emots.is_empty()).then_some(emots)
}

/// 字段可能是对象，也可能是字符串化 JSON（B 站两种都下发过）。
fn parse_json_object(
    value: &serde_json::Value,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    match value {
        serde_json::Value::String(text) => {
            let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
            parsed.as_object().cloned()
        }
        serde_json::Value::Object(map) => Some(map.clone()),
        _ => None,
    }
}

fn to_emote(value: &serde_json::Value) -> Option<DanmakuEmote> {
    let url = value.get("url").and_then(|url| url.as_str())?;
    if url.is_empty() {
        return None;
    }
    // 协议下发的是 http，统一升级：渲染层在 file:// 下加载 http 图片会被混合内容拦
    let url = url.replacen("http://", "https://", 1);
    Some(DanmakuEmote {
        url,
        width: emote_size(value.get("width")),
        height: emote_size(value.get("height")),
    })
}

fn emote_size(value: Option<&serde_json::Value>) -> u32 {
    value
        .and_then(|value| value.as_u64())
        .filter(|size| *size > 0)
        .map(|size| size as u32)
        .unwrap_or(EMOTE_FALLBACK_SIZE)
}

// ===== WBI 签名 =====

/// wbi 口令重排表（B 站前端同一张表，非项目自有常量）
const MIXIN_KEY_TABLE: [usize; 32] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13,
];

fn filename_key(url: &str) -> String {
    url.split('/')
        .next_back()
        .unwrap_or_default()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// 由 nav 接口的 `img_url` / `sub_url` 推出 32 位签名口令。
pub fn build_wbi_key(img_url: &str, sub_url: &str) -> String {
    let shuffled = format!("{}{}", filename_key(img_url), filename_key(sub_url));
    MIXIN_KEY_TABLE
        .iter()
        .map(|index| shuffled.chars().nth(*index).unwrap_or_default())
        .collect()
}

/// 生成带 `wts` 与 `w_rid` 的查询串（不含 `?`）。
/// 签名串是「键升序 urlencode + 口令」的 md5；值里 `!'()*` 已剔除故与 Python quote_plus 等价。
pub fn build_wbi_query(params: &[(&str, String)], wbi_key: &str, wts: u64) -> String {
    let mut pairs: Vec<(String, String)> = params
        .iter()
        .map(|(key, value)| {
            let cleaned: String = value
                .chars()
                .filter(|ch| !matches!(ch, '!' | '\'' | '(' | ')' | '*'))
                .collect();
            (key.to_string(), cleaned)
        })
        .collect();
    // wts 最后加：时戳必须由调用方给定，同名业务参数不得把它顶掉
    pairs.push(("wts".to_string(), wts.to_string()));
    // 键升序（B 站按字典序拼接）
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let query = pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                urlencode(key),
                urlencode(value).replace("%20", "+")
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let digest = md5_hex(&format!("{query}{wbi_key}"));
    format!("{query}&w_rid={digest}")
}

fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 极简 MD5（WBI 需要，且不想为一个 32 位摘要拉整条 crypto 依赖链）。
fn md5_hex(text: &str) -> String {
    let digest = md5_bytes(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn md5_bytes(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let mut message = input.to_vec();
    let bit_len = (input.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    for chunk in message.chunks(64) {
        let mut m = [0u32; 16];
        for (index, slot) in m.iter_mut().enumerate() {
            *slot = u32::from_le_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..].copy_from_slice(&d0.to_le_bytes());
    out
}

// ===== 会话 =====

struct SessionState {
    /// 真实房间号（短号在这一步被换掉）
    real_room_id: u64,
    token: String,
    hosts: Vec<(String, u16)>,
}

/// 单房间弹幕会话：一条 WSS 连接 + 订阅者广播。
pub struct DanmakuSession {
    pub room_id: u64,
    batches: broadcast::Sender<DanmakuBatch>,
    closed: Arc<AtomicBool>,
    reconnect_attempt: AtomicU64,
}

impl DanmakuSession {
    /// 起会话：握手 + 建连 + 后台读帧。`Ok(None)` = 握手失败（房间号 / WBI 被拒）。
    pub async fn start(room_id: u64, http: reqwest::Client) -> Result<Arc<Self>, String> {
        let (batches, _) = broadcast::channel(64);
        let session = Arc::new(Self {
            room_id,
            batches,
            closed: Arc::new(AtomicBool::new(false)),
            reconnect_attempt: AtomicU64::new(0),
        });

        let state = handshake(&http, room_id).await?;
        log_line(&format!(
            "[danmaku] 房间 {room_id} 握手完成：realRoomId={} servers={}",
            state.real_room_id,
            state.hosts.len()
        ));

        let session_for_loop = session.clone();
        let buvid = resolve_buvid(&http).await;
        tokio::spawn(async move {
            session_for_loop.run_loop(http, state, buvid).await;
        });
        Ok(session)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DanmakuBatch> {
        self.batches.subscribe()
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    async fn run_loop(
        self: Arc<Self>,
        http: reqwest::Client,
        mut state: SessionState,
        buvid: String,
    ) {
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return;
            }
            let attempt = self.reconnect_attempt.load(Ordering::Relaxed) as u32;
            // 服务器列表按序轮换，重连一次换一台，避免持续撞同一台故障机
            let host = match state.hosts.get(attempt as usize % state.hosts.len().max(1)) {
                Some(host) => host.clone(),
                None => return,
            };
            let url = format!("wss://{}:{}/sub", host.0, host.1);
            match self.connect_once(&url, &state, &buvid).await {
                Ok(()) => {
                    // 正常关闭（本端主动 close）：直接退出
                    if self.closed.load(Ordering::Relaxed) {
                        return;
                    }
                }
                Err(err) => {
                    log_line(&format!("[danmaku] 房间 {} 连接失败: {err}", self.room_id));
                }
            }
            if self.closed.load(Ordering::Relaxed) {
                return;
            }
            self.reconnect_attempt.fetch_add(1, Ordering::Relaxed);
            let attempt = self.reconnect_attempt.load(Ordering::Relaxed) as u32;
            // 连续失败到阈值就重新握手（token / 服务器列表可能已失效）
            if attempt.is_multiple_of(REINIT_AFTER_ATTEMPTS) {
                match handshake(&http, self.room_id).await {
                    Ok(next) => state = next,
                    Err(err) => {
                        log_line(&format!(
                            "[danmaku] 房间 {} 重新握手失败: {err}",
                            self.room_id
                        ));
                    }
                }
            }
            let delay = (RECONNECT_BASE_MS * 2u64.pow(attempt.min(5))).min(RECONNECT_MAX_MS);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
    }

    async fn connect_once(
        &self,
        url: &str,
        state: &SessionState,
        buvid: &str,
    ) -> Result<(), String> {
        let mut request = url
            .into_client_request()
            .map_err(|err| format!("弹幕地址无效: {err}"))?;
        request
            .headers_mut()
            .insert("user-agent", USER_AGENT.parse().map_err(|_| "UA 无效")?);
        request.headers_mut().insert(
            "origin",
            "https://live.bilibili.com"
                .parse()
                .map_err(|_| "Origin 无效")?,
        );

        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|err| format!("连接弹幕服务器失败: {err}"))?;
        let (mut writer, mut reader) = socket.split();

        // 连上即清零：退避计数语义是「连续失败次数」
        self.reconnect_attempt.store(0, Ordering::Relaxed);
        writer
            .send(Message::Binary(
                pack_packet(
                    &build_auth_body(state.real_room_id, &state.token, buvid),
                    OP_AUTH,
                )
                .into(),
            ))
            .await
            .map_err(|err| format!("发送鉴权包失败: {err}"))?;

        // 心跳：30s 一次（服务端要求 30 秒内至少一次，否则主动断开）
        let closed = self.closed.clone();
        let heartbeat = tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                if closed.load(Ordering::Relaxed) {
                    return;
                }
                let packet = pack_packet(HEARTBEAT_BODY, OP_HEARTBEAT);
                if writer.send(Message::Binary(packet.into())).await.is_err() {
                    return;
                }
            }
        });

        // 读帧 + 攒批
        let mut pending: Vec<LiveDanmaku> = Vec::new();
        let mut flush_timer: Option<tokio::time::Instant> = None;
        loop {
            let timeout = match flush_timer {
                Some(deadline) => {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        flush_batch(&self.batches, self.room_id, &mut pending);
                        flush_timer = None;
                        Duration::from_secs(5)
                    } else {
                        deadline - now
                    }
                }
                None => Duration::from_secs(5),
            };
            match tokio::time::timeout(timeout, reader.next()).await {
                Ok(Some(Ok(Message::Binary(data)))) => {
                    let mut found = 0usize;
                    for (op, body) in unpack_packets(&data) {
                        if op != OP_MESSAGE {
                            continue;
                        }
                        let Some(body) = body else { continue };
                        let items = extract_danmaku(&body);
                        if items.is_empty() {
                            let ignored = list_commands(&body);
                            if !ignored.is_empty() {
                                log_line(&format!(
                                    "[danmaku] 房间 {} 丢弃非弹幕指令: {}",
                                    self.room_id,
                                    ignored.join(",")
                                ));
                            }
                            continue;
                        }
                        found += items.len();
                        pending.extend(items);
                        if pending.len() > MAX_PENDING {
                            let drop = pending.len() - MAX_PENDING;
                            pending.drain(..drop);
                        }
                    }
                    if found > 0 && flush_timer.is_none() {
                        flush_timer = Some(tokio::time::Instant::now() + BATCH_INTERVAL);
                    }
                }
                Ok(Some(Ok(Message::Ping(payload)))) => {
                    let _ = payload;
                }
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                    heartbeat.abort();
                    return Ok(());
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(err))) => {
                    heartbeat.abort();
                    return Err(format!("弹幕连接错误: {err}"));
                }
                Err(_) => {
                    if self.closed.load(Ordering::Relaxed) {
                        heartbeat.abort();
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn flush_batch(
    sender: &broadcast::Sender<DanmakuBatch>,
    room_id: u64,
    pending: &mut Vec<LiveDanmaku>,
) {
    if pending.is_empty() {
        return;
    }
    let items: Vec<LiveDanmaku> = std::mem::take(pending);
    let _ = sender.send(DanmakuBatch { room_id, items });
}

/// 握手：房间号 → 真实房间号 → WBI 口令 → 弹幕服务器与 token。
async fn handshake(http: &reqwest::Client, room_id: u64) -> Result<SessionState, String> {
    let room_info = fetch_json(http, &format!("{ROOM_INFO_URL}?room_id={room_id}")).await?;
    let real_room_id = room_info
        .get("data")
        .and_then(|data| data.get("room_id"))
        .and_then(|value| value.as_u64())
        .ok_or_else(|| {
            format!(
                "未取到 B 站房间号（room_id={room_id}, code={}）",
                room_info
                    .get("code")
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            )
        })?;

    let nav = fetch_json(http, WBI_NAV_URL).await?;
    let wbi = nav.get("data").and_then(|data| data.get("wbi_img"));
    let img_url = wbi
        .and_then(|img| img.get("img_url"))
        .and_then(|url| url.as_str())
        .ok_or_else(|| "未取到 WBI 签名口令".to_string())?;
    let sub_url = wbi
        .and_then(|img| img.get("sub_url"))
        .and_then(|url| url.as_str())
        .ok_or_else(|| "未取到 WBI 签名口令".to_string())?;

    let wts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let query = build_wbi_query(
        &[("id", real_room_id.to_string()), ("type", "0".to_string())],
        &build_wbi_key(img_url, sub_url),
        wts,
    );
    let danmu_info = fetch_json(http, &format!("{DANMU_INFO_URL}?{query}")).await?;
    let code = danmu_info.get("code").and_then(|code| code.as_i64());
    if code != Some(0) {
        return Err(format!(
            "getDanmuInfo 失败：code={} {}",
            code.map(|c| c.to_string()).unwrap_or_default(),
            danmu_info
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
        ));
    }
    let data = danmu_info.get("data");
    let token = data
        .and_then(|data| data.get("token"))
        .and_then(|token| token.as_str())
        .ok_or_else(|| "弹幕服务器列表为空".to_string())?
        .to_string();
    let hosts: Vec<(String, u16)> = data
        .and_then(|data| data.get("host_list"))
        .and_then(|list| list.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|item| {
                    let host = item.get("host")?.as_str()?.to_string();
                    let port = item.get("wss_port")?.as_u64()? as u16;
                    Some((host, port))
                })
                .collect()
        })
        .unwrap_or_default();
    if hosts.is_empty() {
        return Err("弹幕服务器列表为空".to_string());
    }
    Ok(SessionState {
        real_room_id,
        token,
        hosts,
    })
}

async fn fetch_json(http: &reqwest::Client, url: &str) -> Result<serde_json::Value, String> {
    let response = http
        .get(url)
        .timeout(API_TIMEOUT)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::REFERER, REFERER)
        .send()
        .await
        .map_err(|err| format!("B 站接口请求失败: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "B 站接口 HTTP {}: {url}",
            response.status().as_u16()
        ));
    }
    response
        .json()
        .await
        .map_err(|err| format!("B 站接口回包不是 JSON: {err}"))
}

/// 设备标识：留空会被服务端降级到稀疏通道（实测少一半以上），宁可用假值也不能空着。
async fn resolve_buvid(http: &reqwest::Client) -> String {
    if let Ok(value) = fetch_json(http, BUVID_SPI_URL).await
        && let Some(buvid) = value
            .get("data")
            .and_then(|data| data.get("b_3"))
            .and_then(|b| b.as_str())
            .filter(|b| !b.is_empty())
    {
        return buvid.to_string();
    }
    // 指纹接口不可用时的兜底：格式对齐 buvid3（大写 UUID）
    let random: [u8; 16] = rand::random();
    random.iter().map(|byte| format!("{byte:02X}")).collect()
}

/// 供桥层持有：`roomId → 会话`。
#[derive(Default)]
pub struct DanmakuSessions {
    sessions: Mutex<std::collections::HashMap<u64, Arc<DanmakuSession>>>,
}

impl DanmakuSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// 起会话（已存在则复用）；返回是否新建。
    pub async fn start(&self, room_id: u64, http: reqwest::Client) -> Result<bool, String> {
        {
            let guard = self.sessions.lock().await;
            if guard.contains_key(&room_id) {
                return Ok(false);
            }
        }
        let session = DanmakuSession::start(room_id, http).await?;
        self.sessions.lock().await.insert(room_id, session);
        Ok(true)
    }

    pub async fn subscribe(&self, room_id: u64) -> Option<broadcast::Receiver<DanmakuBatch>> {
        self.sessions
            .lock()
            .await
            .get(&room_id)
            .map(|session| session.subscribe())
    }

    pub async fn stop(&self, room_id: u64) -> bool {
        let removed = self.sessions.lock().await.remove(&room_id);
        match removed {
            Some(session) => {
                session.close();
                true
            }
            None => false,
        }
    }

    pub async fn cleanup(&self) {
        let drained: Vec<Arc<DanmakuSession>> = {
            let mut guard = self.sessions.lock().await;
            guard.drain().map(|(_, session)| session).collect()
        };
        for session in drained {
            session.close();
        }
    }

    pub async fn active_rooms(&self) -> Vec<u64> {
        self.sessions.lock().await.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_reference_vectors() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex("The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn packet_round_trip() {
        let packet = pack_packet("{\"a\":1}", OP_MESSAGE);
        assert_eq!(packet.len(), PACKET_HEADER_SIZE + 7);
        let parsed = unpack_packets(&packet);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, OP_MESSAGE);
        assert_eq!(parsed[0].1.as_ref().unwrap()["a"], 1);
    }

    #[test]
    fn multiple_packets_in_one_frame() {
        let mut buffer = pack_packet("{\"a\":1}", OP_MESSAGE);
        buffer.extend_from_slice(&pack_packet(HEARTBEAT_BODY, OP_HEARTBEAT));
        let parsed = unpack_packets(&buffer);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].0, OP_HEARTBEAT);
        // 心跳包正文不解析成 JSON
        assert!(parsed[1].1.is_none());
    }

    #[test]
    fn broken_length_stops_parsing() {
        let mut buffer = pack_packet("{\"a\":1}", OP_MESSAGE);
        // 把总长改大，模拟半包：应当什么都不产出而不是切出垃圾
        buffer[0..4].copy_from_slice(&9999u32.to_be_bytes());
        assert!(unpack_packets(&buffer).is_empty());
    }

    #[test]
    fn extracts_danmaku_and_skips_other_commands() {
        let payload = serde_json::json!([
            { "cmd": "DANMU_MSG:4:0:2:2:2:0", "info": [[], "你好", [123, "观众甲"]] },
            { "cmd": "INTERACT_WORD", "info": [] },
            { "cmd": "DANMU_MSG", "info": [[], "", [1, "空文本"]] },
            { "cmd": "SEND_GIFT", "info": [] },
        ]);
        let items = extract_danmaku(&payload);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "你好");
        assert_eq!(items[0].username, "观众甲");
        assert!(items[0].emots.is_none());
    }

    #[test]
    fn extracts_emotes_from_extra_and_bulge() {
        // info[0] 下标 13 = 粉丝装扮表情，下标 15 = extra.emots；中间用 null 占位
        let mut meta = vec![serde_json::Value::Null; 16];
        meta[13] = serde_json::json!({
            "url": "http://i0.hdslb.com/e1.png",
            "emoticon_unique": "[展示]",
        });
        meta[15] = serde_json::json!({
            "extra": "{\"emots\":{\"[笑]\":{\"url\":\"http://i0.hdslb.com/e2.png\",\"width\":20,\"height\":20}}}",
        });
        let payload = serde_json::json!([{
            "cmd": "DANMU_MSG",
            "info": [meta, "文字[笑]", [1, "观众"]],
        }]);
        let items = extract_danmaku(&payload);
        assert_eq!(items.len(), 1);
        let emots = items[0].emots.as_ref().unwrap();
        assert!(emots.contains_key("[笑]"));
        assert!(emots.contains_key("[展示]"));
        // http 统一升级成 https（file:// 下混合内容会被拦）
        assert!(emots["[笑]"].url.starts_with("https://"));
    }

    #[test]
    fn wbi_key_uses_mixin_table() {
        // 只验证「取文件名 + 按表重排」这条链路自洽
        let key = build_wbi_key(
            "https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png",
            "https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png",
        );
        assert_eq!(key.len(), 32);
        assert!(key.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn wbi_query_is_sorted_and_signed() {
        let query = build_wbi_query(
            &[("id", "63727".to_string()), ("type", "0".to_string())],
            "0123456789abcdef0123456789abcdef",
            1700000000,
        );
        assert!(query.starts_with("id=63727&type=0&wts=1700000000&w_rid="));
        assert_eq!(
            query.len(),
            "id=63727&type=0&wts=1700000000&w_rid=".len() + 32
        );
    }
}
