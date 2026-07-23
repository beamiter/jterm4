# jterm4 AI / Agent / Chat 验收矩阵

本文定义 AI transport、Chat、selected Block 与 Shell Agent 的发布验收边界。它是测试要求，不是功能完成声明；尚未实现或没有证据的项目应记录为 `FAIL` 或 `N/A`，不能据此写入用户文档或变更日志。

本轮 P0 重点是 **Stop/Retry、真实取消、运行时硬上限、Agent 单行审批和 selected Block 上下文**。

## 测试准备

使用隔离配置与状态目录，不连接真实云服务，也不使用真实密钥：

```bash
tmp="$(mktemp -d)"
export XDG_CONFIG_HOME="$tmp/config"
export XDG_STATE_HOME="$tmp/state"
cargo build --locked
source <(target/debug/jterm4 --shell-integration bash)
RUST_LOG=jterm4=debug target/debug/jterm4 --mode block --no-restore
```

provider 测试使用 loopback mock server 或可记录 argv、环境、stdin 与子进程 PID 的 `curl` stub。至少准备成功、延迟、401、429、500、断连、空内容、非法 JSON、迟到回复和超大响应 fixture。每项记录 `PASS/FAIL/N/A`、X11/Wayland、shell、provider、复现步骤和脱敏日志。

selected Block fixture：

```bash
printf 'selected-ok\n'
sh -c 'printf "selected-failed\n" >&2; exit 7'
python3 -c 'print("x" * 1000000)'
```

## 本轮 P0 发布门槛

| 编号 | 场景 | 通过标准 |
|---|---|---|
| P0-1 | Chat **Stop** | 延迟请求开始后 Stop 始终可操作；停止后问题保持可重试，不伪造 assistant turn，不覆盖发送期间输入的下一条 draft。 |
| P0-2 | Chat **Retry** | Retry 只为原 chat 创建一个新 request generation，复用用户确认的原问题与上下文；旧请求的迟到数据不能追加、覆盖或复活已删除 chat。 |
| P0-3 | 真实 transport 取消 | Stop、Agent Cancel、删除在途 chat 或关闭窗口后，相关 transport 子进程应在 1 秒内终止并被回收，而不是只丢弃 UI 回调并等待网络超时；不得残留 orphan/zombie。Agent Cancel 不暗中终止已经批准并启动的普通 shell 命令。 |
| P0-4 | 运行时硬上限 | 用户输入、单 turn、selected Block、完整请求、单响应与 live transcript 都有字节上限。分别测试 `limit-1`、`limit`、`limit+1` 及 CJK/emoji 边界；超限时 UI 给出可恢复错误，进程 RSS、持久化文件和 GTK 主线程保持有界。 |
| P0-5 | Agent 单行审批 | 模型 proposal 和用户编辑值中的 CR、LF、TAB、NUL、ESC/C0/C1 控制字符必须在进入 PTY 前被拒绝；审阅卡显示的文本与批准后写入的文本逐字一致。一次批准最多提交一条可见单行 proposal。 |
| P0-6 | 审批前零执行 | 收到 `run` proposal、渲染审阅卡、编辑或 Reject 都不得向 PTY 写入命令或 Enter。只有一次明确的 **Approve & Run** 可将批准值写入固定 pane，并且只提交一次。 |
| P0-7 | selected Block 准确性 | `Ctrl+Shift+Q` 只附加当前 active Block pane 中明确选择的 finished block；请求包含匹配的 command、cwd、exit code 和有界输出，不得误用最新块、其他 pane、background block 或上一次选择。 |
| P0-8 | selected Block 与 Chat 隔离 | 上下文归属发起请求的稳定 chat；切换 chat、并发回复、Stop/Retry、失败恢复和重启不能串话。Ask selected Block 不覆盖已有 composer draft，并提供可见的已附加/已截断状态。 |

## 分域验收矩阵

### Transport 与 provider

| 编号 | 场景 | 通过标准 | 建议层级 |
|---|---|---|---|
| T-1 | Anthropic | endpoint、`x-api-key`、version header、system/history 与 token limit 符合协议；缺少密钥时离线失败并给出可操作提示。 | mock integration |
| T-2 | OpenAI-compatible | endpoint、Bearer 可选鉴权、system/history 与 response shape 正确；无鉴权 loopback 服务可工作。 | mock integration |
| T-3 | Ollama | `/api/chat`、`stream`/options、system/history 与 response shape 正确；无需密钥的本地服务可工作。 | mock integration |
| T-4 | HTTP/协议错误 | 401、429、5xx、空内容、非法 JSON、断连和超时均转为有界、可复制且不含 secret 的错误；Retry 不重复提交旧 generation。 | mock integration |
| T-5 | 凭据与进程 | API key、请求 body 与 URL 不出现在进程 argv、继承环境或普通日志；host bridge 与本机路径使用相同边界。 | process integration |
| T-6 | 并发 | 多 chat 并发有明确全局上限或背压；排队、Stop、删除和窗口关闭不会泄漏线程、channel、timer 或 transport 子进程。 | soak/integration |

### Chat 与 selected Block

