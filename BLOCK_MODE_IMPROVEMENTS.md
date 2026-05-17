# Block Mode VTE4 功能对齐优化

## 概述
本次优化使 jterm4 的 block mode 与 VTE4 在功能和用户体验上达到对齐，用户使用起来无感知差异。

## 已完成的改进

### 1. URL 超链接点击支持 ✅
**功能**: 为活动块的命令和输出视图添加 Ctrl+Click 打开 URL 功能

**实现位置**: `src/block_view.rs` ActiveBlock::new()
- 添加 GestureClick 控制器
- 检测 Ctrl+Click 事件
- 使用 `get_url_at_position()` 提取 URL
- 调用 `open_uri()` 打开链接

**测试方法**:
```bash
# 在 jterm4 中运行
echo "Visit https://github.com"
# 按住 Ctrl 并点击链接，应该在浏览器中打开
```

### 2. 鼠标光标自动隐藏 ✅
**功能**: 模拟 VTE4 的 `pointer_autohide` 功能，鼠标静止1秒后自动隐藏光标

**实现位置**: `src/block_view.rs`
- ActiveBlock 的 command_view 和 output_view
- FinishedBlock 的 command_view 和 output_view

**实现细节**:
- 添加 EventControllerMotion
- 鼠标移动时显示文本光标
- 使用 glib::timeout 1秒后隐藏
- 自动取消之前的超时

**测试方法**:
```bash
# 在输出区域移动鼠标，应该看到光标
# 停止移动1秒后，光标应该消失
```

### 3. 鼠标事件报告基础 ✅
**功能**: 添加鼠标事件格式化辅助函数

**实现位置**: `src/block_view.rs`
- `format_mouse_event_sgr()` - 格式化 SGR 模式鼠标事件

**说明**: 
- 为未来的完整鼠标事件报告做准备
- 交互式应用会自动切换到 VTE fallback，获得完整鼠标支持
- Block mode 主要用于命令历史查看，鼠标报告优先级较低

## 已有的 VTE4 功能对齐

### VTE Terminal 功能 (VTE Fallback)
Block mode 在检测到 alt-screen 模式时自动切换到 VTE fallback，提供完整的 VTE4 功能:
- ✅ Sixel 图像支持 (`enable_sixel`)
- ✅ 超链接支持 (`allow_hyperlink`)
- ✅ 粗体增亮 (`bold_is_bright`)
- ✅ 光标自动隐藏 (`pointer_autohide`, `mouse_autohide`)
- ✅ 完整的鼠标事件报告

### Block Mode 原生功能
- ✅ 括号粘贴模式 (Bracketed Paste)
- ✅ 应用程序光标模式 (Application Cursor)
- ✅ 光标形状切换 (Block/Underline/Bar)
- ✅ 鼠标报告模式跟踪 (Click/Button/Motion/SGR)
- ✅ 文本选择和复制
- ✅ 字体缩放
- ✅ 颜色主题
- ✅ 滚动和搜索

## 性能优化

### 已有的优化机制
1. **ANSI 缓存 (LRU)**
   - 缓存 ANSI 到 Pango 的转换结果
   - 避免重复解析相同的 ANSI 序列
   - 可配置容量 (默认足够大)

2. **输出批处理**
   - 最小批处理: 10ms
   - 最大批处理: 100ms
   - 自适应调整批处理延迟

3. **滚动防抖**
   - 50ms 防抖延迟
   - 合并快速滚动请求
   - 避免级联定时器

4. **Widget 池复用**
   - 复用已完成的块 widgets
   - 减少分配开销
   - 最大池大小: 20

5. **虚拟滚动视口**
   - 只渲染可见和附近的块
   - 隐藏远离视口的块
   - 显著提升大量历史记录时的性能

## 用户体验改进

### 无缝切换
- Block mode 和 VTE mode 之间自动切换
- 交互式应用 (vim, less, htop) 自动使用 VTE fallback
- 命令历史查看使用 Block mode
- 用户无需手动选择模式

### 一致的交互
- URL 点击行为与 VTE 一致
- 鼠标光标行为与 VTE 一致
- 文本选择和复制与 VTE 一致
- 键盘快捷键与 VTE 一致

## 配置选项

### 批处理优化
```toml
# ~/.config/jterm4/config.toml
output_batch_min_ms = 10  # 最小批处理延迟
output_batch_max_ms = 100 # 最大批处理延迟
```

### 环境变量
```bash
export JTERM4_BATCH_MIN=10
export JTERM4_BATCH_MAX=100
```

## 测试验证

### 手动测试
1. **URL 点击**
   ```bash
   echo "Visit https://github.com"
   # Ctrl+Click 应该打开浏览器
   ```

2. **光标自动隐藏**
   - 在输出区域移动鼠标
   - 停止移动 1 秒
   - 光标应该消失

3. **文本选择**
   - 在输出中拖动鼠标选择文本
   - Ctrl+Shift+C 复制
   - 应该能粘贴到其他应用

4. **交互式应用**
   ```bash
   vim test.txt  # 应该自动切换到 VTE fallback
   less /var/log/syslog  # 应该自动切换到 VTE fallback
   ```

5. **性能测试**
   ```bash
   # 生成大量输出
   for i in {1..1000}; do echo "Line $i: $(date)"; done
   # 应该流畅渲染，无明显延迟
   ```

## 与 VTE4 对比

| 功能 | VTE4 | Block Mode | 说明 |
|-----|------|-----------|------|
| URL 点击 | ✅ | ✅ | Ctrl+Click |
| 光标自动隐藏 | ✅ | ✅ | 1秒延迟 |
| Sixel 图像 | ✅ | ✅ | VTE fallback |
| 鼠标事件报告 | ✅ | ✅ | VTE fallback |
| 括号粘贴 | ✅ | ✅ | 原生支持 |
| 文本选择 | ✅ | ✅ | GTK 原生 |
| 颜色支持 | ✅ | ✅ | 256 色 + RGB |
| 字体缩放 | ✅ | ✅ | Ctrl+Plus/Minus |
| 命令历史 | ❌ | ✅ | Block mode 特有 |
| 块导出 | ❌ | ✅ | JSON/Markdown |
| 块搜索 | ❌ | ✅ | Block mode 特有 |

## 总结

通过本次优化，jterm4 的 block mode 已经在功能和用户体验上与 VTE4 对齐:
- ✅ 所有常用的 VTE4 功能都已支持
- ✅ 交互行为与 VTE4 一致
- ✅ 性能优化确保流畅体验
- ✅ 自动模式切换对用户透明
- ✅ 额外提供了命令历史管理功能

用户可以无缝使用 block mode，无需关心底层实现细节。
