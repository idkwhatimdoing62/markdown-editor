# Markdown 编辑器

一个使用 Rust 和 egui 构建的原生 Markdown 编辑器。采用 Typora 风格的混合编辑：文档始终可编辑，只有当前段落展开编辑器，其余内容保持排版，界面轻量、安静。

当前架构先稳定 `Core + Worker + Snapshot + Revision Guard`：源码、解析树和文档状态由与界面无关的 Rust core 持有，Markdown 解析与全文搜索在后台 worker 执行，egui 的预览与目录每帧只读取当前不可变快照。旧解析或搜索结果不会覆盖新版本；解析追赶期间仍可编辑。文件打开和 HTML/PDF 导出会在后续阶段迁移到同一 worker 边界，详见 [ADR-0004](docs/adr/0004-core-worker-snapshot-revision.md)。

## 主要功能

- 多文件标签页，支持当前段落所见即所得编辑与专注模式
- CommonMark、表格、脚注、任务列表、围栏代码块和 Mermaid 图表
- 原生 egui 混合编辑，目录和段落编辑共用同一份 Markdown 解析结果
- Enter 自动续写列表标记（有序列表递增序号、任务列表续写为未勾选），空列表项上再按 Enter 退出列表
- 专注模式（F8）自动置灰当前段落以外的内容
- 内置“专注写作”主题：素净配色、无装饰，可设置正文字号
- 围栏代码块右上角显示语言
- 打开、保存、另存为、外部修改冲突检测和草稿恢复
- 支持拖入文件打开，并可注册为 `.md`、`.markdown` 的默认应用
- 导出 HTML / PDF，复制 HTML 或渲染内容
- 编辑区与预览区字体统一：英文使用 JetBrains Mono，中文使用霞鹜文楷轻便版

## 环境要求

- Windows 10/11
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

提交前跑一遍与 CI 相同的门禁：

```powershell
cargo fmt -- --check; cargo test; cargo clippy --all-targets -- -D warnings
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
- `Ctrl+Shift+N`：新建独立窗口
- `Ctrl+S`：保存
- `Ctrl+Shift+S`：另存为
- `Ctrl+W`：关闭当前标签
- `Ctrl+Tab`：切换到下一个标签
- `Ctrl+Shift+Tab`：切换到上一个标签
- `Ctrl+E`：聚焦当前段落（未选择段落时聚焦第一段）
- 点击其他排版块：切换当前编辑对象，其余内容保持排版
- `Esc`：收起当前段落编辑区
- `F8`：专注模式（置灰当前段落以外的内容）
- `F9`：打字机模式（编辑段落保持在视口中央）
- `Ctrl+鼠标滚轮`：缩放正文字号

macOS 使用 `⌘` 代替上述快捷键中的 `Ctrl`。

默认启动和双击 Markdown 文件仍会复用现有主窗口。如需从命令行强制新开窗口，可使用：

```powershell
markdown-editor --new-window [文件路径]
```

## 视觉基准

界面遵循一套固定基准。超出基准的取值在加载主题包时会被收敛，并有用例钉住，因此换主题不会让规范失效：

- **间距**：主题间距 token 与区块留白都落在 4px 网格上。第三方主题包里的网格外数值（例如 `block_spacing: 10`、`code_padding_y: 13`）在 `ThemeSpec` 出口吸附到最近的网格点，不会破坏既有主题包。1–3px 的描边、面板内边距和圆角属于刻意的光学微调（与 `focus.css` 里的 1px/3px 边框同理），不参与网格约束。
- **行宽**：正文用等宽字族，字身宽固定为 0.6em，因此一行的字符数完全由字号与栏宽决定。上限 70 字符，内置主题的 `content_width` 是由此反推的 672px（16px 正文），而不是手挑的像素值。
- **对比度**：正文、次要文字（muted）、标题、强调色以及代码块语言标签都必须达到 4.5:1（WCAG AA），专注模式下被弱化的文字同样适用。内置主题的浅色与深色两套配色由单元测试逐项校验，改坏取色会直接让测试失败。
- **字阶**：正文 16px，次要说明 14px，等宽标签与代码 13px。
- **主题颜色格式**：除 `#RRGGBB` 外接受 CSS `oklch(L C H)`（也接受 `55%` 百分比亮度），因此可以按感知均匀的亮度阶组织 token。主题颜色必须不透明，带 `/ alpha` 的写法会报错而不是静默丢弃透明度。

## 字体与授权

- JetBrains Mono：SIL Open Font License 1.1，授权文本见 `fonts/JetBrainsMono-OFL.txt`
- 霞鹜文楷轻便版：SIL Open Font License 1.1，授权文本见 `fonts/LXGWWenKaiLite-OFL.txt`
- Mermaid 11.16.0：MIT License，授权文本见 `assets/Mermaid-MIT.txt`

字体和 Mermaid 运行库随应用内置，运行时不依赖网络。项目代码目前未声明额外的开源许可证。
