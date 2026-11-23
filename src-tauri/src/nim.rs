//! 网易云信聊天室（弹幕）TCP 协议实现。
//!
//! 对照 Electron 版 `src/main/pocket48/nim-common.ts`、`nim-session.ts`、`nim-chatroom.ts`，
//! 覆盖：LBS 取地址 → RSA 握手 → RC4 加密登录 → 收发聊天室消息。
//!
//! 协议要点（与 Electron 版逐字节一致）：
//! - 帧：varint(总长) + header{serviceId:u8, commandId:u8, serial:i16LE, tag} + body
//!   tag bit0 = body 为 zlib 压缩（前 4 字节 i32LE 为解压后长度），bit1 = 头后带 i16LE 结果码
//! - 属性表：varint(数量) + N × (varint(key) + varint(len) + bytes)
//! - 握手（1/1）：i32LE(密钥版本) + RSA 分块加密的 [varint(len)+RC4 密钥 + 登录包]；
//!   服务端应答也用同一 RC4 密钥加密，所以收到握手应答前不能开解密
//! - 聊天室登录（13/2）：`[0x02] + 房间属性表 + 认证属性表`（token 放属性 1000）
//! - 消息（13/7）；属性 38='1' 需回 13/35 回执；心跳 1/2 每 15s
//!
//! 凭据（appKey / accid / IM token / 聊天室 ID）来自登录后的口袋 HTTP 接口
//! （`getImCredentials`），本模块只接收调用方传入，不落盘、不打印 token。

use std::collections::BTreeMap;
use std::io::Read as _;
use std::time::Duration;

use base64::Engine as _;
use flate2::read::ZlibDecoder;
#[cfg(test)]
use rand::Rng;
use rsa::RsaPublicKey;
use rsa::pkcs1v15::Pkcs1v15Encrypt;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(test)]
use tokio::net::TcpStream;

// ===== 常量（与 Electron 版同值） =====

/// 口袋48 内置云信 AppKey。
pub const IM_APP_KEY: &str = "632feff1f4c838541ab75195d1ceb3fa";

const CHAT_LBS_URL: &str = "https://lbs.netease.im/lbs/chat.jsp";
const RSA_PUBLIC_KEY_DER: &str = "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQCBxLuL8+xpQSddSnSvPkvNOHdcr5Euqw+kkOSzO/buDMheCfFILRC/v5+nv8BsL7/YZWVpDA8sIBTxfNRqSCu0uLjlbJqT/sMnPT1xxdQrkb1HSnuSyTbZbqaInQ13tBE2SfcAhsQZJJ1hKQSE2QyKOMxQPhP583qcsIhDbdExvwIDAQAB";
const RSA_KEY_VERSION: i32 = 0;

const SERVICE_HANDSHAKE: u8 = 1;
const CMD_HANDSHAKE: u8 = 1;

// 聊天室服务号 / 指令号：只给本模块的 mock 服务端测试用
//（生产路径的登录包在 nim_chatroom.rs，那边有自己的同名常量）
#[cfg(test)]
const SERVICE_CHATROOM: u8 = 13;
#[cfg(test)]
const CMD_CHATROOM_LOGIN: u8 = 2;
#[cfg(test)]
const CMD_CHATROOM_ACK: u8 = 35;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
pub const LBS_TIMEOUT: Duration = Duration::from_secs(15);

// ===== varint / 属性表 =====

pub fn encode_varint(value: u32) -> Vec<u8> {
    let mut rest = value;
    let mut bytes = Vec::new();
    loop {
        let mut byte = (rest & 0x7F) as u8;
        rest /= 128;
        if rest > 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if rest == 0 {
            break;
        }
    }
    bytes
}

/// 读 varint：返回 (值, 消耗字节数)；不完整返回 None。
pub fn read_varint(buffer: &[u8], offset: usize) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    let mut scale: u32 = 1;
    for (index, byte) in buffer.iter().enumerate().skip(offset).take(5) {
        value += u32::from(byte & 0x7F) * scale;
        if byte & 0x80 == 0 {
            return Some((value, index - offset + 1));
        }
        scale = scale.saturating_mul(128);
    }
    None
}

fn encode_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = encode_varint(bytes.len() as u32);
    out.extend_from_slice(bytes);
    out
}