| 编号 | 场景 | 通过标准 | 建议层级 |
|---|---|---|---|
| C-1 | 多会话归属 | chat A/B 并发回复只进入各自 owner；unread/busy/error 状态随 owner 更新，切换 UI 不改变目的地。 | store + GUI |
| C-2 | Stop/Retry | 满足 P0-1 至 P0-3；连续 Stop→Retry→Stop 也只能保留一个有效 generation。 | mock GUI |
| C-3 | 草稿与恢复 | 每个 chat 的 draft/context/selection 独立；失败、在途退出和 600 ms 防抖内关窗均可恢复，不把未完成问题伪装成成功 turn。 | state + GUI |
| C-4 | 生命周期 | New/Rename/Archive/Unarchive/Delete 与 50-chat 上限行为明确；Delete 必须确认，删除 busy chat 后迟到回复为 no-op。 | store + GUI |
| C-5 | 搜索 | 搜索范围必须与 UI 文案一致，并覆盖 active 与 archived chat；空结果、CJK、大小写和键盘选择可用。 | unit + GUI |
| C-6 | selected Block | 满足 P0-7、P0-8；成功、失败、超长输出、空输出、含 ANSI 与 secret 的块均需覆盖。 | Block + mock GUI |

### Agent safety

| 编号 | 场景 | 通过标准 | 建议层级 |
|---|---|---|---|
| A-1 | 严格协议 | 仅接受一个字段集合严格的 `say`、`run` 或 `done` JSON object；prose、未知字段、错误类型、过期 ID 和非法命令 fail closed。 | unit |
| A-2 | 单行与显式审批 | 满足 P0-5、P0-6；编辑后执行编辑值，Reject 进入上下文并请求替代方案。 | unit + PTY GUI |
| A-3 | 固定目标 | Agent 固定打开时的 Block pane；切 tab/split/zoom 不漂移。VTE、alt-screen、busy prompt、已有输入、dead/closed pane 都拒绝审批且不改输入。 | PTY GUI |
| A-4 | observation 关联 | 只有该 pane 中已批准 proposal 对应的 finished foreground block 可回灌；exit code 与有界 output 正确，不相关/background block 被忽略。 | PTY integration |
| A-5 | 危险提示 | 顶层 `rm -rf`、`mkfs`、raw-device write、download-pipe-to-shell 等识别结果醒目且不靠颜色单独表达；提示本身不授权执行。 | unit + GUI |
| A-6 | 终止状态 | Cancel、target exit、protocol error、transport error、done 与 turn limit 都有明确状态；迟到回复不能生成 proposal，终止后输入/按钮状态一致。 | unit + mock GUI |
| A-7 | 统一命令审阅 | `?` 建议、命令纠正与 Agent proposal 使用相同的编辑校验、Copy 和动态风险反馈；各自主操作必须准确标出 Insert 或 Run，Enter 不得绕过其语义。 | unit + GUI |
| A-8 | 手动审阅分支 | Agent **Insert only** 保留编辑后的精确单行文本、只写入 shell 编辑行且不提交，transcript 明确记录未执行，后续不得等待或伪造 observation。 | unit + PTY GUI |

### Privacy、accessibility 与性能

| 编号 | 场景 | 通过标准 | 建议层级 |
|---|---|---|---|
| R-1 | 出站脱敏 | 开启 `ai_redact_secrets` 时，对 user turn、历史、system prompt、Agent transcript 与 selected Block 的 command/output/cwd 在出站前统一脱敏；关闭时只按用户显式配置发送。 | mock integration |
| R-2 | 持久化脱敏 | active、inactive、archived chat 的 title/turn/draft/context 均脱敏；snapshot owner-only、原子替换、有界且损坏 AI payload 不影响 tab 恢复。 | state integration |
| R-3 | 诊断 | doctor、support bundle 与普通日志不得包含 key、请求/响应正文、命令、输出、主机名或私有路径。 | CLI integration |
| U-1 | 全键盘 | New/Chats/search/send/Stop/Retry/proposal edit/Reject/Approve/Cancel 均可由键盘到达；Enter/Ctrl+Enter 发送，Shift+Enter 换行，Escape 行为一致。 | X11/Wayland GUI |
| U-2 | IME 与读屏 | fcitx/ibus composition 的 Enter 只确认候选；busy/error/stopped/truncated/danger 状态可被辅助技术读出，危险与 unread 不只依赖颜色。 | manual GUI |
| F-1 | 主线程响应 | 延迟或大响应期间输入、滚动、切 chat 和 Stop 无明显冻结；固定 mock 时记录最大主线程停顿，目标不超过 100 ms。 | GUI benchmark |
| F-2 | 有界 soak | 在硬上限附近运行 50 chats、100 turns/chat、反复 Stop/Retry 和至少 20 次错误请求；RSS 最终稳定，timer/thread/curl 数回到基线。 | soak |
| F-3 | 首次反馈 | Send/Retry 后 100 ms 内显示 busy 状态；mock 交付首个可显示结果后 250 ms 内更新 owner chat。 | GUI benchmark |

## 最小发布证据

```bash
cargo fmt --all -- --check
cargo test --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
```

同时保留以下脱敏证据：

- mock provider 的 request/response case 名称与断言结果；
- Stop/Cancel 前后的 child PID、回收时间和 orphan/zombie 检查；
- Agent 审批前后 PTY 写入次数与精确字节；
- hard-limit 边界与 RSS/线程/子进程基线；
- X11、Wayland、fcitx/ibus 与辅助功能的手工结果。

若仓库尚无对应 mock transport 或 GUI harness，相关项目必须作为手工结果记录，不能仅凭 unit test 判定通过。
