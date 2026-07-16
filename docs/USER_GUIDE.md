# jterm4 用户指南

## 1. 启动、诊断与恢复

jterm4 默认启动 Block 后端并恢复最近一个可领取的窗口快照：

```bash
jterm4
jterm4 ~/project
jterm4 --working-directory ~/project
jterm4 --mode vte --no-restore
jterm4 --execute bash -lc 'cargo test'
```

`--execute` 后的参数原样作为 argv，不经过额外 shell 拆词。显式 cwd、`--execute`、`--no-restore` 和 `--safe-mode` 都不会意外领取普通恢复快照；execute/safe-mode 窗口也不发布会话快照。单独使用 `--mode` 只覆盖本窗口后端，仍可恢复窗口布局。

以下命令在 GTK 初始化前完成，可用于 SSH、TTY 和 CI：

```bash
jterm4 --help
jterm4 --version
jterm4 --doctor
jterm4 --doctor --json
jterm4 --config-path
jterm4 --init-config
jterm4 --check-config
jterm4 --check-config --json
jterm4 --restore-config-backup
jterm4 --print-default-config
```

`--doctor` 报告配置语义与权限、有效轮换备份、配置写锁、显示/input 环境、可选工具、AI provider/密钥存在性、workflow 与欢迎 Notebook 发现结果、远程 SSH 就绪度以及 ready/active 快照数量，但不输出快照中的目录、标签或命令，也不会探测任何网络 endpoint。使用独立配置：

```bash
jterm4 --config ~/configs/work.toml
jterm4 --check-config ~/configs/work.toml
```

安装版还提供隐私保护的支持归档工具：

```bash
jterm4-support-bundle ~/Desktop
```

它通过脱敏诊断模式收集权限/大小元数据、聚合计数、非敏感系统特征和选定环境变量是否存在，不打包配置正文、历史、会话、终端输出、剪贴板、API key、SSH 目标、主机名或本地路径，也不会发起网络请求。归档权限为 `0600`；发送给他人前仍应检查每个文件。

`jterm4 --safe-mode` 完全跳过用户配置及 `JTERM4_*` 外观/行为覆盖，使用内置 VTE 主题和默认快捷键；同时禁用配置重载、恢复、配置/会话持久化、历史、仓库探测、远程主机、通知、AI 和可执行 Notebook。它适合确认故障来自用户配置还是图形/终端环境，不能与 `--mode` 或 `--execute` 同时使用；即使同时给出 `--config`，该文件也不会被读取。

## 2. Shell 集成

Block 后端可在没有集成脚本时工作，但加载脚本后能通过 OSC 133/7 精确获取 prompt/command 边界、退出码和 cwd。无需查找安装路径：

```bash
# ~/.bashrc
[[ $TERM_PROGRAM == jterm4 ]] && source <(jterm4 --shell-integration bash)

# ~/.zshrc
[[ $TERM_PROGRAM == jterm4 ]] && source <(jterm4 --shell-integration zsh)
```

fish 和 PowerShell 对应 `fish`、`pwsh`。原生安装还会把四种脚本放到 `${prefix}/share/jterm4/shell-integration/`。其他终端会忽略这些 OSC 序列。

Flatpak 的交互 shell 运行在宿主机，宿主 rc 不应直接引用沙箱内的
`/app/share`。bash/zsh 可在对应 rc 中使用
`source <(flatpak run io.github.beamiter.jterm4 --shell-integration bash)`；fish
使用 `flatpak run io.github.beamiter.jterm4 --shell-integration fish | source`。
两种后端都会在读取 rc 前注入 `TERM_PROGRAM=jterm4`，因此可继续用该变量做条件保护。

## 3. 终端模式与 Pane

默认配置是：

```toml
terminal_mode = "block"
```

- `block` 把命令保存为独立块，提供退出状态、耗时、筛选、跨块搜索、历史回填和 AI 上下文。
- `vte` 是传统终端，适合要求完整滚屏语义的 TUI 或兼容性排查。

