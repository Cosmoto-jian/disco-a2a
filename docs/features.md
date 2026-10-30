# Disco 功能基线（v1.6.5）

用途：冻结 v1.6.5 的全部用户可见功能。每行写「行为契约」和守护它的测试；改动后这些测试必须全绿，契约要改就同时改这里和测试。

覆盖标记：
- `ST:名称` = `crates/disco-app/src/selftest.rs` 里的 `check!("名称")`（debug 构建 `DISCO_SELFTEST=<mock-acp-agent>`，CI 三平台跑，必须输出 `SELFTEST OK`）。
- `UT:模块::函数` = 单元测试（`cargo test --workspace --lib`）；`IT:文件::函数` = crate 集成测试（`cargo test -p <crate>`）。
- `手动验证` = 无法离线确定性测试（真实 Discord 网关、Tailscale、系统对话框、更新服务器），后面写手动步骤。
- `UI` = 纯界面行为，由 ui-test/ 与 `notes/ui-feature-inventory.md` 守护，本文件只登记。

v1.6.5 基线新增的 selftest 检查在下表中用 ★ 标出；v1.8.0 跨设备传文件新增的用 ◆ 标出；v1.9.0 Cloudflare 隧道新增的用 ▲ 标出；v1.10.0 QQ 官方机器人新增的用 ● 标出；v1.11.0 agent 私聊同步到 QQ 新增的用 ◎ 标出。

## 私聊

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 发消息 `send(dm:<agent>)` | 记录我的消息 → agent 回复写入 `dm:<agent>`；空消息忽略；全部停止时报错「先点恢复」 | ST:dm send, ST:dm reply, ST:dm 再发一遍 |
| 历史 `messages` | 返回会话最近 200 条（作者、正文、时间） | ST:messages |
| 停止当前回复 `stop_reply` | 取消该 agent 在该会话的回复，agent 随即空闲 | ★ST:stop_reply 停止私聊里的回复 |
| 权限请求 | ACP 权限请求变成待处理卡片；允许后 agent 执行（mock 回复以 ran 结尾） | ★ST:权限卡片 允许后执行, IT:disco-acp bridge::streams_and_handles_permission |
| 崩溃自愈 | agent 进程退出后，下一条消息自动重启并回复 | ST:崩溃的 agent 下一条消息自动重启, IT:bridge::dead_agent_is_not_alive |
| 计划步骤 | 回复中 overview 给出 `doing`/`busy_since`，结束后清空 | ST:计划步骤：回复中显示 doing，结束后清除, UT:disco-acp plan_step_is_the_first_entry_in_progress |
| Agent 斜杠命令 `agent_commands` | 返回 agent 声明的 `/` 命令（会话开始后才有） | ST:agent_commands |
| 模型/模式 `agent_configs` | 列出模型等配置；支持旧版 `models`/`modes` 接口（Hermes） | ST:旧版模型接口 列出模型 |
| 切换模型 `set_agent_config` | 立即应用到已开会话，并写入 agent 配置，之后新会话沿用 | ST:旧版模型接口 切换模型, ★ST:模型选择被保存 |
| 需要登录 | 回复失败时写 💡 提示，并弹「需要登录」卡片；选 agent 方式登录后能聊；同一卡片不重复 | ST:需要登录 → 卡片, ST:登录后可以聊, ST:登录卡片不重复 |
| 终端登录方式 | 选「在终端登录」打开系统终端运行登录命令 | 手动验证：用需要终端登录的 agent，点卡片里的终端选项，确认弹出终端并执行登录命令 |
| 失败提示 | 常见错误（Claude 原生二进制、os error 193、未登录、超时）附 💡 提示 | UT:lib::hints_for_known_errors, ST:自检提示 |
| 记住命令 | `记住 …`/`/remember …` 存进该 agent 的记忆，不触发回复；「记住了吗？」不算 | ST:记住(开), ST:记住了吗？ 不当成记住, UT:lib::remember_needs_a_separator |
| 回答里的【记住】 | agent 回答中以【记住】开头的行存进它的记忆（记忆开时） | ★ST:回答里的【记住】存进记忆 |
| 发文件 `send_file` | 文件复制进会话共享空间，贴出路径和 SHA-256，不覆盖同名文件（会话里有其他设备上的 agent 时见「跨设备传文件」） | ST:发文件到共享空间, UT:files::copy_in_hashes_and_never_overwrites, UT:files::names_and_sizes |

