use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "微信本地数据终端工具",
    args_conflicts_with_subcommands = true
)]
pub struct Args {
    /// 微信数据目录；不传时查看默认目录的本地账号
    #[arg(value_name = "微信数据目录")]
    pub root: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 选择微信账号并保存密钥
    Setup {
        #[arg(value_name = "微信数据目录")]
        root: Option<PathBuf>,
    },
    /// 管理当前账号
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// 管理监控白名单
    #[command(visible_alias = "whitelist")]
    Watchlist {
        #[command(subcommand)]
        action: WatchlistAction,
    },
    /// 启动新消息监控和 SSE 服务
    Watch {
        #[arg(long, default_value = "127.0.0.1:8787", value_name = "IP:端口")]
        bind: SocketAddr,
    },
    /// 查看当前账号的会话
    Sessions,
    /// 检查当前账号的数据库连接
    DbCheck,
    /// 将加密数据库解密到新文件
    Decrypt {
        #[arg(value_name = "加密数据库")]
        input: PathBuf,
        #[arg(value_name = "输出文件")]
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum AccountAction {
    /// 列出已配置账号
    List,
    /// 切换当前账号
    Use { wxid: String },
}

#[derive(Debug, Subcommand)]
pub enum WatchlistAction {
    /// 列出监控白名单
    List,
    /// 添加一个或多个会话；可以先指定 user 或 group
    Add {
        #[arg(required = true, num_args = 1.., value_name = "会话ID")]
        ids: Vec<String>,
    },
    /// 移除一个会话；可以先指定 user 或 group
    Remove {
        #[arg(required = true, num_args = 1..=2, value_name = "会话ID")]
        ids: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_existing_commands() {
        assert!(matches!(
            Args::try_parse_from(["wx_memo", "sessions"])
                .unwrap()
                .command,
            Some(Command::Sessions)
        ));
        assert!(
            matches!(Args::try_parse_from(["wx_memo", "whitelist", "add", "wxid_a", "1@chatroom"]).unwrap().command, Some(Command::Watchlist { action: WatchlistAction::Add { ids } }) if ids.len() == 2)
        );
        assert!(Args::try_parse_from(["wx_memo", "watchlist", "add"]).is_err());
        assert!(Args::try_parse_from(["wx_memo", "watch", "--bind", "invalid"]).is_err());
    }
}