两个后端共享输入路由、字体/主题、cwd、进程检查和关闭清理。从 Block pane 发起分屏时，原 Block 会留在 pane tree 中，新 sibling 使用 VTE；VTE sibling 可继续嵌套分屏。这避免隐藏 PTY，同时保留 Block 工作区里的分屏能力。

| 操作 | 快捷键 |
|---|---|
| 左右 / 上下分屏 | `Ctrl+Shift+E` / `Ctrl+Shift+D` |
| 方向聚焦 | `Ctrl+Alt+方向键` |
| 调整大小 | `Ctrl+Alt+Shift+方向键` |
| 放大当前 Pane | `Ctrl+Shift+Z` |
| Pane 移到新标签 | `Ctrl+Shift+!` |
| 关闭当前 Pane 或标签 | `Ctrl+Shift+W` |

关闭 pane、标签、多个选中标签或窗口时，jterm4 会扫描所有后端的真实 PTY 和前台进程；存在运行中任务时先给出统一确认。缩放的 pane 会先恢复 pane tree 再关闭，避免漏掉隐藏 sibling。

分屏布局恢复仍建议与命令自身的持久化方案配合使用，尤其是 SSH/TUI 长任务。

## 4. 标签页与窗口恢复

| 操作 | 快捷键 |
|---|---|
| 新建标签 | `Ctrl+Shift+T` |
| 下一个 / 上一个标签 | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| 标签 1 到 8 / 最后一个 | `Ctrl+1`…`Ctrl+8` / `Ctrl+9` |
| 过滤标签 | `Ctrl+Shift+L` |
| 标签栏位置 | `Ctrl+Alt+B` |

标签支持拖放排序、双击重命名、固定、标记、复制和右键菜单。侧栏在 Tabs 与 Files 之间切换；开关、宽度和视图会持久化。

每个进程维护独立 active 快照。正常关闭后才原子发布为 ready；并发窗口不会读取或覆盖彼此 active 状态。后续启动逐个领取最近快照，确认 owner PID 已结束后才回收崩溃遗留的 active 快照，最多保留 32 个 ready 快照。旧版 `tabs.state` 会在首次启动时迁移。

## 5. 搜索与 Block 操作

`Ctrl+Shift+F` 打开当前标签搜索：普通文本不区分大小写，`/expression/` 使用正则；Enter/Shift+Enter 前后跳转，Escape 关闭。清空输入立即清除 VTE 和 Block 高亮。

| Block 功能 | 快捷键 |
|---|---|
| 命令历史面板 | `Ctrl+Shift+H` |
| 跨块行搜索 | `Ctrl+Shift+G` |
| 跳到首个失败块 / 最早块 | `Ctrl+Shift+X` / `Ctrl+Shift+N` |
| workflow | `Ctrl+Shift+M` |
| AI 分析选中块 | `Ctrl+Shift+Q` |
| Shell Agent | `Ctrl+Alt+G` |
| 全选 / 回填 / 清空 | `Ctrl+Shift+A` / `Ctrl+Shift+I` / `Ctrl+Shift+K` |

选择语义与 jterm1 对齐：

- `Ctrl+Up` 从最新块进入选择；普通 `Up/Down` 移动 active edge，`Shift+Up/Down` 扩展范围。
- `Enter` 或 `Ctrl+Shift+I` 按终端顺序回填所有选中命令，不自动执行；`Escape` 清除选择。
- `Ctrl+Shift+B` 收藏 active 块，`Ctrl+,` / `Ctrl+.` 在收藏块之间跳转。
- 多选右键可批量复制命令、输出、完整块或回填命令；复制按界面顺序合并。
- 长块提供顶部/底部导航与 sticky header，后台异步输出使用独立 Block 样式。

命令运行中或 alt-screen TUI 活跃时，Enter 和应用所需按键继续发送给前台进程，不会误触发旧块回填。

