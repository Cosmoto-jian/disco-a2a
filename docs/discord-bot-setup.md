# 新手指南：为 Disco 创建 Discord 机器人

写给第一次用 Discord 开发者后台的人。读完你会知道：

- 机器人要在哪里建
- 所谓“API”到底是什么（其实就是 **Bot Token**）
- 机器人要开哪些开关、给哪些权限，以及每一项的用途

> 简单说：在 Discord 开发者后台 **新建 Application → 打开 Bot 页 → 复制 Token → 打开 Message Content Intent**。
> 权限**不用手动勾选**，Disco 生成的邀请链接已经带上了正确的权限。

---

## 1. 先弄清几个词

| 词 | 意思 |
|---|---|
| **Application（应用）** | 你在开发者后台建的“项目”，一个应用里带一个机器人 |
| **Bot（机器人）** | 应用里那个能进服务器、能收发消息的账号 |
| **Token** | 机器人的“密码”。大家说的“申请 API”，指的就是拿到这个 Token。**不需要另外申请 API Key，也不用审核** |
| **Intent（意图）** | 机器人要接收哪些类型的事件。部分属于“特权”，需要你手动打开 |
| **Permission（权限）** | 机器人进入服务器后能做哪些事，在邀请它的时候授予 |

---

## 2. 创建机器人（约 3 分钟）

1. 打开 <https://discord.com/developers/applications>，用你的 Discord 账号登录。
2. 点右上角 **New Application**，起个名字（例如 `Disco`），勾选同意条款，点 **Create**。
3. 左侧菜单点 **Bot**。新版后台创建应用时会自动生成机器人，这里能直接看到。
4. 可选：上传头像，修改用户名。

### 拿到 Token

1. 还是在 **Bot** 页，找到 **Token**，点 **Reset Token**（第一次也叫这个名字），按提示确认。
2. 点 **Copy**，复制下来的一长串就是 Token。
3. **Token 只会完整显示这一次**。忘了也没关系，再 Reset 一次就行，旧的会立即失效。

> ⚠️ Token 等同于机器人的密码。不要发到群里、截图或者提交到 GitHub。
> 一旦泄露，马上回这里 Reset。Disco 把 Token 存在系统钥匙串（macOS）或凭据管理器（Windows）里，不会写进普通文件。

---

## 3. 必开的开关：Privileged Gateway Intents

在 **Bot** 页往下拉，找到 **Privileged Gateway Intents**：

| 开关 | 要不要开 | 原因 |
|---|---|---|
| **MESSAGE CONTENT INTENT** | ✅ **必须开** | 不开的话机器人只知道“有人发了消息”，**读不到文字内容**，agent 会一直不回复 |
| SERVER MEMBERS INTENT | ❌ 不用 | Disco 不读取成员列表 |
| PRESENCE INTENT | ❌ 不用 | Disco 不关心别人在不在线 |

打开以后记得点页面底部的 **Save Changes**。

> Disco 连接时只订阅三类事件：服务器（Guilds）、服务器消息（Guild Messages）、消息内容（Message Content）。三者中只有最后一个需要手动开。

### 其他选项保持默认

- **Public Bot**：建议**关掉**。关掉后只有你能把机器人拉进服务器，更安全。（Disco 本身也只听应用所有者和白名单用户的指令。）
- **Requires OAuth2 Code Grant**：保持**关闭**，打开会导致邀请失败。

---

## 4. 机器人需要哪些权限

**不需要在开发者后台的 OAuth2 页面手动勾选。** 在 Disco「设置 → Discord」保存 Token 以后，点 **邀请 Bot 进服务器**，就会打开已带好权限的授权页，你只要选择服务器并点「授权」。

链接里包含的权限如下（权限值 `395673979984`）：

| 权限（英文 / 中文） | 用途 |
|---|---|
| View Channels / 查看频道 | 看到 agent 频道和群聊频道 |
| Send Messages / 发送消息 | 回复你的消息 |
| Read Message History / 读取消息历史 | 上下文里引用之前的对话 |
| Manage Channels / 管理频道 | 自动创建 `Disco Agents`、`Disco 群聊` 分类，以及每个 agent 的频道；删除 agent 时清理频道 |
| Manage Webhooks / 管理 Webhook | 让每个 agent 用**自己的名字和头像**发言，而不是都显示成同一个机器人 |
| Embed Links / 嵌入链接 | 链接显示预览 |
| Attach Files / 附加文件 | 发送文件或长输出 |
| Add Reactions / 添加反应 | 给消息加表情反应（预留） |
| Create Public Threads / 创建公开子区 | 在子区（Thread）里对话（预留） |
| Create Private Threads / 创建私密子区 | 同上（预留） |
| Send Messages in Threads / 在子区发消息 | 在子区里回复 |
| Manage Threads / 管理子区 | 整理、归档子区（预留） |

**不需要、也不应该给的权限：**

- **Administrator（管理员）**：权限过大，没有必要。
- 踢人、封禁、管理角色、管理服务器等：Disco 都用不到。

> 「待处理」里的确认按钮（允许 / 拒绝）属于消息组件，不需要额外权限。

### 想自己拼邀请链接？

把 `你的应用ID` 换成开发者后台 **General Information** 页的 **Application ID**：

```
https://discord.com/oauth2/authorize?client_id=你的应用ID&scope=bot&permissions=395673979984
```

---

## 5. 接下来

1. 打开 Disco **设置 → Discord**，粘贴 Token 并保存，状态会变成「在线」。
2. 点 **邀请 Bot 进服务器**，选择你的服务器并授权（没有服务器的话，先在 Discord 左侧点「+」新建一个）。
   如果浏览器没有自动打开，就点 **复制邀请链接**，粘贴到浏览器里打开。
3. 机器人进入服务器后，Disco 会自动识别。只有一个服务器时会直接设为主服务器，并建好 agent 频道。

更完整的使用方法见 [getting-started.md](getting-started.md)。

---

## 6. 常见问题

**Q：机器人在线，但 agent 不回复？**
基本上都是 **MESSAGE CONTENT INTENT** 没开，或者开了没点 Save。

**Q：提示「Bot 缺少权限（403）」？**
机器人进服务器时少了权限。在 Disco 里重新点「邀请 Bot 进服务器」，再授权一次即可，不需要先把机器人踢出去。

**Q：设置里看不到我的服务器？**
说明机器人还没被邀请进去。完成第 4 步授权后，几秒内会自动出现。

**Q：提示「Bot token 无效（401）」？**
Token 复制错了，或者已经被 Reset 过。回开发者后台重新 Reset 并复制。

**Q：朋友在频道里发消息，agent 不理他？**
Disco 默认只听机器人所有者的指令。把朋友的 Discord 用户 ID 填到「设置 → Discord」的白名单里，多个 ID 用逗号分隔。
（获取用户 ID：Discord 设置 → 高级 → 打开「开发者模式」，再右键对方头像 → 复制用户 ID。）