## 群聊 / Discord

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 建群 `save_group` | 至少一个成员、要有名字；无 Discord 时或 `qq: true` 时建本地群 `local-…`；组长不在成员里则取第一个 | ST:save_group |
| 删群/撤销 `delete_group` `restore_group` | 删除保留历史和 webhook，撤销后群恢复 | ST:delete+restore group |
| 不 @ → 组长答 | 只有主人一个人时，不 @ 的消息由组长回答 | ST:群 不@ → 组长, ST:Discord 群 主人不@ → 组长 |
| @ 成员 | 被 @ 的成员回答；同时 @ 多个并发回答；`@all` 叫全部；中文紧跟 ASCII 名也算 | ST:群 @m2, ST:群 @m1 @m2 并发, ST:Discord @m1中文 也能叫到, UT:lib::mention_order_and_self_exclusion, UT:lib::routing_v1_5_12, UT:lib::kelvin_sign_mentions |
| 接力 | 回答里 @ 其他成员继续接力（MAX_WAVES 4 / MAX_TURNS 12），开头 @ 回调用者不算接力 | UT:lib::routing_v1_5_12（after_address） |
| `/help` `/stop` | `/help` 回群规；`/stop` 停止群内所有回复并报告「已停止 N 个」 | ST:/help, ★ST:群 /stop 停止回复 |
| `@agent /命令` | 人把 `/命令` 直接交给该 agent；改状态的命令（/model、/clear…）只有主人能用 | UT:lib::group_rules_helpers, ★ST:Discord 朋友不能用改状态的命令 |
| 群里 `记住` | 主人在群里说「记住 …」存进本群记忆层 | ★ST:群里 记住 存进本群记忆 |
| Discord 私聊频道 | agent 自己的频道里，主人/允许名单的消息进入 `dm:<agent>`；陌生人被拦 | ST:Discord 私聊频道 主人, ST:Discord 私聊频道 陌生人被拦 |
| 其他 bot | 普通 bot 消息忽略；另一个 Disco 以 `**名字**：` 转发的人照常记录 | ST:Discord 别的 bot 被忽略, ST:Discord 另一个 Disco 转发的人 |
| 其他 Disco 的 agent | 外来 webhook 记为「名字 (Disco)」，可 @ 我们的 agent，永远不冒充主人或成员 | ST:Discord 另一个 Disco 的 agent @m1, ST:Discord 别的 Disco 的 webhook 叫 me/m1 也不冒充 |
| 接力限流 | 其他 Disco 的 agent 在一个频道 10 分钟内最多触发 6 次 | ★ST:别的 Disco 的 agent 接力限流 |
| Discord 网关/镜像 | 连接、断线重连（RESUME）、致命关闭码停止；私聊/群消息镜像到 Discord（webhook 以 agent 身份发）；长消息按 1900 UTF-16 分段、保留代码块 | UT:disco-discord resume_decisions, chunks_respect_limit_and_utf8, chunks_keep_code_fences, chunks_count_utf16_and_skip_blank；镜像手动验证：连上 Bot 后在 app 私聊发一句，确认 agent 频道出现「**我**：…」和 agent 的回复 |
| 频道列表/同步 `discord_channels` `discord_sync` | 没连 Discord 时报错不崩 | ★ST:没连 Discord 时设置页不崩、给出原因；有 Bot 时手动验证：设置 → Discord → 同步，主服务器出现每个 agent 的频道 |
| 系统代理 | 读 macOS scutil / Windows 注册表 / 环境变量代理 | UT:disco-discord scutil_parsing, reg_parsing, base64_basic_auth |

## 多人群

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 名单 roster | 群里出现过的其他人进入 overview `people` | ST:roster |
| 有别人时不 @ 安静 | 群里有其他人后，不 @ 的消息不叫任何 agent | ST:多人群 不@ 安静 |
| 回复带 @ | 多人群里 agent 回复以 `@提问者` 开头 | ST:多人群 回复以@开头, ST:Discord 群 朋友 @m1 |
| 真实 @ 提醒 | 回复中的 `@名字` 变成 Discord `<@id>` 提醒 | UT:lib::group_rules_helpers（replace_mention）；手动验证：多人 Discord 群里让 agent @ 一个人，对方收到提醒 |

## 团队

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| `/team 任务` | 组长分工 → 成员在隐藏会话里各自完成 → 组长发「🧩 团队结论」，`team_state` 到 done | ST:/team 分工到完成, UT:team::plan_parsing |
| 黑板 `team_files` | 黑板含 context.md、findings/<角色>.md、summary.md | ★ST:团队黑板文件 |

## 记忆

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 列表/删除/撤销 `memories` `forget` `memory_restore` | 设置页列出所有记忆；删除后可撤销恢复 | ST:forget+restore |
| 每个会话的记忆开关 `set_memory` | 关：不给记忆、不存「记住」、MCP 记忆工具拒绝；overview `memory_off` 反映状态 | ST:memory off, ST:overview memory_off, ST:记住(关), ST:memory on, ★ST:分层记忆 self/group/all 各归其层，新会话按层拿到（MCP 关闭时拒绝） |
| 分层 | all / group-<id> / <agent> 三层；新会话拿到 all + 自己 + 所在群；私聊拿不到群层；私聊里不能存群层 | ★ST:分层记忆 self/group/all 各归其层，新会话按层拿到, UT:memory::slugs_and_groups |
| 改名跟随 | 改名带走记忆、记忆开关、待办、文档 | ST:改名带走记忆开关、待办、文档和记忆 |