/// 属性表编码：按 key 升序、跳过空值（顺序影响对端解析预期）。
pub fn encode_properties(entries: &[(u32, String)]) -> Vec<u8> {
    let mut entries: Vec<(u32, String)> = entries
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .cloned()
        .collect();
    entries.sort_by_key(|(key, _)| *key);
    let mut out = encode_varint(entries.len() as u32);
    for (key, value) in entries {
        out.extend_from_slice(&encode_varint(key));
        out.extend_from_slice(&encode_bytes(value.as_bytes()));
    }
    out
}

pub fn decode_properties(body: &[u8]) -> Result<BTreeMap<u32, Vec<u8>>, String> {
    let (count, mut offset) = read_varint(body, 0).ok_or_else(|| "属性表不完整".to_string())?;
    let mut properties = BTreeMap::new();
    for _ in 0..count {
        let (key, key_size) =
            read_varint(body, offset).ok_or_else(|| "属性表条目不完整".to_string())?;
        offset += key_size;
        let (length, length_size) =
            read_varint(body, offset).ok_or_else(|| "属性表条目不完整".to_string())?;
        offset += length_size;
        let end = offset + length as usize;
        if end > body.len() {
            return Err("属性表内容越界".to_string());
        }
        properties.insert(key, body[offset..end].to_vec());
        offset = end;
    }
    Ok(properties)
}

fn text(bytes: Option<&Vec<u8>>) -> String {
    bytes
        .map(|value| String::from_utf8_lossy(value).trim().to_string())
        .unwrap_or_default()
}

// ===== 帧 =====

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub service_id: u8,
    pub command_id: u8,
    pub serial: i16,
    pub result_code: i16,
    pub body: Vec<u8>,
}

pub fn encode_frame(service_id: u8, command_id: u8, serial: i16, body: &[u8]) -> Vec<u8> {
    encode_frame_tagged(service_id, command_id, serial, 0, body)
}

fn encode_frame_tagged(
    service_id: u8,
    command_id: u8,
    serial: i16,
    tag: u8,
    body: &[u8],
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(body.len() + 8);
    frame.extend_from_slice(&encode_varint((5 + body.len()) as u32));
    frame.push(service_id);
    frame.push(command_id);
    frame.extend_from_slice(&serial.to_le_bytes());
    frame.push(tag);
    frame.extend_from_slice(body);
    frame
}

/// 解一帧；`embedded` = 上行合并帧（service 4）内部的数据包，长度到 buffer 末尾。
pub fn decode_frame(raw: &[u8], embedded: bool) -> Result<Frame, String> {
    let (length, varint_size) = read_varint(raw, 0).ok_or_else(|| "数据帧不完整".to_string())?;
    if raw.len() < varint_size + 5 {
        return Err("数据帧不完整".to_string());
    }
    if !embedded && (length as usize) < 5 {
        return Err("数据帧长度非法".to_string());
    }

    let mut offset = varint_size;
    let service_id = raw[offset];
    let command_id = raw[offset + 1];
    let serial = i16::from_le_bytes([raw[offset + 2], raw[offset + 3]]);
    let tag = raw[offset + 4];
    offset += 5;

    let mut result_code = 200i16;
    if tag & 2 != 0 {
        if offset + 2 > raw.len() {
            return Err("响应状态不完整".to_string());
        }
        result_code = i16::from_le_bytes([raw[offset], raw[offset + 1]]);
        offset += 2;
    }

    let end = if embedded {
        raw.len()
    } else {
        raw.len().min(varint_size + length as usize)
    };
    let mut body = raw[offset..end].to_vec();
    if tag & 1 != 0 {
        body = inflate_body(&body)?;
    }
    Ok(Frame {
        service_id,
        command_id,
        serial,
        result_code,
        body,
    })
}

fn inflate_body(body: &[u8]) -> Result<Vec<u8>, String> {
    if body.len() < 4 {
        return Err("压缩帧不完整".to_string());
    }
    let expected = i32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let mut decoder = ZlibDecoder::new(&body[4..]);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|err| format!("解压失败: {err}"))?;
    if out.len() != expected as usize {
        return Err("压缩帧长度不一致".to_string());
    }
    Ok(out)
}

// ===== RC4 =====

pub struct Rc4 {
    state: [u8; 256],
    x: usize,
    y: usize,
}

