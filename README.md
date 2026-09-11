# ClipPlus

Windows 剪贴板增强工具。捕获 → 本地落盘 → 通过网盘目录异地（非实时）同步。

- **快**：捕获走 `WM_CLIPBOARDUPDATE` 消息推送，空闲时 CPU 占用为 0，全程不轮询剪贴板。
- **省**：单进程，无 WebView / Electron / 运行时宿主。
- **异地同步**：历史存成一个「同步文件夹」，不是数据库。扔进 OneDrive / 坚果云 / 任意网盘目录即可跨机器合并，A 机下午复制的东西，B 机晚上开机就能搜到。

## 同步是怎么工作的

**关键决定：一条剪贴板 = 一个独立文件，且按 machineId 分片。**

```
<OneDrive>/ClipPlus/
  3f9a2c81/2026-02/                     ← 机器 A 独占
    1769000000000-9c1f...e0.clip.json   ← 元数据 + 文本（很小）
    1769000000001-4a77...b3.clip.json
    1769000000001-4a77...b3.bin         ← 图片 / 超长文本的重负载
  7be44105/2026-02/                     ← 机器 B 独占
    ...
```

网盘客户端的同步语义是**文件级 last-writer-wins**。所以：

- ❌ 绝不能同步 SQLite 或任何单一数据库文件——两台机器同时写必然产生冲突副本并损坏。
- ✅ 每台机器只写自己名下的目录，**冲突在物理上不可能发生**，不需要锁、不需要协议、不需要服务器。
- ✅ 合并 = 扫描目录 + 按 id 取并集。删除由网盘的文件删除传播，不需要墓碑文件。

文本和小内容直接内联在 `.clip.json` 里；图片和超长文本拆到 `.bin`。这样扫描整个历史只需要读一堆几百字节的小文件——**不会把异地几个 G 的图片历史全拉到本地磁盘**。

写入顺序是「先 blob 后 json」，且 json 走 `.tmp` + 原子改名：读方要么看不到条目，要么看到完整条目，不会读到半个文件。

## 构建

本地不装 SDK，编译全在 GitHub Actions：

```bash
git add -A && git commit -m "..." && git push
```

Actions 跑 `windows-latest` → `dotnet build` → `dotnet publish` → 产物上传为 artifact。

改自包含（不需要用户装 .NET 运行时，体积约 160MB）：把 `.github/workflows/build.yml` 里的 `--self-contained false` 改成 `true`。

## 运行

1. 下载 artifact 里的 `ClipPlus.exe`（框架依赖版需要先装 [.NET 8 Desktop Runtime](https://dotnet.microsoft.com/download/dotnet/8.0)）。
2. 双击运行，托盘出现图标。
3. 按 `Win+Alt+V` 打开历史，输入即过滤，`↑↓` 选择，`Enter` 粘贴回原窗口，`Esc` 取消。

首次运行自动在 `%OneDrive%\ClipPlus` 建立历史目录。没检测到 OneDrive 时退化为本地模式，功能完整，只是不跨机器。

## 配置

`%LOCALAPPDATA%\ClipPlus\settings.json`，改完重启生效（没有设置界面，故意的）：

| 字段 | 默认 | 说明 |
|---|---|---|
| `Hotkey` | `Win+Alt+V` | `Ctrl`/`Alt`/`Shift`/`Win` + 一个键（A-Z、0-9、F1-F24） |
| `SyncRootOverride` | `null` | 手动指定历史目录，覆盖 OneDrive 自动探测 |
| `InlineTextLimit` | `8192` | 超过这个长度的文本拆到 `.bin` |
| `MaxBlobBytes` | `10485760` | 超过这个大小的图片直接丢弃，不进同步目录 |
| `RescanSeconds` | `60` | 兜底重扫周期（正常情况下靠文件监听，不会扫） |
| `CaptureText` / `CaptureImages` / `CaptureFiles` | `true` | 分别开关 |

日志在同目录的 `clipplus.log`。

## 已知限制（v1）

- **删除不跨机器实时传播**。A 机删掉一条，B 机要重启才会消失（重启时重建索引）。v1 故意不做墓碑文件，见 `ClipStore` 的类注释。
- **搜索只覆盖前 512 字符**。超过的部分存在 `.bin` 里，为了不把异地图片和长文本拉下来，索引进内存的只是前缀。
- **粘贴不到管理员权限的窗口**。程序以 `asInvoker` 运行，Windows 的 UIPI 会拦掉注入的按键。所有非提权剪贴板工具都一样。
- **同内容只存一条**。第二次复制同样的东西不会新建文件，也不会把旧条目顶到最前（文件是不可变的，没法原地更新）。
- **界面是深色占位风格**，没做主题适配。

## 结构

```
src/ClipPlus/
  App.xaml(.cs)        装配 + 单实例 + 粘贴回原窗口
  MsgWindow.cs         隐藏消息窗口：剪贴板监听 + 全局热键
  PasteHelper.cs       剪贴板读写、占用重试
  ClipStore.cs         文件布局 + 内存索引 + 后台写入/摄取/重扫
  ClipModel.cs         磁盘 schema 与索引条目
  PopupWindow.xaml     搜索弹窗
  TrayIcon.cs          托盘图标与菜单（图标运行时绘制，仓库里没有二进制资源）
  Settings.cs          配置 + machineId + 开机自启
  Native.cs            全部 P/Invoke
  Log.cs               落盘日志
```