## 待办 / 文档

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 待办 `space` `todo_new` `todo_set` `todo_remove` | 新增、勾选、删除；空内容报错 | ST:待办 增/完成/删 |
| 文档 `doc_save` | 保存后 `space` 读回 | ST:群文档 |
| MCP 待办 | agent 用 todo_add/todo_list/todo_done；列表默认只列未完成 | ★ST:MCP 待办 增/列/完成, ST:OpenAI 兼容 agent（todo_add） |
| MCP 文档 | doc_write 追加/替换、doc_read 读取；路径不能跳出空间目录 | ★ST:MCP 群文档 追加/替换/读取, UT:mcp::space_names_stay_inside, UT:mcp::tmp_names_are_unique |
| 在访达中显示 `reveal_doc` | 没有文档先建空文档再打开访达/资源管理器 | 手动验证：待办抽屉 → 文档 → 在访达中显示，确认打开并选中 group.md |

## 待处理收件箱

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 列表 `inbox` | 所有待处理权限和提问在一个列表 | ★ST:提问 空回答不算、自由回答、全部停止撤回 |
| 回答 `inbox_answer` | 先答者生效（Discord 或 app），第二个回答报错；提问回答记入会话「回答 X：…」 | ★ST:ask_user 先答者生效 |
| 空回答/自由回答 | 空回答报错且问题保留；自由文本去空白后交给 agent | ★ST:提问 空回答不算、自由回答、全部停止撤回 |
| 过期权限 | 这一轮已结束的权限请求，作答报错且卡片移除 | ★ST:权限 这一轮已结束时作答报错并撤卡 |
| 超时撤回 | ask_user 30 分钟未答撤回；迟到/没人等的回答报错 | ST:ask_user 超时后撤回，迟到的回答报错 |
| 全部停止撤回 | 全部停止时所有提问以「未回答」结束 | ★ST:提问 空回答不算、自由回答、全部停止撤回 |
| Discord 按钮 | 卡片同时发到 Discord 带按钮，允许名单外的人按无效，一端答完另一端结算 | 手动验证：连上 Bot，让 agent 请求权限，在 Discord 点「允许一次」，app 卡片消失并显示「在 Discord 中处理」 |

