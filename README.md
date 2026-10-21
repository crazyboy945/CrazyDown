<p align="center">
  <img src="assets/icon.png" width="160" alt="CrazyDown 图标">
</p>

<h1 align="center">CrazyDown</h1>

<p align="center">基于 Rust 的多协议并发下载器 · HTTP / HTTPS / FTP / BitTorrent</p>

CrazyDown 同时提供命令行与图形界面，支持 HTTP、HTTPS、FTP、磁力链接和本地 `.torrent` 种子。
多连接分段、断点续传、批量任务、实时进度和安全中断都已内置，两种界面共用同一套下载引擎和断点状态。

## 功能特性

- **HTTP / HTTPS**
  - 通过 `Range` 请求把一个文件拆成多段并并发下载，默认 8 个连接。
  - 断点续传：进度写入旁车状态文件，异常退出或 Ctrl+C 后可继续。
  - 使用 `ETag` / `Last-Modified` / `If-Range` 校验远端文件，降低续传时文件已变化的风险。
  - 服务器不支持 `Range` 时自动退回单连接下载，并在服务器断开时于本次运行内重试。
  - 支持空文件（`Content-Length: 0` / `416`）、`Content-Disposition` 文件名（含 RFC 5987）和 URL 内 Basic Auth。
- **FTP**
  - 通过 `SIZE` / `MDTM` 获取大小与修改时间，使用 `REST` 实现多连接分段与断点续传。
  - IPv4 使用 PASV（带 NAT 地址修正），IPv6 自动使用 EPSV；数据连接带连接与读写超时。
  - 服务器不支持 `REST` 时退回单连接下载。
- **BitTorrent**
  - 支持磁力链接与本地 `.torrent` 文件，使用 DHT、Tracker 与 Peer Exchange（基于 librqbit）。
  - 多个 BT 任务共享同一个会话；多文件种子会保存到种子对应的目录。
  - 重新运行相同命令时按已有文件重新校验，跳过已完成的数据，实现 BT 续传。
- **图形界面**
  - 参考 IDM 布局的原生窗口：大图标工具栏、左侧分类树、右侧任务表格与底部状态栏。
  - 任务表格实时显示协议、大小、状态、进度、速度与剩余时间，支持搜索、分类筛选和右键菜单。
  - 多行地址批量添加、列表文件导入、`.torrent` 选择器、保存目录选择与高级设置面板。
  - 可停止、从断点继续、重试或移除任务并打开保存位置；关闭窗口时自动取消任务并等待断点写入。
- **通用能力**
  - 批量列表文件，每行一个地址，可单独指定文件名。
  - 多任务并发（`-j`）、任务级失败隔离、汇总报告与退出码。
  - 断点状态原子写入，每 2 秒刷新一次；完成后自动删除。
  - 密码不会出现在日志、输出路径或状态文件中的源标识里。
  - 种子内路径穿越、非法 Windows 文件名会被拒绝或清理。
  - 附带图标：`assets/icon.png`（1024×1024）与 `assets/icon.ico`（16–256 多尺寸）。

## 构建

需要 Rust stable（支持 edition 2024，建议 1.85 及以上）。

```bash
cd crazy-down
cargo build --release
```

产物为两个可执行文件（Windows 下为 `.exe`）：

- `crazydown`：命令行版本。
- `crazydown-gui`：图形界面版本；Windows release 构建不会弹出控制台窗口。

### Windows GNU 工具链提示

本项目在 Windows 上使用 `x86_64-pc-windows-gnu` 工具链验证。如果构建时报错找不到 `dlltool.exe`，
把工具链的 `self-contained` 目录加入 `PATH`：

```bash
export PATH="$HOME/.rustup/toolchains/stable-x86_64-pc-windows-gnu/lib/rustlib/x86_64-pc-windows-gnu/bin/self-contained:$PATH"
cargo build --release
```

PowerShell：

```powershell
$env:PATH = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained;$env:PATH"
cargo build --release
```

## 快速开始

```bash
# 单个 HTTP 文件（默认保存到当前目录）
crazydown https://example.com/ubuntu.iso

# 指定文件名、保存目录与连接数
crazydown -d D:\Downloads -o ubuntu.iso -s 16 https://example.com/ubuntu.iso

# 多个地址并发下载
crazydown -d D:\Downloads -j 4 https://example.com/a.zip https://example.com/b.zip

# FTP（也可把用户名密码写在 URL 中）
crazydown --ftp-user alice --ftp-pass secret ftp://example.com/pub/file.tar.gz

# 磁力链接 / 本地种子
crazydown -d D:\Downloads "magnet:?xt=urn:btih:..."
crazydown -d D:\Downloads D:\torrents\debian.torrent

# 批量列表
crazydown -i urls.txt -d D:\Downloads
```

