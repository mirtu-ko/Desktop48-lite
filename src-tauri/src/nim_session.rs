//! 网易云信常驻会话（对照 Electron `nim-session.ts` 的 `CommonlinkSession`）。
//!
//! 一段 TCP：握手（RSA 交换 RC4 密钥）→ 登录 → 后台读帧 + 心跳 + 请求/响应。
//! 与 `nim.rs` 里的 spike 版 `NimSession` 的差别：
//!
//! - spike 版是「同步式读一帧」，用于验证协议；这里用**读任务 + 广播通道**，
//!   因为弹幕是持续推送，而且同时要处理请求应答、心跳应答与服务端推送。
//! - `nim.rs` 的帧 / 属性表 / RC4 / 握手原语全部复用，没有第二份实现。
//!
//! 应答匹配规则与 Electron 一致：serial 命中只是必要条件，**服务号 + 指令号也要对上**
//! 才算这条请求的应答（否则服务端推送 24/11 撞上在途 serial 会被误当成 ACK）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rand::Rng;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio::time::timeout;

use crate::nim::{
    CONNECT_TIMEOUT, Frame, Rc4, decode_frame, decode_properties, encode_frame,
    make_handshake_with_default_key,
};

/// LBS 超时也暴露给上层（圈组的 `conf.jsp` 复用同一超时口径）。
pub use crate::nim::LBS_TIMEOUT;
pub use crate::nim::{TcpAddress, parse_tcp_address};

/// 单请求超时（与 Electron 同值）：超时即失败，不无限等。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
/// 断线重连退避上限。
pub const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
/// 心跳间隔：服务端要求 30s 内至少一次。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
/// 空闲超时：这么久没有任何帧就主动断开（由上层决定是否重连）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);

const SERVICE_HANDSHAKE: u8 = 1;
const CMD_HANDSHAKE: u8 = 1;
const CMD_HEARTBEAT: u8 = 2;

/// 一次连接所需的登录参数。
#[derive(Clone, Debug)]
pub struct LoginSpec {
    pub address: TcpAddress,
    /// 未加密的登录帧（完整帧，含 varint 长度与 header）
    pub login_packet: Vec<u8>,
    pub login_service: u8,
    pub login_command: u8,
    /// 诊断标签：`聊天室` / `圈组`，拒绝时报「哪次登录」被拒
    pub login_label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionError {
    pub message: String,
    pub code: i16,
    /// true = 连接层面失败（可重连）；false = 协议 / 鉴权失败
    pub retryable: bool,
}

impl SessionError {
    fn protocol(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 0,
            retryable: false,
        }
    }

    fn coded(message: impl Into<String>, code: i16) -> Self {
        Self {
            message: message.into(),
            code,
            retryable: true,
        }
    }

    fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 0,
            retryable: true,
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.code != 0 {
            write!(formatter, "{}（{}）", self.message, self.code)
        } else {
            write!(formatter, "{}", self.message)
        }
    }
}

/// 请求 / 应答登记项。
struct Pending {
    service_id: u8,
    command_id: u8,
    sender: oneshot::Sender<Result<Frame, SessionError>>,
}

struct Inner {
    stream: TcpStream,
    encryptor: Rc4,
    serial: i16,
    pending: std::collections::HashMap<i16, Pending>,
}

/// 一条已登录的 commonlink 会话。
///
/// 帧的两种去向：命中在途请求 → 交给请求方；其余 → 推给 `packets` 广播，
/// 由上层（聊天室 / 圈组客户端）按服务号分拣。
pub struct CommonlinkSession {
    inner: Arc<Mutex<Inner>>,
    /// 主动发送（心跳 / 回执）走这里，避免与请求写入交错
    outbox: mpsc::UnboundedSender<Vec<u8>>,
    packets: broadcast::Sender<Frame>,
    alive: Arc<AtomicBool>,
    login_label: String,
}

