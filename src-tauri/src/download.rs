//! FFmpeg 在线下载（对照 Electron `src/main/ffmpeg/ffmpeg-download.ts`）。
//!
//! 与业务网络请求分开：下载目标是 npmmirror 的静态二进制，不在业务域名白名单内，
//! 因此这里用独立客户端并允许有限重定向；下载产物落在应用数据目录 `ffmpeg/`，
//! 校验通过后才由渲染层写入 `ffmpegDirectory` 配置。
//!
//! 失败不删除已存在且校验通过的二进制；半成品统一用 `.download` 临时名，
//! 进程中断不会留下看起来可用的坏档。

use std::path::PathBuf;
use std::time::Duration;

use flate2::read::GzDecoder;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_LENGTH};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::ffmpeg;

const VERSION: &str = "b6.0";
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
const PROGRESS_STEP: u64 = 512 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FfmpegDownloadProgress {
    pub received: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegSource {
    pub url: String,
    pub filename: &'static str,
}

/// 按平台/架构选择 ffmpeg-static 的预编译产物（与 Electron 版逐项一致）。
pub fn source() -> Result<FfmpegSource, String> {
    let base = format!("https://registry.npmmirror.com/-/binary/ffmpeg-static/{VERSION}");
    let name = match std::env::consts::OS {
        "macos" => match std::env::consts::ARCH {
            "aarch64" => "ffmpeg-darwin-arm64",
            "x86_64" => "ffmpeg-darwin-x64",
            other => return Err(format!("暂不支持的架构: {other}")),
        },
        "windows" => "ffmpeg-win32-x64",
        "linux" => match std::env::consts::ARCH {
            "aarch64" => "ffmpeg-linux-arm64",
            "x86_64" => "ffmpeg-linux-x64",
            other => return Err(format!("暂不支持的架构: {other}")),
        },
        other => return Err(format!("暂不支持的平台: {other}")),
    };
    Ok(FfmpegSource {
        url: format!("{base}/{name}.gz"),
        filename: if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        },
    })
}

/// 下载器必须挂在 `BridgeContext` 上：同一进程内防重入，并持有应用数据目录。
pub struct FfmpegDownloader {
    data_dir: PathBuf,
    lock: Mutex<()>,
}