impl Rc4 {
    pub fn new(key: &[u8]) -> Result<Self, String> {
        if key.is_empty() {
            return Err("加密密钥为空".to_string());
        }
        let mut state = [0u8; 256];
        for (index, byte) in state.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let mut cursor = 0usize;
        for index in 0..256 {
            cursor = (cursor + state[index] as usize + key[index % key.len()] as usize) & 0xFF;
            state.swap(index, cursor);
        }
        Ok(Self { state, x: 0, y: 0 })
    }

    /// RC4 是对称流：同一实例既加密也解密，但收发必须各持一个实例。
    pub fn apply(&mut self, input: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(input.len());
        for byte in input {
            self.x = (self.x + 1) & 0xFF;
            self.y = (self.y + self.state[self.x] as usize) & 0xFF;
            self.state.swap(self.x, self.y);
            let index = (self.state[self.x] as usize + self.state[self.y] as usize) & 0xFF;
            output.push(byte ^ self.state[index]);
        }
        output
    }
}

// ===== 握手 =====

fn default_public_key() -> Result<RsaPublicKey, String> {
    RsaPublicKey::from_public_key_der(
        &base64::engine::general_purpose::STANDARD
            .decode(RSA_PUBLIC_KEY_DER)
            .map_err(|err| format!("公钥解码失败: {err}"))?,
    )
    .map_err(|err| format!("公钥解析失败: {err}"))
}

fn encrypt_payload_with(public_key: &RsaPublicKey, payload: &[u8]) -> Result<Vec<u8>, String> {
    let chunk = public_key.size() - 11;
    let mut rng = rand::thread_rng();
    let mut out = Vec::new();
    for part in payload.chunks(chunk) {
        out.extend(
            public_key
                .encrypt(&mut rng, Pkcs1v15Encrypt, part)
                .map_err(|err| format!("RSA 加密失败: {err}"))?,
        );
    }
    Ok(out)
}

/// 用内置公钥构造握手包（常驻会话用）。
pub fn make_handshake_with_default_key(key: &[u8], login_packet: &[u8]) -> Result<Vec<u8>, String> {
    make_handshake_with(&default_public_key()?, key, login_packet)
}

/// 握手包：i32LE(密钥版本) + 分块加密的 [varint(len)+RC4 密钥 + 登录包]。
fn make_handshake_with(
    public_key: &RsaPublicKey,
    key: &[u8],
    login_packet: &[u8],
) -> Result<Vec<u8>, String> {
    let mut payload = encode_bytes(key);
    payload.extend_from_slice(login_packet);
    let encrypted = encrypt_payload_with(public_key, &payload)?;
    let mut body = Vec::with_capacity(encrypted.len() + 4);
    body.extend_from_slice(&RSA_KEY_VERSION.to_le_bytes());
    body.extend_from_slice(&encrypted);
    Ok(encode_frame(SERVICE_HANDSHAKE, CMD_HANDSHAKE, 0, &body))
}

// ===== LBS 地址 =====

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpAddress {
    pub host: String,
    pub port: u16,
}

pub fn parse_tcp_address(address: &str) -> Result<TcpAddress, String> {
    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| "云信未返回有效的连接地址".to_string())?;
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let port: u16 = port
        .parse()
        .map_err(|_| "云信未返回有效的连接地址".to_string())?;
    if host.is_empty() || port == 0 {
        return Err("云信未返回有效的连接地址".to_string());
    }
    Ok(TcpAddress {
        host: host.to_string(),
        port,
    })
}

/// 聊天室 LBS：`chat.jsp` 带 k/id/rid/v；回包取 `link[0]`。
pub async fn fetch_chatroom_address(
    app_key: &str,
    account: &str,
    room_id: &str,
    sdk_version: u32,
) -> Result<TcpAddress, String> {
    let client = reqwest::Client::builder()
        .timeout(LBS_TIMEOUT)
        .build()
        .map_err(|err| format!("LBS 客户端创建失败: {err}"))?;
    let sdk = sdk_version.to_string();
    let response = client
        .get(CHAT_LBS_URL)
        .query(&[
            ("k", app_key),
            ("id", account),
            ("rid", room_id),
            ("v", sdk.as_str()),
            ("tp", "1"),
            ("dt", "0"),
            ("nt", "2"),
        ])
        .send()
        .await
        .map_err(|err| format!("获取聊天室地址失败: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "获取聊天室地址失败（HTTP {}）",
            response.status().as_u16()
        ));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|err| format!("聊天室地址回包不是 JSON: {err}"))?;
    let link = value
        .get("link")
        .and_then(Value::as_array)
        .and_then(|links| links.first())
        .and_then(Value::as_str)
        .unwrap_or_default();
    parse_tcp_address(link)
}

