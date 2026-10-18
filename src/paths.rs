//! 路径归一化工具。
//!
//! **约束：存储里不出现绝对路径。** 需要绝对路径时，一律由「可执行文件目录 + 相对路径」
//! 现场拼出来。这样 SQLite 和 data/ 目录可以整包搬到别的机器或别的盘符，历史记录不会失效。
//!
//! 唯一无法相对化的情况是 Windows 跨盘（`D:\pics` 相对 `C:\app` 没有相对表示）——
//! 这时 [`relativize_to_exe`] 返回 `None`，调用方应存空串（表示「源路径未记录」），
//! 而不是把绝对路径写进库里。

use std::path::{Component, Path, PathBuf};

/// 可执行文件所在目录（绝对）。
pub fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let parent = exe.parent()?;
    std::path::absolute(parent).ok()
}

/// `path` 相对 `base` 的表示。
///
/// 返回 `None` 表示无法用相对路径表达（不同盘符 / 不同根）。
pub fn diff_paths(path: &Path, base: &Path) -> Option<PathBuf> {
    let p: Vec<Component<'_>> = path.components().collect();
    let b: Vec<Component<'_>> = base.components().collect();

    // 根必须一致：Windows 的盘符前缀，或 Unix 的 `/`
    let mut pi = 0usize;
    let mut bi = 0usize;
    match (p.first(), b.first()) {
        (Some(Component::Prefix(a)), Some(Component::Prefix(c))) if a == c => {
            pi = 1;
            bi = 1;
        }
        (Some(Component::RootDir), Some(Component::RootDir)) => {
            pi = 1;
            bi = 1;
        }
        (None, None) => {}
        _ => return None,
    }

    while pi < p.len() && bi < b.len() && p[pi] == b[bi] {
        pi += 1;
        bi += 1;
    }

    let mut out = PathBuf::new();
    for _ in bi..b.len() {
        out.push("..");
    }
    for c in &p[pi..] {
        out.push(c.as_os_str());
    }
    Some(out)
}

/// 绝对路径 → 可存的相对形式（统一 `/` 分隔）。
///
/// 已经是相对路径的输入按「相对可执行文件目录」理解后再原样返回。
pub fn relativize_to_exe(path: &Path) -> Option<String> {
    let base = exe_dir()?;
    let target = if path.is_absolute() {
        std::path::absolute(path).ok()?
    } else {
        std::path::absolute(base.join(path)).ok()?
    };
    diff_paths(&target, &base).map(|p| p.to_string_lossy().replace('\\', "/"))
}

/// 存储形式 → 绝对路径。空串表示「未记录」，返回 `None`。
pub fn abs_from_stored(root: &str) -> Option<PathBuf> {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return None;
    }
    let base = exe_dir()?;
    let path = Path::new(trimmed);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Some(normalize(&joined))
}

/// 词法规范化：消掉 `.` 与 `..`（不要求路径存在）。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 相对路径按可执行文件目录解析成绝对路径（本来就绝对的原样返回）。
pub fn absolute_from_exe(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match exe_dir() {
        Some(base) => normalize(&base.join(path)),
        None => normalize(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_paths_same_root() {
        let d = diff_paths(Path::new("/a/b/c"), Path::new("/a/b")).expect("同根应可相对化");
        assert_eq!(d, PathBuf::from("c"));
        let up = diff_paths(Path::new("/a/x"), Path::new("/a/b/c")).expect("应可相对化");
        assert_eq!(up, PathBuf::from("../../x"));
    }

    #[cfg(windows)]
    #[test]
    fn diff_paths_cross_drive_is_none() {
        assert!(diff_paths(Path::new(r"D:\pics"), Path::new(r"C:\app")).is_none());
        let same = diff_paths(Path::new(r"C:\app\data"), Path::new(r"C:\app")).expect("同盘");
        assert_eq!(same, PathBuf::from("data"));
    }

    #[test]
    fn abs_from_stored_normalizes_parent_dirs() {
        let p = abs_from_stored("../../../tmp/x").expect("应能拼回绝对路径");
        assert!(p.is_absolute());
        let text = p.to_string_lossy();
        assert!(!text.contains(".."), "拼回的绝对路径不应残留 ..: {text}");
    }

    #[test]
    fn abs_from_stored_rejects_empty() {
        assert!(abs_from_stored("").is_none());
        assert!(abs_from_stored("   ").is_none());
    }
}
