# M0 验证报告（进行中，2026-10-12）

| 项 | 状态 | 结果 |
|---|---|---|
| M0-1 Discord Gateway | 待做 | 需要 bot token + 测试服务器（用户走 VPN，不再考虑国内网络） |
| M0-2 webhook → 私密线程 | 待做 | 同上 |
| M0-3 A2A 客户端 | 通过 | a2a-client-lf 0.2.8 / a2a-server-lf 0.5.2；JSON-RPC + SSE 流式、鉴权头注入、事件归一化。注意：reqwest 默认读系统代理，本机地址需 NO_PROXY，disco-net 里要显式处理 |
| M0-4 ACP 桥（机制） | 通过 | agent-client-protocol 3.3；mock agent 流式、权限允许/拒绝、子进程环境变量隔离（父进程变量未泄漏）、进程组 kill_on_drop。真实 Claude Code / Codex 待 API key |
| M0-5 SQLite 混合检索 | 通过 | rusqlite 0.37 bundled + FTS5 + sqlite-vec 0.1.9，1 万条 384 维：写入 0.51 s，检索 p50 8.7 ms / p95 8.8 ms（目标 ≤ 50 ms） |
| M0-6 Tauri + Keychain | 待做 | 需 macOS（GitHub Actions macOS runner） |

clippy 全工作区零警告。