按一次 `Ctrl+C`：保存断点并安全退出；连按两次：立即强制退出。

## 图形界面

Windows 下双击 `crazydown-gui.exe`，或在终端中直接启动：

```bash
crazydown-gui.exe
# 也可以预先传入地址，启动后自动加入并开始下载
crazydown-gui.exe https://example.com/a.zip "magnet:?xt=urn:btih:..."
```

Linux / macOS 将命令换成 `./target/release/crazydown-gui`。GUI 只接受地址作为启动参数，
其他选项都在界面中设置。

界面采用参考 Internet Download Manager 的深色布局：顶部品牌栏与图标工具栏、左侧分类树、
右侧任务表格、底部状态栏，常用操作都在一屏内完成。

- **工具栏**：新建下载、继续、停止、全部停止、删除、清除完成、打开目录、日志和设置；
  未选中任务时相关按钮自动禁用。快捷键：`Ctrl+N` 新建、`Ctrl+,` 设置、
  `Delete` 删除选中任务（输入框中不生效）。
- **新建下载**：每行一个地址，支持 HTTP / HTTPS / FTP / 磁力链接和本地 `.torrent` 路径；
  `Ctrl+Enter` 或“开始下载”提交。可指定文件名（地址后加空格和文件名），
  也可导入批量列表或通过文件选择器添加种子。
- **分类与搜索**：左侧按状态（全部 / 下载中 / 等待中 / 已完成 / 失败·取消）和协议
  （HTTP / FTP / BitTorrent）筛选，并显示实时数量；标题栏搜索框按文件名即时过滤。
- **任务表格**：显示协议标签、文件名、大小、状态、进度条、速度与剩余时间，新任务排在顶部。
  单击选中；双击已完成任务打开保存位置，双击失败/取消任务重新排队；
  右键菜单支持打开位置、重试、停止、从列表删除、复制下载地址与打开保存目录。
- **保存目录**：默认使用用户目录下的 `Downloads`，可通过侧栏“更改目录…”或设置面板修改。
  队列中的任务会保留添加时的保存目录，后续修改只影响新任务。
- **高级设置**：并发任务数、分段连接数、重试次数、超时、FTP 用户名密码、覆盖已有文件、
  跳过 HTTPS 证书校验。
- **日志面板**：转发引擎的校验、重试与错误信息，便于排查失败原因。
- **状态栏**：汇总各状态任务数量、总下载速度与快捷键提示。

关闭窗口时会取消所有运行中的任务，等待断点写入磁盘（最多 3 秒）后退出；
重新添加相同地址即可续传。BT 会话在首次添加 BT 任务后固定保存目录，需要更换时重启 GUI。

## 命令行选项

| 选项 | 说明 | 默认值 |
| --- | --- | --- |
| `SOURCE...` | 下载地址：`http(s)://`、`ftp://`、`magnet:` 或本地 `.torrent` | — |
| `-i, --input <FILE>` | 批量下载列表文件 | — |
| `-d, --dir <DIR>` | 保存目录 | `.` |
| `-o, --output <PATH>` | 输出文件名或目录，仅限单个 HTTP/FTP 任务 | 由远端文件名推断 |
| `-j, --jobs <N>` | 同时运行的任务数（1–64） | `3` |
| `-s, --segments <N>` | 每个文件的分段连接数（1–32），别名 `--connections` | `8` |
| `--retries <N>` | 网络失败重试次数（0–20） | `3` |
| `--timeout <SECS>` | 连接与读取超时（1–3600 秒） | `30` |
| `--ftp-user <USER>` | FTP 用户名，地址中未提供时使用 | `anonymous` |
| `--ftp-pass <PASS>` | FTP 密码，别名 `--ftp-password` | 匿名口令 |
| `-f, --force` | 覆盖已有文件、忽略或清除旧断点 | 关闭 |
| `--insecure` | 跳过 HTTPS 证书校验（不安全） | 关闭 |
| `--no-progress` | 不显示动态进度条，仅输出任务日志 | 关闭 |
| `-q, --quiet` | 只输出最终结果与错误 | 关闭 |

退出码：全部成功为 `0`，存在失败为 `1`，被中断为 `130`。

