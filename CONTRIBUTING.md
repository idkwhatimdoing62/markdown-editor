# 参与贡献

感谢愿意花时间改这个编辑器。下面是你动手前需要知道的几件事——大部分是这个仓库里已经成立的约定，不是通用建议。

## 环境

稳定版 Rust 工具链（edition 2024），Windows 10/11 或 macOS 11+。Windows 上做安装版还需要 Inno Setup 6。

## 提交前必须通过的门禁

与 CI（`.github/workflows/release.yml` 的 quality job）完全一致：

```powershell
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo audit
```

四条都必须干净。Clippy 带 `-D warnings`，所以新代码不允许引入新警告。

## 代码约定

- **注释用中文，解释「为什么」**。这个仓库的注释记录的是取舍与踩过的坑（例如某个哨兵值为什么被移除），不写「这里给变量加一」这类复述。
- **测试写在同文件的 `#[cfg(test)] mod tests` 里，用例名用中文描述行为**，例如 `解析落后时点击非活动块被门控丢弃`。断言里优先用会被打破的不变量，而不是实现细节。
- **错误处理不吞上下文**：返回 `Result` 时保留原始错误信息，界面上的失败提示要能让人知道下一步做什么。
- 不引入 `unsafe`，除非是 Win32 边界（`storage.rs`、`file_association.rs` 的模式：最小化边界 + `// SAFETY:` 说明不变量）。

## 架构约束（改之前请先读 ADR）

四篇 ADR 在 `docs/adr/`，其中两条是硬约束，违反会静默破坏正确性：

- **单一解析产物**（[ADR-0002](docs/adr/0002-single-markdown-parse-product.md)）：源码只允许被 `markdown::parse_document` 解析一次，任何消费者都不得再建一个 `pulldown_cmark::Parser`。预览、目录、导出必须共用同一份 `ParsedDocument`。
- **revision 守卫**（[ADR-0004](docs/adr/0004-core-worker-snapshot-revision.md)）：文档状态由 `document_core` 持有，后台 worker 的结果只有 revision 与当前源码一致时才能安装；任何绕过 `DocumentState::set_source` / `mark_source_changed` 的原地改动都会让界面渲染上一版 AST。

性能预算见 [ADR-0003](docs/adr/0003-long-document-performance-budget.md)：长文档的每帧成本按可见块计，不允许出现随文档总长增长的每帧工作。

## 设计基准

`README.md` 的「视觉基准」一节是硬规范，并且有用例钉住：

- 间距 token 在 `ThemeSpec` 出口吸附到 4px 网格；
- 正文、次要文字、标题、强调色、代码块语言标签与代码注释都必须达到 4.5:1（WCAG AA），专注模式下被弱化的文字同样适用（`MIN_CONTRAST_RATIO`、`code_comment_color`）；
- 字阶：正文 16px / 次要说明 14px / 等宽标签 11px。

改配色或间距时，跑 `cargo test` 会直接告诉你哪条基准被打破了——不要让用例迁就新取值，除非你同时在 README 里更新规范。

## 提交与 PR

- 提交信息用 Conventional Commits 前缀（`feat:` / `fix:` / `perf:` / `docs:` / `chore:` / `ci:` / `test:` / `refactor:`），主题用英文短句，正文说明**动机与取舍**，而不是罗列改了哪些文件。
- 一个 PR 只做一件事。修 bug 的 PR 请附上能复现的输入（Markdown 片段、操作步骤、系统版本）。
- 涉及界面的改动请附截图或录屏；涉及性能的改动请给出可复现的测量方式（文档规模、命令、前后数据）。
- 中文交流完全没问题。

## 发布流程（维护者）

1. 在 `Cargo.toml`、`Cargo.lock`、`installer/markdown-editor.iss` 三处同步版本号；
2. 在 `docs/CHANGELOG.md` 顶部新增 `## [x.y.z] - YYYY-MM-DD` 小节（发布说明由 release job 从这里按版本号提取，缺失会让流水线失败）；
3. 打 tag 并推送（`git tag vX.Y.Z && git push origin vX.Y.Z`），GitHub Actions 会跑门禁、构建 Windows 安装版/免安装包与 macOS 通用 App，并发布 Release。