## 6. 统一命令面板、历史与 workflow

`Ctrl+Shift+P` 打开的面板统一模糊搜索四类来源：

| 前缀 | 来源 | 接受后的行为 |
|---|---|---|
| `>` | 应用动作与当前快捷键 | 执行动作 |
| `@` | JSONL 命令历史 | 写入编辑行，不提交 |
| `:` | YAML/TOML workflow | 填参数后写入编辑行，不提交 |
| `?` | 自然语言命令请求 | 交给 AI 生成候选，先审阅 |

JSONL 历史默认位于 `${XDG_STATE_HOME:-~/.local/state}/jterm4/history.jsonl`，只保存 command、cwd、exit code 和完成时间，不保存终端输出。文件权限为 `0600`，重复命令按最新记录展示，损坏或超限记录会跳过，文件会按上限压缩。

用户 workflow 放在 `~/.config/jterm4/workflows/`，支持 `.toml`、`.yaml`、`.yml`；也可用 `JTERM4_WORKFLOW_DIR` 增加以路径列表表示的目录。用户定义优先于已安装示例，同名项不会被示例覆盖。

安装包附带 feature branch、大文件查找、交互式 rebase、SSH 本地端口转发、Docker 日志跟随和端口进程终止示例。所有示例都只生成可编辑的单行命令；选中后不会自动执行，其中会结束进程或建立长连接的模板仍须由用户逐字审阅。

TOML 示例：

```toml
name = "Deploy"
description = "Deploy a branch"
command = "deploy --branch {branch} --env {env}"
tags = ["release"]

[[args]]
name = "branch"
default = "main"

[[args]]
name = "env"
default = "staging"
```

YAML 可使用共享格式的 `{{name}}` placeholder。未提供的必填参数不会静默执行，生成内容始终只进入当前 pane 的编辑行。为保证“只插入、不提交”，history、workflow、文件路径和 AI 候选只接受不含 CR、LF、NUL 或其他终端控制字符的单行文本；不安全条目会被拒绝并提示，而不会写入 PTY。

## 7. 可执行 Notebook

`.jtnb.md` 是普通 Markdown，其中 bash/sh/zsh/fish/pwsh/powershell/shell 或无标签 fence 可执行。双击文件树中的 Notebook 打开；内置 quick start 可在命令面板搜索 **Open welcome & quick start notebook**。

- 每个 cell 可单独 Run/Stop，也可 Run All/Stop All。
- stdout 与 stderr 分开显示，并保留 exit status；单 cell 合计输出有 256 KiB 上限。
- 显式 shell fence 使用对应解释器；`shell` 和无标签 fence 使用 jterm4 的配置 shell argv。
- 非 shell fence 只展示，不执行。
- cell 在独立进程组运行，停止、Stop All 或关闭对话框会清理完整进程组。
- 命令不会注入当前终端，也不会绕过 Notebook 自己的运行按钮；安全模式禁用执行。

安装资产位于 `${prefix}/share/jterm4/notebooks/`；Flatpak 中是 `/app/share/jterm4/notebooks/`。

## 8. 文件树

侧栏 Files 页以当前标签 cwd 为根：双击目录展开/折叠；双击普通文件把 shell 引号保护后的路径插入编辑行，不自动执行；双击 `.jtnb.md` 打开 Notebook。向上按钮进入父目录，主页按钮回到当前终端 cwd。Block 走自管 PTY 输入，VTE 走 VTE child input。

## 9. Flatpak 与桌面安装

Flatpak 应用 ID 是 `io.github.beamiter.jterm4`。打包版本通过 `flatpak-spawn --host` 启动宿主 Shell、SSH、Git、curl 和通知工具，避免命令误跑在一次性应用沙箱；因此 jterm4 Flatpak 本身不是命令隔离边界。

```bash
flatpak run io.github.beamiter.jterm4 --doctor
flatpak run io.github.beamiter.jterm4
```

