# jterm4 用户指南

## 1. 启动与诊断

直接启动：

```bash
jterm4
```

常用的无界面命令：

```bash
jterm4 --help
jterm4 --version
jterm4 --doctor
jterm4 --print-config-path
jterm4 --check-config
```

这些命令在 GTK 初始化前完成，因此可在 SSH、TTY 和 CI 中运行。`--doctor` 还会报告 ready / active 会话快照数量。日志可用 `JTERM4_LOG=debug`，或使用 `RUST_LOG='warn,jterm4=debug,jterm4::state=trace'` 按模块设置；每行包含相对时间、级别和 target。使用其他配置文件：

```bash
jterm4 --config ~/configs/work.toml
jterm4 --check-config ~/configs/work.toml
```

## 2. 终端模式

在 `~/.config/jterm4/config.toml` 中选择：

```toml
terminal_mode = "vte"
```

- `vte` 是传统终端，适合 TUI、分屏和通用 shell 工作。
- `block` 把每条命令保存为独立块，提供退出状态、耗时、筛选、跨块搜索、历史回调和 AI 上下文。

Block 模式目前不开放分屏。旧实现会先启动 PTY、随后因内部 VTE 不是 Pane 根组件而无法挂载；当前版本会在启动进程之前明确提示。需要分屏时请切换为 VTE 模式。

## 3. 标签页

| 操作 | 快捷键 |
|---|---|
| 新建标签 | `Ctrl+Shift+T` |
| 关闭当前 Pane 或标签 | `Ctrl+Shift+W` |
| 下一个标签 | `Ctrl+Tab`、`Ctrl+PageDown` |
| 上一个标签 | `Ctrl+Shift+Tab`、`Ctrl+PageUp` |
| 标签 1 到 9 | `Ctrl+1` 到 `Ctrl+9` |
| 最后一个标签 | `Ctrl+0` |
| 过滤标签 | `Ctrl+Shift+L` |
| 标签栏位置 | `Ctrl+Shift+B` |

标签支持拖放排序、双击重命名、固定、标记、复制以及右键菜单。侧栏可在 Tabs 与 Files 之间切换；标签移到顶栏时，过滤动作仍会显示可见的搜索输入框。

每个 jterm4 窗口维护独立的活动快照。正常关闭后，快照才会发布给未来窗口；同时运行的窗口不会读取或覆盖彼此状态。多个窗口关闭后，后续启动会逐个原子领取最近的快照。异常退出留下的活动快照会在确认原进程已结束后自动回收，旧版 `tabs.state` 也会在首次启动时无损迁移。`jterm4 --doctor` 只报告 ready / active 数量，不暴露路径或标签内容。

## 4. VTE 分屏

| 操作 | 快捷键 |
|---|---|
| 左右分屏 | `Ctrl+Shift+E` |
| 上下分屏 | `Ctrl+Shift+D` |
| 循环焦点 | `Alt+Tab` / `Alt+Shift+Tab` |
| 方向聚焦 | `Alt+方向键` |
| 调整大小 | `Alt+Shift+方向键` |
| 放大当前 Pane | `Ctrl+Shift+Z` |
| Pane 移到新标签 | `Ctrl+Shift+!` |

分屏状态序列化仍属于实验功能。重要工作流不要只依赖自动恢复；启动命令和远程会话也应保留自己的持久化机制。

## 5. 搜索与 Block 工具

`Ctrl+Shift+F` 打开当前标签搜索：

- 普通文本执行不区分大小写的搜索。
- `/expression/` 使用正则表达式。
- `Enter` 下一个结果，`Shift+Enter` 上一个，`Escape` 关闭。
- 清空输入会立即清除 VTE 和 Block 高亮。
- 搜索状态切换标签时，焦点保留在搜索框并对新标签重新应用查询。

Block 模式额外提供：

| 功能 | 快捷键 |
|---|---|
| 命令历史面板 | `Ctrl+Shift+H` |
| 跨块行搜索 | `Ctrl+Shift+G` |
| 工作流模板 | `Ctrl+Shift+M` |
| AI 分析选中块 | `Ctrl+Shift+Q` |

右键块可复制命令、复制输出、重新运行或导出。命令回调只会在 shell 正停留在提示符时写入，避免误发给正在运行的 TUI。

Block 历史选择与 jterm1 保持一致：

