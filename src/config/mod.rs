use crate::monitor::whitelist::Entry;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub active_account: Option<String>,
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountConfig>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_root: Option<PathBuf>,
    pub database_key: String,
    pub image_aes_key: String,
    pub image_xor_key: String,
    #[serde(default)]
    pub whitelist: Vec<Entry>,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(active) = &self.active_account {
            if !self.accounts.contains_key(active) {
                return Err("当前账号不在配置的账号列表中".into());
            }
        } else if !self.accounts.is_empty() {
            return Err("配置缺少当前账号".into());
        }
        for (wxid, account) in &self.accounts {
            validate_wxid(wxid)?;
            account.validate()?;
        }
        Ok(())
    }

    pub fn active(&self) -> Result<&AccountConfig, String> {
        self.active_account
            .as_ref()
            .and_then(|id| self.accounts.get(id))
            .ok_or_else(|| "尚未配置当前账号，请先运行 wx_memo setup".into())
    }

    pub fn active_mut(&mut self) -> Result<&mut AccountConfig, String> {
        self.active_account
            .as_ref()
            .and_then(|id| self.accounts.get_mut(id))
            .ok_or_else(|| "尚未配置当前账号，请先运行 wx_memo setup".into())
    }
}

impl AccountConfig {
    fn validate(&self) -> Result<(), String> {
        if self.database_key.len() != 64
            || !self.database_key.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("数据库密钥必须是 64 位十六进制字符".into());
        }
        if self.image_aes_key.len() != 16 || !self.image_aes_key.is_ascii() {
            return Err("图片 AES 密钥必须是 16 个 ASCII 字符".into());
        }
        let xor = self
            .image_xor_key
            .strip_prefix("0x")
            .or_else(|| self.image_xor_key.strip_prefix("0X"))
            .ok_or("图片 XOR 密钥必须是 0x 开头的两位十六进制数")?;
        if xor.len() != 2 || !xor.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("图片 XOR 密钥必须是 0x 开头的两位十六进制数".into());
        }
        for (index, entry) in self.whitelist.iter().enumerate() {
            Entry::new(entry.kind.clone(), &entry.id)?;
            if self.whitelist[..index].contains(entry) {
                return Err("白名单中存在重复会话".into());
            }
        }
        Ok(())
    }
}

pub fn validate_wxid(wxid: &str) -> Result<(), String> {
    if !(3..=128).contains(&wxid.len())
        || !wxid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("微信用户名必须为 3–128 位英文字母、数字、下划线或连字符".into());
    }
    Ok(())
}

pub fn path() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_CONFIG_HOME") {
        Some(path) if Path::new(&path).is_absolute() => PathBuf::from(path),
        _ => PathBuf::from(env::var_os("HOME").ok_or("无法确定配置目录")?).join(".config"),
    };
    Ok(base.join("wx_memo/config.toml"))
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = fs::read_to_string(path).map_err(|e| {
        format!(
            "无法读取配置 {}：{e}；请先运行 wx_memo setup",
            path.display()
        )
    })?;
    let config: Config = toml::from_str(&text).map_err(|e| format!("配置格式无效：{e}"))?;
    config.validate()?;
    Ok(config)
}

pub fn save(path: &Path, config: &Config) -> Result<(), String> {
    config.validate()?;
    let parent = path.parent().ok_or("配置目录无效")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(parent)
        .map_err(|e| format!("无法创建配置目录：{e}"))?;
    let text = toml::to_string_pretty(config).map_err(|e| format!("无法生成配置：{e}"))?;
    let tmp = parent.join(format!(".config-{}.tmp", std::process::id()));
    let mut created = false;
    let result = (|| -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp)?;
        created = true;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("无法保存配置：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grouped_accounts_roundtrip_and_validate_active() {
        let account = AccountConfig {
            account_root: None,
            database_key: "ab".repeat(32),
            image_aes_key: "abcdefghijklmnop".into(),
            image_xor_key: "0x40".into(),
            whitelist: Vec::new(),
        };
        let mut config = Config::default();
        config.accounts.insert("wxid_example".into(), account);
        config.active_account = Some("wxid_example".into());
        config.validate().unwrap();
        let encoded = toml::to_string_pretty(&config).unwrap();
        assert!(encoded.contains("[accounts.wxid_example]"));
        let mut decoded: Config = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.active().unwrap().database_key, "ab".repeat(32));
        decoded.active_account = Some("missing".into());
        assert!(decoded.validate().is_err());
        assert!(validate_wxid("../invalid").is_err());
    }
}