// ===== 会话：TCP + 握手 + 登录 + 收帧 =====

/// spike 时期「同步读一帧」的会话实现。
///
/// 生产路径已完全走 `nim_session.rs`（读任务 + 广播 + 心跳），这里的值只剩
/// **mock 服务端的全链路回归测试**：它驱动真实的握手 / 登录 / 收帧字节流，
/// 是唯一能在无真机环境下验证协议正确性的测试，故保留但编译进测试目标。
#[cfg(test)]
pub struct NimSession {
    stream: TcpStream,
    encryptor: Rc4,
    decryptor: Rc4,
    buffer: Vec<u8>,
    serial: i16,
}

#[cfg(test)]
impl NimSession {
    /// 指定公钥的握手（单测用自签密钥对驱动完整的登录/消息回路）。
    async fn handshake_with_key(
        address: &TcpAddress,
        login_packet: Vec<u8>,
        public_key: &RsaPublicKey,
    ) -> Result<Self, String> {
        let target = format!("{}:{}", address.host, address.port);
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target))
            .await
            .map_err(|_| "连接云信超时".to_string())?
            .map_err(|err| format!("连接云信失败: {err}"))?;
        let _ = stream.set_nodelay(true);

        let key: Vec<u8> = rand::thread_rng()
            .sample_iter(rand::distributions::Standard)
            .take(16)
            .collect();
        let handshake = make_handshake_with(public_key, &key, &login_packet)?;
        let mut session = Self {
            stream,
            encryptor: Rc4::new(&key)?,
            decryptor: Rc4::new(&key)?,
            buffer: Vec::new(),
            serial: 2,
        };
        session
            .stream
            .write_all(&handshake)
            .await
            .map_err(|err| format!("发送握手包失败: {err}"))?;
        eprintln!("[nim] 握手包已发送（{} 字节）", handshake.len());

        // 握手应答：结果码非 200 即失败；成功后以 RC4 再发一次登录包。
        let handshake_ack = tokio::time::timeout(CONNECT_TIMEOUT, session.read_frame())
            .await
            .map_err(|_| "等待握手应答超时".to_string())??;
        if handshake_ack.service_id != SERVICE_HANDSHAKE
            || handshake_ack.command_id != CMD_HANDSHAKE
        {
            return Err(format!(
                "握手应答的 s/c 不符（实收 {}/{}）",
                handshake_ack.service_id, handshake_ack.command_id
            ));
        }
        if handshake_ack.result_code != 200 {
            return Err(format!("云信握手失败（{}）", handshake_ack.result_code));
        }
        eprintln!("[nim] 握手成功");
        Ok(session)
    }

    /// 在已完成握手的会话上发送加密登录包并等待登录应答。
    async fn login(
        mut self,
        login_packet: &[u8],
        login_service: u8,
        login_command: u8,
        login_label: &str,
    ) -> Result<Self, String> {
        let encrypted_login = self.encryptor.apply(login_packet);
        self.stream
            .write_all(&encrypted_login)
            .await
            .map_err(|err| format!("发送登录包失败: {err}"))?;

        loop {
            let frame = tokio::time::timeout(CONNECT_TIMEOUT, self.read_frame())
                .await
                .map_err(|_| format!("等待{login_label}登录应答超时"))??;
            if frame.service_id != login_service || frame.command_id != login_command {
                eprintln!(
                    "[nim] 登录期间收到其他帧 s={} c={}（忽略）",
                    frame.service_id, frame.command_id
                );
                continue;
            }
            if frame.result_code != 200 {
                let detail = describe_properties(&frame.body);
                let suffix = if detail.is_empty() {
                    String::new()
                } else {
                    format!("，{detail}")
                };
                return Err(format!(
                    "{login_label}登录失败（{}{suffix}）",
                    frame.result_code
                ));
            }
            eprintln!("[nim] {login_label}登录成功");
            return Ok(self);
        }
    }

    fn take_frame(&mut self) -> Result<Option<Frame>, String> {
        loop {
            let Some((length, varint_size)) = read_varint(&self.buffer, 0) else {
                return Ok(None);
            };
            let total = varint_size + length as usize;
            if self.buffer.len() < total {
                return Ok(None);
            }
            let raw = self.buffer[..total].to_vec();
            self.buffer.drain(..total);

            let mut frame = decode_frame(&raw, false)?;
            if frame.service_id == 4
                && matches!(frame.command_id, 1 | 2 | 10 | 11)
                && frame.body.len() > 8
            {
                // 上行合并帧：跳过前 8 字节后是真正的数据包。
                match decode_frame(&frame.body[8..], true) {
                    Ok(inner) => frame = inner,
                    Err(_) => continue,
                }
            }
            return Ok(Some(frame));
        }
    }

    async fn read_frame(&mut self) -> Result<Frame, String> {
        loop {
            if let Some(frame) = self.take_frame()? {
                return Ok(frame);
            }
            let mut chunk = [0u8; 4096];
            let read = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|err| format!("读取云信连接失败: {err}"))?;
            if read == 0 {
                return Err("云信连接已关闭".to_string());
            }
            let plain = self.decryptor.apply(&chunk[..read]);
            self.buffer.extend_from_slice(&plain);
        }
    }

    fn next_serial(&mut self) -> i16 {
        let value = self.serial;
        self.serial = if value == i16::MAX { 2 } else { value + 1 };
        value
    }

    async fn send_frame(
        &mut self,
        service_id: u8,
        command_id: u8,
        body: &[u8],
    ) -> Result<(), String> {
        let serial = self.next_serial();
        let frame = self
            .encryptor
            .apply(&encode_frame(service_id, command_id, serial, body));
        self.stream
            .write_all(&frame)
            .await
            .map_err(|err| format!("发送云信帧失败: {err}"))
    }
}