impl FfmpegDownloader {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            lock: Mutex::new(()),
        }
    }

    pub fn target_dir(&self) -> PathBuf {
        self.data_dir.join("ffmpeg")
    }

    /// 下载 → 解压 → 校验；返回 ffmpeg 所在目录。
    /// 第二个并发调用会等第一个完成，然后直接复用已校验的产物。
    pub async fn download(&self, app: Option<AppHandle>) -> Result<String, String> {
        let _guard = self.lock.lock().await;
        let source = source()?;
        let dir = self.target_dir();
        let dir_text = dir.to_string_lossy().to_string();
        if ffmpeg::inspect(&dir_text).ok {
            eprintln!("[ffmpeg-download] 已存在且校验通过，跳过下载");
            return Ok(dir_text);
        }

        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|err| format!("创建 FFmpeg 目录失败: {err}"))?;

        let target = dir.join(source.filename);
        let gz_path = dir.join(format!("{}.download.gz", source.filename));
        let bin_path = dir.join(format!("{}.download", source.filename));

        let result = self
            .fetch_and_install(&source, &gz_path, &bin_path, &target, app)
            .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&gz_path).await;
            let _ = tokio::fs::remove_file(&bin_path).await;
        }
        result?;

        if !ffmpeg::inspect(&dir_text).ok {
            let _ = tokio::fs::remove_file(&target).await;
            return Err("FFmpeg 下载完成但校验未通过".to_string());
        }
        eprintln!("[ffmpeg-download] 完成: {}", target.to_string_lossy());
        Ok(dir_text)
    }

    async fn fetch_and_install(
        &self,
        source: &FfmpegSource,
        gz_path: &std::path::Path,
        bin_path: &std::path::Path,
        target: &std::path::Path,
        app: Option<AppHandle>,
    ) -> Result<(), String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(Duration::from_secs(8))
            .gzip(false)
            .build()
            .map_err(|err| format!("下载客户端初始化失败: {err}"))?;

        let response = client
            .get(&source.url)
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|err| format!("FFmpeg 下载请求失败: {err}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("FFmpeg 下载失败 HTTP {status}: {}", source.url));
        }
        let total = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);

        let mut file = tokio::fs::File::create(gz_path)
            .await
            .map_err(|err| format!("创建下载临时文件失败: {err}"))?;
        let mut response = response;
        let mut received = 0u64;
        let mut last_reported = 0u64;

        loop {
            let chunk = tokio::time::timeout(STALL_TIMEOUT, response.chunk())
                .await
                .map_err(|_| format!("下载超时：{}s 内无数据", STALL_TIMEOUT.as_secs()))?
                .map_err(|err| format!("FFmpeg 下载中断: {err}"))?;
            let Some(bytes) = chunk else { break };
            file.write_all(&bytes)
                .await
                .map_err(|err| format!("写入下载文件失败: {err}"))?;
            received += bytes.len() as u64;
            if received - last_reported >= PROGRESS_STEP {
                last_reported = received;
                emit_progress(app.as_ref(), received, total);
            }
        }
        file.flush()
            .await
            .map_err(|err| format!("刷新下载文件失败: {err}"))?;
        drop(file);

        if total > 0 && received < total {
            return Err(format!("下载不完整（{received}/{total} 字节）"));
        }
        emit_progress(app.as_ref(), received, total);

        // 解压是 CPU/磁盘密集操作，放到阻塞线程里，避免卡住异步运行时。
        let gz_owned = gz_path.to_path_buf();
        let bin_owned = bin_path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let input =
                std::fs::File::open(&gz_owned).map_err(|err| format!("打开下载文件失败: {err}"))?;
            let mut decoder = GzDecoder::new(input);
            let output = std::fs::File::create(&bin_owned)
                .map_err(|err| format!("创建 FFmpeg 临时文件失败: {err}"))?;
            let mut writer = output;
            std::io::copy(&mut decoder, &mut writer)
                .map_err(|err| format!("解压 FFmpeg 失败: {err}"))?;
            Ok::<(), String>(())
        })
        .await
        .map_err(|err| format!("解压任务失败: {err}"))??;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(bin_path, permissions)
                .map_err(|err| format!("设置 FFmpeg 可执行权限失败: {err}"))?;
        }

        std::fs::rename(bin_path, target).map_err(|err| format!("安装 FFmpeg 失败: {err}"))?;
        let _ = std::fs::remove_file(gz_path);
        Ok(())
    }
}

fn emit_progress(app: Option<&AppHandle>, received: u64, total: u64) {
    if let Some(app) = app {
        let _ = app.emit(
            "ffmpegDownloadProgress",
            FfmpegDownloadProgress { received, total },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    #[test]
    fn source_matches_electron_platform_matrix() {
        let source = source().unwrap();
        assert!(source.url.contains("/b6.0/"));
        assert!(!source.filename.is_empty());
        if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            assert!(source.url.ends_with("ffmpeg-darwin-arm64.gz"));
        }
    }

    #[test]
    fn gzip_payload_decompresses_to_binary() {
        let root = std::env::temp_dir().join(format!(
            "d48-ffmpeg-download-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let gz = root.join("ffmpeg.download.gz");
        let bin = root.join("ffmpeg.download");
        {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(b"fake-ffmpeg-binary").unwrap();
            let payload = encoder.finish().unwrap();
            std::fs::write(&gz, payload).unwrap();
        }
        let input = std::fs::File::open(&gz).unwrap();
        let mut decoder = GzDecoder::new(input);
        let output = std::fs::File::create(&bin).unwrap();
        let mut writer = output;
        std::io::copy(&mut decoder, &mut writer).unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), b"fake-ffmpeg-binary");
        let _ = std::fs::remove_dir_all(&root);
    }
}