impl CommonlinkSession {
    /// 建立连接并登录；返回时会话已可用。失败即终态（重连由调用方决定）。
    pub async fn connect(spec: &LoginSpec) -> Result<Self, SessionError> {
        let target = format!("{}:{}", spec.address.host, spec.address.port);
        let mut stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(&target))
            .await
            .map_err(|_| SessionError::transport("连接云信超时"))?
            .map_err(|err| SessionError::transport(format!("连接云信失败: {err}")))?;
        let _ = stream.set_nodelay(true);

        let key: Vec<u8> = rand::thread_rng()
            .sample_iter(rand::distributions::Standard)
            .take(16)
            .collect();
        let handshake = make_handshake_with_default_key(&key, &spec.login_packet)
            .map_err(SessionError::protocol)?;
        let mut encryptor = Rc4::new(&key).map_err(SessionError::protocol)?;
        let mut decryptor = Rc4::new(&key).map_err(SessionError::protocol)?;

        stream
            .write_all(&handshake)
            .await
            .map_err(|err| SessionError::transport(format!("发送握手包失败: {err}")))?;

        let (ack, leftover) = read_one_frame(&mut stream, &mut decryptor).await?;
        if ack.service_id != SERVICE_HANDSHAKE || ack.command_id != CMD_HANDSHAKE {
            return Err(SessionError::protocol(format!(
                "握手应答的 s/c 不符（实收 {}/{}）",
                ack.service_id, ack.command_id
            )));
        }
        if ack.result_code != 200 {
            return Err(SessionError::coded("云信握手失败", ack.result_code));
        }

        // 握手成功后登录包要再以 RC4 发一次
        let encrypted_login = encryptor.apply(&spec.login_packet);
        stream
            .write_all(&encrypted_login)
            .await
            .map_err(|err| SessionError::transport(format!("发送登录包失败: {err}")))?;

        let mut buffer = leftover;
        let login_frame = loop {
            let frame = read_frame_from(&mut stream, &mut decryptor, &mut buffer).await?;
            if frame.service_id != spec.login_service || frame.command_id != spec.login_command {
                continue;
            }
            break frame;
        };
        if login_frame.result_code != 200 {
            let detail = describe_properties(&login_frame.body);
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!("，{detail}")
            };
            return Err(SessionError::coded(
                format!("{}登录失败{suffix}", spec.login_label),
                login_frame.result_code,
            ));
        }

        let (outbox, outbox_rx) = mpsc::unbounded_channel();
        let (packets, _) = broadcast::channel(256);
        let alive = Arc::new(AtomicBool::new(true));
        let inner = Arc::new(Mutex::new(Inner {
            stream,
            encryptor,
            serial: 2,
            pending: std::collections::HashMap::new(),
        }));

        spawn_reader(
            inner.clone(),
            decryptor,
            buffer,
            outbox_rx,
            packets.clone(),
            alive.clone(),
        );
        spawn_heartbeat(outbox.clone(), alive.clone());

        Ok(Self {
            inner,
            outbox,
            packets,
            alive,
            login_label: spec.login_label.clone(),
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Frame> {
        self.packets.subscribe()
    }

    /// 发一个请求并等应答；只在 s/c 与 serial 都对上时才视为应答。
    pub async fn request(
        &self,
        service_id: u8,
        command_id: u8,
        body: Vec<u8>,
        failure_message: &str,
    ) -> Result<Frame, SessionError> {
        if !self.is_alive() {
            return Err(SessionError::transport("云信会话未连接"));
        }
        let (sender, receiver) = oneshot::channel();
        let serial = {
            let mut inner = self.inner().await;
            let serial = inner.next_serial();
            inner.pending.insert(
                serial,
                Pending {
                    service_id,
                    command_id,
                    sender,
                },
            );
            let encrypted = inner
                .encryptor
                .apply(&encode_frame(service_id, command_id, serial, &body));
            let _ = self.outbox.send(encrypted);
            serial
        };
        match timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(SessionError::transport(format!(
                "{failure_message}：连接已关闭"
            ))),
            Err(_) => {
                let mut inner = self.inner().await;
                inner.pending.remove(&serial);
                Err(SessionError::coded(
                    format!("{failure_message}：请求超时"),
                    0,
                ))
            }
        }
    }

    /// fire-and-forget（聊天室回执 13/35 用）；未连接时静默丢弃。
    pub fn send(&self, service_id: u8, command_id: u8, body: Vec<u8>) {
        if !self.is_alive() {
            return;
        }
        // fire-and-forget 也用真实 serial：服务端按 serial 关联，固定 0 可能被去重
        let Ok(mut inner) = self.inner.try_lock() else {
            let outbox = self.outbox.clone();
            let frame = encode_frame(service_id, command_id, 0, &body);
            tokio::spawn(async move {
                let _ = outbox.send(frame);
            });
            return;
        };
        let serial = inner.next_serial();
        let encrypted = inner
            .encryptor
            .apply(&encode_frame(service_id, command_id, serial, &body));
        let _ = self.outbox.send(encrypted);
    }

    pub fn login_label(&self) -> &str {
        &self.login_label
    }

    pub fn destroy(&self) {
        self.alive.store(false, Ordering::Relaxed);
    }

    async fn inner(&self) -> tokio::sync::MutexGuard<'_, Inner> {
        self.inner.lock().await
    }
}