## 共享 / Tailscale / 对方群

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 生成邀请 `create_share` → `accept_invite` | 邀请码 `disco1:…`，对方接入后能和共享的 agent 聊 | ST:共享 邀请码接入后能聊 |
| 撤销 `revoke_share` | 撤销后对方消息被拒，进行中的回复被取消 | ST:共享 撤销后被拒, UT:share::revoke_takes_only_that_shares_turns |
| 不泄露 | 错误 token 与不存在的共享返回同一拒绝；名片不带系统用户名 | ST:共享 不泄露存在与用户名 |
| 状态/重发 `share_status` `share_invite` | 列出共享（模式、额度、今日用量）；可重发邀请码；撤销后不能再取 | ★ST:共享状态、重发邀请码、坏邀请码 |
| 坏邀请码 | 非 `disco1:`、乱码、缺字段的邀请码被拒 | ★ST:共享状态、重发邀请码、坏邀请码, UT:share::b64_roundtrip |
| 每日额度 | 额度用完当天拒绝；agent 没开始就失败的消息退回额度；按本地日期 | ★ST:共享 每日额度用完即拒、没开始的失败退回额度, UT:share::quota_is_counted_once_and_refunded |
| 模式 | chat：需审批的操作一律拒绝；ask：由主人在收件箱审批 | ★ST:共享 chat 模式拒绝需审批的操作，ask 模式由主人审批 |
| 访问规则 `net_policy` `net_policy_set` | 规则规范化保存并读回；无效规则报错且不覆盖旧规则；deny 优先；allowlist | ★ST:免打扰 net_policy_set 往返，期间拒绝且不扣额度, UT:tailnet::rules_and_policy, UT:tailnet::names_are_scoped |
| 免打扰 | 免打扰时段内拒绝共享消息且不扣额度 | ★ST:免打扰 net_policy_set 往返，期间拒绝且不扣额度, UT:tailnet::quiet_hours |
| 只许 tailnet/本机 | 服务监听 0.0.0.0，guard 只放行回环和 100.64/10、fd7a:115c:a1e0::/48；凭据只发给 tailnet/回环地址；VPN 代理不接管 100.x | UT:tailnet::tailnet_ips, UT:disco-a2a cred_target_rules, cred_target_tricks, origin_follows_whatwg, IT:disco-a2a echo::prefixed_share_with_bearer |
| Tailscale 检测 | CLI Running 用 Self IP，否则扫描 Tailscale 网卡；给出能操作的原因 | UT:tailnet::own_ips_only_while_running, interface_fallback_only_takes_tailscale, why_not_says_what_to_do；手动验证：两台登录 Tailscale 的机器互发邀请并聊天 |
| 设备身份 whois | 非回环连接用 `tailscale whois` 识别设备并套规则；无 CLI 且规则严格时拒绝 | 手动验证：设 allowlist 只含另一台设备，第三台用邀请码连接应被拒 |
| 对方群候选 | 客人在 Discord 群用共享 agent 时（含 `team:group:<ch>:<a>`、`ask:<a>:group:<ch>`）记为 `group:<ch>`，状态重置为确认中；私聊不覆盖 | ★ST:对方群 peer_turn 记下候选群（team:/ask: 形式），私聊不覆盖，换群重新确认, ST:对方群可回话 |
| 跨设备传文件 访客 → 主人 | 和共享来的（A2A）agent 的会话里附件或 MCP `send_file`：先放进本地共享空间，再随下一条消息作为 A2A 文件部件（inline base64）发过去；主人端 guard 取出文件（绕开 A2A 服务器 10 MB 正文上限），校验大小与 SHA-256，用 safe_name 存进 `peer:<id>` 共享空间，会话里显示 📎，agent 的提示里带保存路径和完整 SHA-256；每条消息合计 ≤20 MB；没送到（连不上）的文件留到下一条 | ◆ST:跨设备文件 访客附件随下一条消息送达，主人校验 SHA 后存下并告诉 agent, UT:disco-a2a file_tests::file_part_survives_the_wire；手动验证：两台电脑经 Tailscale 接入邀请码，访客在私聊附一个 5–15 MB 文件再发一句话，主人端 peer 会话出现 📎 且 agent 能读到文件 |
| 跨设备传文件 主人 agent → 访客 | 朋友会话里 agent 只有 `send_file` 一个 MCP 工具；文件只在对方这一轮进行中发回（A2A 文件部件带 url），访客 Disco 用同一共享 token 到同一来源取一次（1 小时内），校验 SHA 后存进自己的会话空间并显示 📎；「只聊天」共享只能发共享空间里的文件，「需要时问我」其他文件要主人在收件箱同意（5 分钟） | ◆ST:跨设备文件 主人的 agent 用 MCP send_file 发回访客（只聊天只能发共享空间里的）, ◆ST:跨设备文件 需要时问我：主人拒绝不发、同意后对方不在线也不发, ST:MCP 伪造 context 被拒；手动验证：主人的 Claude 在朋友会话里被要求「把刚才那个文件发回给我」，访客端出现 📎 |
| 跨设备传文件 限制与安全 | >20 MB 在附件、MCP、接收三处都拒绝并给中文原因；SHA 不符或不完整的文件不落盘；文件名只取安全的基名；主人旁观 peer 会话不能发文件；取回地址需共享 token、只能取一次 | ◆ST:跨设备文件 超过 20 MB 被拒（附件、MCP、接收）, ◆ST:跨设备文件 SHA 不符或不完整就丢弃，文件名不能越界, ◆ST:跨设备文件 旁观中不能发、取回地址要共享 token 且只能取一次 |
| Cloudflare 隧道 设置 `tunnel_status` `tunnel_install` `tunnel_login` `tunnel_setup` `tunnel_set` | 找 cloudflared（Disco 自己下载的 → PATH → Homebrew / Program Files）；没有就从 Cloudflare 官方 GitHub 下载到 `<appdata>/bin`；`cloudflared tunnel login` 打开浏览器登录用户自己的账号（页面打不开时给链接，UI 轮询 cert.pem）；填域名后创建本机的命名隧道（一次，凭据在 `<appdata>/cloudflared/<id>.json`）、`route dns --overwrite-dns` 绑定域名、写 config.yml（只把该域名转到隧道监听口，其余 404）并运行；`share_via=cloudflare` 时启动时自动拉起、退出时结束；与 Tailscale 并存，已发的 Tailscale 邀请继续有效 | UT:tunnel::hosts_and_ids, UT:tunnel::config_routes_only_the_hostname, ▲ST:隧道 邀请码带 Access 令牌，设置往返, UI:T1–T5；手动验证：mac / Windows 各一台，设置里安装 cloudflared → 登录 Cloudflare → 填 disco.<你的域名> → 状态「已连接」 |
| Cloudflare 隧道 安全 | 隧道请求来自本机 cloudflared，但不给本机信任：专用回环监听口（7490–7499）或回环连接上带 Cf-Connecting-Ip / Cf-Ray 即视为隧道；每个请求都要该共享的 token；按来源地址（120/分钟，验 token 前）和按共享（60/分钟）限频；邀请可设有效期（隧道默认 7 天，过期一律拒绝，Tailscale 也适用）；保存 Access Service Token 后没有 Cf-Access-Jwt-Assertion 的隧道请求被拒；名片在隧道上给 `https://<域名>`；共享服务仍监听 0.0.0.0 | ▲ST:隧道 每个请求都要 token，名片给 https 地址, ▲ST:隧道 按共享限频，本机直连不受影响, ▲ST:隧道 开了 Access 后没过 Access 的请求被拒, ▲ST:隧道 邀请过期即拒（隧道和本机都一样） |
| Cloudflare 隧道 邀请与传文件 | 邀请码指向 `https://<域名>/a/<id>`，访客不需要 Tailscale；开了 Access 时邀请码带 CF-Access-Client-Id/Secret，访客 Disco 每个请求（含名片、取文件）都带上；共享 agent 回复中每 25 秒发一次「仍在处理」，避开 Cloudflare 100 秒无响应断开；20 MB 文件照常走隧道 | ▲ST:隧道 文件随消息经隧道送达, UI:T6；手动验证：两台不在同一 Tailscale 的电脑，主人开隧道发邀请码，访客接入后聊天、互发 10 MB 文件；agent 跑一个 >2 分钟的工具也不断 |
| 对方群回话 `peer_check` | 只有 Bot 能看到频道且主人账号在该服务器时才能说话；否则只读并说明缺什么 | ST:对方群需你和 Bot 都在, ★ST:对方群 peer_turn …（peer_check 返回原因）；手动验证：朋友把你的 Bot 和你拉进他的服务器，你的 agent 被 @ 后，peer 会话出现输入框 |

