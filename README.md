<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="design/brand/aogiri-horizontal-dark.svg">
    <img src="design/brand/aogiri-horizontal-light.svg" alt="Aogiri · 梧桐" width="440">
  </picture>

  <h3>LynShen CLI</h3>

  <p>在终端里运行的编程智能体，也是 Aogiri 桌面端和手机端的引擎。</p>

  <p>
    <a href="https://github.com/Libra1337/Aogiri-CLI/releases/latest">下载</a> ·
    <a href="https://github.com/Libra1337/Aogiri-CLI/releases">全部版本</a> ·
    <a href="https://github.com/Libra1337/Aogiri-CLI/issues">问题反馈</a>
  </p>

  <p><strong>简体中文</strong> · <a href="README.en-US.md">English</a></p>
</div>

命令是 `lynshen`，用 Rust 写成。在项目目录里启动它，用自然语言让它读代码、改代码、运行命令和测试。同一个可执行文件还提供后台服务 `lynshen daemon`：[Aogiri 桌面端](https://github.com/Libra1337/LynShen_Agent)和手机上的每个会话都在里面运行，可以是 LynShen、Claude Code、Codex 或 [ACP](https://agentclientprotocol.com) 智能体。支持 macOS、Linux 和 Windows。

## 安装

| 平台 | GitHub Releases 文件 |
| --- | --- |
| macOS（Apple 芯片） | `lynshen-aarch64-apple-darwin` |
| macOS（Intel） | `lynshen-x86_64-apple-darwin` |
| Linux x64 | `lynshen-x86_64-unknown-linux-gnu` |
| Windows x64 | `lynshen-x86_64-pc-windows-msvc.exe` |

下载后放进 PATH 里的目录。例如 Apple 芯片的 Mac：

```sh
curl -fLo lynshen https://github.com/Libra1337/Aogiri-CLI/releases/latest/download/lynshen-aarch64-apple-darwin
chmod +x lynshen
mkdir -p ~/.local/bin && mv lynshen ~/.local/bin/
lynshen --version
```

同样的文件也在 LynShen 软件库：`https://software.lynshen.org/cli/<版本>/<文件名>`。

- **已经装了 Aogiri 桌面端：** 桌面端自带一份 CLI（`~/.lynshen/bin`），随应用更新。在「设置 → 编码智能体」里点「在终端中使用 lynshen 命令」即可。
- **Linux：** 先安装 `bubblewrap`（例如 `sudo apt install bubblewrap`）。智能体的 shell 命令在它的沙箱里运行，没有它命令会失败；LynShen 不会退回到无沙箱运行。
- **npm：** 发布流程已经会打包 `@lynshen/cli`，但 npm 上还没有这个包。上架前请用上面的二进制。

**更新。** `lynshen`（TUI）和 `lynshen serve` 启动时检查软件库。有新版本时，release 构建会在后台下载并替换自己，下次启动生效；debug 构建只提示。版本清单有 Ed25519 签名，文件按 sha256 校验后才替换。手动更新用 `lynshen update`。在 `~/.lynshen/config.json` 里设 `"auto_update": false` 后只提示，不自动安装。桌面端那份 CLI 随桌面端更新。

## 开始使用

1. **启动。** 在项目目录里运行 `lynshen`。第一次运行会创建 `~/.lynshen/config.json`，默认服务商是 LynShen 网关，默认模型 `gpt-5.5`。
2. **登录或配置模型。** 在 TUI 里输入 `/login`，从列表里选一个服务商：
   - **LynShen 账号：** `/login lynshen`。浏览器打开 [LynShen 官网](https://www.lynshen.org)，同意授权后回到终端。之后用账号余额调用模型，`/usage` 查看套餐、余额和用量。
   - **自己的 API Key：** `/login <服务商> <key>`，例如 `/login deepseek sk-...`。`/login list` 列出全部服务商，`lynshen providers` 以 JSON 打印它们和模型。
   - **订阅账号：** 部分服务商支持浏览器登录，例如 `/login openai-codex` 用 ChatGPT 订阅。
3. **发送任务。** 直接用中文或英文描述，例如「修复 `cargo test` 里失败的那个测试，并说明原因」。

用 LynShen 账号登录时，CLI 只取得这台设备的授权凭据，不会取得账号密码。`lynshen logout` 退出登录并吊销这份凭据。

## 日常使用

- **掌握操作权限：** 按 `Shift+Tab` 依次切换审批模式 `manual` → `plan` → `auto-edit` → `auto` → `full-access`，或用 `/permissions <模式>`。收到审批请求时，可以允许一次、本会话内始终允许或拒绝。详见下方「审批和沙箱」。
- **终端界面（TUI）：** `!` 开头的输入直接在本机 shell 执行（不发给模型），`@` 加几个字符模糊选择文件，`/image <路径>` 附图片，`Ctrl+T` 切换推理强度，`Shift+Enter` 换行，`PageUp`/`PageDown` 翻看历史。`~/.lynshen/commands/*.md` 里的每个文件变成一个 `/文件名` 命令，文件里的 `$ARGUMENTS` 换成命令后面的文字。
- **智能体工作时继续说：** 回合进行中发出的消息会排队，在下一回合开始。输入框为空时按 `↑` 把最后一条排队消息取回来修改；`Esc` 中断当前回合，排队的消息照常运行。桌面端和 `serve` 协议还可以把排队消息插入当前回合（steer），不打断正在运行的工具。
- **回退和分叉：** `/rewind` 把对话和本会话改过的文件退回到之前某一回合；`/tree` 查看对话分支树，`/fork`、`/checkout` 在同一会话里开分支和切换。桌面端的「从这里分叉」由 daemon 把对话复制成一个独立的新会话，可以放进新的 git worktree。
- **多种引擎：** daemon 托管 LynShen、Claude Code、Codex 和 ACP 智能体的会话，并转成同一种事件流。OpenCode（1.x）、oh-my-pi 和 Pi 是内置 ACP 智能体，装好就能用，不用注册命令；缺少或版本不对时，错误会写明要装什么。Pi 从不询问，只能在全自动模式下运行。其他 ACP 智能体可以自己注册。
- **后台服务：** `lynshen daemon` 在 `ws://127.0.0.1:7788` 上同时托管多个会话，客户端都断开后会话继续运行。`lynshen daemon install` 让它登录时自动启动（macOS 用 launchd，Linux 用 systemd 用户服务），`uninstall` 取消。令牌在 `~/.lynshen/daemon/token`，日志在 `~/.lynshen/daemon/daemon.log`。
- **定时任务：** 长期 Agent（`~/.lynshen/agents/<id>/`）可以按时间自动收到任务：一次、每小时、每天、工作日或每周。在桌面端创建和管理，手机上可以查看和立即运行。每次运行可以开新会话（可以放进新的 git worktree），也可以接着上次的会话，还可以设花费上限；连续失败 3 次自动关闭，保留最近 20 次运行记录。有任务在运行时，daemon 阻止电脑空闲休眠。Agent 自己提议的任务要你打开后才生效。
- **技能和 MCP：** 技能从 `~/.lynshen/skills`、`~/.agents/skills` 和已信任项目的 `.lynshen/skills`、`.agents/skills` 读取，用 `/skills` 列出、安装、更新、启用或停用（见 [docs/skills.md](docs/skills.md)）。MCP 服务器写在 `config.json` 的 `mcp_servers` 里，支持 stdio 和 HTTP，用 `/mcp` 管理（见 [docs/mcp.md](docs/mcp.md)）。项目里的本地资源要先用 `/trust yes` 信任这个项目才会加载。
- **用自己的 API 跑 Claude Code 和 Codex：** daemon 可以让 Claude Code 或 Codex 会话使用你提供的 API（地址、密钥、模型），在桌面端「设置 → 编码智能体」添加。请求只发到这个提供商，只带它自己的密钥，不带 LynShen 令牌或任何 `X-LynShen-*` 请求头，也不计入 LynShen 账号的用量。会话一直使用开始时选定的提供商。见 [docs/daemon-protocol.md](docs/daemon-protocol.md) 的 "Custom providers"。
- **无界面和协议模式：** `lynshen --headless "<任务>"` 运行一个任务并输出 JSONL，最后一个事件是 `final_result`；默认 `manual`，需要审批的操作会被拒绝，要改文件或运行命令就传 `--approval-mode`。`lynshen serve` 通过标准输入输出收发 JSON（[docs/serve-protocol.md](docs/serve-protocol.md)），`lynshen acp` 把 LynShen 接到 Zed 等支持 ACP 的编辑器（[docs/serve-vs-acp.md](docs/serve-vs-acp.md)）。

### 常用命令

| 命令 | 作用 |
| --- | --- |
| `/model [模型] [强度]` | 查看或切换模型和推理强度 |
| `/permissions [模式]` | 查看或切换审批模式 |
| `/sandbox [模式]` | 查看或切换 shell 命令的沙箱 |
| `/style [风格] [--global]` | 查看或切换回复风格；`--global` 同时设为新对话的默认 |
| `/resume [会话 ID]` | 列出或恢复以前的会话（也可以 `lynshen --resume <id>`） |
| `/rewind [回合 ID]` | 把对话和文件退回到之前的某一回合 |
| `/context`、`/compact` | 查看上下文用量；立即压缩上下文 |
| `/goal <目标>` | 设置一个跨回合的目标 |
| `/skills`、`/mcp` | 管理技能和 MCP 服务器 |
| `/usage` | LynShen 账号的套餐、余额和用量 |
| `/doctor` | 检查运行环境 |
| `/new`、`/quit` | 新会话；退出 |

### 审批和沙箱

| 模式 | 改文件 | 运行 shell 命令 |
| --- | --- | --- |
| `manual`（默认） | 询问 | 询问 |
| `plan` | 不改，只读调研后交一份计划 | 只读 |
| `auto-edit` | 直接改 | 询问 |
| `auto` | 直接改 | 安全模型判断为安全的直接运行，其余询问 |
| `full-access` | 直接改 | 直接运行，不询问 |

计划出来后，可以批准并选 `auto-edit`、`manual` 或 `auto` 开始执行，也可以写下要改的地方让它重出一版；按 `Esc` 先放着，之后用 `/plan` 调出来。

在 macOS（Seatbelt）和 Linux（bubblewrap）上，shell 命令在沙箱里运行。默认 `workspace-write` 只允许写工作目录、你配置的目录、临时目录和包缓存目录，其中 `.git` 只读；`~/.ssh`、`~/.aws`、`~/.lynshen/auth.json` 等路径不可读。默认规则下 `git add`、`git commit` 可以离开沙箱，`git push` 总是先问你。Windows 默认没有系统沙箱，有一个可选的预览版。`full-access` 以你的用户权限运行，不受工作目录限制，只在信任的仓库里使用。详见 [docs/sandbox.md](docs/sandbox.md)。

### 自己的服务商和模型

`config.json` 里的 `provider`、`base_url`、`protocol`（`responses`、`anthropic` 或 `chat`）和 `models` 描述 LynShen 引擎当前使用的服务商：

```json
{
  "provider": "my-llm",
  "base_url": "https://llm.example.com/v1",
  "protocol": "chat",
  "model": "my-model",
  "models": [
    {
      "name": "my-model",
      "context_window": 200000,
      "max_output_tokens": 32000,
      "reasoning_efforts": ["low", "medium", "high"],
      "input_modalities": ["text", "image"]
    }
  ]
}
```

每个模型还可以写 `display_name` 和 `reasoning`（`false` 时请求不带思考参数）。`reasoning_efforts` 可选 `none`、`low`、`medium`、`high`、`xhigh`、`max`、`ultra`；没有 `image` 时，发给模型的图片换成一行文字说明。字段说明见 [docs/serve-protocol.md](docs/serve-protocol.md)。

## 远程访问

远程访问默认关闭。手机通过以下方式连接电脑上的 daemon：

- **中继 v2（账号）：** 这台电脑的桌面端登录了 LynShen 账号时，daemon 打开中继后注册为账号下的一台电脑，只有登录同一账号的手机能连接。新手机可以用配对码，也可以发起连接请求，在桌面端确认。连不上 v2 时，daemon 暂时退回 v1，10 分钟后再试。
- **中继 v1（配对）：** `lynshen daemon relay on` 打开中继（`off` 关闭，`status` 查看），`lynshen daemon pair` 打印一次性配对链接，5 分钟内在手机上打开。然后在电脑上核对两边显示的 6 位校验码并确认（桌面端完成）。
- **局域网和 Tailscale 直连（高级）：** daemon 保持监听 `127.0.0.1`，用 `tailscale serve` 或反向代理以 HTTPS 暴露端口。新的直连配对需要电脑已登录 LynShen 账号。

中继连接端到端加密（Noise），中继服务读不到内容。已配对的设备可以随时吊销；设备默认只能查看、发消息和处理普通审批，全自动、离开沙箱等权限要在电脑上单独授予。协议见 [docs/daemon-protocol.md](docs/daemon-protocol.md) 和 [docs/relay-protocol.md](docs/relay-protocol.md)。daemon 还可以接 Telegram、Discord 和 QQ 机器人，见 [docs/chat-connectors.md](docs/chat-connectors.md)。

## 数据与隐私

- 提示词、代码和附件发给当前会话使用的模型服务商：LynShen 网关，或你配置的服务商。Claude Code 和 Codex 使用自定义提供商时，请求只发给那个提供商，不带 LynShen 账号信息。
- 凭据默认以明文保存在 `~/.lynshen/auth.json`（权限 0600）。在 `config.json` 里设 `"encrypt_secrets": true` 可启用静态加密，但它不能防御以你的身份运行的程序，启用前请阅读 [docs/secrets.md](docs/secrets.md)。
- 登录 LynShen 账号后，daemon 会把每一轮的会话 ID、引擎、模型、请求数、Token 用量和起止时间上传到账号，用于在各台电脑上显示同样的统计；不上传提示词、代码或项目路径。自定义提供商的轮次只记在本机，不上传，也不算作网关用量。
- 发给 LynShen 账号接口的部分请求（中继 v2、反馈，以及桌面端登录后的网关请求）带一个设备标识，用于按电脑计算账号的设备数。它是硬件 ID 的 HMAC 哈希，不含原始 ID。
- `web_search` 依次尝试：模型服务商自带的搜索、LynShen 网关的搜索，最后从本机请求 Bing、DuckDuckGo、搜狗的网页结果。
- CLI 从 `software.lynshen.org` 检查和下载更新。聊天机器人的消息由 daemon 直接与聊天平台收发，不经过 LynShen 服务器。

## 遇到问题

- 先运行 `/doctor` 检查运行环境。日志在 `~/.lynshen/logs/lynshen.log`，daemon 日志在 `~/.lynshen/daemon/daemon.log`。
- **Linux 上 shell 命令全部失败：** 安装 `bubblewrap`。Ubuntu 24.04 及以后版本还要允许非特权用户命名空间（`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`，或为 bwrap 配置 AppArmor 规则），或者切到 `full-access`。
- **macOS 拦截用浏览器下载的二进制：** 运行 `xattr -d com.apple.quarantine <文件>`。用 `curl` 下载不会被拦截。

其他问题请到 [GitHub Issues](https://github.com/Libra1337/Aogiri-CLI/issues) 提交，附上 `lynshen --version` 的输出、系统和日志中相关的部分；不要粘贴 API Key 或访问令牌。桌面端用户也可以在「设置 → 通用 → 反馈问题」里提交。

## 从源码构建

需要 Rust 稳定版。`crates/llm-provider-kit` 是 git 子模块，克隆时带上 `--recurse-submodules`：

```sh
git clone --recurse-submodules https://github.com/Libra1337/Aogiri-CLI.git
cd Aogiri-CLI
cargo build --release
./target/release/lynshen --version
```

设置了 `CARGO_TARGET_DIR` 时，产物在 `$CARGO_TARGET_DIR/release/`，不在 `./target/`。多个工作树共用一个 `CARGO_TARGET_DIR` 可以省下重复编译。

中继服务在 `relay/`，用 Go 编写：`cd relay && go build -o lynshen-relay .`，见 [relay/README.md](relay/README.md)。

常用检查：

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

在 Linux 上运行全部测试需要 `ripgrep`、`bubblewrap`，并允许非特权用户命名空间。代码结构：`src/main.rs` 是命令行入口；`crates/agent-core` 是智能体循环、工具、配置、沙箱、技能和 MCP；`crates/tui` 是终端界面；`crates/daemon` 是后台服务；`crates/llm-provider-kit` 是各服务商的协议和模型目录。开发约定见 [AGENTS.md](AGENTS.md)，发布流程见 [docs/releasing.md](docs/releasing.md) 和 [docs/software-library.md](docs/software-library.md)。

## 许可证

代码采用 [Apache License 2.0](LICENSE)，另见 [NOTICE](NOTICE)。`crates/llm-provider-kit` 包含来自 [oh-my-pi](https://github.com/can1357/oh-my-pi) 的代码和数据，使用 MIT 许可证，见[它的 NOTICE](https://github.com/Libra1337/llm-provider-kit/blob/main/NOTICE)。LynShen 名称和图标的商标权归 LynShen Innovations INC. 所有；第三方智能体及工具归各自所有者所有。
