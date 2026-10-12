//! Preset import templates. ACP agents run locally over stdio and use whatever
//! login the CLI already has on this Mac (Claude, ChatGPT, DeepSeek …).
use serde::Serialize;

#[derive(Serialize, Clone, Copy)]
pub struct Template {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: &'static str, // "acp" | "a2a" | "command"
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub blurb: &'static str,
    pub color: &'static str,
}

pub const TEMPLATES: &[Template] = &[
    Template { id: "claude-code", name: "Claude Code", kind: "acp", command: "npx",
        args: &["-y", "@agentclientprotocol/claude-agent-acp"], blurb: "Anthropic 的编码 agent，用本机已登录的 Claude 账户", color: "#d97757" },
    Template { id: "codex", name: "Codex", kind: "acp", command: "npx",
        args: &["-y", "@agentclientprotocol/codex-acp"], blurb: "OpenAI Codex CLI，用 ChatGPT 账户或 API key", color: "#10a37f" },
    Template { id: "hermes", name: "Hermes", kind: "acp", command: "hermes",
        args: &["acp"], blurb: "Nous Hermes agent，需先安装 hermes", color: "#7c5cff" },
    Template { id: "pi", name: "pi", kind: "acp", command: "npx",
        args: &["-y", "pi-acp"], blurb: "轻量编码 agent pi", color: "#e5484d" },
    Template { id: "opencode", name: "OpenCode", kind: "acp", command: "opencode",
        args: &["acp"], blurb: "开源终端编码 agent，需先安装 opencode", color: "#3b82f6" },
    Template { id: "dsh", name: "dsh", kind: "acp", command: "dsh",
        args: &["--profile", "acp"], blurb: "DeepSeek Harness，需先安装 dsh", color: "#4d6bfe" },
    Template { id: "a2a", name: "A2A 远程 Agent", kind: "a2a", command: "",
        args: &[], blurb: "输入 Agent Card 所在 URL，可选鉴权头", color: "#5865f2" },
    Template { id: "command", name: "自定义命令", kind: "command", command: "",
        args: &[], blurb: "任何说 ACP 协议的本地命令", color: "#8a8a8e" },
];

pub fn get(id: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.id == id)
}