文件树需要宿主文件系统权限。AI 密钥可通过可信启动器、显式 Flatpak override 或 sandbox 内可见的 owner-only 独立文件提供。完整权限说明见 `docs/FLATPAK.md`。

## 10. SSH 远程会话

jterm4 不预置主机。在配置中显式添加：

```toml
[[remote_hosts]]
name = "dev"
host = "dev.example.com"
user = "alice"
remote_shell = "rsh"
session = "dev-main"
ssh_args = ["-p", "2222"]
login_shell = true
multiplex = true
```

`Ctrl+Shift+S` 打开主机选择器。连接复用由 OpenSSH ControlMaster 完成，异常断开按上限退避重连；用户正常退出不会重连。

## 11. AI 与 Agent 安全边界

AI 总开关、provider 和 endpoint 由配置控制。支持 Anthropic、OpenAI-compatible 和 Ollama wire protocol。密钥内容不会写入 TOML；环境变量优先，也可配置独立的 owner-only 密钥文件：

```bash
export ANTHROPIC_API_KEY='...'
# 或 OPENAI_API_KEY / OLLAMA_API_KEY / 通用 JTERM4_AI_API_KEY
jterm4
```

不便向桌面启动器传递环境变量时，可创建独立文件：

```bash
mkdir -p ~/.config/jterm4
install -m 600 /dev/null ~/.config/jterm4/ai.key
read -rsp 'AI API Key: ' JTERM4_KEY; printf '\n'
printf '%s\n' "$JTERM4_KEY" > ~/.config/jterm4/ai.key
unset JTERM4_KEY
chmod 600 ~/.config/jterm4/ai.key
```

并在 `config.toml` 中设置：

```toml
ai_api_key_file = "~/.config/jterm4/ai.key"
```

文件必须是当前用户所有的普通文件，Unix 权限不得向 group/other 开放，最大 16 KiB，且只能包含一行非空密钥。环境 Key 优先于文件；`JTERM4_AI_API_KEY_FILE` 可覆盖文件路径。相关配置为 `ai_enabled`、`ai_provider`、`ai_base_url`、`ai_api_key_file`、`ai_model`、`ai_max_tokens` 和 `ai_redact_secrets`。请求通过系统 `curl`/Flatpak host bridge 发送；运行 `--doctor` 可离线检查凭据文件和 curl。右侧聊天面板使用 `Ctrl+Alt+Shift+A`，Block 选择后 `Ctrl+Shift+Q` 可发送命令、退出码、cwd 和截断输出。

自然语言转命令与 Agent 坚持 review-first：模型只能提出候选，不会自行写入 PTY、提交 Enter 或执行。`Ctrl+Alt+G` 在当前 active Block pane 打开原生 **Shell Agent**；它在打开时固定目标 pane，切换标签不会悄悄改变执行目标。VTE pane 不提供 Agent。

一次 Agent 会话的安全流程是：

1. 输入任务后，模型回复必须是严格 JSON `say`、`run` 或 `done`；夹杂 prose、未知字段、错误类型、过期 proposal 或非法控制字符都会失败关闭，不能退化为可运行命令。
2. `run` 显示单独 proposal 卡片和可编辑命令。识别到顶层 `rm -rf`、`mkfs`、下载后 pipe 到 shell 等模式时显示醒目的危险提示。
3. 每张卡片只能 **Reject** 或显式 **Approve & Run**。Reject 会进入 transcript 并要求模型换方案；批准执行的是用户最后编辑后的精确文本。
4. 批准前再次检查固定 Block prompt：正在运行任务或已有未提交输入时拒绝写入，待 prompt 空闲且清空后才能重试。
5. 已批准命令形成 finished block 后，匹配的 exit code 和有界输出作为 observation 回灌，Agent 才能提出下一步。不相关命令不会被当成该 proposal 的结果。
6. **Cancel Agent** 或关闭窗口会取消会话并作废待处理模型回复；`agent_max_turns` 达到上限后停止继续请求。已经由用户批准并启动的普通终端命令不会被取消按钮暗中 kill，仍使用标准 pane/tab 关闭确认管理。

