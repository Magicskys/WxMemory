# WxMemory

一个使用 Rust 编写的微信本地消息监听工具。它从本机微信 4.x 数据库中读取新消息，并通过 HTTP Server-Sent Events（SSE）被动推送给订阅客户端，适合将个人聊天和群消息接入自己的本地服务。

项目目前处于早期开发阶段，当前测试的微信版本为 **macOS 微信 4.1.13.63**。微信数据库结构可能随客户端版本变化，其他版本的兼容性尚未充分验证。

## 功能

- **被动推送**：监听当前账号的新消息，通过 SSE 提供实时订阅。
- **会话白名单**：按个人或群聊筛选订阅内容，也可选择接收全部会话。
- **历史补发**：传入时间戳，查询本地保留的历史消息后继续接收实时消息。
- **多账号配置**：按 wxid 分别保存密钥和白名单，切换当前账号。
- **会话列表**：在终端查看会话名称、ID 和最近聊天时间。

## 准备工作

- 在本机使用微信 4.1.13.63，并确保程序能读取微信数据目录。
- 自行准备账号对应的 **数据库密钥**、**图片 AES 密钥**和**图片 XOR 密钥**。当前 `setup` 需要输入这三项，项目不会自动获取密钥；图片密钥目前仅保存，尚未用于解密图片。

默认账号发现路径针对 macOS；其他路径可以在 `setup` 时手动指定。当前密钥隐藏输入依赖 Unix 终端，Windows 等其他平台尚未完整支持或验证。

## 快速开始

在项目目录编译：

```sh
cargo build --release
```

下面的命令使用 `cargo run --`；编译后也可以用 `target/release/wx_memo` 运行。

### 1. 配置账号

```sh
cargo run -- setup
```

程序会列出本机发现的账号目录，输入编号后填写密钥。wxid 如果由目录名推测，会提示核对。非默认微信数据目录可以作为参数传入：

```sh
cargo run -- setup /path/to/xwechat_files
```

数据库密钥为 64 位十六进制字符（可带 `0x` 前缀），图片 AES 密钥为 16 个 ASCII 字符，图片 XOR 密钥为 `0x` 加两位十六进制数。程序暂不自动验证 wxid 与密钥是否匹配。

配置默认写入 `~/.config/wx_memo/config.toml`；如果设置了绝对路径的 `XDG_CONFIG_HOME`，则使用 `$XDG_CONFIG_HOME/wx_memo/config.toml`。**密钥以明文保存在本地配置中**，Unix 文件权限设置为 `0600`；不要分享或提交配置文件。

### 2. 查看会话并设置白名单

```sh
cargo run -- sessions
cargo run -- watchlist add wxid_example 123456789@chatroom
cargo run -- watchlist list
```

`watchlist add` 支持一次输入多个会话 ID：以 `@chatroom` 结尾的识别为群聊，其余识别为个人会话。也可以在 ID 前显式指定 `user` 或 `group`。如果本次有无效或重复条目，整个批次不会保存。

```sh
cargo run -- watchlist remove wxid_example
```

`whitelist` 是 `watchlist` 的别名。白名单按账号分别保存；为空时，默认订阅不会收到消息。

### 3. 启动监听并订阅

```sh
cargo run -- watch
```

在另一个终端订阅：

```sh
curl -N http://127.0.0.1:8787/api/events
```

默认监听 `127.0.0.1:8787`，可以指定地址：

```sh
cargo run -- watch --bind 127.0.0.1:9000
```

接口目前没有身份验证。绑定到非本机地址后，能够访问端口的客户端可以订阅消息；仅应向可信客户端开放。

## SSE 接口

`GET /api/events` 返回 `Content-Type: text/event-stream`。每条消息使用 `message` 事件，`data` 是单行 JSON，事件之间以空行分隔：

```text
event: message
data: {"chat_kind":"group","chat_id":"123456789@chatroom","sender_id":"wxid_sender","content":"你好","timestamp":1730000001}

```

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `chat_kind` | 字符串 | `user` 为个人聊天，`group` 为群聊。 |
| `chat_id` | 字符串 | 会话 ID；群聊 ID 通常以 `@chatroom` 结尾。 |
| `sender_id` | 字符串 | 发送者微信 ID；无法识别时为空字符串。 |
| `content` | 字符串 | 消息内容；非文本消息可能无法直接显示。 |
| `timestamp` | 整数 | 消息时间，Unix 秒时间戳。 |

### 查询参数

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `watchlist` | `true` | `true` 只推送白名单会话；`false` 推送所有会话。 |
| `since` | 不设置 | Unix 秒时间戳；先补发本地数据库中时间严格大于该值的历史消息，再继续接收符合条件的实时消息。 |

```sh
# 接收所有会话
curl -N 'http://127.0.0.1:8787/api/events?watchlist=false'

# 补发指定时间之后的消息，再接收实时消息
curl -N 'http://127.0.0.1:8787/api/events?since=1730000000'

# 参数可以组合
curl -N 'http://127.0.0.1:8787/api/events?watchlist=false&since=1730000000'
```

历史补发在每次连接时执行一次，补发与实时消息按数据库分片、消息表和消息 ID 去重。单次补发最多 100,000 条，超过上限或查询失败返回 HTTP 500；历史查询期间实时缓冲区溢出返回 HTTP 503。参数无效返回 HTTP 400。客户端处理过慢时连接可能关闭。

## 工作方式与当前限制

`watch` 启动时记录已有消息的位置，不主动推送历史消息；之后每两秒只读查询本地数据库，并把新增消息交给 SSE。传入 `since` 的订阅会另外查询历史数据库。游标保存在内存中，重启后会重新建立基线；新分片中与启动时间处于同一秒的历史记录可能被纳入。

## 其他命令

```sh
cargo run -- account list                 # 查看已配置账号
cargo run -- account use wxid_example     # 切换当前账号
cargo run -- db-check                     # 检查当前账号的数据库连接
cargo run -- decrypt /path/to/encrypted.db /path/to/decrypted.db
cargo run -- --help                       # 查看全部命令
```

`setup` 保存的账号会成为当前账号；运行中的 `watch` 不会自动切换账号或重新加载白名单，需要重启。`decrypt` 输出到新文件，不覆盖现有文件；遇到非空 WAL 时需要先退出微信并等待检查点完成。

## 开发

```sh
cargo fmt --check
cargo test
cargo clippy -- -D warnings
```

```text
src/
├── app/       命令行和终端展示
├── config/    多账号 TOML 配置
├── wechat/    本地账号和数据库访问
├── monitor/   消息轮询与白名单
└── push/      SSE 推送
```

## 许可证

本项目代码采用 [0BSD](LICENSE) 许可证。第三方依赖适用各自的许可证。
