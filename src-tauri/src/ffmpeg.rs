//! FFmpeg 环境判定（对照 Electron `src/main/ffmpeg/ffmpeg-process.ts`）。
//!
//! FFmpeg 由用户自己配置目录，不随应用打包。主进程是唯一判定方：
//! 播放转流、未来的下载/录制都走这里，避免各处 `exists` 口径不一致。
//! `D48_FFMPEG_DIR` 仅作为探针/开发环境的显式覆盖，生产用户仍以设置页配置为准。

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::database::Database;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegStatus {
    pub ok: bool,
    /// `unset` / `missing-dir` / `missing-binary`；ok 时恒为 null。
    pub problem: Option<String>,
    pub directory: String,
    /// 校验通过时的可执行文件完整路径；未通过时为空串。
    pub binary_path: String,
    /// 生效来源；当前只支持用户配置/下载目录，系统探测留给 Linux 包管理器场景。
    pub source: String,
}

pub fn ffmpeg_dir(db: &Database) -> String {
    let configured = db
        .config("ffmpegDirectory")
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    // 仅供无 GUI 探针 / 开发环境验证链路使用；设置页写入的配置永远优先。
    std::env::var("D48_FFMPEG_DIR")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
}

pub fn binary_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

pub fn binary_path(directory: &str) -> PathBuf {
    Path::new(directory).join(binary_name())
}

pub fn inspect(directory: &str) -> FfmpegStatus {
    let directory = directory.trim();
    if directory.is_empty() {
        return FfmpegStatus {
            ok: false,
            problem: Some("unset".to_string()),
            directory: String::new(),
            binary_path: String::new(),
            source: "directory".to_string(),
        };
    }
    if !Path::new(directory).exists() {
        return FfmpegStatus {
            ok: false,
            problem: Some("missing-dir".to_string()),
            directory: directory.to_string(),
            binary_path: String::new(),
            source: "directory".to_string(),
        };
    }
    let binary = binary_path(directory);
    if !binary.exists() {
        return FfmpegStatus {
            ok: false,
            problem: Some("missing-binary".to_string()),
            directory: directory.to_string(),
            binary_path: String::new(),
            source: "directory".to_string(),
        };
    }
    FfmpegStatus {
        ok: true,
        problem: None,
        directory: directory.to_string(),
        binary_path: binary.to_string_lossy().to_string(),
        source: "directory".to_string(),
    }
}

fn problem_message(problem: Option<&str>) -> &'static str {
    match problem {
        Some("unset") => "尚未设置 ffmpeg 目录",
        Some("missing-dir") => "ffmpeg 目录不存在",
        _ => "ffmpeg 二进制文件不存在",
    }
}

pub fn resolve(directory: &str) -> Result<PathBuf, String> {
    let status = inspect(directory);
    if status.ok {
        return Ok(PathBuf::from(status.binary_path));
    }
    Err(problem_message(status.problem.as_deref()).to_string())
}

pub fn status(db: &Database, directory: Option<&str>) -> FfmpegStatus {
    inspect(directory.unwrap_or(&ffmpeg_dir(db)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_directory_has_problem_unset() {
        let status = inspect("   ");
        assert!(!status.ok);
        assert_eq!(status.problem.as_deref(), Some("unset"));
        assert_eq!(status.directory, "");
        assert_eq!(status.source, "directory");
    }

    #[test]
    fn missing_directory_and_binary_are_distinguished() {
        let root = std::env::temp_dir().join(format!(
            "d48-ffmpeg-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let missing = inspect(&root.to_string_lossy());
        assert_eq!(missing.problem.as_deref(), Some("missing-dir"));

        std::fs::create_dir_all(&root).unwrap();
        let no_binary = inspect(&root.to_string_lossy());
        assert_eq!(no_binary.problem.as_deref(), Some("missing-binary"));

        let binary = binary_path(&root.to_string_lossy());
        std::fs::write(&binary, b"").unwrap();
        let ok = inspect(&root.to_string_lossy());
        assert!(ok.ok, "{ok:?}");
        assert_eq!(ok.binary_path, binary.to_string_lossy());
        assert_eq!(ok.source, "directory");
        assert!(resolve(&root.to_string_lossy()).is_ok());

        let _ = std::fs::remove_dir_all(&root);
    }
}
