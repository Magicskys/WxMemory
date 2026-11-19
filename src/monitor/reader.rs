use crate::{
    monitor::whitelist::{ChatKind, Entry},
    push::Message,
    wechat::{accounts, live::Database},
};
use openssl::hash::{MessageDigest, hash};
use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

struct Shard {
    db: Database,
    cursors: HashMap<String, i64>,
}

pub struct Reader {
    root: PathBuf,
    key: String,
    session: Database,
    shards: HashMap<PathBuf, Shard>,
    started_at: i64,
}

impl Reader {
    pub fn open(root: &Path, key: &str) -> Result<Self, String> {
        let session = Database::open(&accounts::session_database(root)?, key)?;
        // Confirm required tables exist before accepting clients.
        session.query_rows("SELECT username FROM SessionTable")?;
        let mut reader = Self {
            root: root.to_owned(),
            key: key.to_owned(),
            session,
            shards: HashMap::new(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_secs() as i64,
        };
        reader.scan(true)?;
        Ok(reader)
    }

    fn shard_paths(&self) -> Result<Vec<PathBuf>, String> {
        let dir = ["message", "Message"]
            .into_iter()
            .map(|name| self.root.join("db_storage").join(name))
            .find(|path| path.is_dir())
            .ok_or("未找到消息数据库目录 db_storage/message")?;
        let mut paths = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| format!("无法读取消息数据库目录：{e}"))?
        {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let number = name
                .strip_prefix("message_")
                .or_else(|| name.strip_prefix("biz_message_"))
                .and_then(|s| s.strip_suffix(".db"));
            if entry.file_type().map_err(|e| e.to_string())?.is_file()
                && number.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                paths.push(entry.path());
            }
        }
        paths.sort();
        if paths.is_empty() && self.shards.is_empty() {
            return Err("未找到消息数据库分片 message_N.db".into());
        }
        Ok(paths)
    }

    pub fn scan(&mut self, baseline: bool) -> Result<Vec<Message>, String> {
        let checkpoint: HashMap<_, _> = self
            .shards
            .iter()
            .map(|(path, shard)| (path.clone(), shard.cursors.clone()))
            .collect();
        let result = self.scan_inner(baseline, None, None);
        if result.is_err() {
            self.shards.retain(|path, _| checkpoint.contains_key(path));
            for (path, cursors) in checkpoint {
                if let Some(shard) = self.shards.get_mut(&path) {
                    shard.cursors = cursors;
                }
            }
        }
        result
    }

    fn scan_inner(
        &mut self,
        baseline: bool,
        history_since: Option<i64>,
        watchlist: Option<&[Entry]>,
    ) -> Result<Vec<Message>, String> {
        let mut sessions = HashMap::new();
        for row in self
            .session
            .query_rows("SELECT username FROM SessionTable")?
        {
            if let Some(id) = row.get("username") {
                let digest =
                    hash(MessageDigest::md5(), id.as_bytes()).map_err(|e| e.to_string())?;
                let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                sessions.insert(format!("msg_{hex}"), id.clone());
            }
        }
        let paths = self.shard_paths()?;
        let mut result = Vec::new();
        for path in paths {
            if !self.shards.contains_key(&path) {
                let db = Database::open(&path, &self.key)
                    .map_err(|e| format!("无法打开消息分片 {}：{e}", path.display()))?;
                self.shards.insert(
                    path.clone(),
                    Shard {
                        db,
                        cursors: HashMap::new(),
                    },
                );
            }
            let shard = self.shards.get_mut(&path).expect("shard inserted");
            let tables = shard.db.table_names()?;
            let sender_map: HashMap<String, String> = if tables
                .iter()
                .any(|name| name.eq_ignore_ascii_case("Name2Id"))
            {
                shard
                    .db
                    .query_rows("SELECT rowid AS sender_rowid, user_name FROM Name2Id")?
                    .into_iter()
                    .filter_map(|r| {
                        Some((r.get("sender_rowid")?.clone(), r.get("user_name")?.clone()))
                    })
                    .collect()
            } else {
                HashMap::new()
            };
            if history_since.is_some() {
                for username in sender_map.values() {
                    let digest = hash(MessageDigest::md5(), username.as_bytes())
                        .map_err(|e| e.to_string())?;
                    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
                    sessions
                        .entry(format!("msg_{hex}"))
                        .or_insert_with(|| username.clone());
                }
            }
            for table in tables {
                let lower = table.to_ascii_lowercase();
                if lower.len() != 36
                    || !lower.starts_with("msg_")
                    || !lower[4..].bytes().all(|b| b.is_ascii_hexdigit())
                {
                    continue;
                }
                let Some(chat_id) = sessions.get(&lower) else {
                    continue;
                };
                if let Some(allowed) = watchlist
                    && !allowed.iter().any(|entry| {
                        entry.id == *chat_id && entry.kind == ChatKind::from_id(chat_id)
                    })
                {
                    continue;
                }
                let cursor = if history_since.is_some() {
                    None
                } else {
                    shard.cursors.get(&table).copied()
                };
                if baseline && cursor.is_none() {
                    let rows = shard.db.query_rows(&format!(
                        "SELECT COALESCE(MAX(local_id), 0) AS max_id FROM \"{table}\""
                    ))?;
                    let max = rows
                        .first()
                        .and_then(|r| r.get("max_id"))
                        .and_then(|s| s.parse().ok())
                        .ok_or("消息表缺少有效 local_id")?;
                    shard.cursors.insert(table, max);
                    continue;
                }
                let floor = cursor.unwrap_or(0);
                let time_filter = if let Some(since) = history_since {
                    format!(" AND create_time > {since}")
                } else if cursor.is_none() {
                    format!(" AND create_time >= {}", self.started_at)
                } else {
                    String::new()
                };
                let limit = if history_since.is_some() {
                    " LIMIT 100001"
                } else {
                    ""
                };
                let sql = format!(
                    "SELECT local_id, create_time, real_sender_id, local_type, hex(message_content) AS content_hex FROM \"{table}\" WHERE local_id > {floor}{time_filter} ORDER BY local_id ASC{limit}"
                );
                for row in shard.db.query_rows(&sql)? {
                    let id: i64 = row
                        .get("local_id")
                        .and_then(|s| s.parse().ok())
                        .ok_or("消息 local_id 无效")?;
                    let timestamp = row
                        .get("create_time")
                        .and_then(|s| s.parse().ok())
                        .ok_or("消息 create_time 无效")?;
                    let mut content =
                        decode_content(row.get("content_hex").map_or("", String::as_str))?;
                    let mut sender_id = row
                        .get("real_sender_id")
                        .and_then(|id| sender_map.get(id))
                        .cloned()
                        .unwrap_or_default();
                    if chat_id.ends_with("@chatroom")
                        && let Some((sender, text)) = content.split_once(":\n")
                        && !sender.is_empty()
                        && sender.len() < 128
                        && !sender.chars().any(char::is_whitespace)
                    {
                        if sender_id.is_empty() {
                            sender_id = sender.to_owned();
                        }
                        content = text.to_owned();
                    }
                    result.push(Message {
                        chat_kind: ChatKind::from_id(chat_id),
                        chat_id: chat_id.clone(),
                        sender_id,
                        content,
                        timestamp,
                        identity: Some(format!("{}:{}:{}", path.display(), table, id)),
                    });
                    if history_since.is_some() && result.len() > 100_000 {
                        return Err("历史消息超过单次补发上限 100000 条，请缩小时间范围".into());
                    }
                    if history_since.is_none() {
                        shard.cursors.insert(table.clone(), id);
                    }
                }
                // Record newly discovered empty tables, but don't advance over a failed query.
                if history_since.is_none() {
                    shard.cursors.entry(table).or_insert(floor);
                }
            }
        }
        Ok(result)
    }
}