## QQ 群（官方 QQ 机器人）

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 扫码接入 `qq_status` `qq_bind` `qq_bind_cancel` | 设置 → QQ 只有一个「扫码接入」：Disco 向 q.qq.com 建绑定任务（随机 32 字节 key），在 app 里显示二维码；手机 QQ 扫一次、选择或新建机器人并确认后，AppID 和 AES-256-GCM 加密的 AppSecret 回来，本机解密、用 `/users/@me` 验证后存进钥匙串/凭据管理器并连网关；二维码过期自动换新的；任何地方都没有粘贴 AppID / AppSecret 的输入框 | ●ST:QQ 扫码绑定：显示二维码，扫码确认后保存机器人, UT:disco-qq（解密、分段、事件解析）, UI:Q1–Q4；手动验证：mac / Windows 各一台，设置 → QQ → 扫码接入，用手机 QQ 扫码确认，状态变「在线 · 机器人名」 |
| QQ 群绑定 `qq_group_code` | 新建群聊时打开「同步到 QQ 群」（群保持本地，不建 Discord 频道），或在已有本地群的群设置里「获取绑定码」：4 位码 10 分钟有效，同时写进会话；在 QQ 群里 @机器人 发「绑定 1234」即连上，发码的人被认作主人本人；Discord 群聊不能再绑 QQ；可断开 | ●ST:QQ 绑定码把 QQ 群连上，发码的人算本人, ●ST:QQ 解绑群和机器人, UI:Q5–Q8；手动验证：把机器人拉进真实 QQ 群，发绑定码，会话出现「已和 QQ 群连上」 |
| QQ 群收发 | QQ 群里 @机器人 没点名 → 组长；写 @agent 名字点名；其他人记为「名字 (QQ)」；本地和 agent 的消息以 `**名字**：内容` 发到 QQ 群（5 分钟内对最新消息被动回复、最多 5 条，超出走主动消息）；图片/文件附件存进群共享空间并把路径给 agent；agent `send_file` 的文件也发到 QQ 群；重复投递的消息只处理一次；Discord 群聊的行为不变 | ●ST:QQ @机器人 交给组长；回复是对那条消息的被动回复，带名字前缀, ●ST:QQ 群里其他人 @agent：提及改写、附件存进共享空间、名字带 (QQ), ●ST:QQ agent 发文件到 QQ 群，Discord 群聊路径不变 |
| 其他 Disco 的 QQ 机器人 | 另一个 Disco 的机器人以 `**名字**：` 说话时记为「名字 (Disco)」，可接力（同 Discord 限流）；其他机器人与未绑定的群忽略 | ●ST:QQ 其他 Disco 的机器人按「**名字**：」识别，别的机器人和重复消息忽略；手动验证：两个 Disco 的机器人在同一个 QQ 群里，确认 QQ 是否把一个机器人的消息推给另一个（官方 API 默认只推 @机器人 的消息） |
| 待处理卡片 | 需要批准/提问时，QQ 群里发带按钮的卡片；只有主人（发绑定码的人）点按钮才算，其他人点了无效 | ●ST:QQ 待处理卡片带按钮，只有本人点了才算；手动验证：真实 QQ 群里点按钮 |
| 解绑机器人 `qq_unbind` | 断开网关、删除钥匙串里的凭据；已绑定的 QQ 群保留，重新扫码后恢复 | ●ST:QQ 解绑群和机器人, UI:Q9 |