- 单击块头进入选择；`Shift+单击` 选择连续范围，`Ctrl+Shift+单击` 切换单个块。
- 选中后用 `↑` / `↓` 移动活动块，`Shift+↑` / `Shift+↓` 扩展范围，`Ctrl+Shift+↑` / `Ctrl+Shift+↓` 对齐活动块顶部或底部。
- `Enter` 回填活动块命令，`Ctrl+Enter` 回填并执行，`Delete` 删除活动块，`Escape` 清除选择。
- shell 空闲时，`Home` / `End` 跳到历史两端，`PageUp` / `PageDown` 翻页。
- `Ctrl+B` 收藏活动块，`Ctrl+,` / `Ctrl+.` 在收藏块之间跳转；相关动作也可从命令面板调用或在配置中绑定。
- 复制多选块时按终端顺序合并，块之间保留一个空行；`Alt+Ctrl+Shift+C` 只复制输出。

## 6. 文件树

侧栏 Files 页以当前标签目录为根：

- 双击目录展开或折叠。
- 双击文件会把经过 shell 引号保护的路径插入当前输入行，不自动执行。
- “向上”按钮进入父目录，“主页”按钮回到当前终端工作目录。

Block 模式的路径插入走其自管 PTY 输入通道；VTE 模式走 VTE child 输入。

## 7. SSH 远程会话

jterm4 不会预置任何个人主机。在配置中显式添加：

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

按 `Ctrl+Shift+S` 打开主机选择器。连接复用通过 OpenSSH ControlMaster 完成，异常断开会按上限退避重连；用户正常退出不会重连。

## 8. 工作流模板

将一个或多个 TOML 文件放到 `~/.config/jterm4/workflows/`：

```toml
name = "Deploy"
description = "Deploy a branch"
command = "deploy --branch {branch} --env {env}"

[[args]]
name = "branch"
description = "Git branch"
default = "main"

[[args]]
name = "env"
default = "staging"
```

`Ctrl+Shift+M` 选择模板并填写参数。生成的命令只写入编辑行，不会自动按 Enter。

## 9. AI 面板

启动前设置密钥：

```bash
export ANTHROPIC_API_KEY='...'
jterm4
```

`Ctrl+Shift+A` 打开面板；在 Block 模式选中命令块后按 `Ctrl+Shift+Q` 发送命令、退出码、工作目录和截断后的输出。`ai_redact_secrets = true` 默认在发送前遮蔽常见密钥格式。

AI 请求目前依赖系统 `curl`。运行 `jterm4 --doctor` 可检查其是否可用。不要把终端脱敏当成唯一的秘密保护边界，发送前仍应检查上下文。

## 10. 配置与快捷键

完整字段见仓库根目录的 `config.toml.example`。保存后自动热重载，`Ctrl+Shift+R` 可手动重载。语法或语义错误的热重载会被拒绝，当前有效配置保持不变。

覆盖或解除快捷键：

```toml
[keybindings]
show_remote_picker = "F8"
toggle_ai_panel = false
```

修饰键名称不区分大小写，`Ctrl` 与 `Control` 等价。若两个 action 使用同一组合，配置检查器会报告冲突。

视觉与常用动作：

| 操作 | 快捷键 |
|---|---|
| 命令面板 | `Ctrl+Shift+P` |
| 设置 | `Ctrl+Shift+O` |
| 重载配置 | `Ctrl+Shift+R` |
| 显示/隐藏侧栏 | `Ctrl+\` |
| 复制 / 粘贴 | `Ctrl+Shift+C` / `Ctrl+Shift+V` |
| 放大 / 缩小字体 | `Ctrl+Shift++` / `Ctrl+-` |
| 增加 / 降低透明度 | `Ctrl+Shift+K` / `Ctrl+Shift+J` |
| 调试面板 | `F12` |

## 11. 状态与历史

- 标签快照：`~/.config/jterm4/tabs.state`
- 可选 Block 历史：由 `block_history_path` 指定
- 工作流：`~/.config/jterm4/workflows/*.toml`

状态与历史保存使用同目录临时文件和原子替换。Block 历史路径支持开头的 `~/` 并自动创建父目录。

## 12. 故障排查

先运行：

```bash
jterm4 --doctor
jterm4 --check-config
JTERM4_LOG=debug jterm4
```

常见检查：

- GUI 无法启动：确认 `DISPLAY` 或 `WAYLAND_DISPLAY`，以及 GTK/VTE 动态库。
- 中文输入无预编辑：检查 `GTK_IM_MODULE`、`XMODIFIERS` 和 fcitx5/ibus GTK4 模块。
- AI 不可用：检查 `ANTHROPIC_API_KEY` 与 `curl`。
- 长命令无通知：检查 `notify-send` 与桌面通知服务。
- SSH 无目标：添加 `[[remote_hosts]]`，再按 `Ctrl+Shift+S`。
- 配置修改没生效：先运行 `--check-config`；错误配置不会替换当前有效配置。
