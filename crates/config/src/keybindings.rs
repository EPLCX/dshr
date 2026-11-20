//! 可编辑的快捷键：`keybindings.json`，动作名 → 按键（GPUI 写法，如 `ctrl-shift-n`）。
//! 只记录用户改过的项，没写的动作用内置默认值；“全部恢复”就是删掉这个文件。

use std::collections::BTreeMap;
use std::{fs, io};

use serde::{Deserialize, Serialize};

use crate::{ConfigError, Paths};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Keybindings {
    pub bindings: BTreeMap<String, String>,
}

impl Keybindings {
    /// 读取 `keybindings.json`，不存在时为空。
    pub fn load(paths: &Paths) -> Result<Self, ConfigError> {
        let path = paths.keybindings_file();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        serde_json::from_str(&text).map_err(|source| ConfigError::ParseJson { path, source })
    }

    /// 写回文件；没有任何自定义时删除文件。
    pub fn save(&self, paths: &Paths) -> Result<(), ConfigError> {
        let path = paths.keybindings_file();
        let write_error = |source| ConfigError::Write { path: path.clone(), source };
        if self.bindings.is_empty() {
            return match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(write_error(error)),
                _ => Ok(()),
            };
        }
        fs::create_dir_all(&paths.home).map_err(write_error)?;
        let text = serde_json::to_string_pretty(self).expect("快捷键总能序列化");
        fs::write(&path, text).map_err(write_error)
    }

    pub fn get(&self, action: &str) -> Option<&str> {
        self.bindings.get(action).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_reset() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { home: dir.path().to_owned() };
        assert_eq!(Keybindings::load(&paths).unwrap(), Keybindings::default());

        let mut keys = Keybindings::default();
        keys.bindings.insert("new_session".into(), "ctrl-shift-n".into());
        keys.save(&paths).unwrap();
        assert_eq!(Keybindings::load(&paths).unwrap().get("new_session"), Some("ctrl-shift-n"));

        Keybindings::default().save(&paths).unwrap();
        assert!(!paths.keybindings_file().exists());
    }
}
