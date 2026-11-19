use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatKind {
    User,
    Group,
}

impl ChatKind {
    pub fn from_id(id: &str) -> Self {
        if id.ends_with("@chatroom") {
            Self::Group
        } else {
            Self::User
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "user" => Ok(Self::User),
            "group" => Ok(Self::Group),
            _ => Err("聊天类型必须是 user 或 group".into()),
        }
    }
}

impl fmt::Display for ChatKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::Group => "group",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub kind: ChatKind,
    pub id: String,
}

impl Entry {
    pub fn new(kind: ChatKind, id: &str) -> Result<Self, String> {
        if id.is_empty() || id.trim() != id || id.chars().any(char::is_control) {
            return Err("会话 ID 不能为空或包含首尾空白、控制字符".into());
        }
        Ok(Self {
            kind,
            id: id.into(),
        })
    }
}

pub fn add(entries: &mut Vec<Entry>, entry: Entry) -> Result<(), String> {
    if entries.contains(&entry) {
        return Err("该会话已在白名单中".into());
    }
    entries.push(entry);
    Ok(())
}

pub fn remove(entries: &mut Vec<Entry>, entry: &Entry) -> Result<(), String> {
    let index = entries
        .iter()
        .position(|item| item == entry)
        .ok_or("该会话不在白名单中")?;
    entries.remove(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_chatroom_suffix() {
        assert_eq!(ChatKind::from_id("123@chatroom"), ChatKind::Group);
        assert_eq!(ChatKind::from_id("wxid_123"), ChatKind::User);
    }
    #[test]
    fn membership_is_exact_and_includes_chat_kind() {
        let user = Entry::new(ChatKind::User, "wxid_example").unwrap();
        let group = Entry::new(ChatKind::Group, "wxid_example").unwrap();
        let mut entries = Vec::new();
        add(&mut entries, user.clone()).unwrap();
        assert!(add(&mut entries, user.clone()).is_err());
        assert!(!entries.contains(&group));
        assert!(remove(&mut entries, &group).is_err());
        remove(&mut entries, &user).unwrap();
        assert!(entries.is_empty());
        assert!(Entry::new(ChatKind::User, " ").is_err());
    }
}
