use super::{accounts, live::Database};
use chrono::{Local, TimeZone};
use std::{collections::HashMap, path::Path};

pub struct Session {
    pub id: String,
    pub name: String,
    pub timestamp: Option<i64>,
}

pub fn list(root: &Path, key: &str) -> Result<(Vec<Session>, Option<String>), String> {
    let session_db = Database::open(&accounts::session_database(root)?, key)?;
    let rows = session_db
        .query_rows(
            "SELECT username, last_timestamp FROM SessionTable ORDER BY last_timestamp DESC",
        )
        .map_err(|error| format!("无法读取会话列表（微信数据库结构可能不兼容）：{error}"))?;
    let contact_path = root.join("db_storage/contact/contact.db");
    let (names, warning) = match read_names(&contact_path, key) {
        Ok(names) => (names, None),
        Err(error) => (
            HashMap::new(),
            Some(format!("无法读取联系人名称，将使用会话 ID：{error}")),
        ),
    };
    let sessions = rows
        .into_iter()
        .filter_map(|row| {
            let id = row.get("username")?.trim().to_owned();
            if id.is_empty() {
                return None;
            }
            let name = names.get(&id).cloned().unwrap_or_else(|| id.clone());
            let timestamp = row
                .get("last_timestamp")
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|&t| t > 0);
            Some(Session {
                id,
                name,
                timestamp,
            })
        })
        .collect();
    Ok((sessions, warning))
}

fn read_names(path: &Path, key: &str) -> Result<HashMap<String, String>, String> {
    let db = Database::open(path, key)?;
    let tables = db.table_names()?;
    let mut names = HashMap::new();
    for table in ["contact", "stranger"] {
        if !tables.iter().any(|name| name.eq_ignore_ascii_case(table)) {
            continue;
        }
        let columns = db.query_rows(&format!("PRAGMA table_info({table})"))?;
        let available: Vec<_> = [
            "username",
            "user_name",
            "remark",
            "nick_name",
            "nickname",
            "alias",
        ]
        .into_iter()
        .filter(|column| {
            columns.iter().any(|row| {
                row.get("name")
                    .is_some_and(|name| name.eq_ignore_ascii_case(column))
            })
        })
        .collect();
        if !available.contains(&"username") && !available.contains(&"user_name") {
            continue;
        }
        let rows = db.query_rows(&format!("SELECT {} FROM {table}", available.join(", ")))?;
        for row in rows {
            let id = row.get("username").or_else(|| row.get("user_name"));
            let name = ["remark", "nick_name", "nickname", "alias"]
                .iter()
                .filter_map(|key| row.get(*key))
                .find(|value| !value.trim().is_empty());
            if let (Some(id), Some(name)) = (id, name) {
                names
                    .entry(id.clone())
                    .or_insert_with(|| name.trim().to_owned());
            }
        }
    }
    Ok(names)
}

pub fn format_time(timestamp: Option<i64>) -> String {
    let Some(value) = timestamp else {
        return "未知".into();
    };
    let seconds = if value > 10_000_000_000 {
        value / 1000
    } else {
        value
    };
    Local
        .timestamp_opt(seconds, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S %:z").to_string())
        .unwrap_or_else(|| "未知".into())
}
