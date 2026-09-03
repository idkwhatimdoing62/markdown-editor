# Typora 架构与流畅度借鉴笔记

> 目的：把 Typora 的公开行为转化为本项目可以验证、分阶段落地的性能方案。Typora 没有公开渲染器源码，因此文中将“官方明确说明”和“根据行为作出的推断”分开，不把推断当成事实。

## 1. Typora 公开确认的交互模型

Typora 官方把产品定位为“reader and writer”，去掉独立预览窗口、模式切换器和 Markdown 标记，采用无缝 Live Preview。官方说明：行内样式在输入完成后立即显示，块样式通常在输入块或按 Enter 进入下一段时显示。[Typora 首页](https://typora.io/?lang=en)、[Quick Start / Live Preview](https://support.typora.io/Quick-Start/)

这意味着 Typora 的用户感知是“一个文档表面持续更新”，而不是“源码编辑器更新后再刷新另一棵预览 DOM”。这并不能证明 Typora 内部一定只有一棵 AST 或采用某个具体编辑器库；内部实现未公开，以下关于其增量更新的部分只能作为工程推断。

Typora 还公开了几项与长文档流畅度直接相关的行为：

- Outline 点击目标标题后定位，并在滚动或编辑时高亮当前章节；支持按关键词过滤标题。[Outline / Catalog](https://support.typora.io/Outline/)
- Typewriter Mode 会在输入时保持插入点位置；Focus Mode 淡出非当前行/块，减少视觉干扰。[Focus Mode and Typewriter Mode](https://support.typora.io/Focus-and-Typewriter-Mode/)
- 切换源码模式与混合编辑模式要保留滚动位置；官方更新记录也专门修复过大纲锚点和列表缩进问题。[Typora 1.13 更新记录](https://support.typora.io/What%27s-New-1.13/)
- 官方支持自定义字体、宽度、行距和段落间距，说明排版参数属于文档表面的一部分，不能在每次输入时被渲染器重置。[Typesetting with CSS](https://support.typora.io/Typeset/)、[Line Spacing](https://support.typora.io/Line-Spacing/)

## 2. 当前项目和 Typora 的关键架构差异

当前项目是“源码编辑器 + 浏览器预览”双表面：Rust/egui 编辑区保留 Markdown 文本，`src/markdown.rs` 生成解析结果，`src/web_preview.rs` 再生成主题 HTML，Windows/macOS 通过 WebView2/wry 显示；非支持平台使用 egui 原生预览。双栏同步、主题 CSS、Mermaid、图片和导出因此可以复用浏览器能力，但每次文本变化仍可能经过“解析 → HTML 生成 → WebView 通信/布局”的链路。

项目已经有缓存、200 ms 防抖、超过 512 KiB 或 8 张图片时的虚拟块、Mermaid 延迟加载、WOFF 字体和隐藏时释放 WebView。当前解析层对文本编辑会定位受影响的顶层块，并连同前后相邻的安全块一起重解析；段落、标题、代码块、分隔线以及完整的列表、引用、表格块都可走这条路径。未受影响的 `Block` 与 `SpannedEvent` 会复用，后续事件范围按字节差量平移。脚注、原生 HTML、跨代码围栏或无法证明边界安全的编辑仍回退完整解析；在列表等结构块内部插入空行也会保守回退。普通预览在同一安全范围内会复用旧 HTML 壳，只生成变化块并保留未变化块；虚拟预览会保留原分块，只重建包含变化内容的分块。安全的顶层块数量变化（插入、删除）会通过稳定块 ID 和可变增量窗口重建受影响后缀，不再因为数量变化本身触发整页导航；只有块锚点、虚拟分块边界、Mermaid 状态或其他结构无法证明安全时才回退完整渲染。这些优化解决的是首屏和容量问题，也让常规输入更接近 Typora 的局部更新体验。

## 3. 可借鉴的架构策略（按收益/风险排序）

### A. 保持一个长期存活的 WebView 文档壳，只补丁化变更块

不要在每次编辑后重新导航完整 HTML。让 WebView 只加载一次稳定 shell，shell 内包含主题 `<style>`、块清单、滚动同步和查找接口；Rust 发送 `{revision, changed_chunks, source_range}`，浏览器替换受影响块的 `<template>` 内容。未改变的块保留 DOM、图片缓存和布局状态。

实现要点：

1. 把 Markdown 按顶层块记录 `source_start/source_end`、稳定 block id 和 HTML；编辑后从受影响行向前后扩展到安全边界（空行、围栏代码、列表/引用上下文），只重算这些块。
2. 用 revision/generation 丢弃过期补丁；输入连续到来时只保留最新版本，避免 WebView 队列堆积。
3. 替换块前记录当前可见块 id 与其偏移，替换后恢复锚点，再执行源码/预览双向同步。
4. 主题变更只替换可标记的主题 `<style>`，字体大小变更只更新 CSS 变量；只有解析规则或主题结构变化时才重建块。

这是最接近 Typora“文档表面连续更新”体验的方案，也是预计收益最大的改动。`RenderWorker` 会为接受的结果记录端到端耗时、替换块数、虚拟分片数和整页导航次数，并保留最近样本计算 P95；验收指标还应包括滚动锚点漂移。

### B. 将解析结果从整篇文档升级为可增量失效的 block model

当前 `pulldown_cmark` 全文解析路径适合保证语义一致，但不提供编辑器级增量 AST。可以在其上增加一层 block model：保存块边界、标题/列表/围栏状态和渲染 HTML；输入只使局部块失效。

Markdown 的列表、引用、表格和代码围栏可能跨行影响后续语义，不能简单按单行重算。安全做法是：先向前回溯到最近的结构边界，再向后解析到上下文恢复；无法证明边界安全时回退全文解析。新增语法必须同时通过现有 AST、浏览器 HTML 和导出 HTML 的结构对照测试。

### C. 后台解析 + 最新版本优先的调度器

把解析和 HTML 生成移到后台线程，UI 线程只处理输入、光标和 WebView 消息。每个任务带文档版本；新编辑到达时取消或忽略旧任务，完成后仅提交仍是最新版本的结果。防抖不应是唯一机制：防抖控制启动频率，版本门控负责防止旧结果覆盖新结果。

推荐调度顺序：编辑事件合并 → 后台增量/全文解析 → 生成受影响块 → 一次批量 IPC → 浏览器下一帧校正滚动。这样可避免连续输入时 UI 线程被大文档 HTML 生成阻塞。

### D. 通过真实布局锚点改善双栏跟随

Typora 对大纲定位、模式切换和输入位置都有“保留当前位置”的产品承诺。当前项目的估算高度和块懒加载已经降低了长文档成本，但左右内容高度不同，按百分比滚动会天然漂移。

更稳的方案是维护源行/块 id 到预览 DOM 的锚点表：左侧滚动先找到可见源块，右侧对相同 block id 做 `getBoundingClientRect`，再用局部插值或二分查找对齐；目标块未加载时先加载它，布局稳定后再进行一次校正。不要依赖一次性的文档总高度比例。

### E. 将“写作首屏”与“阅读长文档”分开优化

Typora 的 Live Preview 让首屏内容立即可写；WebView2 官方则明确建议不要用 WebView2 渲染启动页或简单 UI，并指出每个控件会启动多进程、带来启动和内存开销。[WebView2 性能最佳实践](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/performance)

因此本项目应保持空工作区、启动提示、菜单和编辑光标由 egui/native 绘制；仅在分栏/阅读模式实际需要主题 CSS 时创建或唤醒 WebView。当前隐藏时释放 WebView 的生命周期策略应保留，不应为了“预热”而常驻多个标签的 WebView。

## 4. WebView2 官方建议对本项目的直接映射

Microsoft 官方确认 WebView2 使用 Edge 的多进程模型；一个进程组包含 browser、renderer、GPU 等进程，多个控件会增加内存。官方建议复用单一环境、避免冗余 WebView，并在暂时不用时设置低内存目标或 `TrySuspendAsync`。[Process model](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/process-model)、[Performance best practices](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/performance)

对本项目的可执行规则：

- 一个窗口只维护一个预览 WebView；多个标签共享环境，不为每个标签创建控件。
- 标签切换只替换当前文档数据/块清单，不销毁并新建 WebView。
- 写作模式、空工作区或长时间后台标签可隐藏/暂停渲染；切回时恢复并校验 revision。
- 用批量 WebMessage/内部协议传递块补丁，避免逐节点、逐字段的频繁跨进程调用；官方建议批量通信并减少不必要通信。
- 保持 Evergreen WebView2 Runtime；固定运行时需要定期更新，否则容易错过性能和内存修复。
- 保留硬件加速；官方指出 GPU 对渲染性能重要，但会带来额外缓冲区内存，基准必须同时记录主进程和后代进程。
- 通过 DevTools/ETW 记录长文档的 DOM、JS heap、布局和进程树峰值，不能只看 Rust 主进程。

WebView2 控件必须在创建它的 UI/STA 线程上访问，耗时操作应异步，不能在 UI 线程 `.Wait()`/`.Result` 阻塞消息泵。[WebView2 threading model](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/threading-model)

## 5. 建议的实施顺序和验收门槛

1. **先做后台调度与版本门控**：不改变 Markdown 语义；在 10 MiB 文档连续输入 30 次时，UI 线程不出现 >50 ms 阻塞，旧版本结果不得覆盖最新版本。
2. **再做稳定 WebView shell + 块补丁**：单段修改只更新相邻安全块，不触发完整导航；记录替换块数、IPC 字节数和光标/滚动漂移。
3. **最后做增量 block model**：先覆盖段落、标题、列表、引用、表格、代码围栏和图片，再逐步纳入 Mermaid/HTML；任何边界不确定时允许全文回退。
4. **同步改进滚动锚点**：以 block id/source range 对齐，验证大纲第 10 章首次点击不跳到第 11 章，双栏跟随在 100 KiB、1 MiB、10 MiB 语料上分别测 P95 漂移。
5. **持续性能门禁**：保留现有 100 KiB/1 MiB/10 MiB 的 WebView 就绪与进程树预算，并新增编辑输入 P95、块补丁数量和整页导航次数。

## 6. 结论

Typora 最值得借鉴的不是某个无法确认的内部库，而是“单一文档表面 + 局部更新 + 位置连续”的产品架构。对本项目而言，继续压缩字体或脚本只能改善首屏；要显著提升输入和滚动流畅度，应把更新单位从“整篇预览”降到“受影响块”，并用后台任务、最新版本门控和真实 DOM 锚点保证交互连续。WebView2 仍适合作为主题忠实的阅读/导出渲染器，但不应承担启动 UI、频繁全量导航或每个标签一个实例的职责。