impl Inner {
    fn next_serial(&mut self) -> i16 {
        let value = self.serial;
        self.serial = if value == i16::MAX { 2 } else { value + 1 };
        value
    }
}

/// 读任务：持续解帧，命中在途请求就交给请求方，否则广播。
fn spawn_reader(
    inner: Arc<Mutex<Inner>>,
    mut decryptor: Rc4,
    mut buffer: Vec<u8>,
    mut outbox: mpsc::UnboundedReceiver<Vec<u8>>,
    packets: broadcast::Sender<Frame>,
    alive: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        loop {
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            let mut chunk = [0u8; 4096];
            let read = {
                let mut guard = inner.lock().await;
                // 先把待发字节刷出去（心跳 / 回执 / 请求）
                while let Ok(bytes) = outbox.try_recv() {
                    if guard.stream.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                match timeout(IDLE_TIMEOUT, guard.stream.read(&mut chunk)).await {
                    Ok(Ok(count)) => count,
                    Ok(Err(_)) | Err(_) => 0,
                }
            };
            if read == 0 {
                alive.store(false, Ordering::Relaxed);
                let mut guard = inner.lock().await;
                for (_, pending) in guard.pending.drain() {
                    let _ = pending
                        .sender
                        .send(Err(SessionError::transport("云信连接已关闭")));
                }
                break;
            }
            let plain = decryptor.apply(&chunk[..read]);
            buffer.extend_from_slice(&plain);
            while let Some(frame) = take_frame(&mut buffer) {
                let Ok(frame) = frame else {
                    alive.store(false, Ordering::Relaxed);
                    return;
                };
                let mut guard = inner.lock().await;
                if let Some(pending) = guard.pending.remove(&frame.serial) {
                    if pending.service_id == frame.service_id
                        && pending.command_id == frame.command_id
                    {
                        let result = if frame.result_code == 200 {
                            Ok(frame)
                        } else {
                            Err(SessionError::coded("云信请求失败", frame.result_code))
                        };
                        let _ = pending.sender.send(result);
                        continue;
                    }
                    // s/c 不符：说明这是推送而不是应答，放回去并按推送处理
                    guard.pending.insert(frame.serial, pending);
                }
                drop(guard);
                let _ = packets.send(frame);
            }
        }
    });
}