#[cfg(test)]
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

// ===== 聊天室消息 =====

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// `text` / `custom`（与 Electron `PocketRawChatMessage.type` 同值）
    pub kind: String,
    pub uuid: String,
    pub msg_type: i64,
    /// 0 = 文本，非 0 = 自定义扩展（JSON 在 custom）
    pub text: String,
    pub attach: String,
    pub custom: String,
    pub from_nick: String,
    pub from_avatar: String,
    pub from: String,
    pub time: i64,
    pub needs_ack: bool,
}

/// 13/7 消息体归一（字段与 Electron `nim-chatroom.handleMessage` 一致）。
pub fn normalize_message(body: &[u8]) -> Result<ChatMessage, String> {
    let properties = decode_properties(body)?;
    let msg_type = text(properties.get(&2)).parse::<i64>().unwrap_or(-1);
    let is_text = msg_type == 0;
    let attach = text(properties.get(&3));
    let time = text(properties.get(&20))
        .parse::<i64>()
        .unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as i64)
                .unwrap_or(0)
        });
    Ok(ChatMessage {
        kind: if is_text { "text" } else { "custom" }.to_string(),
        uuid: text(properties.get(&1)),
        msg_type,
        text: if is_text {
            attach.clone()
        } else {
            text(properties.get(&13))
        },
        attach,
        custom: text(properties.get(&4)),
        from_nick: text(properties.get(&7)),
        from_avatar: text(properties.get(&8)),
        from: text(properties.get(&21)),
        time,
        needs_ack: text(properties.get(&38)) == "1",
    })
}

