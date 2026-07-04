# X11 与 Wayland 剪切板桥接

[English](README.md)

`clip-bridge` 用于在 X11 和 Wayland 环境之间同步剪切板内容。它面向混合桌面会话：原生 Wayland 应用和 X11/XWayland 应用需要可靠共享剪切板数据。

## 功能

- **双向剪切板同步**：同步 X11 和 Wayland 的常规 Clipboard selection。
- **现代 UTF-8 文本支持**：支持 `text/plain;charset=utf-8`、`text/plain` 和 `UTF8_STRING`。
- **图片剪切板支持**：支持 `image/png`、`image/jpeg` 和 `image/jpg`。
- **Primary selection 支持**：支持将 X11 Primary selection 镜像到 Wayland Primary selection。
- **内容去重**：文本和二进制数据都会去重；图片等二进制内容使用 hash 比较，避免反复比较完整字节缓冲。
- **事件驱动运行循环**：X11 和 Wayland 主循环会阻塞等待协议 fd 和内部唤醒 pipe，不再使用固定定时轮询。

## 范围与限制

- 常规 Clipboard selection 支持双向同步。
- X11 Primary selection 变化可以镜像到 Wayland Primary selection。目前还没有读取 Wayland Primary selection 并同步回 X11。
- 不再主动声明旧式 X11 文本 target，例如 `STRING` 和 `TEXT`。当前实现聚焦现代 UTF-8 文本格式。
- 图片数据按原始 MIME 传输，不做格式转换。例如 `image/jpeg` 仍然会以 `image/jpeg` 传递。
- 已支持 X11 `INCR`，用于读取较大的剪切板负载。

## 构建与运行

### 前置条件

- Rust 1.88.0 或更高版本
- X11 和 Wayland 开发库
- `xclip`，用于 X11 侧手动测试
- `wl-clipboard`，用于 Wayland 侧手动测试

### 构建

```bash
cargo build --release
```

### 运行

```bash
cargo run
```

默认日志级别是 `info`。可以用 `RUST_LOG` 调整：

```bash
RUST_LOG=debug cargo run
RUST_LOG=error cargo run
```

## 手动测试

启动桥接程序：

```bash
cargo run
```

### 文本

从 X11 设置文本：

```bash
echo "X11 text $(date)" | xclip -selection clipboard
```

从 Wayland 读取：

```bash
wl-paste
```

从 Wayland 设置文本：

```bash
echo "Wayland text $(date)" | wl-copy
```

从 X11 读取：

```bash
xclip -selection clipboard -o
```

### 图片

从 X11 设置 PNG 图片：

```bash
xclip -selection clipboard -t image/png -i image.png
```

从 Wayland 读取：

```bash
wl-paste -t image/png > out.png
```

从 Wayland 设置 PNG 图片：

```bash
wl-copy -t image/png < image.png
```

从 X11 读取：

```bash
xclip -selection clipboard -t image/png -o > out.png
```

### Primary Selection

设置 X11 Primary 文本：

```bash
echo "Primary selection $(date)" | xclip -selection primary
```

当合成器支持 data-control primary-selection 路径时，桥接程序可以将它镜像到 Wayland Primary selection。

## 工作原理

### X11 侧

- 创建隐藏的 X11 窗口，用于持有和响应 selection。
- 使用 XFixes selection notification 监听 Clipboard 和 Primary selection 的 owner 变化。
- 请求 selection owner 的 `TARGETS`，再按优先级选择最合适的 MIME 或 X11 target。
- 支持现代 UTF-8 文本 target 和图片 target。
- 使用 `poll()` 等待 X11 connection fd 和内部唤醒 pipe；空闲时 X11 主循环会休眠。

### Wayland 侧

- 使用 `zwlr_data_control_v1` 协议监听和设置剪切板内容。
- 记录 data offer 声明的 MIME 类型，并请求最优先支持的类型。
- 对外只 offer 当前剪切板内容实际拥有的 MIME 类型。
- 使用 `EventQueue::prepare_read()` 配合 `poll()` 等待 Wayland fd 和内部唤醒 pipe；空闲时 Wayland 主循环会休眠。

### 同步逻辑

- 文本使用紧凑 UTF-8 字符串存储。
- 二进制内容使用共享字节缓冲，并用 `xxh3` hash 做去重。
- 同步事件通过中心任务转发，避免直接形成反馈循环。
- 设置剪切板请求会通过 pipe 唤醒对应协议线程，避免定时轮询。

## 支持的格式

### 文本

- `text/plain;charset=utf-8`
- `text/plain`
- `UTF8_STRING`

### 图片

- `image/png`
- `image/jpeg`
- `image/jpg`

## 故障排查

### 构建失败

确认已经安装 X11 和 Wayland 开发库。Arch 系发行包元数据中需要这些运行依赖：

```text
wayland
wayland-protocols
libx11
libxkbcommon
libxkbcommon-x11
```

### 剪切板没有同步

- 确认当前会话里 X11 和 Wayland 连接都可用。
- 使用 `RUST_LOG=debug` 运行，检查是否收到 XFixes 或 Wayland data-control 事件。
- 直接用 `xclip`、`wl-copy` 和 `wl-paste` 验证剪切板工具是否正常。

### 图片无法粘贴

- 检查当前提供的类型：

  ```bash
  wl-paste --list-types
  xclip -selection clipboard -t TARGETS -o
  ```

- 确认复制的图片以 `image/png`、`image/jpeg` 或 `image/jpg` 提供。
- 桥接程序不会在图片格式之间做转换。

### CPU 占用异常

X11 和 Wayland 主循环现在是事件驱动的。如果仍然有明显 CPU 占用，先用 `RUST_LOG=info` 或 `RUST_LOG=error` 运行，再查看 debug 日志确认是否有应用在反复变更剪切板 owner。

## 技术细节

### 依赖

- `x11rb`：X11 绑定
- `wayland-client`：Wayland 客户端库
- `wayland-protocols`：Wayland 协议定义
- `wayland-protocols-wlr`：`zwlr_data_control_v1` 协议绑定
- `tokio`：异步运行时
- `tracing`：日志
- `compact_str`：紧凑 UTF-8 文本存储
- `xxhash-rust`：二进制内容 hash

### 协议支持

- X11 Clipboard selection
- X11 Primary selection 监听
- X11 `TARGETS`、`MULTIPLE` 和 `INCR`
- Wayland `zwlr_data_control_v1`
- 现代 UTF-8 文本 target
- PNG 和 JPEG 图片 MIME 类型

## 许可证

本项目使用 MIT 许可证。

## 贡献

欢迎提交 issue 或 pull request。
