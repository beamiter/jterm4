# jterm4 Block Mode / jterm1 体验对齐验收清单

用于在 X11、Wayland 和不同 shell 下验收 Block 模式。建议先完成 P0，再进行长会话和全屏程序回归。

## 测试准备

```bash
cargo build
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

## P1 长会话与虚拟滚动

准备至少 250 个独立块后检查：

- `Home/End`、`PageUp/PageDown`、选择与书签跳转无明显卡顿。
- 全选不会冻结 UI，active edge 始终唯一。
- 清空后没有残留空白高度、旧滚动范围或不可见块。
- 清空后继续执行十条命令，所有新块都能显示、复制、过滤和回填。

## 设置面板

- Settings 中的 **Compact Block Spacing** 与 `block_compact` 配置一致。
- 该选项只影响新建 Block pane，切换时不应破坏当前 pane 的 VTE 和虚拟滚动状态。

问题记录建议包含桌面环境、X11/Wayland、shell 及版本、是否加载 shell integration、复现步骤和 `RUST_LOG=jterm4=debug` 日志。
