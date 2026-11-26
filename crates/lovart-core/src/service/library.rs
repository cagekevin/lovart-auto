//! 素材库 = `data/library/` **这个目录本身**（`docs/plan/20`）。
//!
//! ── 判据 ──────────────────────────────────────────────────────────────
//! **库 = 目录，不建索引文件。** 每次打开都扫目录 ⇒ 文件管理器里加/删的图，
//! 下一次打开就出现/消失。加 `index.json` 就是第二个真相：图被手动删了索引还在，
//! 界面会说谎（ADR-0003 / 0008）。
//!
//! 三个动作都只是"目录里多/少一个文件"，所以命令只有三条、实现只有这一处：
//! `list`（扫目录）· `send`（复制进目录）· `remove`（删目录里的文件）。
//! 删除**不查引用**：编辑区里那张会变成破图，用户一眼看得见（ADR-0010）。

use std::path::{Component, Path};

use crate::engine::refs::{materialize_image, ImageSource, IMAGE_EXTS};
use crate::service::files::list_files_in;

/// 网格里的一格：`path` 给 `<img src>`，`name` 是文件名（删除与提示都用它）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LibraryImage {
    pub path: String,
    pub name: String,
}

/// 失败原因（判别位不折成值，ADR-0003）。前端只翻译这两个词。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryError {
    /// 要删的文件不在目录里（多半是刚被文件管理器删掉 / 重开面板前就没了）。
    Missing(String),
    /// 读目录 / 复制 / 删文件失败 —— `detail` 带原文，不吞。
    Storage(String),
}

impl LibraryError {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Missing(_) => "missing",
            Self::Storage(_) => "storage",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::Missing(file) => format!("missing: {file}"),
            Self::Storage(message) => format!("storage: {message}"),
        }
    }
}

/// 列库里的全部图片（按文件名排序；不递归子目录）。
pub fn list_images(dir: &Path) -> Result<Vec<LibraryImage>, LibraryError> {
    let paths = list_files_in(dir, IMAGE_EXTS).map_err(LibraryError::Storage)?;
    Ok(paths
        .into_iter()
        .map(|path| LibraryImage {
            name: file_name_of(&path),
            path: path.display().to_string(),
        })
        .collect())
}

/// 把一张图**复制**进库里，返回它的文件名。
///
/// 落盘原语与预设共用（`engine::refs::materialize_image`）⇒ 同一个库目录里
/// 不会因为"来源是剪贴板还是文件选择器"出现两种文件名。
pub fn send(dir: &Path, source: &ImageSource) -> Result<String, LibraryError> {
    let path = materialize_image(dir, source).map_err(LibraryError::Storage)?;
    Ok(file_name_of(Path::new(&path)))
}

/// 删库里的一张图（**只删文件**：不查引用、不动编辑区、不碰预设）。
pub fn remove(dir: &Path, file: &str) -> Result<(), LibraryError> {
    // 只接受**文件名**：带目录 / `..` 的输入不是这个命令的形状（前端永不传路径）。
    if !is_plain_name(file) {
        return Err(LibraryError::Storage(format!("不是文件名：{file}")));
    }
    let path = dir.join(file);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(LibraryError::Missing(file.to_string()))
        }
        Err(error) => Err(LibraryError::Storage(format!("删不掉 {}：{error}", path.display()))),
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 一个**名字**（不是路径）：只有一段 `Normal` 分量。
fn is_plain_name(file: &str) -> bool {
    let mut parts = Path::new(file).components();
    matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const DATA_PNG: &str = "data:image/png;base64,AAAA";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lovart-library-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn data_source() -> ImageSource {
        ImageSource::Data { value: DATA_PNG.to_string() }
    }

    fn names(images: &[LibraryImage]) -> Vec<String> {
        images.iter().map(|image| image.name.clone()).collect()
    }

    #[test]
    fn 空目录给空列表() {
        let dir = temp_dir("empty");
        assert!(list_images(&dir).expect("空目录不算失败").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 只列图片且同名不同扩展算两条() {
        let dir = temp_dir("mixed");
        std::fs::write(dir.join("a.txt"), b"x").expect("写文本");
        std::fs::write(dir.join("pic.png"), b"x").expect("写 png");
        std::fs::write(dir.join("pic.webp"), b"x").expect("写 webp");

        let listed = list_images(&dir).expect("应能列目录");
        assert_eq!(names(&listed), vec!["pic.png".to_string(), "pic.webp".to_string()]);
        assert!(listed[0].path.ends_with("pic.png"), "path 必须是绝对路径：{}", listed[0].path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 发送同内容两次同一个文件_且文件名带原名与哈希() {
        let dir = temp_dir("send");
        let first = send(&dir, &data_source()).expect("第一次应落盘");
        let second = send(&dir, &data_source()).expect("第二次应落盘");
        assert_eq!(first, second, "同内容不应重复写盘");
        assert!(first.starts_with("图片-") && first.ends_with(".png"), "命名规则不对：{first}");
        assert_eq!(list_images(&dir).expect("应能列目录").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 发送路径来源后清单里就有它() {
        let dir = temp_dir("path-send");
        let src_dir = temp_dir("path-send-src");
        let src = src_dir.join("shot.png");
        std::fs::write(&src, b"\0\0\0").expect("写源图");
        let source = ImageSource::Path { value: src.display().to_string() };

        let name = send(&dir, &source).expect("路径来源应落盘");
        assert!(name.starts_with("shot-"), "文件名应是原名+哈希：{name}");
        assert_eq!(names(&list_images(&dir).expect("应能列目录")), vec![name]);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&src_dir);
    }

    #[test]
    fn 删除存在文件后清单里没有它() {
        let dir = temp_dir("remove");
        let name = send(&dir, &data_source()).expect("应落盘");
        remove(&dir, &name).expect("应能删");
        assert!(list_images(&dir).expect("应能列目录").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 删除不存在的文件报_missing() {
        let dir = temp_dir("remove-missing");
        let error = remove(&dir, "没有这张.png").expect_err("不该静默成功");
        assert_eq!(error.reason(), "missing");
        assert!(error.detail().contains("没有这张.png"), "原因里要带文件名：{}", error.detail());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 删除只接受文件名不接受路径() {
        let dir = temp_dir("remove-path");
        std::fs::write(dir.join("keep.png"), b"x").expect("写文件");
        let outside = dir.parent().map(|parent| parent.join("x.png")).unwrap_or_default();
        let error = remove(&dir, &outside.display().to_string()).expect_err("带路径的输入必须拒绝");
        assert_eq!(error.reason(), "storage");
        assert!(dir.join("keep.png").exists(), "别把库里的文件删了");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