pub fn history(
    root: &Path,
    key: &str,
    since: i64,
    watchlist: Option<&[Entry]>,
) -> Result<Vec<Message>, String> {
    let mut reader = Reader::open(root, key)?;
    let mut messages = reader.scan_inner(false, Some(since), watchlist)?;
    messages.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.identity.cmp(&b.identity))
    });
    Ok(messages)
}

fn decode_content(hex: &str) -> Result<String, String> {
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("消息内容编码无效".into());
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let decoded = if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        let decoder = zstd::stream::read::Decoder::new(bytes.as_slice())
            .map_err(|e| format!("消息解压失败：{e}"))?;
        let mut out = Vec::new();
        decoder
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut out)
            .map_err(|e| format!("消息解压失败：{e}"))?;
        if out.len() > 16 * 1024 * 1024 {
            return Err("消息解压后超过大小上限".into());
        }
        out
    } else {
        bytes
    };
    Ok(String::from_utf8_lossy(&decoded).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsqlite3_sys as sqlite;
    use std::{ffi::CString, ptr};

    struct Writer(*mut sqlite::sqlite3);
    impl Writer {
        fn new(path: &Path) -> Self {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let name = CString::new(path.to_str().unwrap()).unwrap();
            let mut raw = ptr::null_mut();
            assert_eq!(
                unsafe {
                    sqlite::sqlite3_open_v2(
                        name.as_ptr(),
                        &mut raw,
                        sqlite::SQLITE_OPEN_READWRITE | sqlite::SQLITE_OPEN_CREATE,
                        ptr::null(),
                    )
                },
                sqlite::SQLITE_OK
            );
            let key = [0xabu8; 32];
            assert_eq!(
                unsafe { sqlite::sqlite3_key(raw, key.as_ptr().cast(), 32) },
                sqlite::SQLITE_OK
            );
            let db = Self(raw);
            db.exec("PRAGMA journal_mode=WAL");
            db
        }
        fn exec(&self, sql: &str) {
            let sql = CString::new(sql).unwrap();
            assert_eq!(
                unsafe {
                    sqlite::sqlite3_exec(
                        self.0,
                        sql.as_ptr(),
                        None,
                        ptr::null_mut(),
                        ptr::null_mut(),
                    )
                },
                sqlite::SQLITE_OK
            );
        }
    }
    impl Drop for Writer {
        fn drop(&mut self) {
            unsafe {
                sqlite::sqlite3_close(self.0);
            }
        }
    }
    fn table(id: &str) -> String {
        let digest = hash(MessageDigest::md5(), id.as_bytes()).unwrap();
        format!(
            "Msg_{}",
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }
    #[tokio::test]
    async fn baselines_existing_rows_and_reads_new_wal_rows_once() {
        use axum::{body::Body, http::Request};
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("wx-watch-{}-{unique}", std::process::id()));
        let session = Writer::new(&root.join("db_storage/session/session.db"));
        session.exec("CREATE TABLE SessionTable(username TEXT); INSERT INTO SessionTable VALUES ('wxid_a'), ('123@chatroom')");
        let shard = Writer::new(&root.join("db_storage/message/message_0.db"));
        let user = table("wxid_a");
        let group = table("123@chatroom");
        shard.exec("CREATE TABLE Name2Id(user_name TEXT); INSERT INTO Name2Id(rowid,user_name) VALUES (1, 'wxid_sender')");
        for name in [&user, &group] {
            shard.exec(&format!("CREATE TABLE {name}(local_id INTEGER PRIMARY KEY, create_time INTEGER, real_sender_id INTEGER, local_type INTEGER, message_content BLOB)"));
        }
        shard.exec(&format!(
            "INSERT INTO {user} VALUES(1, 1700000000, 1, 1, 'old')"
        ));
        let mut reader = Reader::open(&root, &"ab".repeat(32)).unwrap();
        assert!(reader.scan(false).unwrap().is_empty());
        let service = crate::push::PushService::new(vec![
            crate::monitor::whitelist::Entry::new(ChatKind::Group, "123@chatroom").unwrap(),
        ]);
        let response = crate::push::router(service.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body = response.into_body();
        shard.exec(&format!(
            "INSERT INTO {user} VALUES(2, 1700000001, 1, 1, 'new')"
        ));
        shard.exec(&format!(
            "INSERT INTO {group} VALUES(1, 1700000002, 1, 1, 'wxid_sender:' || char(10) || 'hello')"
        ));
        let mut messages = reader.scan(false).unwrap();
        messages.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
        assert_eq!(messages.len(), 2);
        assert_eq!(
            (
                &messages[0].chat_id,
                &messages[0].content,
                &messages[0].sender_id
            ),
            (
                &"123@chatroom".to_string(),
                &"hello".to_string(),
                &"wxid_sender".to_string()
            )
        );
        assert_eq!(messages[1].content, "new");
        let historical = crate::push::PushService::new(vec![
            crate::monitor::whitelist::Entry::new(ChatKind::Group, "123@chatroom").unwrap(),
        ])
        .with_history(root.clone(), "ab".repeat(32));
        let replay = crate::push::router(historical.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/events?since=1700000000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replay.status(), 200);
        let mut replay_body = replay.into_body();
        let replay_frame =
            tokio::time::timeout(std::time::Duration::from_secs(2), replay_body.frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
        assert!(
            std::str::from_utf8(&replay_frame)
                .unwrap()
                .contains("hello")
        );
        historical.publish(&messages[0]).unwrap(); // The same row may also be observed by the live poller.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), replay_body.frame())
                .await
                .is_err()
        );
        let all_response = crate::push::router(historical.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/events?watchlist=false&since=1699999999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(all_response.status(), 200);
        let mut all_body = all_response.into_body();
        for expected in ["old", "new", "hello"] {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), all_body.frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            assert!(std::str::from_utf8(&frame).unwrap().contains(expected));
        }
        let all = history(&root, &"ab".repeat(32), 1699999999, None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].content, "old");
        assert_eq!(
            history(&root, &"ab".repeat(32), 1700000000, None)
                .unwrap()
                .len(),
            2
        );
        for message in &messages {
            service.publish(message).unwrap();
        }
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        let payload = std::str::from_utf8(&frame).unwrap();
        assert!(payload.contains("123@chatroom") && payload.contains("hello"));
        assert!(!payload.contains("wxid_a"));
        assert!(reader.scan(false).unwrap().is_empty());
        let second = Writer::new(&root.join("db_storage/message/message_1.db"));
        second.exec(&format!("CREATE TABLE {user}(local_id INTEGER PRIMARY KEY, create_time INTEGER, real_sender_id INTEGER, local_type INTEGER, message_content BLOB)"));
        second.exec(&format!(
            "INSERT INTO {user} VALUES(1, {}, 0, 1, 'new shard')",
            reader.started_at
        ));
        let messages = reader.scan(false).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "new shard");
        assert!(reader.scan(false).unwrap().is_empty());
        drop(second);
        drop(reader);
        drop(shard);
        drop(session);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn decodes_zstd_content() {
        let compressed = zstd::stream::encode_all("消息".as_bytes(), 1).unwrap();
        let hex: String = compressed.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(decode_content(&hex).unwrap(), "消息");
    }
}
