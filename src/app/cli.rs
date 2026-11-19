use super::{
    args::{AccountAction, Args, Command, WatchlistAction},
    prompt::prompt,
    table,
};
use crate::{
    config, monitor,
    monitor::whitelist,
    wechat::{accounts, database, identity, live, sessions},
};
use clap::Parser;
use std::path::PathBuf;

fn setup(root: Option<PathBuf>) -> Result<(), String> {
    let path = config::path()?;
    let mut config = if path.exists() {
        config::load(&path)?
    } else {
        config::Config::default()
    };
    let root = root.map(Ok).unwrap_or_else(identity::default_data_root);
    let found = match root.and_then(|root| accounts::discover(&root)) {
        Ok(found) => found,
        Err(error) => {
            eprintln!("无法自动发现账号：{error}");
            Vec::new()
        }
    };
    let (wxid, account_root) = if found.is_empty() {
        eprintln!("未发现本地账号目录，请手动输入微信用户名。");
        (prompt("微信用户名 wxid: ", false)?, None)
    } else {
        println!("发现以下本地微信账号：");
        for (index, account) in found.iter().enumerate() {
            let wxid = account
                .wxid
                .as_deref()
                .unwrap_or("无法识别，选择后手动输入");
            let source = if account.from_profile {
                "本地配置"
            } else {
                "目录推测，请核对"
            };
            println!(
                "  {}. {} （目录: {}，{}）",
                index + 1,
                wxid,
                account.directory,
                source
            );
        }
        let index = loop {
            let input = prompt("请输入账号编号: ", false)?;
            match input.parse::<usize>() {
                Ok(number) if number > 0 && number <= found.len() => break number - 1,
                _ => eprintln!("编号无效，请输入 1 到 {} 之间的数字。", found.len()),
            }
        };
        let account = &found[index];
        let wxid = match &account.wxid {
            Some(wxid) if account.from_profile => wxid.clone(),
            Some(wxid) => {
                let input = prompt(
                    &format!("请核对 wxid（回车使用 {wxid}，或输入正确值）: "),
                    false,
                )?;
                if input.is_empty() {
                    wxid.clone()
                } else {
                    input
                }
            }
            None => prompt("无法从目录确定 wxid，请手动输入: ", false)?,
        };
        (wxid, Some(account.root.clone()))
    };
    config::validate_wxid(&wxid)?;
    let database = prompt("数据库密钥（64 位十六进制，输入不显示）: ", true)?;
    let whitelist = config
        .accounts
        .get(&wxid)
        .map(|account| account.whitelist.clone())
        .unwrap_or_default();
    let account = config::AccountConfig {
        account_root,
        database_key: database
            .strip_prefix("0x")
            .or_else(|| database.strip_prefix("0X"))
            .unwrap_or(&database)
            .to_owned(),
        image_aes_key: prompt("图片 AES 密钥（16 个 ASCII 字符，输入不显示）: ", true)?,
        image_xor_key: prompt("图片 XOR 密钥（例如 0x40）: ", true)?,
        whitelist,
    };
    config.accounts.insert(wxid.clone(), account);
    config.active_account = Some(wxid);
    config::save(&path, &config)?;
    println!("配置已保存到 {}（密钥以明文保存）", path.display());
    Ok(())
}

