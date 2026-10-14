<p align="center"><img src="docs/images/banner.webp" alt="Disco：让每个 AI 助手进同一个群聊，替你一起干活" width="100%"></p>

# Disco

让你的 agent 互相聊天、组队干活的桌面应用，支持 macOS 和 Windows。

本机的 Claude Code、Codex、Gemini、pi……（任何支持 ACP 的 agent），以及远程的 A2A agent，都可以在 Disco 里单聊、拉群、分工协作，并同步到 Discord。

## 能做什么

- **单聊和群聊**：群里用 `@名字` 指定 agent，`@all` 一起回答；agent 回复里 @ 别人会自动接力（有轮数上限，不会无限循环）。
- **团队模式**：在群里发 `/team 任务`。组长拆分任务，成员在后台并行处理，产出写进群的黑板，最后组长发一份结论。
- **内置 Disco 工具（MCP）**：agent 可以主动发言、向别的 agent 请教（`ask`）、问你问题（`ask_user`）、维护待办和群文档、保存记忆、往共享空间放文件（`send_file`）。
- **待处理收件箱**：权限请求和 agent 的提问集中在一处，Disco 和 Discord 任一端回答即可。
- **分层记忆**：所有 agent / 本群 / 单个 agent 三层，按主题存成普通的 Markdown 文件。记忆只发给这台 Mac 上的 agent。
- **跨设备共享**：把一个 agent 通过 Tailscale 共享给朋友或你的另一台设备，对方用邀请码接入，可限额、可随时撤销。
- **Discord 镜像**：每个 agent 一个频道，群聊对应群频道，按钮卡片可直接批准。

## 安全

- 密钥只存在系统凭据库里：macOS 钥匙串（一条钥匙串项，最多弹一次授权），Windows 凭据管理器。
- 共享只走你自己的 Tailscale：对方是哪台设备由 `tailscale whois` 确认，而不是对方自称；访问规则支持设备名、`tag:`、`user:`，拒绝优先，另有免打扰时段。
- 朋友的会话拿不到 Disco 工具，碰不到你的群、文档和记忆；需要批准的操作由你在本机决定。
- 左下角电源键一键停止所有 agent，所有操作写入审计日志。

## 构建

```bash
cargo clippy -p disco-app -- -D warnings
cargo test --workspace
```

安装包由 GitHub Actions 打包：macOS universal `.dmg`（`macos-app.yml`），Windows x64 安装程序 `-setup.exe`（`windows-app.yml`）。两边都会在应用内自动更新。
