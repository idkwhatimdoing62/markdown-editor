# Markdown 编辑器与预览器

一个使用 Rust、egui 和 WebView2 构建的 Windows Markdown 编辑器。左侧专注写作，右侧使用浏览器引擎实时执行 Markdown 主题 CSS。

## 主要功能

- 写作、阅读、分栏与专注模式
- CommonMark、表格、脚注、任务列表和围栏代码块
- WebView2 实时预览，忠实执行导入的 CSS 主题
- 支持导入 `.css`、`.zip` 和 JSON 主题包
- 内置少数派经典主题，可设置正文字号
- 围栏代码块右上角显示语言
- 打开、保存、另存为、外部修改冲突检测和草稿恢复
- 导出 HTML / PDF，复制 HTML 或渲染内容
- 编辑区与预览区字体统一：英文使用 JetBrains Mono，中文使用霞鹜文楷轻便版

## 环境要求

- Windows 10/11
- [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)
- 从源码构建需要稳定版 Rust 工具链

## 运行与构建

```powershell
cargo run --release
```

```powershell
cargo test
cargo build --release
```

构建产物位于 `target/release/markdown-editor.exe`。

## 快捷键

- `Ctrl+O`：打开
- `Ctrl+S`：保存
- `Ctrl+Shift+S`：另存为
- `Ctrl+1`：写作
- `Ctrl+2`：阅读
- `Ctrl+3`：分栏
- `Ctrl+/`：切换写作/阅读
- `F8`：专注模式

## 字体与授权

- JetBrains Mono：SIL Open Font License 1.1，授权文本见 `fonts/JetBrainsMono-OFL.txt`
- 霞鹜文楷轻便版：SIL Open Font License 1.1，授权文本见 `fonts/LXGWWenKaiLite-OFL.txt`

字体随应用内置，运行时不依赖网络或系统安装字体。项目代码目前未声明额外的开源许可证。
