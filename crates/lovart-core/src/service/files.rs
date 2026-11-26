//! 目录列举 —— 业务层唯一的「读目录 → 过滤扩展名 → 排序」实现（`docs/plan/20` §2.5 S2）。
//!
//! 消费者：`list_inbox`（`*.jsonl` 提示词）与 `list_library_images`（图片）。
//! 它只回答「目录里有哪些符合条件的文件」，**不读内容、不校验格式** ——
//! 校验（`inspect_jsonl`）与组形状（`{path,name}`）各留各的。

use std::path::{Path, PathBuf};

/// 列出 `dir` 下的**直属**文件（不递归），扩展名属于 `exts`，按路径排序。
///
/// - 目录不存在 ⇒ `Ok(vec![])`：调用方语境里"还没建"是正常状态；
/// - 读目录失败 ⇒ `Err(原因)`：**读不到 ≠ 没有**（折成空列表，界面就会说"这里没有文件"，
///   而实际是读不了 —— 本仓头号伪装型债）；
/// - 扩展名比较**大小写不敏感**（`IMG.PNG` 也算图片，Windows 上很常见）；
/// - 单个目录项取不出来（多为权限）跳过：它不影响其余项，而"整个目录读不了"已在上一条抛掉。
pub fn list_files_in(dir: &Path, exts: &[&str]) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("读不了目录（{}）：{error}", dir.display())),
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
                .is_some_and(|ext| exts.iter().any(|want| ext == *want))
        })
        .collect();
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lovart-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").expect("写文件");
    }

    fn names(paths: Vec<PathBuf>) -> Vec<String> {
        paths
            .into_iter()
            .map(|path| path.file_name().unwrap_or_default().to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn 空目录给空列表() {
        let dir = temp_dir("empty");
        assert!(list_files_in(&dir, &["jsonl"]).expect("空目录不算失败").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 目录不存在当空_读不了才报错() {
        let dir = temp_dir("absent");
        let gone = dir.join("never");
        assert!(list_files_in(&gone, &["jsonl"]).expect("不存在 ⇒ 空").is_empty());

        // 目录位置被一个**文件**占着 ⇒ 读不了，必须报错（不能折成空）。
        let blocked = dir.join("not-a-dir");
        std::fs::write(&blocked, b"x").expect("写占位文件");
        assert!(list_files_in(&blocked, &["jsonl"]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 按扩展名过滤且大小写不敏感() {
        let dir = temp_dir("exts");
        touch(&dir, "b.jsonl");
        touch(&dir, "A.JSONL");
        touch(&dir, "note.txt");
        let listed = list_files_in(&dir, &["jsonl"]).expect("应能列目录");
        assert_eq!(names(listed), vec!["A.JSONL".to_string(), "b.jsonl".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 按路径排序且不递归子目录() {
        let dir = temp_dir("sort");
        touch(&dir, "c.png");
        touch(&dir, "a.png");
        touch(&dir, "b.png");
        let nested = dir.join("sub");
        std::fs::create_dir_all(&nested).expect("建子目录");
        std::fs::write(nested.join("inner.png"), b"x").expect("写子目录文件");

        let listed = list_files_in(&dir, &["png"]).expect("应能列目录");
        assert_eq!(
            names(listed),
            vec!["a.png".to_string(), "b.png".to_string(), "c.png".to_string()],
            "按文件名排序，且只列直属文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