pub fn run() -> Result<(), String> {
    let args = Args::parse();
    match args.command {
        None => show_identity(match args.root {
            Some(root) => root,
            None => identity::default_data_root()?,
        }),
        Some(Command::Setup { root }) => setup(root),
        Some(Command::Account { action }) => account_command(action),
        Some(Command::Watchlist { action }) => whitelist_command(action),
        Some(Command::Watch { bind }) => {
            let config = config::load(&config::path()?)?;
            let account = config.active()?;
            let root = account
                .account_root
                .as_deref()
                .ok_or("当前账号没有配置账号目录，请运行 wx_memo setup 并选择本地账号")?;
            monitor::start(bind, root, &account.database_key, account.whitelist.clone())
        }
        Some(Command::Sessions) => {
            let config = config::load(&config::path()?)?;
            let account = config.active()?;
            let root = account
                .account_root
                .as_deref()
                .ok_or("当前账号没有配置账号目录，请运行 wx_memo setup 并选择本地账号")?;
            let (items, warning) = sessions::list(root, &account.database_key)?;
            if let Some(warning) = warning {
                eprintln!("提示: {warning}");
            }
            if items.is_empty() {
                println!("暂无会话");
            } else {
                println!("{}", table::sessions(&items));
            }
            Ok(())
        }
        Some(Command::DbCheck) => {
            let config = config::load(&config::path()?)?;
            let account = config.active()?;
            let root = account
                .account_root
                .as_deref()
                .ok_or("当前账号没有配置账号目录，请运行 wx_memo setup 并选择本地账号")?;
            let path = accounts::session_database(root)?;
            let db = live::Database::open(&path, &account.database_key)?;
            println!("数据库只读连接成功，发现 {} 张表", db.table_names()?.len());
            Ok(())
        }
        Some(Command::Decrypt { input, output }) => {
            let config = config::load(&config::path()?)?;
            database::decrypt(&input, &output, &config.active()?.database_key)?;
            println!("数据库解密完成: {}", output.display());
            Ok(())
        }
    }
}

fn account_command(action: AccountAction) -> Result<(), String> {
    let path = config::path()?;
    let mut config = config::load(&path)?;
    match action {
        AccountAction::List => {
            if config.accounts.is_empty() {
                println!("没有已配置账号");
            }
            for id in config.accounts.keys() {
                println!(
                    "{} {}",
                    if config.active_account.as_deref() == Some(id) {
                        "*"
                    } else {
                        " "
                    },
                    id
                );
            }
        }
        AccountAction::Use { wxid } => {
            if !config.accounts.contains_key(&wxid) {
                return Err("账号未配置，请先运行 setup".into());
            }
            config.active_account = Some(wxid.clone());
            config::save(&path, &config)?;
            println!("当前账号已切换为 {wxid}");
        }
    }
    Ok(())
}

fn whitelist_command(action: WatchlistAction) -> Result<(), String> {
    let path = config::path()?;
    let mut config = config::load(&path)?;
    let (ids, adding) = match action {
        WatchlistAction::List => {
            if config.active()?.whitelist.is_empty() {
                println!("白名单为空");
            }
            for entry in &config.active()?.whitelist {
                println!("{}\t{}", entry.kind, entry.id);
            }
            return Ok(());
        }
        WatchlistAction::Add { ids } => (ids, true),
        WatchlistAction::Remove { ids } => (ids, false),
    };
    let explicit = if ids.len() >= 2 && matches!(ids[0].as_str(), "user" | "group") {
        Some(whitelist::ChatKind::parse(&ids[0])?)
    } else {
        None
    };
    let ids = &ids[if explicit.is_some() { 1 } else { 0 }..];
    if !adding && ids.len() != 1 {
        return Err("remove 只能指定一个会话 ID".into());
    }
    let entries = ids
        .iter()
        .map(|id| {
            let kind = explicit
                .clone()
                .unwrap_or_else(|| whitelist::ChatKind::from_id(id));
            whitelist::Entry::new(kind, id)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for entry in entries {
        if adding {
            whitelist::add(&mut config.active_mut()?.whitelist, entry)?;
        } else {
            whitelist::remove(&mut config.active_mut()?.whitelist, &entry)?;
        }
    }
    config::save(&path, &config)?;
    println!("白名单已更新");
    Ok(())
}

fn show_identity(root: PathBuf) -> Result<(), String> {
    let account = identity::read_identity(&root)?;
    println!("本地保存的微信账号：\n用户名 (wxid): {}", account.wxid);
    if let Some(nickname) = account.nickname {
        println!("昵称: {nickname}");
    }
    println!(
        "数据目录: {}\n提示：本地账号信息不能证明微信当前处于登录状态。",
        root.display()
    );
    Ok(())
}
