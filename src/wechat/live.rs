//! Read-only SQLCipher connections. SQLite owns transaction and WAL handling.
use libsqlite3_sys as sqlite;
use std::{
    ffi::{CStr, CString},
    path::Path,
    ptr,
};

pub struct Database {
    handle: *mut sqlite::sqlite3,
}

impl Database {
    pub fn open(path: &Path, hex: &str) -> Result<Self, String> {
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("数据库密钥必须是 64 位十六进制字符".into());
        }
        let key = (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let path = CString::new(path.to_str().ok_or("数据库路径不是有效的 UTF-8")?)
            .map_err(|_| "数据库路径含有空字符")?;
        let mut last_error = String::new();
        for page_size in [4096, 1024] {
            match Self::open_with_page_size(&path, &key, page_size) {
                Ok(db) => return Ok(db),
                Err(error) => last_error = error,
            }
        }
        Err(format!(
            "无法打开加密数据库，请检查密钥和数据库格式：{last_error}"
        ))
    }

    fn open_with_page_size(path: &CStr, key: &[u8], page_size: usize) -> Result<Self, String> {
        let mut raw = ptr::null_mut();
        let rc = unsafe {
            sqlite::sqlite3_open_v2(
                path.as_ptr(),
                &mut raw,
                sqlite::SQLITE_OPEN_READONLY,
                ptr::null(),
            )
        };
        if raw.is_null() {
            return Err(format!("SQLite 打开失败，错误码 {rc}"));
        }
        let db = Self { handle: raw };
        if rc != sqlite::SQLITE_OK {
            return Err(db.error());
        }
        let rc = unsafe { sqlite::sqlite3_key(db.handle, key.as_ptr().cast(), key.len() as i32) };
        if rc != sqlite::SQLITE_OK {
            return Err(db.error());
        }
        db.execute(&format!("PRAGMA cipher_page_size = {page_size}"))?;
        db.execute("SELECT count(*) FROM sqlite_master")?;
        Ok(db)
    }

