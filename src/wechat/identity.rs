use openssl::symm::{Cipher, decrypt};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024;

pub struct Identity {
    pub wxid: String,
    pub nickname: Option<String>,
}

pub fn default_data_root() -> Result<PathBuf, String> {
    if !cfg!(target_os = "macos") {
        return Err("当前平台请手动指定微信 4.0 的 xwechat_files 目录".into());
    }
    let home = env::var_os("HOME").ok_or("无法获取 HOME，请手动指定微信数据目录")?;
    Ok(PathBuf::from(home)
        .join("Library/Containers/com.tencent.xinWeChat/Data/Documents/xwechat_files"))
}

pub fn read_identity(root: &Path) -> Result<Identity, String> {
    let file = root.join("all_users/config/global_config");
    let metadata = fs::metadata(&file)
        .map_err(|e| format!("无法读取 {}：{e}；请检查路径和读取权限", file.display()))?;
    if !metadata.is_file() || metadata.len() <= 4 || metadata.len() > MAX_FILE_BYTES {
        return Err(format!("微信配置文件大小或类型无效：{}", file.display()));
    }
    let source = fs::read(&file).map_err(|e| format!("无法读取 {}：{e}", file.display()))?;
    if source.len() <= 4 || source.len() as u64 > MAX_FILE_BYTES {
        return Err("微信配置文件大小无效".into());
    }
    let decoded = decrypt(
        Cipher::aes_128_cfb128(),
        b"xwechat_crypt_ke",
        Some(&[0; 16]),
        &source[4..],
    )
    .map_err(|e| format!("无法解密微信配置：{e}"))?;
    parse_identity(&decoded)
        .ok_or_else(|| "未能从微信配置中识别账号；请确认使用微信 4.0 且已在本机登录过".into())
}

fn varint(bytes: &[u8], offset: usize) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for i in 0..5 {
        let byte = *bytes.get(offset.checked_add(i)?)?;
        value |= ((byte & 0x7f) as usize) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, offset + i + 1));
        }
    }
    None
}

fn record(bytes: &[u8], offset: usize) -> Option<(&str, &[u8], usize)> {
    let (key_len, start) = varint(bytes, offset)?;
    if !(1..=128).contains(&key_len) {
        return None;
    }
    let end = start.checked_add(key_len)?;
    let key = std::str::from_utf8(bytes.get(start..end)?).ok()?;
    if !key.starts_with("mmkv_key_") || !key.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return None;
    }
    let (len, start) = varint(bytes, end)?;
    if !(1..=MAX_RECORD_BYTES).contains(&len) {
        return None;
    }
    let end = start.checked_add(len)?;
    Some((key, bytes.get(start..end)?, end))
}

fn text_value(bytes: &[u8]) -> Option<&str> {
    let (len, start) = varint(bytes, 0)?;
    if start.checked_add(len)? != bytes.len() {
        return None;
    }
    let text = std::str::from_utf8(&bytes[start..])
        .ok()?
        .trim_end_matches('\0')
        .trim();
    if text.is_empty()
        || text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return None;
    }
    Some(text)
}

fn parse_identity(bytes: &[u8]) -> Option<Identity> {
    let (mut wxid, mut nickname) = (None, None);
    let mut offset = 0;
    while offset < bytes.len() {
        if let Some((key, value, end)) = record(bytes, offset) {
            if let Some(value) = text_value(value) {
                match key {
                    "mmkv_key_user_name" => wxid = Some(value.to_owned()),
                    "mmkv_key_nick_name" => nickname = Some(value.to_owned()),
                    _ => {}
                }
            }
            offset = end;
        } else {
            offset += 1;
        }
    }
    let wxid: String = wxid?;
    if !(3..=128).contains(&wxid.len())
        || !wxid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some(Identity { wxid, nickname })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_original_project_encrypted_fixture() {
        let hex = "01000000ef788dc81fe141bab423a7c6991aa568a1b9f374deea563445c17edb66b6c34719f3f04efe9de2b1c9e4143d7d341655cf10b879f57029354e64387985b88872871068321a7ff799ca71949231964b7fcbb20589fa1f1b0f144a145870e6bc8f9f56d8430bf7ce9dabe396a3f70dd7d8aadcf1e23216c32ee80689661d78330020f23f";
        let data: Vec<u8> = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let decoded = decrypt(
            Cipher::aes_128_cfb128(),
            b"xwechat_crypt_ke",
            Some(&[0; 16]),
            &data[4..],
        )
        .unwrap();
        let identity = parse_identity(&decoded).unwrap();
        assert_eq!(identity.wxid, "fixture_account");
        assert_eq!(identity.nickname.as_deref(), Some("测试账号"));
    }
    #[test]
    fn rejects_invalid_and_truncated_data() {
        assert!(parse_identity(b"garbage").is_none());
        assert!(record(&[0x80], 0).is_none());
        assert!(text_value(&[5, b'a']).is_none());
    }
}