## Agent 私聊同步到 QQ（v1.11.0）

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 导入时选同步位置 `add_agent`（`sync`） | 绑了 QQ 机器人后，添加 Agent 页顶部有「同步到 Discord / QQ」分段选择（快速导入、全部导入、展开表单添加都用这一个选择，全部导入整批一个选择）；没绑 QQ 时不出现、`sync` 为空。默认：上次的选择（QQ 只在机器人仍绑定时），否则看连了什么，两个都连时 Discord；明确选过才记住（`sync_last`）。每个 agent 存 `sync/<名字>`；选 Discord = 以前的行为（连着 Discord 就建自己的频道）；选 QQ 不建 Discord 频道，Discord 连上/同步时也跳过它，会话里写一条用法说明；没绑 QQ 却要求 QQ 时报错 | ◎ST:QQ 私聊：导入时选 QQ，不建 Discord 频道，记住选择；Discord 的 agent 不变, ◎ST:QQ 解绑群和机器人（之后选 QQ 被拒）, UI:S0–S3 |
| 之后改 `set_agent_sync` | agent 信息「本对话」里的「同步到」分段选择（绑了 QQ 或它已同步到 QQ 时出现）。改成 QQ：放开它的 Discord 频道（频道留在 Discord 不删，发一句说明，不再同步）；改回 Discord：重新建/找回频道。改名、删除 agent 时选择跟着走 | ◎ST:QQ 私聊：agent 发文件到私聊；改回 Discord 后不再同步, UI:S4 |
| QQ 私聊收发 | 所有同步到 QQ 的 agent 共用机器人和主人（扫码绑定的人，`qq_owner` = 绑定结果里的 openid）的一个 C2C 私聊。agent 的回复以 `**名字**：` 发过去（失败原因也发）；主人发的消息：以 `@名字` 开头 → 给该 agent 并设为当前；否则给当前 agent（默认第一个 QQ agent）；只写 `@名字` 只切换；`/help` 列出 agent。只认主人的 openid，别人的私聊忽略（记审计）；重复投递只处理一次。被动回复：对最新消息 60 分钟内最多 5 条，带 msg_id + msg_seq，超出或被拒时改发主动消息 | ◎ST:QQ 私聊：主人的消息给当前 agent，回复带「**名字**：」、是被动回复，审批卡片也发到私聊, ◎ST:QQ 私聊：@名字 换 agent，之后的消息也给它；别人的私聊被忽略, UT:qq::picks_the_named_agent, UT:qq::codes_relays_and_reply_slots；手动验证：手机 QQ 给机器人发私聊，确认回复、@切换、主动消息额度 |
| QQ 私聊附件 / 卡片 / 发文件 | 私聊里的图片/文件存进该 agent 的会话空间（同群文件），路径给 agent；它的待处理卡片（批准/提问）发到私聊，按钮只认主人；agent `send_file` 的文件上传到私聊（`/v2/users/{openid}/files` → msg_type 7） | ◎ST:QQ 私聊：附件存进 agent 的空间；卡片按钮只认主人, ◎ST:QQ 私聊：agent 发文件到私聊；改回 Discord 后不再同步, UT:disco-qq parses_group_messages（C2C 按钮、路径） |

## Agent 接入（ACP / OpenAI / A2A / 邀请码）

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 添加命令行 ACP agent `add_agent` | 自定义命令（支持引号路径）加入并能聊；保留名（me/system/all/group-*）拒绝 | ST:add_agent m1, ST:保留名称不能用, UT:lib::split_args_quotes |
| 多实例 | count=N 生成 name、name-2…（可选 git worktree） | ★ST:一次添加多个实例, UT:git::repo_and_worktree |
| OpenAI 兼容 | 流式回复、工具调用走 Disco MCP、同会话两轮排队 | ST:OpenAI 兼容 agent, ST:OpenAI 同一会话两轮排队, UT:openai::sse_lines_keep_split_utf8, tool_calls_merge_without_index, base_url_forms |
| A2A / 邀请码 | 远端 A2A agent 用 bearer 访问 | ST:共享 邀请码接入后能聊, IT:disco-a2a echo::streams_echo_over_jsonrpc |
| 检测已装 agent `detect_agents` `conda_envs` | 列出已知 agent（找到路径与否）和 conda 环境 | ★ST:检测已安装的 agent 和 conda 环境 |
| 启动环境 | 继承登录 shell 环境（PATH、nvm）；Windows 用 .exe/.cmd；Claude/Codex 用用户自己的 CLI | UT:hub parses_env_dump, reads_login_shell_env_quickly, finds_own_cli_not_adapter, login_script_quotes, cmd_script_escapes, pi_mcp_entry_merges |
| 启动超时/并发/取消 | initialize 超时、会话并发、超时取消、kill 结束回合 | IT:disco-acp bridge::initialize_times_out, sessions_run_concurrently, timeout_cancels_the_turn, kill_ends_the_turn |
| 选择文件/文件夹 `choose_path` | 系统对话框，取消返回空 | 手动验证：添加 agent → 程序/文件夹「选择」，选中后路径填入，取消不改 |

