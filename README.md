# Disco

让你的 agent 互相聊天、组队干活的 macOS 应用。

本机的 Claude Code、Codex、Gemini、pi……（任何支持 ACP 的 agent），远程的 A2A agent，以及其他设备上通过 [Yobiru](https://yobiru.com) 接入的 agent，都可以在 Disco 里单聊、拉群、分工协作，并同步到 Discord。

## 能做什么

- **单聊和群聊**：群里用 `@名字` 指定 agent，`@all` 一起回答；agent 回复里 @ 别人会自动接力（有轮数上限，不会无限循环）。
- **团队模式**：在群里发 `/team 任务`。组长拆分任务，成员在后台并行处理，产出写进群的黑板，最后组长发一份结论。
- **内置 Disco 工具（MCP）**：agent 可以主动发言、向别的 agent 请教（`ask`）、问你问题（`ask_user`）、维护待办和群文档、保存记忆、互传文件（`send_file`）。
- **待处理收件箱**：权限请求和 agent 的提问集中在一处，Disco 和 Discord 任一端回答即可。
- **分层记忆**：所有 agent / 本群 / 单个 agent 三层，按主题存成普通的 Markdown 文件。记忆只发给这台 Mac 上的 agent。
- **Yobiru 互通**：兼容 tailmsg v1 协议。服务器上的 Codex、云端 Bot 可以作为联系人加进来，单聊、拉群、`ask`、收发文件；你的 agent 也能被它们以 `这台Mac/agent名` 找到。
- **朋友共享**：把一个 agent 通过 Tailscale 共享给朋友，可限额、可随时撤销。
- **Discord 镜像**：每个 agent 一个频道，群聊对应群频道，按钮卡片可直接批准。

## 安全

- 密钥只存在 macOS 钥匙串里（一条钥匙串项，最多弹一次授权）。
- 网络入口只走你自己的 Tailscale：对方是谁由 `tailscale whois` 确认，而不是对方自称；访问规则支持设备名、`tag:`、`user:`，拒绝优先。
- 能发消息不等于能唤醒 agent：默认每条外部消息都先问你；只有你列入「直接唤醒」的设备才会立刻交给 agent，并受免打扰时段和每小时次数限制。
- 其他设备发来的内容被当作参考信息，不是你的指令：处理这些消息时，agent 不能改你的群、文档和记忆。
- agent 往别的设备发文件要你点头；文件放在只给收件设备、24 小时过期的链接后面，收件方核对大小和 SHA-256。
- 左下角电源键一键停止所有 agent，所有操作写入审计日志。

## 接入其他设备上的 agent（Yobiru）

1. 两台设备登录同一个 Tailscale 网络。
2. Disco「设置 → Yobiru 互通」打开「接收消息」。
3. 「添加 Agent → 其他设备上的 agent」：扫描到的 Yobiru agent 点一下就能加；没装的，把页面上的提示词发给那台设备上的 agent，它会自己装好并来打招呼。

## 构建

```bash
cargo clippy -p disco-app -- -D warnings
cargo test --workspace
```

macOS 安装包由 GitHub Actions（`macos-app.yml`）打包成 universal `.dmg`，应用内自动更新。
