# jterm4

jterm4 是一个面向开发工作流的原生 GTK4 终端。它默认使用 Block 后端，把命令、输出、退出状态和工作目录组织成可搜索的结构化块；需要传统终端语义时也可切换到 VTE 后端。

## 能力概览

- 默认 Block、可选 VTE 的双终端后端
- 标签页、混合后端分屏、方向导航、缩放、前台进程关闭确认与多窗口独立会话恢复
- 跨命令块搜索、失败/慢命令筛选、只记录元数据的 JSONL 命令历史
- 统一模糊面板：动作、历史、YAML/TOML workflow 和自然语言命令入口
- 可执行 `.jtnb.md` Notebook，逐 cell 运行/停止并分离 stdout 与 stderr
- 基于现代 GTK4 列表模型的异步文件树、Git 分支/脏状态条、长命令桌面通知
- SSH 主机选择、连接复用与自动重连
- 可选多 provider AI、可搜索和归档的多会话 Chats 库，以及绑定当前 Block pane 的原生 Shell Agent；每条候选命令均可编辑且需单独批准
- 配置热重载、可覆盖快捷键、8 套内置主题
- CJK 输入法和 Unicode 安全的搜索/通知显示

从 Block pane 发起分屏时会保留当前 Block，并创建一个传统 VTE sibling；VTE pane 可继续嵌套分屏。每个可见 pane 都独立拥有并清理其进程，关闭 pane、标签或窗口前会统一检查前台任务。

## 构建与运行

推荐使用仓库提供的 Nix 开发环境：

```bash
nix develop
cargo run
```

也可以在安装 GTK4、libadwaita、VTE GTK4、PCRE2 与 `pkg-config` 开发包后直接使用 Cargo。完整质量门禁：

```bash
cargo fmt --all -- --check
cargo test --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked
cargo build --release --all-features --locked
```

安装脚本默认优先使用 Nix；没有 Nix 时自动退回 Cargo，并且不会覆盖已有配置：

```bash
./scripts/install.sh
./scripts/install.sh --backend cargo
./scripts/install.sh --prefix /opt/jterm4 --no-config
./scripts/install.sh --dry-run
```

默认安装到 `~/.local/bin/jterm4`，同时安装 `jterm4-support-bundle`，并把 shell 集成、内置 workflow 和欢迎 Notebook 安装到 `~/.local/share/jterm4/`。配置使用 `0600`。脚本支持 `DESTDIR`、`XDG_CONFIG_HOME` 和 `CARGO_TARGET_DIR`；使用非默认 prefix 时可通过 `JTERM4_ASSET_DIR` / `JTERM4_WORKFLOW_DIR` 指向对应的 `share/jterm4` 目录。卸载默认保留用户配置、状态与历史：

```bash
./scripts/uninstall.sh
./scripts/uninstall.sh --purge-config   # 明确删除全部配置和状态
```

`nix build` / `nix run` 分别构建和启动 flake 中的默认 package/app，
`nix flake check` 验证同一 package。也可为已有 release binary 生成确定性、
带 SHA-256 的本地安装归档：

```bash
cargo build --release --all-features --locked
./scripts/package-release.sh target/release/jterm4
(cd target/dist && sha256sum --check *.sha256)
```

该归档可换目录后安装，但仍动态依赖兼容的 GTK4、libadwaita、VTE GTK4
和 PCRE2 系统运行库，并非静态或自包含的 portable 应用。


## Flatpak 与桌面集成

项目使用稳定应用 ID `io.github.beamiter.jterm4`，提供 desktop、AppStream、
SVG/PNG 图标以及可复现 Flatpak 清单。Flatpak 中的 Shell、SSH、Git、curl
和通知命令通过 `flatpak-spawn --host` 运行，因此终端操作的是宿主环境而
不是一次性沙箱；原生安装路径保持直接执行。内置 shell 集成、workflow 和
欢迎 Notebook 一并安装在 `/app/share/jterm4/`。