## MCP 工具

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 会话令牌 | 每会话一个 token，服务端忽略请求体里的 context；假 token 401；朋友会话没有工具 | ST:MCP 伪造 context 被拒 |
| ask | 向另一个 agent 请教；满载时不死锁；被请教时不能再转问 | ST:满载时 ask 不死锁 |
| ask_user | 弹卡片等待用户回答，返回「用户回答：…」 | ★ST:ask_user 先答者生效 |
| todo_* / doc_* | 见「待办 / 文档」 | ★ST:MCP 待办 增/列/完成, ★ST:MCP 群文档 追加/替换/读取 |
| remember / memory_read | 按 self/group/all 存取；记忆关时拒绝 | ★ST:分层记忆 self/group/all 各归其层，新会话按层拿到 |
| overview 工具 | 一行一个 agent（忙碌时长、分支、步骤） | UT:mcp::agent_lines |
| stdio 代理 | `--mcp` 代理：app 不在时返回空工具列表 | UT:mcp::offline_is_an_empty_server |
| post / send_file 工具 | post 发到群，被 @ 的成员会回答；send_file 放进共享空间（会话里有其他设备上的 agent 时也随下一条消息发给它） | ★ST:MCP post 叫来被 @ 的成员、send_file 放进空间 |

## 头像 / 改名 / 重启 / 停止

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 自定义头像 `set_avatar` | data URL 设置、改名保留、清除；不存在的 agent 报错 | ST:自定义头像：设置、改名保留、清除, UT:avatar::data_urls |
| 旧形象 `set_look` | 只接受短 ASCII 值，出现在 overview `look` | ★ST:set_look 校验并出现在 overview |
| 改名 `rename_agent` | 历史、群成员、记忆、头像跟随；改名后群里 @ 新名字能回答 | ST:rename, ST:改名后群聊 |
| 重启 `restart_agent` | 杀进程再启动，之后能聊 | ST:重启 agent 后还能聊 |
| 删除 `remove_agent` | 删除 agent（可清历史），其共享随之撤销 | ST:remove_agent |
| 全部停止/恢复 `stop_all` `resume` | 停止所有 agent、清收件箱、暂停路由；恢复后能聊 | ST:stop_all, ST:恢复后私聊 |
| overview | agent、群、peer、会话头、Discord 状态、暂停、记忆开关 | ST:overview(空), ST:overview(2), ST:overview(结束) |

## 设置 / 诊断 / 更新

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 一键诊断 `diagnose` | 返回代理、钥匙串、Discord token、Tailscale、Node.js 等检查项，每项有说明 | ★ST:一键诊断返回检查项 |
| Discord 设置 `discord_settings` | 没 token 时返回 problem 不崩；允许名单只保留数字 id | ★ST:没连 Discord 时设置页不崩、给出原因 |
| 保存 token `save_discord_token` | 不完整的 token 直接拒绝；完整的先验证再存 | ★ST:没连 Discord 时设置页不崩、给出原因（拒绝部分）；手动验证：粘贴真实 token，状态变「在线」 |
| 重连 `discord_connect` | 没保存 token 时报错 | ★ST:没连 Discord 时设置页不崩、给出原因 |
| 邀请 Bot | Bot 不在任何服务器时显示邀请链接并轮询；只在一个服务器时自动设为主服务器 | 手动验证：新 Bot 保存 token → 点邀请 → 授权后 4 秒内设置页出现服务器 |
| 检查更新 `check_update` `install_update` | 读 latest.json，有新版显示更新条，更新后重启 | 手动验证：装旧版本，设置 → 检查更新，出现新版本 → 更新并重启后版本号变化 |
| 存储迁移 | 数据库迁移到最新、拒绝更新版本的库 | UT:disco-store migrates_to_latest_and_reopens, rejects_newer_schema, settings_roundtrip, groups_and_messages, agents_upsert_list_delete |

## 安全

| 功能 | 行为契约 | 覆盖 |
|---|---|---|
| 打开链接 `open_url` | 只打开 discord.com、github.com/Cosmoto-jian、tailscale.com | ★ST:只打开白名单链接、只显示数据目录里的文件 |
| 显示文件 `reveal_path` | 只显示 Disco 数据目录里存在的文件，拒绝 `..` 和外部路径 | ★ST:只打开白名单链接、只显示数据目录里的文件 |
| 朋友会话隔离 | `peer:` 会话没有记忆；MCP 只有 `send_file`（只能发回给这位朋友），读群文档等一律拒绝 | ST:MCP 伪造 context 被拒 |
| 冒充防护 | 外来 webhook 永远不是主人；保留名不能用 | ST:Discord 别的 Disco 的 webhook 叫 me/m1 也不冒充, ST:保留名称不能用 |
| 密钥 | 所有密钥存在一条钥匙串/凭据管理器项（Windows 分片）；selftest 用内存保险箱 | 手动验证：设置 → 一键诊断「钥匙串 可读写」 |
| 存储安全 | 全文搜索查询防注入；作用域隔离 | UT:disco-store fts_query_is_injection_safe, scopes_are_isolated_and_quarantine_hides |

