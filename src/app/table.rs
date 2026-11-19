use crate::wechat::sessions::{Session, format_time};
use tabled::{Table, Tabled, settings::Style};

#[derive(Tabled)]
struct SessionRow {
    #[tabled(rename = "类型")]
    kind: &'static str,
    #[tabled(rename = "名称")]
    name: String,
    #[tabled(rename = "微信 ID / 群 ID")]
    id: String,
    #[tabled(rename = "最近聊天时间（本地时区）")]
    last_time: String,
}

fn clean(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

pub fn sessions(items: &[Session]) -> String {
    let rows = items.iter().map(|item| SessionRow {
        kind: if item.id.ends_with("@chatroom") {
            "群聊"
        } else {
            "聊天"
        },
        name: clean(&item.name),
        id: clean(&item.id),
        last_time: format_time(item.timestamp),
    });
    Table::new(rows).with(Style::rounded()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_chinese_and_sanitizes_multiline_cells() {
        let rows = vec![
            Session {
                id: "1@chatroom".into(),
                name: "测试\n群".into(),
                timestamp: None,
            },
            Session {
                id: "wxid_1".into(),
                name: "Alice".into(),
                timestamp: None,
            },
        ];
        let result = sessions(&rows);
        assert!(result.contains("测试 群"));
        assert!(result.contains("微信 ID / 群 ID"));
        assert!(result.contains("群聊"));
        assert!(result.contains("未知"));
    }
}