fn spawn_heartbeat(outbox: mpsc::UnboundedSender<Vec<u8>>, alive: Arc<AtomicBool>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            // serial 固定 0：心跳应答无人等待，只需让服务端看到流量
            let _ = outbox.send(encode_frame(SERVICE_HANDSHAKE, CMD_HEARTBEAT, 0, &[]));
        }
    });
}

/// 从 buffer 里取一帧（含上行合并帧的展开）。
fn take_frame(buffer: &mut Vec<u8>) -> Option<Result<Frame, String>> {
    let (length, varint_size) = crate::nim::read_varint(buffer, 0)?;
    let total = varint_size + length as usize;
    if buffer.len() < total {
        return None;
    }
    let raw: Vec<u8> = buffer.drain(..total).collect();
    let frame = match decode_frame(&raw, false) {
        Ok(frame) => frame,
        Err(err) => return Some(Err(err)),
    };
    if frame.service_id == 4 && matches!(frame.command_id, 1 | 2 | 10 | 11) && frame.body.len() > 8
    {
        return Some(decode_frame(&frame.body[8..], true));
    }
    Some(Ok(frame))
}

async fn read_one_frame(
    stream: &mut TcpStream,
    decryptor: &mut Rc4,
) -> Result<(Frame, Vec<u8>), SessionError> {
    let mut buffer = Vec::new();
    let frame = read_frame_from(stream, decryptor, &mut buffer).await?;
    Ok((frame, buffer))
}

async fn read_frame_from(
    stream: &mut TcpStream,
    decryptor: &mut Rc4,
    buffer: &mut Vec<u8>,
) -> Result<Frame, SessionError> {
    loop {
        if let Some(frame) = take_frame(buffer) {
            return frame.map_err(SessionError::protocol);
        }
        let mut chunk = [0u8; 4096];
        let read = timeout(CONNECT_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| SessionError::transport("等待云信应答超时"))?
            .map_err(|err| SessionError::transport(format!("读取云信连接失败: {err}")))?;
        if read == 0 {
            return Err(SessionError::transport("云信连接已关闭"));
        }
        let plain = decryptor.apply(&chunk[..read]);
        buffer.extend_from_slice(&plain);
    }
}

/// 登录被拒时回包是属性表；摘要截断防刷屏。
fn describe_properties(body: &[u8]) -> String {
    let Ok(properties) = decode_properties(body) else {
        return String::new();
    };
    properties
        .iter()
        .take(6)
        .map(|(key, value)| {
            let text = String::from_utf8_lossy(value);
            let clipped = if text.chars().count() > 24 {
                format!("{}…", text.chars().take(24).collect::<String>())
            } else {
                text.to_string()
            };
            format!("{key}={clipped}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// LBS 超时也暴露给上层（圈组有独立的 lbs 地址需求）。
pub const LBS_REQUEST_TIMEOUT: Duration = LBS_TIMEOUT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_through_take_frame() {
        let frame = encode_frame(13, 7, 5, b"hello");
        let mut buffer = frame.clone();
        let parsed = take_frame(&mut buffer).unwrap().unwrap();
        assert_eq!(parsed.service_id, 13);
        assert_eq!(parsed.command_id, 7);
        assert_eq!(parsed.serial, 5);
        assert_eq!(parsed.body, b"hello");
        assert!(buffer.is_empty());
    }

    #[test]
    fn partial_frame_waits_for_more_bytes() {
        let frame = encode_frame(13, 7, 1, b"abc");
        let mut buffer = frame[..3].to_vec();
        assert!(take_frame(&mut buffer).is_none());
        buffer.extend_from_slice(&frame[3..]);
        assert!(take_frame(&mut buffer).unwrap().is_ok());
    }

    #[test]
    fn error_display_includes_code() {
        assert_eq!(
            SessionError::coded("登录失败", 417).to_string(),
            "登录失败（417）"
        );
        assert_eq!(SessionError::transport("超时").to_string(), "超时");
    }
}