`agent_enabled = false` 可独立关闭 Agent，`agent_max_turns` 限制模型回合数；`ai_enabled = false` 和 safe mode 都会同时阻止打开。Agent 必须被视为有用户权限的命令执行辅助工具，危险模式提示不是完整 shell 安全分析，也不替代逐字审阅。

`ai_redact_secrets = true` 默认遮蔽常见密钥格式，但脱敏不是秘密保护边界；发送前仍应检查上下文。`--safe-mode` 同时关闭 AI 与 Agent。

## 12. 配置保存与快捷键

完整字段见 `config.toml.example`。保存后自动热重载，`Ctrl+Shift+R` 手动重载。语法或语义错误不会替换当前有效配置。

应用内设置保存还会：获取进程级 advisory lock、检查加载时 revision、拒绝并发编辑冲突、用 owner-only 临时文件 `fsync` 后原子替换，并轮换 `.bak` / `.bak.1` 两份经过验证的备份。恢复前的当前文件另存为 `.before-restore`。冲突、验证拒绝、锁超时和 I/O 错误会在窗口中明确提示；内存中的临时改动仍有效，但磁盘不会被覆盖。发生冲突时先重载配置再重新应用改动；必要时运行 `jterm4 --restore-config-backup`。safe mode 中的设置只影响当前窗口，也会明确提示不会保存。

覆盖或解除快捷键：

```toml
[keybindings]
show_remote_picker = "F8"
toggle_ai_panel = false
```

修饰键名称不区分大小写，`Ctrl` 与 `Control` 等价。若两个 action 使用同一组合，配置检查器会报告冲突。`Ctrl+R` / `Ctrl+P` 留给 shell/readline。

## 13. 状态与历史位置

- 配置：`~/.config/jterm4/config.toml` 及 `.bak` / `.bak.1`。
- 窗口快照：`~/.config/jterm4/windows/window-*.active|state`。
- JSONL 命令历史：`${XDG_STATE_HOME:-~/.local/state}/jterm4/history.jsonl`，可用配置覆盖。
- 可选 Block 全量历史：由 `block_history_path` 指定，可能包含输出。
- 用户 workflow：`~/.config/jterm4/workflows/*.{toml,yaml,yml}`。
- 已安装示例与 Notebook：`${prefix}/share/jterm4/`。

配置、快照与历史包含敏感工作信息，备份或分享前应主动检查。

## 14. 故障排查

```bash
jterm4 --doctor
jterm4 --check-config
JTERM4_LOG=debug jterm4 --no-restore
jterm4 --safe-mode
jterm4-support-bundle .
```

- GUI 无法启动：确认 `DISPLAY` 或 `WAYLAND_DISPLAY` 以及 GTK/VTE 动态库。
- 中文输入无预编辑：检查 `GTK_IM_MODULE`、`XMODIFIERS` 和 fcitx5/ibus GTK4 模块。
- Block 缺少准确 exit/cwd：加载对应 shell integration。
- AI 不可用：检查 `ai_enabled`、provider 对应密钥、base URL 和 `curl`。
- 欢迎 Notebook 找不到：重新安装资产，或设置 `JTERM4_ASSET_DIR=/path/to/share/jterm4`。
- workflow 示例找不到：检查 `${prefix}/share/jterm4/workflows`；非默认 prefix 可设置 `JTERM4_WORKFLOW_DIR`。
- 长命令无通知：检查 `notify_long_blocks`、阈值、`notify-send` 和通知服务。
- SSH 无目标：添加 `[[remote_hosts]]` 后按 `Ctrl+Shift+S`。
- 配置修改没生效：先运行 `--check-config`；并发冲突需要重载后再保存。
