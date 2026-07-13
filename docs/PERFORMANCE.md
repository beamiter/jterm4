# jterm4 性能与可靠性指南

本文只描述当前代码中可验证的机制，不给出与硬件、字体、显示服务器无关的固定性能数字。

## 当前机制

### PTY 输出合并

Block reader 会合并连续的 byte 事件，并限制每次 GTK idle 回调处理的消息数量，避免一批输出触发大量零碎重绘。高吞吐场景仍使用无界消息队列，这是后续需要改成有界 byte budget 的架构项。

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

### Git 状态探测

Git 分支、dirty、ahead/behind 查询有 500ms 子进程硬超时；超时会 kill 并 wait，避免僵尸进程或无限冻结。查询目前仍在 GTK 主线程执行，多次慢查询可能造成短暂停顿，后续应迁移到可取消后台 worker。

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
- 自管 PTY reader 使用无界队列，极端生产速率可能超过 UI 消费速度。
- 文件树、历史加载和部分 Git 工作仍有主线程 I/O。
- `TermView` 的状态由较多 `Rc<RefCell<_>>` 回调共享，难以做纯 reducer 基准。

后续性能工作应先建立 parser microbenchmark、PTY soak test、固定虚拟显示的 GUI smoke benchmark，再以 byte budget 和 frame time 为验收指标。
