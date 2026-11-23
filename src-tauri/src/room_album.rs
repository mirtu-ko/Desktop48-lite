//! 房间相册批量下载（对照 Electron `main/pocket48/room-album-download.ts`）。
//!
//! 目录选择只在 Rust 侧弹**一次**，随后逐张请求并写盘——不走渲染层的 `<a download>`，
//! 那样每张图都会弹一次系统「另存为」。
//!
//! 收窄与 Electron 同口径：
//! - 单张图片 40MB 上限、30s 超时、单批最多 1000 张
//! - 图片地址必须过出口白名单（`is_allowed_http_url`）
//! - 响应 Content-Type 是 `text/*` / JSON / HTML 时判为「不是图片」
//! - `filter_small` 时用小图阈值过滤（尺寸已知直接判，未知则下载后判）
//! - 文件名冲突用 `wx`（O_EXCL）避让，最多试 10000 次

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::app_log::log_line;

/// 单张图片上限：防止异常响应无限占内存
const MAX_IMAGE_BYTES: usize = 40 * 1024 * 1024;
/// 单张超时
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 单批最多张数：超出部分记为失败（与 Electron 一致）
const MAX_IMAGES: usize = 1000;
/// 小图阈值（短边小于该值即视为小图），与 `common/image-download.ts` 同值
const SMALL_IMAGE_MIN_SIDE: u32 = 200;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumSource {
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub msg_time: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AlbumProgress {
    pub processed: usize,
    pub total: usize,
    pub downloaded: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AlbumResult {
    pub canceled: bool,
    pub directory: String,
    pub downloaded: usize,
    pub skipped: usize,
    pub failed: usize,
}

struct FetchedImage {
    data: Vec<u8>,
    content_type: String,
}

async fn fetch_image(http: &reqwest::Client, url: &str) -> Result<FetchedImage, String> {
    let response = http
        .get(url)
        .timeout(REQUEST_TIMEOUT)
        .header(
            reqwest::header::ACCEPT,
            "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
        )
        .send()
        .await
        .map_err(|err| format!("请求失败: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    if let Some(length) = response.content_length()
        && length as usize > MAX_IMAGE_BYTES
    {
        return Err(format!("图片超过 {}MB 上限", MAX_IMAGE_BYTES / 1024 / 1024));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let lower = content_type.to_ascii_lowercase();
    if lower.starts_with("text/") || lower.contains("json") || lower.contains("html") {
        return Err(format!("响应不是图片: {content_type}"));
    }
    let data = response
        .bytes()
        .await
        .map_err(|err| format!("读取响应失败: {err}"))?
        .to_vec();
    if data.is_empty() {
        return Err("图片响应为空".to_string());
    }
    if data.len() > MAX_IMAGE_BYTES {
        return Err(format!("图片超过 {}MB 上限", MAX_IMAGE_BYTES / 1024 / 1024));
    }
    Ok(FetchedImage { data, content_type })
}

/// 从图片字节里读尺寸：支持 PNG / JPEG / GIF / WebP 的头部解析。
/// 读不出（格式不支持）返回 `None`，调用方按「尺寸未知」处理，不做小图过滤。
fn image_size(data: &[u8]) -> Option<(u32, u32)> {
    // PNG：签名 + IHDR 里的宽高（大端 u32）
    if data.len() > 24 && data.starts_with(&[0x89, b'P', b'N', b'G']) {
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((width, height));
    }
    // GIF：`GIF8?a` + 小端 u16 宽高
    if data.len() > 10 && data.starts_with(b"GIF8") {
        let width = u16::from_le_bytes([data[6], data[7]]) as u32;
        let height = u16::from_le_bytes([data[8], data[9]]) as u32;
        return Some((width, height));
    }
    // JPEG：扫描 SOF0/SOF2 段
    if data.len() > 4 && data[0] == 0xFF && data[1] == 0xD8 {
        let mut index = 2usize;
        while index + 9 < data.len() {
            if data[index] != 0xFF {
                index += 1;
                continue;
            }
            let marker = data[index + 1];
            // SOF0..SOF3 / SOF5..SOF7 / SOF9..SOF11 都带尺寸
            if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB) {
                let height = u16::from_be_bytes([data[index + 5], data[index + 6]]) as u32;
                let width = u16::from_be_bytes([data[index + 7], data[index + 8]]) as u32;
                return Some((width, height));
            }
            let length = u16::from_be_bytes([data[index + 2], data[index + 3]]) as usize;
            if length < 2 {
                break;
            }
            index += 2 + length;
        }
        return None;
    }
    // WebP（VP8X）：`RIFF....WEBP` + 24 位宽高
    if data.len() > 30
        && data.starts_with(b"RIFF")
        && &data[8..12] == b"WEBP"
        && &data[12..16] == b"VP8X"
    {
        let width = 1 + (data[24] as u32 | (data[25] as u32) << 8 | (data[26] as u32) << 16);
        let height = 1 + (data[27] as u32 | (data[28] as u32) << 8 | (data[29] as u32) << 16);
        return Some((width, height));
    }
    None
}

/// 短边小于阈值即小图；尺寸未知（0 / None）时不算小图。
fn is_small_image(width: Option<u32>, height: Option<u32>) -> bool {
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => w.min(h) < SMALL_IMAGE_MIN_SIDE,
        _ => false,
    }
}

/// 以独占模式写入并自动避让重名；返回最终文件路径。
async fn write_unique_file(
    directory: &Path,
    preferred_name: &str,
    data: &[u8],
) -> Result<PathBuf, String> {
    let path = Path::new(preferred_name);
    let stem = path
        .file_stem()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    for index in 0..10_000 {
        let name = if index == 0 {
            preferred_name.to_string()
        } else {
            format!("{stem}-{index}{extension}")
        };
        let target = directory.join(&name);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .await
        {
            Ok(mut file) => {
                file.write_all(data)
                    .await
                    .map_err(|err| format!("写入失败: {err}"))?;
                return Ok(target);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("写入失败: {err}")),
        }
    }
    Err(format!("同名文件过多: {preferred_name}"))
}

/// 文件名合成：`昵称_20261022-215544_1200x1600_01.jpg`（与 `buildAlbumFileName` 同构）。
pub fn build_album_file_name(
    source: &AlbumSource,
    width: Option<u32>,
    height: Option<u32>,
    sequence: usize,
    content_type: &str,
) -> String {
    let nickname = sanitize(source.nickname.as_deref().unwrap_or(""));
    let time = format_time(source.msg_time.unwrap_or(0));
    let size = match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => format!("_{w}x{h}"),
        _ => String::new(),
    };
    let stem = format!(
        "{}{}{}",
        if nickname.is_empty() {
            String::new()
        } else {
            format!("{nickname}_")
        },
        time,
        size
    );
    // 无元数据时回落 URL 原名（不凭空造时间戳名）
    let stem = if nickname.is_empty() && source.msg_time.unwrap_or(0) <= 0 && size.is_empty() {
        url_file_stem(&source.url)
    } else {
        stem
    };
    let extension = pick_extension(&source.url, content_type);
    let sequence = format!("{:02}", sequence.max(1));
    if extension.is_empty() {
        format!("{stem}_{sequence}")
    } else {
        format!("{stem}_{sequence}.{extension}")
    }
}

/// 中文 / 特殊字符替换为 `_`，与 `common/image-download.ts` 的 `sanitizeFileName` 同口径。
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    cleaned.trim_matches('_').to_string()
}

/// 毫秒时间戳 → `20261022-215544`（本地时区近似：按 UTC+8 折算，与用户预期一致）。
fn format_time(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let seconds = (ms / 1000 + 8 * 3600) as u64;
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Howard Hinnant 的 days → (y, m, d)，不引入时间库。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn url_file_stem(url: &str) -> String {
    let last = url.split(['?', '#']).next().unwrap_or(url);
    let name = last.rsplit('/').next().unwrap_or("");
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
    let cleaned = sanitize(stem);
    if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned
    }
}

/// 扩展名：优先 URL 后缀，其次 Content-Type，都取不到留空。
fn pick_extension(url: &str, content_type: &str) -> String {
    let last = url.split(['?', '#']).next().unwrap_or(url);
    if let Some((_, extension)) = last.rsplit_once('.') {
        let extension = extension.to_ascii_lowercase();
        if (2..=5).contains(&extension.len()) && extension.chars().all(|ch| ch.is_alphanumeric()) {
            return extension;
        }
    }
    let lower = content_type.to_ascii_lowercase();
    for (needle, extension) in [
        ("jpeg", "jpg"),
        ("jpg", "jpg"),
        ("png", "png"),
        ("gif", "gif"),
        ("webp", "webp"),
        ("avif", "avif"),
        ("bmp", "bmp"),
    ] {
        if lower.contains(needle) {
            return extension.to_string();
        }
    }
    String::new()
}

/// 执行一次批量下载。`directory = None` 表示用户取消。
#[allow(clippy::too_many_arguments)]
pub async fn run_download<F>(
    http: &reqwest::Client,
    sources: &[AlbumSource],
    filter_small: bool,
    directory: Option<PathBuf>,
    mut on_progress: F,
) -> AlbumResult
where
    F: FnMut(AlbumProgress),
{
    let Some(directory) = directory else {
        return AlbumResult {
            canceled: true,
            ..Default::default()
        };
    };
    let selected = &sources[..sources.len().min(MAX_IMAGES)];
    let mut result = AlbumResult {
        canceled: false,
        directory: directory.to_string_lossy().to_string(),
        downloaded: 0,
        skipped: 0,
        failed: sources.len().saturating_sub(selected.len()),
    };
    for (index, source) in selected.iter().enumerate() {
        let processed = index + 1;
        // 地址白名单：不校验就等于给渲染层开了一条任意下载旁路
        if !crate::net::is_allowed_http_url(&source.url) {
            result.failed += 1;
            log_line(&format!(
                "[room-album-download] 跳过非白名单图片: {}",
                source.url.chars().take(80).collect::<String>()
            ));
        } else if filter_small && is_small_image(source.width, source.height) {
            // 尺寸已知：不必下载
            result.skipped += 1;
        } else {
            match fetch_image(http, &source.url).await {
                Ok(image) => {
                    let (width, height) = match (source.width, source.height) {
                        (Some(w), Some(h)) if w > 0 && h > 0 => (Some(w), Some(h)),
                        _ => image_size(&image.data)
                            .map(|(w, h)| (Some(w), Some(h)))
                            .unwrap_or((None, None)),
                    };
                    if filter_small && is_small_image(width, height) {
                        result.skipped += 1;
                    } else {
                        let filename = build_album_file_name(
                            source,
                            width,
                            height,
                            index + 1,
                            &image.content_type,
                        );
                        match write_unique_file(&directory, &filename, &image.data).await {
                            Ok(_) => result.downloaded += 1,
                            Err(err) => {
                                result.failed += 1;
                                log_line(&format!(
                                    "[room-album-download] 写入失败 {filename}: {err}"
                                ));
                            }
                        }
                    }
                }
                Err(err) => {
                    result.failed += 1;
                    log_line(&format!(
                        "[room-album-download] 图片下载失败: {} {err}",
                        source.url.chars().take(80).collect::<String>()
                    ));
                }
            }
        }
        on_progress(AlbumProgress {
            processed,
            total: sources.len(),
            downloaded: result.downloaded,
            skipped: result.skipped,
            failed: result.failed,
        });
    }

    log_line(&format!(
        "[room-album-download] 完成：下载 {}，过滤 {}，失败 {}",
        result.downloaded, result.skipped, result.failed
    ));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(url: &str, nickname: &str, msg_time: i64) -> AlbumSource {
        AlbumSource {
            url: url.to_string(),
            width: Some(1200),
            height: Some(1600),
            nickname: Some(nickname.to_string()),
            msg_time: Some(msg_time),
        }
    }

    #[test]
    fn file_name_matches_electron_layout() {
        let item = source("https://x/abc.jpg", "粉丝甲", 1795000000000);
        let name = build_album_file_name(&item, Some(1200), Some(1600), 1, "image/jpeg");
        assert!(name.starts_with("粉丝甲_"), "{name}");
        assert!(name.ends_with("_1200x1600_01.jpg"), "{name}");
    }

    #[test]
    fn file_name_falls_back_to_url_stem() {
        let item = AlbumSource {
            url: "https://x/photo_2026.png?token=1".to_string(),
            width: None,
            height: None,
            nickname: None,
            msg_time: None,
        };
        let name = build_album_file_name(&item, None, None, 2, "");
        assert_eq!(name, "photo_2026_02.png");
    }

    #[test]
    fn small_image_threshold() {
        assert!(is_small_image(Some(100), Some(100)));
        assert!(!is_small_image(Some(400), Some(400)));
        // 尺寸未知不算小图（避免误过滤）
        assert!(!is_small_image(None, None));
        assert!(!is_small_image(Some(0), Some(0)));
    }

    #[test]
    fn png_size_is_parsed() {
        let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        data.extend_from_slice(&1200u32.to_be_bytes());
        data.extend_from_slice(&1600u32.to_be_bytes());
        data.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(image_size(&data), Some((1200, 1600)));
    }

    #[test]
    fn gif_size_is_parsed() {
        let mut data = b"GIF89a".to_vec();
        data.extend_from_slice(&320u16.to_le_bytes());
        data.extend_from_slice(&240u16.to_le_bytes());
        data.extend_from_slice(&[0, 0, 0]);
        assert_eq!(image_size(&data), Some((320, 240)));
    }

    #[test]
    fn unknown_format_returns_none() {
        assert_eq!(image_size(b"not an image"), None);
        assert_eq!(image_size(&[]), None);
    }

    #[test]
    fn extension_prefers_url_then_content_type() {
        assert_eq!(pick_extension("https://x/a.JPG?t=1", "image/png"), "jpg");
        assert_eq!(pick_extension("https://x/noext", "image/webp"), "webp");
        assert_eq!(pick_extension("https://x/noext", ""), "");
    }

    #[tokio::test]
    async fn disallowed_url_is_counted_as_failed() {
        let http = reqwest::Client::new();
        let items = vec![AlbumSource {
            url: "https://evil.example.com/a.jpg".to_string(),
            width: None,
            height: None,
            nickname: None,
            msg_time: None,
        }];
        let mut progress = Vec::new();
        let result = run_download(&http, &items, false, Some(std::env::temp_dir()), |p| {
            progress.push(p)
        })
        .await;
        assert_eq!(result.failed, 1);
        assert_eq!(result.downloaded, 0);
        assert_eq!(progress.len(), 1);
        assert_eq!(progress[0].processed, 1);
    }

    #[tokio::test]
    async fn canceled_when_no_directory() {
        let http = reqwest::Client::new();
        let result = run_download(&http, &[], false, None, |_| {}).await;
        assert!(result.canceled);
        assert_eq!(result.downloaded, 0);
    }
}