## UI（只登记，由 ui-test/ 守护）

侧栏标签与快捷键、列表搜索与折叠、聊天头部（头像上传裁剪、双击改名、模型胶囊、记忆/待办/信息按钮）、Inspector（成员/待办/文档/黑板）、@ 与 / 选择器、团队开关、通知卡片、撤销提示、更新条、首次运行导入、Windows/macOS 外观差异。完整清单与验证见 `notes/ui-feature-inventory.md`（项目笔记，64 项）。

## Tauri 命令覆盖总表（invoke_handler 共 65 个）

| 命令 | 覆盖 |
|---|---|
| overview | ST:overview(空)/(2)/(结束) |
| detect_agents, conda_envs | ★ST:检测已安装的 agent 和 conda 环境 |
| choose_path | 手动验证（系统对话框） |
| memories, forget, memory_restore | ★ST:分层记忆…（memories）, ST:forget+restore |
| add_agent, remove_agent, rename_agent, restart_agent | ST:add_agent m1, ST:remove_agent, ST:rename, ST:重启 agent 后还能聊 |
| set_look | ★ST:set_look 校验并出现在 overview |
| set_avatar | ST:自定义头像：设置、改名保留、清除 |
| stop_reply | ★ST:stop_reply 停止私聊里的回复 |
| messages, send | ST:messages, ST:dm send |
| inbox, inbox_answer | ★ST:提问 空回答不算、自由回答、全部停止撤回 |
| net_policy, net_policy_set | ★ST:免打扰 net_policy_set 往返，期间拒绝且不扣额度 |
| send_file | ST:发文件到共享空间, ◆ST:跨设备文件 访客附件随下一条消息送达…, ◆ST:跨设备文件 旁观中不能发… |
| team_state, team_files | ST:/team 分工到完成, ★ST:团队黑板文件 |
| agent_configs, set_agent_config | ST:旧版模型接口 列出模型/切换模型, ★ST:模型选择被保存 |
| stop_all, resume | ST:stop_all, ST:恢复后私聊 |
| save_discord_token, discord_connect, discord_settings, discord_sync, discord_channels | ★ST:没连 Discord 时设置页不崩、给出原因；真实 Bot 手动验证 |
| diagnose | ★ST:一键诊断返回检查项 |
| open_url, reveal_path | ★ST:只打开白名单链接、只显示数据目录里的文件（拒绝路径；真正打开浏览器/访达手动验证） |
| save_group, delete_group, restore_group | ST:save_group, ST:delete+restore group |
| set_memory | ST:memory off / memory on |
| agent_commands | ST:agent_commands |
| check_update, install_update | 手动验证（更新服务器） |
| space, todo_new, todo_set, todo_remove, doc_save | ST:待办 增/完成/删, ST:群文档 |
| reveal_doc | 手动验证（打开访达） |
| share_status, share_invite | ★ST:共享状态、重发邀请码、坏邀请码 |
| peer_check | ★ST:共享 chat 模式…（无群返回空）, ★ST:对方群 peer_turn … |
| create_share, revoke_share, accept_invite | ST:共享 邀请码接入后能聊, ST:共享 撤销后被拒, ▲ST:隧道 邀请过期即拒（`days`） |
| tunnel_status, tunnel_install, tunnel_login, tunnel_setup, tunnel_set | ▲ST:隧道 邀请码带 Access 令牌，设置往返；安装/登录/建隧道手动验证（真实 Cloudflare 账号） |
| qq_status, qq_bind, qq_bind_cancel, qq_unbind, qq_group_code | ●ST:QQ 扫码绑定…, ●ST:QQ 绑定码把 QQ 群连上…, ●ST:QQ 解绑群和机器人；真实扫码与 QQ 群手动验证 |
| set_agent_sync | ◎ST:QQ 私聊：agent 发文件到私聊；改回 Discord 后不再同步, UI:S4 |

## 维护规则

- 新功能：先在这里加一行，再写对应 `check!`（`within()` 包超时、`wait_msg` 等回复、Discord 用 `on_incoming` 喂消息、共享走 127.0.0.1），不能离线测的写「手动验证」和步骤。
- selftest 里不要在一条语句、`if let`/`for`/`match` 的条件里拿两次 `core.db()` 或其他锁（会死锁），先绑定到变量。
