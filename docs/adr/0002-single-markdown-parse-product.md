# ADR-0002：所有消费者共享单一 Markdown 解析产物

> 状态：已采纳
> 日期：2026-08-13
> 影响版本：下一发布版本起

## 背景

旧实现有两条 Markdown 解析路径：

- `markdown.rs` 解析为内部 `Vec<Block>`，供目录、纯文本和 egui 后备预览使用；
- `web_preview.rs` 和 `export.rs` 再次调用 `pulldown-cmark`，生成浏览器 HTML 和导出 HTML。

两条路径共用了部分解析选项，但每个消费者仍能独立改变解析入口。新增语法可能只进入一侧，造成目录、后备预览、浏览器预览和导出对同一份源码理解不同。重复解析还会增加长文档编辑时的计算和临时分配。

## 决策

`markdown.rs` 提供唯一入口 `parse_document(markdown)`，返回 `ParsedDocument`：

```text
ParsedDocument = {
  source: String,
  blocks: Vec<Block>,
  events: Vec<SpannedEvent>
}

SpannedEvent = {
  event: pulldown_cmark::Event<'static>,
  range: Range<usize>
}
```

一次解析同时生成两种消费视图：

- `blocks` 为目录、纯文本和 egui 后备预览提供稳定内部模型；
- `events` 为浏览器 HTML、源码锚点、复制 HTML 和 HTML/PDF 导出提供完整 CommonMark 事件及源码范围。

消费者规则：

1. `Parser::new_ext` 只能出现在 `markdown.rs`；
2. `web_preview.rs` 从 `ParsedDocument.events` 克隆事件，添加标题 ID、源码锚点和本地图片协议；Markdown 图片事件与原生 HTML `img[src]` 使用同一本地资源边界；
3. `export.rs` 从同一事件流生成 HTML，按输出类型处理 Markdown 图片和原生 HTML 图片；
4. `preview.rs`、目录和纯文本读取 `ParsedDocument.blocks`；
5. 活动标签持有 `ParsedDocument`；文本变化优先尝试增量更新，无法证明块边界安全时整份替换；
6. 新语法必须通过事件流与内部模型的结构对照测试。

## 结果

- 同一次编辑只执行一次 Markdown 语义解析；
- 浏览器预览和导出共享同一份源码解析结果与解析选项；
- 源码范围与 HTML 事件来自同一次解析，滚动锚点不会引用另一份解析结果；
- 每个标签会长期保存拥有所有权的事件流，内存占用可能略有增加；
- 段落、标题、代码块和分隔线内的局部文字编辑会复用未受影响的块与事件范围；列表、引用、表格、脚注、HTML 等结构性编辑仍回退全量解析；
- 增量路径只在新旧事件结构可映射时启用，语义不确定时优先保证正确性；
- 内部 `Block` 是事件流的消费模型，复杂新语法仍需补充对应 Builder 分支。

## 验证

结构对照用例必须同时覆盖：

- 标题；
- 有序和无序列表及列表项；
- 表格；
- 围栏代码块和语言；
- 图片；
- 原生 HTML `<img>` 的相对本地路径、保留属性，以及数值型 `width` 在主题 CSS 下仍然生效；
- 链接；
- 加粗。

测试先比较 `ParsedDocument.events` 与 `ParsedDocument.blocks` 的结构计数，再检查同一事件流生成的 HTML 标签。代码审计命令应确认 `Parser::new_ext` 只存在于 `markdown.rs`：

```powershell
rg -n "Parser::new" src
```

## 重新评估条件

出现下面任一情况时重新评估当前产物结构：

- 10 MiB 文档的事件流常驻内存超过产品预算；
- 编辑延迟基准证明全量解析不达标；
- 数学公式、定义列表或复杂脚注无法可靠映射到现有 `Block`；
- 需要插件直接读取语法树或执行增量语义分析。
