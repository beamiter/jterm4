# jterm4 Block Mode / jterm1 体验对齐验收清单

用于在 X11、Wayland 和不同 shell 下验收 Block 模式。建议先完成 P0，再进行长会话和全屏程序回归。

## 测试准备

```bash
cargo build
source <(target/debug/jterm4 --shell-integration bash)
RUST_LOG=jterm4=debug target/debug/jterm4 --mode block --no-restore
```

依次创建成功、失败、多行、长输出和后台输出：

```bash
printf 'alpha\n'
sh -c 'printf "failed\n" >&2; exit 7'
printf 'one\ntwo\nthree\n'
seq 1 300
(sleep 1; printf 'background-ready\n') &
```

## P0 核心行为

- **选择**：`Ctrl+Up` 选择最新块；`Up/Down` 移动 active edge；`Shift+Up/Down` 扩展范围；`Escape` 清除。
- **全选**：`Ctrl+Shift+A` 后全部块为浅描边，最新 active 块为强描边并保留快捷操作。
- **回填**：多选后按 `Enter` 或 `Ctrl+Shift+I`，命令按从旧到新顺序进入输入区，不自动执行；background block 被忽略。
- **多行安全**：支持 bracketed paste 时保留多行；不支持时只回填第一逻辑行，不能意外执行后续命令。
- **右键多选**：检查 `Copy Commands`、`Copy Outputs`、`Copy Blocks`、`Insert Commands at Prompt` 的顺序与内容。
- **右键块操作**：检查 `Scroll to Top of Block`、`Jump to Bottom of Block`、输出过滤以及 `Bookmark Block` / `Remove Bookmark`。
- **清空**：`Ctrl+Shift+K` 同时清除块、选择、书签、搜索、未读徽标、虚拟滚动范围与持久化历史；当前提示符仍可输入。
- **后台输出**：未开始编辑时，异步输出形成无命令的 `Background output` 块；开始编辑后输出保持在输入区，不误拆块。

## P1 复制与导航

- `Ctrl+Shift+C` 复制选中块的命令和输出。
- `Ctrl+Alt+Shift+C` 只复制选中块输出。
- 未选择整块时，在输出 VTE 内拖选文本，复制结果只包含文本选区。
- 跨块拖选后，内容按界面顺序合并。
- `Ctrl+Shift+Up/Down` 对齐 active 块顶部/底部。
- sticky header 点击命令区域跳到块顶部；底部按钮跳到长块底部；最小化/恢复状态正常。
- `Ctrl+Shift+B` 切换 active 块书签；`Ctrl+,` / `Ctrl+.` 在书签间跳转。
- `Alt+Shift+F` 打开 active 或最新块过滤器，关闭后查询内容仍保留。

## P1 运行中和全屏程序

```bash
sh -c 'printf "start\n"; sleep 5; printf "done\n"'
read -r value; printf 'value=%s\n' "$value"
```

- 命令运行中，Enter 必须透传给进程，不能回填旧块。
- 运行中清空旧块不能向进程注入 form-feed，命令最终仍输出 `done`。
- 在 `vim`、`less`、`top` 等 alt-screen 程序中，Block-only 快捷键不能操作隐藏块。
- 退出 alt-screen 后，选择、过滤、书签、复制和回填继续正常。

## P1 分屏、进程检查与关闭

- 在 Block pane 按 `Ctrl+Shift+E` / `Ctrl+Shift+D`，当前 Block 保持可见，新建 VTE sibling，没有隐藏或孤儿 PTY。
- 在 sibling 中继续嵌套分屏，方向聚焦、resize、zoom/unzoom 和关闭当前 pane 正常。
- 对普通 sibling、原始 primary 和 remote pane 分别执行 **Move pane to new tab**，重复移动后旧/新 tab 的 close、Pin、tooltip、进程状态和 session id 仍指向各自可见 pane。
- 让 remote 异常退出并进入重连倒计时：直接移动 dead pane 后重连应跟随新 tab；倒计时期间新建 split 或 zoom 时只移除 dead remote leaf，不能关闭或 kill 仍存活的 local sibling；手动关闭 dead leaf 也应取消 timer 并立即 collapse。
- Block 和 VTE 分别运行 `sleep 60`，关闭单 pane、整个标签、批量标签和窗口时都出现前台任务确认。
- zoom 到任意 pane 后关闭标签，确认隐藏 sibling 也被扫描并终止，pane tree 没有残留进程。
- 空闲 shell 关闭时不误报；关闭含前台任务的窗口后，相关 shell/child process group 均退出。

