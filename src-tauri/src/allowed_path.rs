//! 本地路径白名单（对照 Electron `src/main/allowed-path.ts`）。
//!
//! `openPath` 只允许打开系统标准用户目录与用户配置的目录；判定在主进程完成，
//! 不信任渲染层传入的参数，与 `net::is_allowed_http_url` 是同类安全策略。

use std::path::{Component, Path, PathBuf};

/// `file_path` 是否落在任一 `allowed_roots` 之内。
///
/// 先规整相对路径与 `..`；比较必须到分隔符边界，否则 `Downloads_backup`
/// 会被当作在 `Downloads` 内。Windows 按大小写不敏感比较。
pub fn is_path_in_allowed_roots(
    file_path: &str,
    allowed_roots: &[PathBuf],
    case_insensitive: bool,
) -> bool {
    let normalize = |path: &Path| -> String {
        let text = path.to_string_lossy().to_string();
        if case_insensitive {
            text.to_lowercase()
        } else {
            text
        }
    };
    let target = normalize(&resolve(file_path));
    allowed_roots.iter().any(|root| {
        // 空根目录会被解析成当前工作目录，等于白送一个放行根，直接跳过。
        if root.as_os_str().is_empty() {
            return false;
        }
        let root_text = normalize(&resolve(&root.to_string_lossy()));
        if target == root_text {
            return true;
        }
        let prefix = if root_text.ends_with(std::path::MAIN_SEPARATOR) {
            root_text
        } else {
            format!("{root_text}{}", std::path::MAIN_SEPARATOR)
        };
        target.starts_with(&prefix)
    })
}

/// 绝对化 + 消掉 `.` / `..`（不触碰文件系统，路径不存在也能判定）。
fn resolve(path: &str) -> PathBuf {
    let raw = Path::new(path);
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(raw)
    };
    let mut output = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    output
}

/// 本平台是否按大小写不敏感比较（与 Node `process.platform === 'win32'` 同口径）。
pub fn case_insensitive() -> bool {
    crate::platform::path_case_insensitive()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn accepts_paths_inside_roots() {
        let roots = roots(&["/tmp/d48-home/Downloads"]);
        assert!(is_path_in_allowed_roots(
            "/tmp/d48-home/Downloads/a.png",
            &roots,
            false
        ));
        assert!(is_path_in_allowed_roots(
            "/tmp/d48-home/Downloads",
            &roots,
            false
        ));
    }

    #[test]
    fn rejects_sibling_prefix_and_escapes() {
        let roots = roots(&["/tmp/d48-home/Downloads"]);
        assert!(!is_path_in_allowed_roots(
            "/tmp/d48-home/Downloads_backup/a.png",
            &roots,
            false
        ));
        assert!(!is_path_in_allowed_roots(
            "/tmp/d48-home/Downloads/../secrets",
            &roots,
            false
        ));
        assert!(!is_path_in_allowed_roots("/etc/passwd", &roots, false));
    }

    #[test]
    fn empty_root_is_ignored() {
        let roots = roots(&[""]);
        assert!(!is_path_in_allowed_roots("/tmp/x", &roots, false));
    }
}
