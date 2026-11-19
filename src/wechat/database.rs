use openssl::{
    hash::MessageDigest,
    pkcs5::pbkdf2_hmac,
    pkey::PKey,
    sign::Signer,
    symm::{Cipher, Crypter, Mode},
};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

const PAGE: usize = 4096;
const RESERVE: usize = 80;
const HEADER: &[u8; 16] = b"SQLite format 3\0";

fn derive(key: &[u8], salt: &[u8], raw: bool) -> Result<([u8; 32], [u8; 32]), String> {
    let mut encryption = [0u8; 32];
    if raw {
        encryption.copy_from_slice(key);
    } else {
        pbkdf2_hmac(key, salt, 256_000, MessageDigest::sha512(), &mut encryption)
            .map_err(|e| e.to_string())?;
    }
    let mac_salt: Vec<_> = salt.iter().map(|b| b ^ 0x3a).collect();
    let mut mac_key = [0; 32];
    pbkdf2_hmac(
        &encryption,
        &mac_salt,
        2,
        MessageDigest::sha512(),
        &mut mac_key,
    )
    .map_err(|e| e.to_string())?;
    Ok((encryption, mac_key))
}

fn valid_mac(page: &[u8], number: u32, mac_key: &[u8]) -> Result<bool, String> {
    let start = if number == 1 { 16 } else { 0 };
    let end = PAGE - RESERVE + 16;
    let key = PKey::hmac(mac_key).map_err(|e| e.to_string())?;
    let mut signer = Signer::new(MessageDigest::sha512(), &key).map_err(|e| e.to_string())?;
    signer
        .update(&page[start..end])
        .map_err(|e| e.to_string())?;
    signer
        .update(&number.to_le_bytes())
        .map_err(|e| e.to_string())?;
    let mac = signer.sign_to_vec().map_err(|e| e.to_string())?;
    Ok(openssl::memcmp::eq(&mac, &page[end..end + 64]))
}

fn decrypt_page(page: &[u8], number: u32, key: &[u8]) -> Result<Vec<u8>, String> {
    let start = if number == 1 { 16 } else { 0 };
    let end = PAGE - RESERVE;
    let mut cipher = Crypter::new(
        Cipher::aes_256_cbc(),
        Mode::Decrypt,
        key,
        Some(&page[end..end + 16]),
    )
    .map_err(|e| e.to_string())?;
    cipher.pad(false);
    let mut plain = vec![0; end - start + 16];
    let n = cipher
        .update(&page[start..end], &mut plain)
        .map_err(|e| e.to_string())?;
    let n = n + cipher
        .finalize(&mut plain[n..])
        .map_err(|e| e.to_string())?;
    plain.truncate(n);
    let mut result = Vec::with_capacity(PAGE);
    if number == 1 {
        result.extend_from_slice(HEADER);
    }
    result.extend_from_slice(&plain);
    result.extend_from_slice(&page[end..]);
    Ok(result)
}

/// Read and authenticate individual pages of a stable SQLCipher database.
/// This does not interpret WAL files or provide a transactional snapshot.
struct PageReader {
    source: File,
    page_count: u32,
    encryption: [u8; 32],
    mac: [u8; 32],
}

impl PageReader {
    fn open(path: &Path, hex: &str) -> Result<Self, String> {
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("数据库密钥必须是 64 位十六进制字符".into());
        }
        let key: Vec<u8> = (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        let mut source = File::open(path).map_err(|e| format!("无法打开数据库：{e}"))?;
        let len = source.metadata().map_err(|e| e.to_string())?.len();
        if len == 0 || len % PAGE as u64 != 0 || len / PAGE as u64 > u32::MAX as u64 {
            return Err("数据库大小不符合支持的 4096 字节分页格式".into());
        }
        let mut first = [0; PAGE];
        source.read_exact(&mut first).map_err(|e| e.to_string())?;
        let mut keys = None;
        for raw in [false, true] {
            let candidate = derive(&key, &first[..16], raw)?;
            if valid_mac(&first, 1, &candidate.1)? {
                keys = Some(candidate);
                break;
            }
        }
        let (encryption, mac) =
            keys.ok_or("数据库密钥不匹配，或数据库格式不是支持的 SQLCipher 4 格式")?;
        let plain = decrypt_page(&first, 1, &encryption)?;
        if plain[16..18] != [0x10, 0x00] || plain[20] != RESERVE as u8 {
            return Err("解密后的 SQLite 页头无效".into());
        }
        Ok(Self {
            source,
            page_count: (len / PAGE as u64) as u32,
            encryption,
            mac,
        })
    }

    fn read_page(&mut self, number: u32) -> Result<Vec<u8>, String> {
        if number == 0 || number > self.page_count {
            return Err(format!("数据库页号无效：{number}"));
        }
        let mut page = [0; PAGE];
        self.source
            .seek(SeekFrom::Start((number as u64 - 1) * PAGE as u64))
            .map_err(|e| e.to_string())?;
        self.source
            .read_exact(&mut page)
            .map_err(|e| e.to_string())?;
        if !valid_mac(&page, number, &self.mac)? {
            return Err(format!(
                "第 {number} 页完整性校验失败；数据库可能已损坏或正在变化"
            ));
        }
        decrypt_page(&page, number, &self.encryption)
    }
}

pub fn decrypt(input: &Path, output: &Path, hex: &str) -> Result<(), String> {
    let mut wal = input.as_os_str().to_os_string();
    wal.push("-wal");
    match fs::metadata(&wal) {
        Ok(meta) if meta.len() > 0 => {
            return Err(
                "发现非空 WAL 文件；请先退出微信并等待数据库检查点完成，再重试，避免导出不完整数据"
                    .into(),
            );
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("无法检查数据库 WAL：{e}"));
        }
        _ => {}
    }
    let mut reader = PageReader::open(input, hex)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut dest = options
        .open(output)
        .map_err(|e| format!("无法创建输出文件（不会覆盖已有文件）：{e}"))?;
    let result = (|| -> Result<(), String> {
        for number in 1..=reader.page_count {
            dest.write_all(&reader.read_page(number)?)
                .map_err(|e| e.to_string())?;
        }
        dest.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    })();
    drop(dest);
    if result.is_err() {
        let _ = fs::remove_file(output);
    }
    result
}
