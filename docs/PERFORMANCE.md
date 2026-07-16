# jterm4 性能与可靠性指南

本文只描述当前代码中可验证的机制，不给出与硬件、字体、显示服务器无关的固定性能数字。

## 当前机制

### PTY 输出合并

Block reader 使用容量为 8 的有界队列传递最多 32KiB 的输出块，GTK 每次事件回调只处理一块；当 UI 落后时，reader 和内核 PTY 缓冲区会自然施加背压。首次 eventfd 唤醒立即分发，持续积压的后续块按 8ms 间隔调度，避免输出源反复立即就绪而长期占用 GTK 主循环。

### Block 输出上限

Block 模式提供以下实际生效的配置：

```toml
max_visible_blocks = 200
lazy_load_threshold = 1000
truncation_threshold_lines = 50000
max_collapsed_output_lines = 25
virtual_scroll_margin = 1
```

- `max_visible_blocks` 限制内存中的近期块数量。
- `lazy_load_threshold` 限制从持久化历史恢复的近期记录。
- `truncation_threshold_lines` 限制极大输出的展示高度。
- `max_collapsed_output_lines` 控制折叠摘要。
- `virtual_scroll_margin` 控制视口外预留范围。

当前“虚拟滚动”主要隐藏视口外 widget，并未把每个 Block 的 VTE renderer 真正销毁或循环复用。因此大量长输出仍可能占用显著内存；不要把这些选项视为严格的内存配额。

### 历史写入

Block 历史保存会：

1. 展开路径开头的 `~/`；
2. 创建父目录；
3. 在目标目录写临时文件；
4. flush、`fsync` 后原子 rename；
5. 失败时保留旧文件并清理临时文件。

可选 zstd 压缩：

```toml
block_history_path = "~/.local/share/jterm4/block-history.bin"
block_history_compress = true
```

独立的命令面板历史是轻量 JSONL 索引，只保存 command、cwd、exit code 和完成时间：单条最多 1 MiB、文件超过 32 MiB 会触发压缩，并按 `command_history_max_entries` 保留近期记录。它不复制 Block 输出，因此即使关闭全量 Block 历史也能提供 `@` 搜索。

### Notebook 输出

每个 Notebook cell 的 stdout/stderr 合计最多保留 256 KiB。输出在 worker 中读取并定期送回 GTK；Stop/Stop All 使用进程组取消，避免只终止外层 shell 而留下孙进程。Notebook 不是无限日志查看器，长输出应重定向到文件。

### Git 状态探测

Git 分支、dirty、ahead/behind 通过单个 `git status --porcelain=v2 --branch` 获取。进程级 worker 会合并同一路径的并发请求并缓存最近结果；GTK 主线程最多等待 12ms，慢查询继续在后台执行。Git 子进程仍有 500ms 硬超时，超时会 kill 并 wait，避免僵尸进程。

### Release 配置

`Cargo.toml` 的 release profile 已启用：

```toml
[profile.release]
lto = "thin"
codegen-units = 1
strip = "symbols"
```

这在保留 panic unwind 的同时减少最终二进制体积，并允许跨 crate 优化。

## 可复现测量

先固定 Rust 工具链、GTK/VTE 版本、字体、显示服务器和窗口大小。然后记录：

```bash
cargo build --release --locked
./scripts/benchmark.sh
```

脚本报告二进制大小、headless CLI 启动耗时、测试耗时和直接依赖数。GUI 冷启动、渲染吞吐和 RSS 应在相同的 Xvfb/Xephyr 或 Wayland compositor 环境单独测量，不能用 `timeout GUI --help` 代替。

调试日志：

```bash
JTERM4_LOG=debug target/release/jterm4
JTERM_PROF=1 target/release/jterm4
```

Linux 上可进一步使用：

```bash
perf record -g target/release/jterm4
perf report
```

## 调优建议

### 输出极多

降低 `max_visible_blocks` 和 `truncation_threshold_lines`，并开启压缩历史。命令自身可以落盘时，优先使用 `tee` 或日志文件，不要把无限流量长期保留在终端 widget 中。

### 内存优先

```toml
scrollback = 2000
max_visible_blocks = 50
lazy_load_threshold = 250
virtual_scroll_margin = 0
preserve_live_scrollback = false
```

### 交互优先

保留默认值；不要盲目增大 `virtual_scroll_margin`。Git 状态条在超大或网络文件系统仓库中仍可能有短暂等待，可关闭：

```toml
show_repo_strip = false
```

## 已知架构瓶颈

- Block 完成记录使用多个 VTE widget 和多份输出表示，尚未实现 model-backed renderer recycling。
- 自管 PTY reader 已有约 256KiB 的队列上限和 8ms 积压调度间隔；仍需用 soak test 校验多 PTY 同时输出时的吞吐、公平性和输入延迟。
- 文件树枚举已移到 worker；命令面板打开时仍同步读取有大小上限的 JSONL 历史，极慢网络状态目录可能造成短暂停顿。
- `TermView` 的状态由较多 `Rc<RefCell<_>>` 回调共享，难以做纯 reducer 基准。

后续性能工作应先建立 parser microbenchmark、PTY soak test、固定虚拟显示的 GUI smoke benchmark，再以 byte budget 和 frame time 为验收指标。