```bash
flatpak-builder --user --install-deps-from=flathub --force-clean \
  --disable-rofiles-fuse --repo=flatpak-repo flatpak-build \
  packaging/flatpak/io.github.beamiter.jterm4.yml
flatpak build-bundle flatpak-repo io.github.beamiter.jterm4.flatpak \
  io.github.beamiter.jterm4
```

权限模型、宿主桥接、安全边界、安装命令与已知限制见
[Flatpak 指南](docs/FLATPAK.md)。

## 启动与配置

默认配置路径为 `~/.config/jterm4/config.toml`。从完整示例开始：

```bash
jterm4 --init-config
jterm4 --check-config
```

也可使用独立配置：

```bash
jterm4 --config ~/my-jterm4.toml
jterm4 --check-config ~/my-jterm4.toml
```

常用启动覆盖不会修改配置：

```bash
jterm4 ~/project
jterm4 --mode block --no-restore
jterm4 -d /tmp --execute bash -lc 'printf "hello\\n"'
jterm4 --safe-mode
```

`--safe-mode` 不读取指定或默认配置，也不采用 `JTERM4_*` 外观/行为覆盖；它使用内置 VTE 主题与默认快捷键，并禁用配置重载、恢复、持久化、远程主机、历史、仓库探测、AI/Agent 与 Notebook 执行，适合排查损坏配置或启动环境。

诊断命令均可在没有图形显示的 SSH/CI 环境运行：

```bash
jterm4 --help
jterm4 --doctor --json       # 同时报告 ready / active 会话快照数量
jterm4 --check-config --json
jterm4 --config-path
jterm4 --restore-config-backup
jterm4 --print-default-config
jterm4 --shell-integration bash
jterm4-support-bundle ~/Desktop
```

`--doctor` 除配置语义和运行时依赖外，还检查配置权限、有效轮换备份、写锁、AI provider/密钥存在性、workflow 搜索位置、欢迎 Notebook、历史和 SSH 就绪度；不会发起网络请求。support bundle 使用额外的脱敏诊断模式，只收集权限/大小、计数、非敏感系统特征和选定环境变量的“存在/不存在”，不包含配置、命令/输出、会话内容、密钥、主机名或本地路径。分享前仍应逐项检查归档内容。

配置文件保存后会自动热重载；`Ctrl+Shift+R` 可手动重载。应用内保存会先验证 TOML 与语义、获取进程锁并检查磁盘 revision，再以 `0600` 临时文件同步、原子替换并轮换两份有效备份；冲突、锁超时、无效内容或 I/O 错误会显示原生提示，且不会覆盖磁盘。`--restore-config-backup` 可恢复最近的有效备份。

日志支持普通级别和标准 target 指令，并输出进程内相对时间、级别与模块名：

```bash
JTERM4_LOG=debug jterm4
RUST_LOG='warn,jterm4=debug,jterm4::state=trace' jterm4
```

`JTERM4_LOG` 优先于 `RUST_LOG`；未知指令会被忽略，默认级别保持 `warn`。

Block 模式可通过 `finished_block_viewport_rows` 调整长块出现顶部/底部导航控件的行数阈值；`block_compact = true` 可启用更接近 jterm1/Warp 的紧凑块间距。两项配置均保持 GTK4 原生实现，不增加运行时依赖。

## 核心快捷键