    fn execute(&self, sql: &str) -> Result<(), String> {
        let sql = CString::new(sql).map_err(|_| "SQL 含有空字符")?;
        let rc = unsafe {
            sqlite::sqlite3_exec(
                self.handle,
                sql.as_ptr(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc != sqlite::SQLITE_OK {
            return Err(self.error());
        }
        Ok(())
    }

    pub fn table_names(&self) -> Result<Vec<String>, String> {
        let sql = c"SELECT name FROM sqlite_master WHERE type='table' ORDER BY name";
        let mut stmt = ptr::null_mut();
        let rc = unsafe {
            sqlite::sqlite3_prepare_v2(self.handle, sql.as_ptr(), -1, &mut stmt, ptr::null_mut())
        };
        if rc != sqlite::SQLITE_OK {
            return Err(self.error());
        }
        struct Statement(*mut sqlite::sqlite3_stmt);
        impl Drop for Statement {
            fn drop(&mut self) {
                unsafe {
                    sqlite::sqlite3_finalize(self.0);
                }
            }
        }
        let stmt = Statement(stmt);
        let mut names = Vec::new();
        loop {
            match unsafe { sqlite::sqlite3_step(stmt.0) } {
                sqlite::SQLITE_ROW => {
                    let value = unsafe { sqlite::sqlite3_column_text(stmt.0, 0) };
                    if value.is_null() {
                        return Err("SQLite 返回了空表名".into());
                    }
                    names.push(
                        unsafe { CStr::from_ptr(value.cast()) }
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                sqlite::SQLITE_DONE => return Ok(names),
                _ => return Err(self.error()),
            }
        }
    }

    /// Executes a fixed read-only query and returns text representations by column name.
    pub fn query_rows(
        &self,
        sql: &str,
    ) -> Result<Vec<std::collections::HashMap<String, String>>, String> {
        let sql = CString::new(sql).map_err(|_| "SQL 含有空字符")?;
        let mut raw = ptr::null_mut();
        let rc = unsafe {
            sqlite::sqlite3_prepare_v2(self.handle, sql.as_ptr(), -1, &mut raw, ptr::null_mut())
        };
        if rc != sqlite::SQLITE_OK {
            return Err(self.error());
        }
        struct Statement(*mut sqlite::sqlite3_stmt);
        impl Drop for Statement {
            fn drop(&mut self) {
                unsafe {
                    sqlite::sqlite3_finalize(self.0);
                }
            }
        }
        let stmt = Statement(raw);
        let count = unsafe { sqlite::sqlite3_column_count(stmt.0) };
        let mut rows = Vec::new();
        loop {
            match unsafe { sqlite::sqlite3_step(stmt.0) } {
                sqlite::SQLITE_ROW => {
                    let mut row = std::collections::HashMap::new();
                    for col in 0..count {
                        let name =
                            unsafe { CStr::from_ptr(sqlite::sqlite3_column_name(stmt.0, col)) }
                                .to_string_lossy()
                                .to_ascii_lowercase();
                        let value = unsafe { sqlite::sqlite3_column_text(stmt.0, col) };
                        if !value.is_null() {
                            let bytes = unsafe { sqlite::sqlite3_column_bytes(stmt.0, col) };
                            let data = unsafe { std::slice::from_raw_parts(value, bytes as usize) };
                            row.insert(name, String::from_utf8_lossy(data).into_owned());
                        }
                    }
                    rows.push(row);
                }
                sqlite::SQLITE_DONE => return Ok(rows),
                _ => return Err(self.error()),
            }
        }
    }

    fn error(&self) -> String {
        unsafe { CStr::from_ptr(sqlite::sqlite3_errmsg(self.handle)) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        unsafe {
            sqlite::sqlite3_close(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn reads_encrypted_wal_and_rejects_wrong_key() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("wx-memo-db-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("test.db");
        let filename = CString::new(path.to_str().unwrap()).unwrap();
        let mut handle = ptr::null_mut();
        let flags = sqlite::SQLITE_OPEN_READWRITE | sqlite::SQLITE_OPEN_CREATE;
        assert_eq!(
            unsafe { sqlite::sqlite3_open_v2(filename.as_ptr(), &mut handle, flags, ptr::null()) },
            sqlite::SQLITE_OK
        );
        let writer = Database { handle };
        let key = [0xabu8; 32];
        assert_eq!(
            unsafe { sqlite::sqlite3_key(writer.handle, key.as_ptr().cast(), key.len() as i32) },
            sqlite::SQLITE_OK
        );
        writer.execute("PRAGMA journal_mode = WAL").unwrap();
        writer.execute("CREATE TABLE first (value TEXT)").unwrap();
        let reader = Database::open(&path, &"ab".repeat(32)).unwrap();
        assert!(reader.table_names().unwrap().contains(&"first".to_owned()));
        writer.execute("CREATE TABLE second (value TEXT)").unwrap();
        assert!(reader.table_names().unwrap().contains(&"second".to_owned()));
        assert!(Database::open(&path, &"cd".repeat(32)).is_err());
        drop(reader);
        drop(writer);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn lists_sessions_from_encrypted_account_databases() {
        use crate::wechat::sessions;
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("wx-memo-sessions-{}-{unique}", std::process::id()));
        let session_path = dir.join("db_storage/session/session.db");
        let contact_path = dir.join("db_storage/contact/contact.db");
        fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        fs::create_dir_all(contact_path.parent().unwrap()).unwrap();
        fn writer(path: &Path) -> Database {
            let filename = CString::new(path.to_str().unwrap()).unwrap();
            let mut handle = ptr::null_mut();
            assert_eq!(
                unsafe {
                    sqlite::sqlite3_open_v2(
                        filename.as_ptr(),
                        &mut handle,
                        sqlite::SQLITE_OPEN_READWRITE | sqlite::SQLITE_OPEN_CREATE,
                        ptr::null(),
                    )
                },
                sqlite::SQLITE_OK
            );
            let db = Database { handle };
            let key = [0xabu8; 32];
            assert_eq!(
                unsafe { sqlite::sqlite3_key(db.handle, key.as_ptr().cast(), 32) },
                sqlite::SQLITE_OK
            );
            db
        }
        let session = writer(&session_path);
        session.execute("CREATE TABLE SessionTable (username TEXT, last_timestamp INTEGER, sort_timestamp INTEGER); INSERT INTO SessionTable VALUES ('wxid_one', 1700000000, 1), ('123@chatroom', 1700000100, 0), ('unknown', 0, 2)").unwrap();
        let contact = writer(&contact_path);
        contact.execute("CREATE TABLE contact (username TEXT, remark TEXT, nick_name TEXT); INSERT INTO contact VALUES ('wxid_one','备注','昵称'), ('123@chatroom','','群名称')").unwrap();
        let (items, warning) = sessions::list(&dir, &"ab".repeat(32)).unwrap();
        assert!(warning.is_none());
        assert_eq!(items.len(), 3);
        assert_eq!(
            (&items[0].id, &items[0].name, items[0].timestamp),
            (
                &"123@chatroom".to_string(),
                &"群名称".to_string(),
                Some(1700000100)
            )
        );
        assert_eq!(items[1].name, "备注");
        assert_eq!(items[2].name, "unknown");
        assert_eq!(sessions::format_time(None), "未知");
        drop(session);
        drop(contact);
        fs::remove_dir_all(dir).unwrap();
    }
}
