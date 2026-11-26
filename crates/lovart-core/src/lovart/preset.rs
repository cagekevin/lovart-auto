//! 提示词预设 —— **存储形状真源**（`docs/plan/18`）。
//!
//! ── 边界 ──────────────────────────────────────────────────────────────
//! 本模块只管「形状 + 落盘 + 原子写」。**语义校验**（trim / 名字兜底 / id 唯一）
//! 不在这里：前端只读 `reason` 并翻译措辞（`presetMessage`），后端不写用户话术。
//!
//! Preset = { id, name, text, images: [{file}] }
//!   - `file` = 预设目录里的文件名（可读名 + 内容哈希），**不是绝对路径**
//!     ⇒ 清单与图同目录、跟着工作区走。
//!   - `label` **不存**：显示名由后端从文件名派生（`resolve_preset_images` 返回），
//!     派生规则只有这一处（ADR-0003）。
//!
//! 文件：`data/presets/manifest.json`（目录由 `Workbench::presets_dir()` 给）。
//! 写：**整表原子写**，实现只有一处 —— [`crate::fsio::write_atomic`]（`.tmp` → `rename`）。
//!     `engine/state.rs` 的 `compact()` 仍是自己那份：动它要跨到出图花钱边界 + 全量回归，
//!     已登记为债（`docs/plan/21` P11）—— 不是"没看见"。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 一条预设。序列化形状 = `localStorage` 老预设的 `{id,name,text}` + 新字段 `images`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub text: String,
    #[serde(default)]
    pub images: Vec<PresetImage>,
}

/// 预设里的一张图：只存文件名，不存路径 / label。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresetImage {
    pub file: String,
}

/// 失败原因（**判别位不折成值**，ADR-0003）。前端只翻译这一个词表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetError {
    /// 清单文件坏了（JSON 解析失败 / 形状不对）⇒ 回退空清单 + 告警，不覆盖原文件。
    Storage(String),
    /// 没有这条 id（改 / 删 / 取用时列表已变）。
    Missing,
    /// 正文为空（只有名字不算一条预设）。
    Empty,
}
impl PresetError {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Storage(_) => "storage",
            Self::Missing => "missing",
            Self::Empty => "empty",
        }
    }
    pub fn detail(&self) -> String {
        match self {
            Self::Storage(message) => format!("storage: {message}"),
            Self::Missing => "missing".to_string(),
            Self::Empty => "empty".to_string(),
        }
    }
}

/// 生成新 id：毫秒时间戳；**与现有 id 撞了就加后缀重来**。
///
/// ⚠️ 这里**故意不用单调计数器**：那会让"同毫秒连建"天然不撞，
/// 查重就成了删掉也不影响行为的死代码（ADR-0003）。去重只由这一处判据承担。
pub fn new_id(taken: &[Preset]) -> String {
    let base = format!("p{}", chrono::Utc::now().timestamp_millis());
    if !taken.iter().any(|item| item.id == base) {
        return base;
    }
    let mut n = 1u64;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.iter().any(|item| item.id == candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// 读清单：文件不存在 = **空清单**（正常，还没存过）；解析失败 = `Storage(reason)`。
pub fn load(dir: &Path) -> Result<Vec<Preset>, PresetError> {
    let path = dir.join("manifest.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PresetError::Storage(format!("读不了 {}：{error}", path.display()))),
    };
    serde_json::from_str(&text)
        .map_err(|error| PresetError::Storage(format!("{} 格式不对：{error}", path.display())))
}

/// 写清单（整表覆盖）：走 [`crate::fsio::write_atomic`]（同卷原子，全 crate 唯一实现）。
pub fn save(dir: &Path, items: &[Preset]) -> Result<(), PresetError> {
    std::fs::create_dir_all(dir)
        .map_err(|error| PresetError::Storage(format!("建不了 {}：{error}", dir.display())))?;
    let body = serde_json::to_string_pretty(items)
        .map_err(|error| PresetError::Storage(format!("序列化预设失败：{error}")))?;
    crate::fsio::write_atomic(&dir.join("manifest.json"), body.as_bytes())
        .map_err(|error| PresetError::Storage(format!("原子替换清单失败：{error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lovart-preset-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn preset(id: &str, text: &str) -> Preset {
        Preset {
            id: id.to_string(),
            name: String::new(),
            text: text.to_string(),
            images: vec![],
        }
    }

    #[test]
    fn 不存在等于空清单() {
        let dir = temp_dir("empty");
        assert_eq!(load(&dir).unwrap(), Vec::<Preset>::new());
    }

    #[test]
    fn 原子写覆盖两次_第二版胜出() {
        let dir = temp_dir("atomic");
        save(&dir, &[preset("p1", "第一版")]).unwrap();
        save(&dir, &[preset("p1", "第二版")]).unwrap();
        let list = load(&dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].text, "第二版");
        assert!(!dir.join("manifest.json.tmp").exists(), "tmp 应被 rename 走");
    }

    #[test]
    fn 坏清单报_storage_不覆盖原文件() {
        let dir = temp_dir("broken");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest.json");
        std::fs::write(&path, "{坏 JSON").unwrap();
        assert!(matches!(load(&dir), Err(PresetError::Storage(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{坏 JSON");
    }

    #[test]
    fn 同毫秒连建_id_不撞() {
        let a = preset("占位", "A");
        // 第一次：空清单 ⇒ 时间戳本身。
        let first = new_id(&[]);
        // 第二次：第一次已经落进清单 ⇒ 撞车必须换一个（真实调用序列）。
        let taken = vec![Preset { id: first.clone(), ..a.clone() }];
        let second = new_id(&taken);
        assert_ne!(first, second, "同毫秒连建两条必须不同 id");
        assert!(second.starts_with(&first), "撞车应加后缀：{first} → {second}");
    }

    #[test]
    fn images_缺省向后兼容() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"[{"id":"p1","name":"","text":"老预设"}]"#,
        )
        .unwrap();
        let list = load(&dir).unwrap();
        assert_eq!(list[0].images, Vec::<PresetImage>::new());
    }
}