| 功能 | 快捷键 |
|---|---|
| 新建 / 关闭 | `Ctrl+Shift+T` / `Ctrl+Shift+W` |
| 下一个 / 上一个标签 | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| 标签 1–8 / 最后一个 | `Ctrl+1`…`Ctrl+8` / `Ctrl+9` |
| 搜索 / 命令面板 | `Ctrl+Shift+F` / `Ctrl+Shift+P` |
| 左右 / 上下分屏 | `Ctrl+Shift+E` / `Ctrl+Shift+D` |
| 聚焦 / 调整 Pane | `Ctrl+Alt+方向键` / `Ctrl+Alt+Shift+方向键` |
| 复制 / 粘贴 | `Ctrl+Shift+C` / `Ctrl+Shift+V` |
| 配置 / 重载 | `Ctrl+Shift+O` / `Ctrl+Shift+R` |
| SSH 主机选择 | `Ctrl+Shift+S` |
| Block 历史 / 跨块搜索 | `Ctrl+Shift+H` / `Ctrl+Shift+G` |
| workflow / 失败块 / 最早块 | `Ctrl+Shift+M` / `Ctrl+Shift+X` / `Ctrl+Shift+N` |
| 全选 / 回填 / 清空 Block | `Ctrl+Shift+A` / `Ctrl+Shift+I` / `Ctrl+Shift+K` |
| Block 过滤 / 书签 / 标签栏位置 | `Alt+Shift+F` / `Ctrl+Shift+B` / `Ctrl+Alt+B` |
| AI 面板 / 询问选中块 | `Ctrl+Alt+Shift+A` / `Ctrl+Shift+Q` |
| Shell Agent（Block） | `Ctrl+Alt+G` |
| 字号增 / 减 / 复位 | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` |

全部命令和当前绑定可在 `Ctrl+Shift+P` 中搜索。快捷键可在 `[keybindings]` 中覆盖，设为 `false` 可解除绑定。`Ctrl+R` 与 `Ctrl+P` 保留给 shell/readline；Block 历史统一使用 `Ctrl+Shift+H`。

AI 面板的分隔条宽度会随配置持久化。**New chat** 会创建并选中一个新会话，旧会话继续保留在可搜索的 **Chats** 会话库中；会话自动取标题，也可 Rename、Archive/Unarchive，Delete 前会要求确认。输入框使用 `Enter` 或 `Ctrl+Enter` 发送，`Shift+Enter` 换行，并保留输入法候选确认语义。请求期间可 **Stop**，失败或停止后可按原 chat/context **Retry**；选中 Block 会显示可清除的 context chip，输出被截断时 chip 会明确提示。空会话也提供只填充、不自动发送的快捷提示。

当前选择、每个 chat 的草稿和实际发送的选中 Block 上下文会跟随各自窗口快照恢复；快速关窗会先强制刷新草稿，发送失败或中途退出的问题也会作为可重试 draft 恢复，Ask selected Block 不会覆盖正在编辑的文字，关窗时其内存重试也会转成可恢复 draft/context。集合最多保存 50 条 chat metadata、每个 chat 最多 100 个 turn，紧凑 JSON 总预算仍为 8 MiB；超出总预算时只裁剪最旧的完整问答对，不会删除在途问题，并在对应会话显示 `truncated`。出站请求另保留最近至多 40 个 turn/256 KiB，单条输入、Block 输出和模型文本分别有 64 KiB、64 KiB、256 KiB 硬上限，可见 AI/Agent activity 各限制为 1 MiB，同时最多运行 4 个 provider 请求。窗口状态另为完整 chat metadata 预留 64 KiB，Pane/Tab 数据挤压空间时也不会静默删除整个 Chats 库。旧版单会话 schema v1 会自动迁移。后台回复始终绑定发起请求的 chat，切换不会串话，已经 Delete 的 chat 收到迟到回复时会直接丢弃。默认脱敏覆盖 active、non-active、archived chat 及其 draft/context；`--safe-mode` 与 `--no-restore` 的隔离和恢复语义保持不变。

命令面板使用模糊匹配；输入 `>` 只看动作、`@` 只看 JSONL 历史、`:` 只看 workflow、`?` 提交自然语言命令请求。历史和 workflow 只写入当前编辑行；AI 候选先留在审阅界面，所有路径都不会自动按 Enter。所有审阅式插入都拒绝 CR、LF、NUL 和终端控制字符，避免多行条目越过“不提交”边界。`Ctrl+Alt+G` 或顶部栏的 **Agent** 开关会打开绑定当前 Block pane 的 Shell Agent dashboard；若打开时已选中 finished Block，它会作为可见、可移除的不可信上下文附加。dashboard 显示目标、provider/model、shell、回合进度与 activity，可单独 Stop/Retry 当前模型请求，并可切换持久化的 typo-like 命令纠正。Agent 严格解析模型 proposal，允许复制、编辑、Reject 或逐条 **Approve & Run**；编辑后会重新识别风险，危险命令还需第二次确认。完成块的退出码和截断输出随后回灌到下一轮。欢迎 Notebook 也可从面板中的 **Open welcome & quick start notebook** 打开。

若希望 Block 准确记录命令边界、退出码和 cwd，可加载内置 shell 集成：

```bash
source <(jterm4 --shell-integration bash)
```

也可从已安装的 `share/jterm4/shell-integration/` 加载 bash、zsh、fish 或 PowerShell 脚本。

Block 模式与 jterm1 保持相同的选择语义：`Ctrl+Up` 从最新块进入选择，`Shift+Up/Down`
扩展范围，普通 `Up/Down` 移动 active edge，`Enter` 按终端顺序把所有选中命令回填为
可编辑文本而不执行，`Escape` 取消选择。右键多选区域可批量复制命令、输出或完整块；长 Block 提供顶部/底部跳转与 sticky header，后台异步输出使用独立 Block 样式。

后台输出只会在提示符空闲且用户尚未开始编辑时归入独立 Block；一旦输入开始，后续输出保持在当前终端中，避免把 shell 回显、补全或交互输出错误拆块。

## 许可证

jterm4 以 **MIT OR Apache-2.0** 双许可证发布，使用者可任选其一；完整文本见
[`LICENSE-MIT`](LICENSE-MIT) 与 [`LICENSE-APACHE`](LICENSE-APACHE)。向本仓库提交
贡献即表示贡献者同意按相同的双许可证条款授权该贡献。仓库许可与 crates.io 发布
是两个独立决定，因此 Cargo 包目前仍保留 `publish = false`。

## 安全默认值

- 新安装不会写入任何远程主机、用户名、IP 或个人路径。
- OSC 52 远程剪贴板写入默认关闭。
- AI 会话库默认对常见云密钥、PAT、JWT 和私钥进行脱敏，覆盖 active、non-active、archived chat 以及草稿和 Block 上下文。
- Agent 只支持显式选中的 Block pane；prompt 忙或已有输入时拒绝提交，危险模式会醒目标注，但最终批准仍由用户负责。
- 可执行 Notebook 在独立进程组运行，关闭或停止 cell 会终止其进程组；安全模式完全禁用 Notebook 执行。
- 命令历史只保存 command、cwd、exit code 和完成时间，不保存输出，并限制单条/总文件大小。
- 每个窗口使用独立的原子会话快照；并发窗口互不覆盖，崩溃遗留快照会在下次启动回收。
- 配置、会话快照、JSONL 命令历史和 Block 历史使用 owner-only 权限；关键替换路径使用同步写入与原子 rename，降低信息泄露和断电损坏风险。
- `jterm4-support-bundle` 不读取或打包上述内容，只报告脱敏诊断与文件元数据，并以 `0600` 创建归档。
- 项目采用 `MIT OR Apache-2.0` 双许可证；Cargo 包仍有意保留 `publish = false`，不将仓库许可自动等同于 crates.io 发布。依赖继续由每周 RustSec 审计与 Dependabot 检查。

进一步说明见 [用户指南](docs/USER_GUIDE.md)、[架构说明](docs/ARCHITECTURE.md)、[Block 模式验收清单](docs/BLOCK_MODE_ACCEPTANCE.md)、[AI / Agent / Chat 验收矩阵](docs/AI_AGENT_CHAT_ACCEPTANCE.md)、[性能指南](docs/PERFORMANCE.md)、[发布流程](docs/RELEASING.md) 和 [Tailscale/SSH 配置](docs/tailscale-setup.md)。参与开发前请阅读 [贡献指南](CONTRIBUTING.md)、[安全策略](SECURITY.md) 与 [变更日志](CHANGELOG.md)。
