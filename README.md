# Markdown 编辑器与预览器

一个使用 Rust、egui 和系统 WebView 构建的 Windows/macOS Markdown 编辑器。左侧专注写作，右侧使用浏览器引擎实时执行 Markdown 主题 CSS。

## 主要功能

- 多文件标签页，以及写作、阅读、分栏与专注模式
- CommonMark、表格、脚注、任务列表、围栏代码块和 Mermaid 图表
- WebView2（Windows）或 WKWebView（macOS）实时预览，忠实执行导入的 CSS 主题
- 支持导入 `.css`、`.zip` 和 JSON 主题包
- 内置少数派经典主题，可设置正文字号
- 围栏代码块右上角显示语言
- 打开、保存、另存为、外部修改冲突检测和草稿恢复
- 支持拖入文件打开，并可注册为 `.md`、`.markdown` 的默认应用
- 导出 HTML / PDF，复制 HTML 或渲染内容
- 编辑区与预览区字体统一：英文使用 JetBrains Mono，中文使用霞鹜文楷轻便版

## 环境要求

- Windows 10/11，运行时需要 [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)
- macOS 11 或更高版本，支持 Apple Silicon 与 Intel
- 从源码构建需要稳定版 Rust 工具链

## 运行与构建

可在 [Releases](https://github.com/idkwhatimdoing62/markdown-editor/releases) 下载 Windows 安装版 Setup、Windows 免安装 ZIP 或 macOS 通用版 App ZIP。

```powershell
cargo run --release
```

```powershell
cargo test
cargo build --release
```

构建产物位于 `target/release/markdown-editor.exe`。

生成安装版需要 Inno Setup 6：

```powershell
& "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" installer\markdown-editor.iss
```

如使用自定义 Cargo 构建目录，可通过 `/DBuildDir=路径` 传给 Inno Setup 编译器。

在 macOS 上生成同时支持 Apple Silicon 与 Intel 的通用 App：

```bash
bash scripts/build-macos.sh
```

macOS 包采用本地临时签名，首次运行若被 Gatekeeper 拦截，可在 Finder 中右键应用并选择“打开”，或在“系统设置 → 隐私与安全性”中允许启动。

## 默认应用

Windows 安装版会把本应用注册为 `.md` 与 `.markdown` 的候选程序，但不会静默修改用户的默认选择。在应用中点击“文件 → 设为 Markdown 默认应用…”，再在 Windows“默认应用”页面确认两个扩展名即可。免安装版也支持该菜单，会按照当前 exe 所在位置注册。

macOS App 声明支持 `md` 与 `markdown` 文档，可在 Finder 的“显示简介 → 打开方式”中选择 Markdown Editor，并点击“全部更改”。

## 快捷键

- `Ctrl+O`：打开
- `Ctrl+N`：新建标签
- `Ctrl+S`：保存
- `Ctrl+Shift+S`：另存为
- `Ctrl+W`：关闭当前标签
- `Ctrl+Tab`：切换到下一个标签
- `Ctrl+Shift+Tab`：切换到上一个标签
- `Ctrl+1`：写作
- `Ctrl+2`：阅读
- `Ctrl+3`：分栏
- `Ctrl+/`：切换写作/阅读
- `F8`：专注模式

macOS 使用 `⌘` 代替上述快捷键中的 `Ctrl`。

## 字体与授权

- JetBrains Mono：SIL Open Font License 1.1，授权文本见 `fonts/JetBrainsMono-OFL.txt`
- 霞鹜文楷轻便版：SIL Open Font License 1.1，授权文本见 `fonts/LXGWWenKaiLite-OFL.txt`
- Mermaid 11.16.0：MIT License，授权文本见 `assets/Mermaid-MIT.txt`

字体和 Mermaid 运行库随应用内置，运行时不依赖网络。项目代码目前未声明额外的开源许可证。
