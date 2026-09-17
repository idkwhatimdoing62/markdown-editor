# 安全策略

## 支持的版本

只处理**最新发布版本**（见 [Releases](https://github.com/idkwhatimdoing62/markdown-editor/releases)）的安全问题。修复会进入下一个补丁版本，不为旧版本回补。

## 报告方式

请**不要**用公开 issue 报告安全问题。用 GitHub 的私密报告通道：

1. 打开仓库的 **Security** 标签页；
2. 点 **Report a vulnerability**（GitHub Security Advisories），写明复现步骤与影响。

如果通道不可用，可以开一个只写「需要私下报告安全问题」的 issue，我会联系你换渠道。

## 这个应用的安全边界

它是一个本机应用：不上传文档、不联网（字体与 Mermaid 随包内置，运行时不请求网络）。因此重点关注的是**本机数据与外部输入**：

- **文件读写**：打开/保存/草稿恢复都在用户目录内，采用原子写入与 compare-and-swap 冲突检测；单个文件上限 10 MB。
- **导入的 Markdown 与原始 HTML**：导出 HTML/PDF 时会消毒链接与图片（拒绝 `javascript:` 等可执行协议、`data:text/html`、绝对路径、`file://`、UNC 与 `//host` 协议相对地址，图片只允许文档目录内、解析后仍在该目录内的相对路径）。
- **主题包与已保存主题**：主题 CSS 会被消毒，像素值解析对多字节字符做了防御；release 构建不再接受外部主题包导入，但会读取 `%APPDATA%/Markdown Editor/themes/current.json`，该文件属于本机用户可写范围。
- **多实例 IPC**：只在回环地址监听，且要求携带本用户配置目录中的共享令牌；伪造或缺失令牌的请求收到否定应答，其携带的文件路径不会进入窗口。
- **注册表写入**：注册文件关联时只写 `HKEY_CURRENT_USER`，不触碰系统级键，也不会静默修改默认应用。

## 不属于安全问题的情形

- 恶意 Markdown 导致的渲染错乱或卡顿（可以提普通 issue，附上文件）；
- 已获得本机用户权限的程序读写你自己的文档与配置；
- Gatekeeper / SmartScreen 对未签名构建的拦截提示（见 README 的安装说明）。
