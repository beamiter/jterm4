# jterm4

jterm4 是一个面向开发工作流的 GTK4 终端：既能作为传统 VTE 终端使用，也能把命令、输出、退出状态和工作目录组织成可搜索的结构化块。

## 能力概览

- VTE 与 Block 两种终端模式
- 标签页、VTE 分屏、方向导航、缩放与会话恢复
- 跨命令块搜索、失败/慢命令筛选、命令历史与工作流模板
- 文件树、Git 分支/脏状态条、长命令桌面通知
- SSH 主机选择、连接复用与自动重连
- 可选 AI 面板；发送前默认脱敏常见密钥
- 配置热重载、可覆盖快捷键、8 套内置主题
- CJK 输入法和 Unicode 安全的搜索/通知显示

> Block 模式的分屏目前会被明确拒绝，因为旧实现会启动不可见的孤儿 PTY。需要分屏时请使用 `terminal_mode = "vte"`；Block 原生 Pane 模型是后续架构升级项。

## 构建与运行

推荐使用仓库提供的 Nix 开发环境：

```bash
nix develop
cargo run
```

常用命令：

```bash
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

安装到 `~/.local/bin`：

```bash
./scripts/install.sh
```

系统需要 GTK4、libadwaita、VTE GTK4、PCRE2 和 `pkg-config` 的开发包。

## 配置

默认配置路径为 `~/.config/jterm4/config.toml`。从完整示例开始：

```bash
mkdir -p ~/.config/jterm4
cp config.toml.example ~/.config/jterm4/config.toml
jterm4 --check-config
```

也可使用独立配置：

```bash
jterm4 --config ~/my-jterm4.toml
jterm4 --check-config ~/my-jterm4.toml
```

诊断命令均可在没有图形显示的 SSH/CI 环境运行：

```bash
jterm4 --help
jterm4 --doctor
jterm4 --print-config-path
jterm4 --print-default-config
```

配置文件保存后会自动热重载；`Ctrl+Shift+R` 可手动重载。无效的新配置不会覆盖当前正在运行的有效配置。

Block 模式可通过 `finished_block_viewport_rows` 调整长块出现顶部/底部导航控件的行数阈值；`block_compact = true` 可启用更接近 jterm1/Warp 的紧凑块间距。两项配置均保持 GTK4 原生实现，不增加运行时依赖。

## 核心快捷键

| 功能 | 快捷键 |
|---|---|
| 新建 / 关闭 | `Ctrl+Shift+T` / `Ctrl+Shift+W` |
| 下一个 / 上一个标签 | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| 标签 1–9 / 最后一个 | `Ctrl+1`…`Ctrl+9` / `Ctrl+0` |
| 搜索 / 命令面板 | `Ctrl+Shift+F` / `Ctrl+Shift+P` |
| 左右 / 上下分屏（VTE） | `Ctrl+Shift+E` / `Ctrl+Shift+D` |
| 方向聚焦 Pane | `Ctrl+Alt+Shift+方向键` |
| 复制 / 粘贴 | `Ctrl+Shift+C` / `Ctrl+Shift+V` |
| 配置 / 重载 | `Ctrl+Shift+O` / `Ctrl+Shift+R` |
| SSH 主机选择 | `Ctrl+Shift+S` |
| Block 历史 / 跨块搜索 | `Ctrl+Shift+H` / `Ctrl+Shift+G` |
| 全选 / 回填 / 清空 Block | `Ctrl+Shift+A` / `Ctrl+Shift+I` / `Ctrl+Shift+K` |
| Block 过滤 / 书签 / 标签栏位置 | `Alt+Shift+F` / `Ctrl+Shift+B` / `Ctrl+Alt+B` |
| AI 面板 / 询问选中块 | `Ctrl+Alt+Shift+A` / `Ctrl+Shift+Q` |

全部命令和当前绑定可在 `Ctrl+Shift+P` 中搜索。快捷键可在 `[keybindings]` 中覆盖，设为 `false` 可解除绑定。

Block 模式与 jterm1 保持相同的选择语义：`Ctrl+Up` 从最新块进入选择，`Shift+Up/Down`
扩展范围，普通 `Up/Down` 移动 active edge，`Enter` 按终端顺序把所有选中命令回填为
可编辑文本而不执行，`Escape` 取消选择。右键多选区域可批量复制命令、输出或完整块；长 Block 提供顶部/底部跳转与 sticky header，后台异步输出使用独立 Block 样式。

后台输出只会在提示符空闲且用户尚未开始编辑时归入独立 Block；一旦输入开始，后续输出保持在当前终端中，避免把 shell 回显、补全或交互输出错误拆块。

## 安全默认值

- 新安装不会写入任何远程主机、用户名、IP 或个人路径。
- OSC 52 远程剪贴板写入默认关闭。
- AI 上下文默认对常见云密钥、PAT、JWT 和私钥进行脱敏。
- 标签状态与 Block 历史使用临时文件加原子替换，降低中断时损坏风险。

进一步说明见 [用户指南](docs/USER_GUIDE.md)、[性能指南](docs/PERFORMANCE.md) 和 [Tailscale/SSH 配置](docs/tailscale-setup.md)。