## P1 长会话与虚拟滚动

准备至少 250 个独立块后检查：

- `Home/End`、`PageUp/PageDown`、选择与书签跳转无明显卡顿。
- 全选不会冻结 UI，active edge 始终唯一。
- 清空后没有残留空白高度、旧滚动范围或不可见块。
- 清空后继续执行十条命令，所有新块都能显示、复制、过滤和回填。
- 在同一方向连续创建 3、4 个 split，所有 pane 应按 1/3、1/4 近似等分，不得保留一个 1/2 主 pane 后把其余 pane 递归挤小；横向、纵向和 2x2 混合树都要检查，divider 仍可手动调整。
- 在 2x2 split 中分别从 live prompt、finished block 文本和 pane 内滚动控件按 `Ctrl+Alt+方向键`；四个方向都应聚焦最近的对应 pane，外边缘不循环，容器短暂接管焦点时从最后活动 pane 继续。

## 设置面板

- Settings 中的 **Compact Block Spacing** 与 `block_compact` 配置一致。
- Settings 中切换 backend 只影响后续新建 tab；安全模式下 backend 控件不可修改。
- Compact Block Spacing 与相关 Block 配置热更新到当前所有嵌套 Block pane，不能破坏 live VTE、选择或虚拟滚动状态。

## 配套工作流

- 执行若干成功/失败命令后，`Ctrl+Shift+P` 的 `@` 历史只显示 command/cwd/status，不包含输出；接受只回填不执行。
- `:` 能模糊匹配安装的 YAML 示例和用户 TOML/YAML workflow，参数替换后只写入编辑行。
- 在文件树双击 `.jtnb.md`，逐 cell 与 Run All 均能运行；Stop/Stop All/关闭对话框会终止完整 cell process group。
- `?` 请求必须固定当前 Block pane，在块流中显示 selected Block/context、Stop/Retry/Regenerate 和可编辑候选；切换 tab 不得漂移目标，关闭或 Stop 必须取消 transport。**Insert for review** 只能写入单行审阅文本，不能自行向 PTY 写入 Enter。
- 在 active Block pane 按 `Ctrl+Alt+G` 打开 Shell Agent；VTE pane、safe mode、`ai_enabled = false` 或 `agent_enabled = false` 都必须拒绝打开。
- Agent dashboard 应显示固定 cwd、provider/model、shell、review 状态、回合进度与实时 prompt readiness；readiness 必须区分已有输入、运行中、alt-screen、初始化和缺少 shell integration。切换 **AI command correction** 后 `command_correction_enabled` 持久化，关闭时新的 typo-like 失败和仍在途的纠正都不得弹框。
- 要求 Agent 完成一个两步任务：每个严格 JSON proposal 都显示为与一次性建议一致的可编辑审阅卡；修改后 **Approve & Run** 执行修改值，Reject 会把拒绝写入上下文并请求替代方案。**Insert only** 只回填普通 prompt、记录未执行并回到可输入状态，不能等待或伪造 observation。
- 对 `rm -rf /`、`mkfs` 或 download-pipe-to-shell 类型 proposal 显示醒目危险提示，但仍不得自动执行。
- prompt 正在运行命令或已有未提交输入时点击批准，必须拒绝且不改动输入；清空并空闲后才允许再次批准。
- 批准命令完成后，Agent transcript 显示 exit code/输出并自动进入下一轮；不相关 Block 完成事件不能被误关联。
- `done` 后 **Follow up** 保留原 transcript 并允许追问；达到 `agent_max_turns` 后 **New task** 在原 pane 清空旧模型上下文、重置回合预算。**Cancel Agent** 和关闭卡片会停止模型回合，取消时的迟到回复不能形成 proposal。已明确批准且已经启动的终端命令仍按普通 Block 任务管理。

问题记录建议包含桌面环境、X11/Wayland、shell 及版本、是否加载 shell integration、复现步骤和 `RUST_LOG=jterm4=debug` 日志。
