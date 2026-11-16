# CC Switch 4.0.44 定制版

本仓库以已构建的 Windows x64 4.0.44 定制版为基础整理公开源码，基于 `farion1231/cc-switch`。公开上传时，Google OAuth 客户端配置改为由环境变量提供，详见下文。

## 定制功能

- **模型保活**：Claude Code 与 Codex 可分别按模型开启，使用本机真实客户端生成请求模板；支持成功间隔、失败间隔、超时和恢复并发设置。
- **保活提示词模式（4.0.44）**：每个模型可设置“使用精简提示词”，旧配置默认开启。开启时使用 `Reply only OK.` 并保留 Codex / Claude 的原生客户端身份标识；关闭时使用完整的原生捕获模板。设置随供应商保存生效。
- **Claude 保活兼容性（4.0.44）**：精简模式补回原生身份行，修复已诊断网关因缺少客户端身份而返回 503 的问题。
- **npm Claude Code 保活（4.0.44）**：识别 npm 的 `claude.cmd` / `claude.ps1` 启动入口，从包的 `package.json` 读取实际客户端路径。JS 入口通过 Node 启动，原生入口直接启动；JS 入口跳过旧版不支持的 `--effort` / `--safe-mode` 参数。Node 或入口文件更新后会重新捕获保活模板。
- **聚合重试**：在有效输出开始前重试可恢复的上游错误，支持并发尝试、次数与累计时长限制。
- **重试等待模式**：保活与聚合重试都支持固定间隔和指数退避加随机抖动。固定间隔支持 0 秒；退避模式默认初始上限 1 秒、最大 60 秒，按 1、2、4、8…倍增长，每条并发请求独立取 0 到当前上限的随机等待值，成功后重置。
- **独立上游错误页**：单独保存保活和聚合重试的失败记录，支持来源、应用、状态码、时间范围与关键词筛选，查看和复制详情；凭据脱敏后仅本机保存，保留最近 10,000 条。

旧配置缺少重试模式字段时继续使用固定间隔。模式切换后在对应设置中保存即可生效。

## 构建

依赖：Node.js 20、项目 `package.json` 指定的 pnpm、Rust 1.95；Windows 需要 Visual Studio C++ Build Tools 与 Windows SDK。

```powershell
pnpm install --frozen-lockfile
pnpm tauri build --bundles nsis --no-sign --ci --config src-tauri/tauri.custom.bundle.json
```

安装包输出到 `src-tauri/target/release/bundle/nsis/`；若设置 `CARGO_TARGET_DIR`，请到该目录下的 `release/bundle/nsis/` 查找。

保活运行还需要本机安装对应的 Claude Code 或 Codex 客户端，并在应用中配置供应商、聚合代理及模型保活。

npm 安装的 Claude Code 可直接用于保活。客户端查找保留进程 PATH 的优先顺序，并补充 Windows 注册表 PATH、常见 npm / Node 版本管理器目录。自定义 npm 全局前缀也可通过 `NPM_CONFIG_PREFIX` 指定。

需要指定 Claude Code 安装位置时，可设置 `CC_SWITCH_CLAUDE_EXECUTABLE` 为客户端的绝对路径，支持原生 `.exe`、npm 的 `claude.cmd` / `claude.ps1`，或 `cli.js` 等 JS 入口。JS 入口需要可用的 Node.js；程序优先查找同一安装目录的 Node，再使用工具搜索目录。

`tauri.custom.bundle.json` 用于无需更新签名密钥的本地安装包构建。源码中的应用自动更新地址与公钥沿用上游；自行分发自动更新时需要配置自己的更新来源与签名。上游 GitHub Actions 文件保存在 `.github/upstream-workflows/` 供按需配置。

## Google OAuth 客户端配置

公开源码不内置 Gemini / Antigravity 的 Google OAuth 客户端凭据。需要由 CC Switch 刷新 Gemini 令牌，或使用 Antigravity 的内置登录 / 刷新流程时，可在启动进程的环境中设置以下配套参数；构建时设置同名变量也可将配置用于自己的构建。

| 功能 | 客户端 ID | 客户端密钥 |
| --- | --- | --- |
| Gemini 令牌刷新 | `CC_SWITCH_GEMINI_OAUTH_CLIENT_ID` | `CC_SWITCH_GEMINI_OAUTH_CLIENT_SECRET` |
| Antigravity 登录 / 刷新 | `CC_SWITCH_AGY_OAUTH_CLIENT_ID` | `CC_SWITCH_AGY_OAUTH_CLIENT_SECRET` |

参数必须与相应客户端及令牌的签发客户端相匹配。未配置时，Gemini 可通过原生客户端重新登录以更新访问令牌；Antigravity 的内置登录 / 刷新会提示缺少配置。模型保活、聚合重试、指数退避和上游错误页不依赖这些 Google OAuth 参数。

请通过自己的环境或凭据管理配置这些值，不要提交到 Git。上述公开发布调整未重新运行测试或构建。

## 来源与构建记录

- 上游仓库：`farion1231/cc-switch`
- 本地定制基点：`cf341a8aebc4660782a6e7ce362e5fb16635f684`
- 定制版本：`4.0.44`
- 已完成 Windows x64 Release / NSIS 构建。
- 按使用者要求，该版本未运行测试。
- 本次在首次公开源码提交的基础上，同步已打包的 4.0.44 源码快照；未导入完整上游 Git 历史。
- 含 npm Claude Code 保活修复的 4.0.44 Windows x64 安装包构建已通过；本次公开仓库同步未重新运行测试或构建。

保留原 MIT 许可证与版权声明，详见 [LICENSE](LICENSE)。
