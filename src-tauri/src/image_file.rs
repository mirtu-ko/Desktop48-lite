//! 图片写入剪贴板 / 磁盘前的收窄（对照 Electron `src/main/image-file.ts`）。
//!
//! 渲染层传来的 data URL 与文件名都不当可信输入：只接受结构完整的 PNG
//! （签名 + IHDR 开头 + IEND 收尾），避免任意字节被当图片写出。

use std::path::Path;

const PNG_DATA_URL_PREFIX: &str = "data:image/png;base64,";
const PNG_DATA_URL_MAX_CHARS: usize = 16 * 1024 * 1024;
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const PNG_IHDR_PREFIX: [u8; 8] = [0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52];
const PNG_IEND: [u8; 12] = [0, 0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82];
const PNG_FORMAT_ERROR: &str = "图片数据格式不正确";

/// 严格解析 `data:image/png;base64,...`：只接受结构完整的 PNG。
pub fn decode_png_data_url(data_url: &str) -> Result<Vec<u8>, String> {
    let Some(encoded) = data_url.strip_prefix(PNG_DATA_URL_PREFIX) else {
        return Err(PNG_FORMAT_ERROR.to_string());
    };
    decode_png_base64(encoded)
}

/// 校验并解码 PNG 的 base64 正文（不含 `data:` 前缀）。
fn decode_png_base64(encoded: &str) -> Result<Vec<u8>, String> {
    if encoded.is_empty()
        || encoded.len() > PNG_DATA_URL_MAX_CHARS
        || !encoded.len().is_multiple_of(4)
        || !encoded.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/' || byte == b'='
        })
    {
        return Err(PNG_FORMAT_ERROR.to_string());
    }
    // base64 crate 的 strict 模式不接受尾部非法 `=`，与 Electron 正则 `={0,2}$` 同口径。
    use base64::Engine as _;
    let buffer = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| PNG_FORMAT_ERROR.to_string())?;
    let signature_end = PNG_SIGNATURE.len();
    if buffer.len() < signature_end + PNG_IHDR_PREFIX.len() + PNG_IEND.len()
        || buffer[..signature_end] != PNG_SIGNATURE
        || buffer[signature_end..signature_end + PNG_IHDR_PREFIX.len()] != PNG_IHDR_PREFIX
        || buffer[buffer.len() - PNG_IEND.len()..] != PNG_IEND
    {
        return Err(PNG_FORMAT_ERROR.to_string());
    }
    Ok(buffer)
}

/// PNG 字节 → 剪贴板需要的 RGBA 位图。
///
/// 只有在「生成图片」这一侧才需要解码像素（如生日贺卡），所以解码交给 `tauri` 的
/// `image-png` 特性（`Image::from_bytes`），不额外引入图像库。
pub fn decode_png_to_rgba(bytes: &[u8]) -> Result<tauri::image::Image<'static>, String> {
    let image =
        tauri::image::Image::from_bytes(bytes).map_err(|err| format!("解析 PNG 失败: {err}"))?;
    let rgba = image.rgba().to_vec();
    Ok(tauri::image::Image::new_owned(
        rgba,
        image.width(),
        image.height(),
    ))
}

/// 文件名一律补成 `.png`：保存对话框的过滤器只是 UI 提示，用户仍可输入任意后缀。
pub fn ensure_png_extension(file_path: &str) -> String {
    if file_path.to_lowercase().ends_with(".png") {
        file_path.to_string()
    } else {
        format!("{file_path}.png")
    }
}

/// 建议名只作对话框默认值：剥目录 + 白名单替换 + 补后缀。
pub fn safe_png_file_name(suggested_name: &str) -> String {
    let base = Path::new(suggested_name)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned = base
        .chars()
        .map(|ch| {
            let allowed = ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.'
                // 中日韩统一表意文字（与原实现 `\u4E00-\u9FA5` 同范围）
                || ('\u{4E00}'..='\u{9FA5}').contains(&ch);
            if allowed { ch } else { '_' }
        })
        .collect::<String>();
    if cleaned.trim_matches('.').is_empty() {
        "birthday-card.png".to_string()
    } else {
        ensure_png_extension(&cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小可用的 PNG：8 字节签名 + IHDR 段头 + IEND（结构校验只需要这三段）。
    fn tiny_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&PNG_SIGNATURE);
        bytes.extend_from_slice(&PNG_IHDR_PREFIX);
        bytes.extend_from_slice(&PNG_IEND);
        bytes
    }

    #[test]
    fn decodes_structurally_valid_png() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(tiny_png());
        let url = format!("{PNG_DATA_URL_PREFIX}{encoded}");
        assert_eq!(decode_png_data_url(&url).unwrap(), tiny_png());
    }

    #[test]
    fn rejects_non_png_payloads() {
        assert!(decode_png_data_url("data:image/jpeg;base64,AAAA").is_err());
        assert!(decode_png_data_url("data:image/png;base64,QUJD").is_err());
        assert!(decode_png_data_url("data:image/png;base64,!!!").is_err());
        assert!(decode_png_data_url("").is_err());
    }

    /// 1×1 RGBA PNG（真实可解码，不只是结构校验）。
    fn one_pixel_png() -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
            )
            .unwrap()
    }

    #[test]
    fn decodes_png_to_rgba_for_clipboard() {
        let image = decode_png_to_rgba(&one_pixel_png()).unwrap();
        assert_eq!(image.width(), 1);
        assert_eq!(image.height(), 1);
        assert_eq!(image.rgba().len(), 4, "RGBA 每像素 4 字节");
    }

    #[test]
    fn undecodable_png_is_reported() {
        // 结构校验能过、但像素数据不完整：剪贴板路径必须明确报错而不是 panic
        assert!(decode_png_to_rgba(&tiny_png()).is_err());
        assert!(decode_png_to_rgba(b"not a png").is_err());
    }

    #[test]
    fn file_names_are_narrowed() {
        assert_eq!(safe_png_file_name("a/evil.png"), "evil.png");
        // 反斜杠在 Windows 上是分隔符、在 POSIX 上只是普通字符：
        // 前者剥掉 `..\` 只剩 `evil.png`，后者清洗成 `.._evil.png`。两种都不含分隔符，
        // 都满足「不能穿越目录」，所以按平台分别断言预期值。
        #[cfg(windows)]
        assert_eq!(safe_png_file_name("..\\evil.png"), "evil.png");
        #[cfg(not(windows))]
        assert_eq!(safe_png_file_name("..\\evil.png"), ".._evil.png");
        // 跨平台都必须做到「结果里没有路径分隔符」
        for name in ["a/evil.png", "..\\evil.png", "../../etc/passwd"] {
            let narrowed = safe_png_file_name(name);
            assert!(
                !narrowed.contains('/') && !narrowed.contains('\\'),
                "{name} → {narrowed}"
            );
        }
        assert_eq!(safe_png_file_name("a b:c"), "a_b_c.png");
        assert_eq!(safe_png_file_name("..."), "birthday-card.png");
        assert_eq!(safe_png_file_name(""), "birthday-card.png");
        assert_eq!(ensure_png_extension("x.PNG"), "x.PNG");
        assert_eq!(ensure_png_extension("x"), "x.png");
    }
}
