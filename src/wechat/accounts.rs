use super::identity;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct Account {
    pub directory: String,
    pub root: PathBuf,
    pub wxid: Option<String>,
    pub from_profile: bool,
}

fn matches_identity(directory: &str, wxid: &str) -> bool {
    directory.eq_ignore_ascii_case(wxid)
        || directory
            .strip_prefix(wxid)
            .and_then(|tail| tail.strip_prefix('_'))
            .is_some_and(|suffix| {
                suffix.len() == 4 && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
            })
}

fn derive_wxid(directory: &str) -> Option<String> {
    if directory
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("wxid_"))
    {
        let rest = &directory[5..];
        let prefix = rest.split('_').next()?;
        if !prefix.is_empty() {
            return Some(format!("{}{}", &directory[..5], prefix));
        }
    }
    let (name, suffix) = directory.rsplit_once('_')?;
    if !name.is_empty() && suffix.len() == 4 && suffix.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Some(name.to_owned());
    }
    None
}

pub fn discover(root: &Path) -> Result<Vec<Account>, String> {
    let root = fs::canonicalize(root)
        .map_err(|e| format!("无法定位微信数据目录 {}：{e}", root.display()))?;
    let entries =
        fs::read_dir(&root).map_err(|e| format!("无法读取微信数据目录 {}：{e}", root.display()))?;
    let mut accounts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("无法读取微信账号目录：{e}"))?;
        let kind = entry
            .file_type()
            .map_err(|e| format!("无法检查微信账号目录：{e}"))?;
        if !kind.is_dir() || !entry.path().join("db_storage").is_dir() {
            continue;
        }
        let Some(directory) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        accounts.push(Account {
            wxid: derive_wxid(&directory),
            root: entry.path(),
            directory,
            from_profile: false,
        });
    }
    accounts.sort_by(|a, b| a.directory.cmp(&b.directory));
    if let Ok(profile) = identity::read_identity(&root) {
        let matched: Vec<_> = accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| matches_identity(&account.directory, &profile.wxid))
            .map(|(index, _)| index)
            .collect();
        if let [index] = matched.as_slice() {
            accounts[*index].wxid = Some(profile.wxid);
            accounts[*index].from_profile = true;
        }
    }
    Ok(accounts)
}

/// Resolve the current account's session database in supported WeChat layouts.
pub fn session_database(root: &Path) -> Result<PathBuf, String> {
    for relative in ["db_storage/session/session.db", "db_storage/session.db"] {
        let candidate = root.join(relative);
        match fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => return Ok(candidate),
            Ok(_) => return Err(format!("会话数据库不是文件：{}", candidate.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "无法访问会话数据库 {}：{error}",
                    candidate.display()
                ));
            }
        }
    }
    Err(format!(
        "账号目录 {} 下未找到 session.db；请确认微信数据目录并重新运行 setup",
        root.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_account_directories_without_guessing_unknown_names() {
        let dir = std::env::temp_dir().join(format!("wx-memo-accounts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for name in [
            "wxid_bbb_cdef",
            "wxid_aaa_1234",
            "custom_account_ab12",
            "unknown",
        ] {
            fs::create_dir_all(dir.join(name).join("db_storage")).unwrap();
        }
        fs::create_dir_all(dir.join("Backup")).unwrap();
        let accounts = discover(&dir).unwrap();
        assert_eq!(accounts.len(), 4);
        assert_eq!(accounts[0].wxid.as_deref(), Some("custom_account"));
        assert_eq!(accounts[1].wxid, None);
        assert_eq!(accounts[2].wxid.as_deref(), Some("wxid_aaa"));
        assert_eq!(accounts[3].wxid.as_deref(), Some("wxid_bbb"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn resolves_session_layouts_and_reports_missing() {
        let dir = std::env::temp_dir().join(format!("wx-memo-session-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("db_storage/session")).unwrap();
        assert!(session_database(&dir).is_err());
        let fallback = dir.join("db_storage/session.db");
        fs::write(&fallback, []).unwrap();
        assert_eq!(session_database(&dir).unwrap(), fallback);
        let preferred = dir.join("db_storage/session/session.db");
        fs::write(&preferred, []).unwrap();
        assert_eq!(session_database(&dir).unwrap(), preferred);
        fs::remove_dir_all(dir).unwrap();
    }
}