## 批量列表格式

每行一个地址；地址后可用空格或引号指定目标文件名；空行和 `#` 开头的行会被忽略。

```text
# HTTP
https://example.com/a.zip
https://example.com/b.zip  saved-b.zip

# 文件名含空格时使用引号
"ftp://user:pass@example.com/path/c.iso"  "c copy.iso"

# BT 任务只使用 -d 指定目录，不能单独指定文件名
magnet:?xt=urn:btih:...
```

列表中的目标文件名相对于 `-d` 指定的目录。BT 任务无法通过列表指定输出文件名。
`--output` 只能用于单个 HTTP/FTP 任务。

## 断点续传与状态文件

- HTTP / FTP 分段下载会在目标文件旁生成 `<文件名>.crazydown.json`，记录源标识哈希、文件大小、
  ETag / Last-Modified、各分段进度等。状态通过“临时文件 + 重命名”原子更新，每 2 秒保存一次。
- 续传时会校验：源地址哈希、文件总大小、下载模式，以及 ETag / Last-Modified（HTTP 要求一致）。
  任一不一致会拒绝续传，提示使用 `--force` 重新下载；服务器未提供校验头时只能按文件大小续传，并会给出警告。
- 目标文件已存在且没有断点时：自动推导的文件名会改用 `file (1).zip` 这类不冲突名称；
  显式 `-o` 指定的路径则要求加 `--force`。
- 单连接模式（服务器不支持 `Range` / `REST`）无法断点续传：HTTP 会在本次运行内重新下载，
  退出后临时文件会被清理。若已有断点却只能单连接，程序会明确报错。
- BT 没有旁车状态文件：重新运行相同命令时，librqbit 会对已有文件做一次校验（recheck），
  校验通过的分块会被跳过。大文件首次校验可能需要一些时间。
- `--force` 会删除旧断点并允许覆盖已有文件。

## 图标

图标为深蓝到紫色的圆角方形底，叠加青白渐变的下载箭头与速度轨迹，无文字。
`assets/icon.png` 适合文档与展示；`assets/icon.ico` 包含 16、24、32、48、64、128、256
七个尺寸，可用于 Windows 快捷方式：右键快捷方式 → 属性 → 更改图标，选择该 ICO 文件。

## 协议说明与限制

- HTTP 分段下载要求服务器正确处理 `Range` 并返回 `206 Partial Content`；程序通过
  `bytes=0-0` 探测并校验 `Content-Range`。响应经过压缩时不会进行分段续传。
- FTP 仅支持明文 FTP，不支持 FTPS / SFTP；服务器需支持 `REST` 才能分段和续传。
- BT 功能基于 librqbit：支持磁力链接元数据获取、DHT、HTTP/UDP Tracker 与 PEX；
  不包含做种管理、选择性下载或种子制作功能。
- BT 下载的文件由种子内容决定；多文件种子返回目录，单文件种子返回具体文件路径。
- `--insecure` 仅用于自签名或测试环境，会关闭 HTTPS 证书校验。

## 开发与测试

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
```

测试包含 27 个单元测试和 5 个 HTTP 集成测试（本地 TCP 服务器，覆盖分段写入、断点续传、
无 Range 回退、空文件与批量列表）。GUI 复用同一套引擎，另通过真实窗口验证了下载、筛选搜索、
右键菜单、日志与设置弹窗、关闭保存断点与续传。

## 目录结构

```text
src/
  lib.rs               库入口，供 CLI 与 GUI 共用
  main.rs              CLI 入口、Ctrl+C、汇总与退出码
  gui.rs               egui 图形界面、任务调度与中文字体
  bin/crazydown-gui.rs GUI 可执行文件入口
  cli.rs               clap 参数与批量列表解析
  ui.rs                进度条与日志输出
  util.rs              文件名、大小、时间、源地址脱敏
  engine/
    mod.rs             任务分类、并发调度、HTTP 客户端
    http.rs            HTTP/HTTPS 探测、分段下载、单连接回退
    ftp.rs             FTP 探测、分段下载、单连接回退
    torrent.rs         BitTorrent 元数据、下载与状态轮询
    segments.rs        分段模型、位置写入、旁车保存
    state.rs           断点状态结构与原子读写
    output.rs          输出路径分配与冲突处理
tests/
  http_download.rs     端到端 HTTP 集成测试
assets/
  icon.png             应用图标（1024×1024 PNG）
  icon.ico             Windows 多尺寸图标（16–256）
```