/// 聊天室登录包体：`[0x02] + 房间属性表 + 认证属性表`。
pub fn build_chatroom_login_body(
    app_key: &str,
    account: &str,
    token: &str,
    room_id: &str,
    device_id: &str,
    sdk_version: u32,
    package_name: &str,
) -> Vec<u8> {
    let room = encode_properties(&[
        (1, app_key.to_string()),
        (2, account.to_string()),
        (3, device_id.to_string()),
        (5, room_id.to_string()),
        (8, "1".to_string()),
    ]);
    let auth = encode_properties(&[
        (3, "1".to_string()),
        (4, "8.0.0".to_string()),
        (6, sdk_version.to_string()),
        (9, "1".to_string()),
        (13, device_id.to_string()),
        (18, app_key.to_string()),
        (19, account.to_string()),
        (25, package_name.to_string()),
        (1000, token.to_string()),
    ]);
    let mut body = vec![2u8];
    body.extend_from_slice(&room);
    body.extend_from_slice(&auth);
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_and_known_values() {
        assert_eq!(encode_varint(0), vec![0]);
        assert_eq!(encode_varint(300), vec![0xAC, 0x02]);
        assert_eq!(read_varint(&[0xAC, 0x02], 0), Some((300, 2)));
        assert_eq!(read_varint(&[0x80], 0), None);
    }

    #[test]
    fn rc4_matches_rfc_vectors() {
        let mut cipher = Rc4::new(b"Key").unwrap();
        assert_eq!(
            cipher.apply(b"Plaintext"),
            [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]
        );

        let mut cipher = Rc4::new(b"Wiki").unwrap();
        assert_eq!(cipher.apply(b"pedia"), [0x10, 0x21, 0xBF, 0x04, 0x20]);

        let mut cipher = Rc4::new(b"Secret").unwrap();
        assert_eq!(
            cipher.apply(b"Attack at dawn"),
            [
                0x45, 0xA0, 0x1F, 0x64, 0x5F, 0xC3, 0x5B, 0x38, 0x35, 0x52, 0x54, 0x4B, 0x9B, 0xF5
            ]
        );
    }

    #[test]
    fn properties_encode_sorted_and_skip_empty() {
        let encoded = encode_properties(&[
            (5, "4321".to_string()),
            (1, "app".to_string()),
            (2, String::new()),
        ]);
        let decoded = decode_properties(&encoded).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(text(decoded.get(&1)), "app");
        assert_eq!(text(decoded.get(&5)), "4321");
        assert!(!decoded.contains_key(&2));
    }

    fn encode_response_frame(
        service_id: u8,
        command_id: u8,
        serial: i16,
        result_code: i16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut body = result_code.to_le_bytes().to_vec();
        body.extend_from_slice(payload);
        encode_frame_tagged(service_id, command_id, serial, 2, &body)
    }

    #[test]
    fn frame_roundtrip_with_result_code() {
        let raw = encode_response_frame(13, 2, 1, 200, &[1, 2, 3]);
        let frame = decode_frame(&raw, false).unwrap();
        assert_eq!(frame.service_id, 13);
        assert_eq!(frame.command_id, 2);
        assert_eq!(frame.serial, 1);
        assert_eq!(frame.result_code, 200);
        assert_eq!(frame.body, vec![1, 2, 3]);
    }

    #[test]
    fn compressed_frame_inflates() {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write as _;

        let payload = b"hello nim";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut body = Vec::new();
        body.extend_from_slice(&(payload.len() as i32).to_le_bytes());
        body.extend_from_slice(&compressed);
        let raw = encode_frame_tagged(13, 7, 0, 1, &body);
        let frame = decode_frame(&raw, false).unwrap();
        assert_eq!(frame.body, payload);
    }

    #[test]
    fn handshake_uses_rsa_blocks_and_version_header() {
        let login_packet = encode_frame(13, 2, 1, &[2]);
        let key = vec![7u8; 16];
        let handshake =
            make_handshake_with(&default_public_key().unwrap(), &key, &login_packet).unwrap();
        let frame = decode_frame(&handshake, false).unwrap();
        assert_eq!((frame.service_id, frame.command_id), (1, 1));
        assert_eq!(
            i32::from_le_bytes([frame.body[0], frame.body[1], frame.body[2], frame.body[3]]),
            0
        );
        // 1024-bit RSA 密文块固定 128 字节；分块数由明文长度决定。
        let encrypted = &frame.body[4..];
        assert_eq!(encrypted.len() % 128, 0);
        assert!(encrypted.len() >= 128);
    }

    #[test]
    fn parse_address_handles_host_and_ipv6() {
        assert_eq!(
            parse_tcp_address("1.2.3.4:9939").unwrap(),
            TcpAddress {
                host: "1.2.3.4".to_string(),
                port: 9939
            }
        );
        assert_eq!(parse_tcp_address("[::1]:9939").unwrap().host, "::1");
        assert!(parse_tcp_address("1.2.3.4").is_err());
        assert!(parse_tcp_address("1.2.3.4:0").is_err());
    }

    #[test]
    fn login_body_carries_identity_and_token() {
        let body = build_chatroom_login_body(
            "app-key",
            "accid-1",
            "im-token-1",
            "4321",
            "0123456789abcdef",
            92012,
            "com.pocket.snh48.activity",
        );
        assert_eq!(body[0], 2);
        let room = decode_properties(&body[1..]).unwrap();
        assert_eq!(text(room.get(&1)), "app-key");
        assert_eq!(text(room.get(&5)), "4321");
        // 认证属性表起点 = 1 + 房间属性表长度；直接搜 token 尾部不可靠，改用完整解析偏移。
        let room_len = encode_properties(&[
            (1, "app-key".to_string()),
            (2, "accid-1".to_string()),
            (3, "0123456789abcdef".to_string()),
            (5, "4321".to_string()),
            (8, "1".to_string()),
        ])
        .len();
        let auth = decode_properties(&body[1 + room_len..]).unwrap();
        assert_eq!(text(auth.get(&6)), "92012");
        assert_eq!(text(auth.get(&25)), "com.pocket.snh48.activity");
        assert_eq!(text(auth.get(&1000)), "im-token-1");
    }

    async fn read_packet(stream: &mut TcpStream) -> Vec<u8> {
        let mut buffer = Vec::new();
        loop {
            if let Some((length, varint_size)) = read_varint(&buffer, 0) {
                let total = varint_size + length as usize;
                if buffer.len() >= total {
                    return buffer[..total].to_vec();
                }
            }
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).await.expect("读取连接");
            assert!(read > 0, "服务端会话被提前关闭");
            buffer.extend_from_slice(&chunk[..read]);
        }
    }

    async fn read_encrypted_packet(stream: &mut TcpStream, decryptor: &mut Rc4) -> Vec<u8> {
        let mut buffer = Vec::new();
        loop {
            if let Some((length, varint_size)) = read_varint(&buffer, 0) {
                let total = varint_size + length as usize;
                if buffer.len() >= total {
                    return buffer[..total].to_vec();
                }
            }
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).await.expect("读取连接");
            assert!(read > 0, "服务端会话被提前关闭");
            let plain = decryptor.apply(&chunk[..read]);
            buffer.extend_from_slice(&plain);
        }
    }

    #[tokio::test]
    async fn login_message_and_ack_roundtrip_against_local_server() {
        use rsa::RsaPrivateKey;
        use tokio::net::TcpListener;

        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 1024).expect("生成测试密钥");
        let public = private.to_public_key();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("监听端口");
        let port = listener.local_addr().expect("端口").port();
        let address = TcpAddress {
            host: "127.0.0.1".to_string(),
            port,
        };

        let login_packet = encode_frame(
            SERVICE_CHATROOM,
            CMD_CHATROOM_LOGIN,
            1,
            &build_chatroom_login_body(
                "app-key",
                "accid-1",
                "im-token-1",
                "4321",
                "0123456789abcdef",
                92012,
                "com.pocket.snh48.activity",
            ),
        );
        let expected_login = login_packet.clone();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("接受连接");

            let handshake = read_packet(&mut stream).await;
            let frame = decode_frame(&handshake, false).expect("解析握手");
            assert_eq!((frame.service_id, frame.command_id), (1, 1));
            assert_eq!(
                i32::from_le_bytes([frame.body[0], frame.body[1], frame.body[2], frame.body[3]]),
                0
            );

            let mut payload = Vec::new();
            for block in frame.body[4..].chunks(128) {
                payload.extend(private.decrypt(Pkcs1v15Encrypt, block).expect("解 RSA 块"));
            }
            let (key_len, varint_size) = read_varint(&payload, 0).expect("密钥长度");
            let key = payload[varint_size..varint_size + key_len as usize].to_vec();
            assert_eq!(
                &payload[varint_size + key_len as usize..],
                &expected_login[..]
            );

            let mut encryptor = Rc4::new(&key).expect("服务端加密器");
            let mut decryptor = Rc4::new(&key).expect("服务端解密器");
            let ack = encode_response_frame(1, 1, 0, 200, &[]);
            stream
                .write_all(&encryptor.apply(&ack))
                .await
                .expect("回握手应答");

            let login_raw = read_encrypted_packet(&mut stream, &mut decryptor).await;
            let login = decode_frame(&login_raw, false).expect("解析登录包");
            assert_eq!((login.service_id, login.command_id), (13, 2));
            assert_eq!(login.serial, 1);
            let (_, header_size) = read_varint(&expected_login, 0).expect("登录包长度");
            assert_eq!(&login.body[..], &expected_login[header_size + 5..]);

            let login_ack = encode_response_frame(13, 2, 1, 200, &[]);
            stream
                .write_all(&encryptor.apply(&login_ack))
                .await
                .expect("回登录应答");

            let message = encode_properties(&[
                (1, "uuid-1".to_string()),
                (2, "0".to_string()),
                (3, "上播了".to_string()),
                (7, "昵称".to_string()),
                (20, "1700000000000".to_string()),
                (21, "123".to_string()),
                (38, "1".to_string()),
            ]);
            stream
                .write_all(&encryptor.apply(&encode_frame(13, 7, 0, &message)))
                .await
                .expect("推送消息");

            let ack_raw = read_encrypted_packet(&mut stream, &mut decryptor).await;
            let ack = decode_frame(&ack_raw, false).expect("解析回执");
            assert_eq!((ack.service_id, ack.command_id), (13, 35));
            let properties = decode_properties(&ack.body).expect("回执属性");
            assert_eq!(text(properties.get(&1)), "uuid-1");
            assert_eq!(text(properties.get(&2)), "4321");
        });

        let session = NimSession::handshake_with_key(&address, login_packet.clone(), &public)
            .await
            .expect("客户端握手");
        let mut session = session
            .login(
                &login_packet,
                SERVICE_CHATROOM,
                CMD_CHATROOM_LOGIN,
                "聊天室",
            )
            .await
            .expect("客户端登录");

        let frame = session.read_frame().await.expect("接收消息");
        assert_eq!((frame.service_id, frame.command_id), (13, 7));
        let message = normalize_message(&frame.body).expect("解析消息");
        assert_eq!(message.text, "上播了");
        assert!(message.needs_ack);

        let ack = encode_properties(&[(1, message.uuid.clone()), (2, "4321".to_string())]);
        session
            .send_frame(SERVICE_CHATROOM, CMD_CHATROOM_ACK, &ack)
            .await
            .expect("发送回执");

        server.await.expect("服务端任务");
    }

    #[test]
    fn login_packet_matches_electron_fixture() {
        // 由 Electron 版 `buildChatroomLoginBody` + `makePacket`（同一组输入）生成，
        // 用来固定跨实现字节级一致：属性顺序、数字转字符串、varint、帧头。
        const BODY_HEX: &str = "020501076170702d6b6579020761636369642d31031030313233343536373839616263646566050434333231080131090301310405382e302e30060539323031320901310d103031323334353637383961626364656612076170702d6b6579130761636369642d311919636f6d2e706f636b65742e736e6834382e6163746976697479e8070a696d2d746f6b656e2d31";
        const FRAME_HEX: &str = "95010d02010000";
        let body = build_chatroom_login_body(
            "app-key",
            "accid-1",
            "im-token-1",
            "4321",
            "0123456789abcdef",
            92012,
            "com.pocket.snh48.activity",
        );
        assert_eq!(body, unhex(BODY_HEX));
        let frame = encode_frame(SERVICE_CHATROOM, CMD_CHATROOM_LOGIN, 1, &body);
        let prefix = unhex(FRAME_HEX);
        assert_eq!(&frame[..prefix.len()], &prefix[..]);
        assert_eq!(&frame[prefix.len()..], &body[..]);
    }

    fn unhex(input: &str) -> Vec<u8> {
        (0..input.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&input[index..index + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn message_normalizes_text_and_ack_flag() {
        let body = encode_properties(&[
            (1, "uuid-1".to_string()),
            (2, "0".to_string()),
            (3, "你好".to_string()),
            (7, "昵称".to_string()),
            (20, "1700000000000".to_string()),
            (21, "123".to_string()),
            (38, "1".to_string()),
        ]);
        let message = normalize_message(&body).unwrap();
        assert_eq!(message.kind, "text");
        assert_eq!(message.text, "你好");
        assert_eq!(message.msg_type, 0);
        assert!(message.needs_ack);
        assert_eq!(message.from, "123");
    }
}
