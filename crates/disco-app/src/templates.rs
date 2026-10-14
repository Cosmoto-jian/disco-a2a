//! Agents Disco can import. Local ACP agents are found by looking for their CLI on
//! the user's PATH (a few file stats, no subprocesses), so the list only shows what
//! is really installed. Remote A2A agents and custom commands are added by hand.
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::hub::which;

/// A local agent Disco knows how to launch over ACP.
pub struct Known {
    pub id: &'static str,
    pub name: &'static str,
    /// Present if any of these binaries is on PATH.
    pub detect: &'static [&'static str],
    /// Launch candidates in order of preference; the first whose command exists wins
    /// (a globally installed ACP adapter starts faster than `npx`).
    pub launch: &'static [(&'static str, &'static [&'static str])],
    pub blurb: &'static str,
    pub color: &'static str,
}

pub const KNOWN: &[Known] = &[
    Known { id: "claude-code", name: "Claude Code", detect: &["claude", "claude-agent-acp", "claude-code-acp"],
        launch: &[("claude-agent-acp", &[]), ("claude-code-acp", &[]), ("npx", &["-y", "@agentclientprotocol/claude-agent-acp"])],
        blurb: "Anthropic 编码 agent，用本机已登录的 Claude 账户", color: "#d97757" },
    Known { id: "codex", name: "Codex", detect: &["codex", "codex-acp"],
        launch: &[("codex-acp", &[]), ("npx", &["-y", "@agentclientprotocol/codex-acp"])],
        blurb: "OpenAI Codex CLI，用 ChatGPT 账户或 API key", color: "#10a37f" },
    Known { id: "gemini", name: "Gemini CLI", detect: &["gemini"], launch: &[("gemini", &["--experimental-acp"])],
        blurb: "Google Gemini CLI", color: "#4285f4" },
    Known { id: "qwen", name: "Qwen Code", detect: &["qwen"], launch: &[("qwen", &["--experimental-acp"])],
        blurb: "通义千问 Qwen Code", color: "#615ced" },
    Known { id: "goose", name: "Goose", detect: &["goose"], launch: &[("goose", &["acp"])],
        blurb: "Block 开源 agent Goose", color: "#111111" },
    Known { id: "opencode", name: "OpenCode", detect: &["opencode"], launch: &[("opencode", &["acp"])],
        blurb: "开源终端编码 agent", color: "#3b82f6" },
    Known { id: "hermes", name: "Hermes", detect: &["hermes"], launch: &[("hermes", &["acp"])],
        blurb: "Nous Hermes agent", color: "#7c5cff" },
    Known { id: "dsh", name: "dsh", detect: &["dsh"], launch: &[("dsh", &["--profile", "acp"])],
        blurb: "DeepSeek Harness", color: "#4d6bfe" },
    Known { id: "pi", name: "pi", detect: &["pi-acp", "pi"], launch: &[("pi-acp", &[]), ("npx", &["-y", "pi-acp"])],
        blurb: "轻量编码 agent pi", color: "#e5484d" },
];

#[derive(Serialize)]
pub struct Detected {
    pub id: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    pub color: &'static str,
    /// Where the CLI was found, or None when it is not installed.
    pub found: Option<String>,
}

pub fn detect(path: &str) -> Vec<Detected> {
    KNOWN.iter().map(|k| Detected {
        id: k.id, name: k.name, blurb: k.blurb, color: k.color,
        found: k.detect.iter().find_map(|b| which(b, path)).map(|p| p.to_string_lossy().into_owned()),
    }).collect()
}

/// (command, args, blurb) to launch the known agent `id`.
pub fn resolve(id: &str, path: &str) -> Option<(String, Vec<String>, &'static str)> {
    let k = KNOWN.iter().find(|k| k.id == id)?;
    let (cmd, args) = k.launch.iter().find(|(c, _)| which(c, path).is_some()).or(k.launch.last())?;
    Some(((*cmd).to_owned(), args.iter().map(|s| s.to_string()).collect(), k.blurb))
}

#[derive(Serialize, Clone)]
pub struct CondaEnv {
    pub name: String,
    pub path: String,
}

/// Local conda/mamba environments: ~/.conda/environments.txt plus the usual install
/// roots and their envs/ folders. Pure file system reads, so it is instant.
pub fn conda_envs(home: &Path) -> Vec<CondaEnv> {
    let mut paths: Vec<PathBuf> = std::fs::read_to_string(home.join(".conda/environments.txt")).unwrap_or_default()
        .lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect();
    let roots = ["miniconda3", "anaconda3", "miniforge3", "mambaforge", "micromamba", ".micromamba", "opt/miniconda3", "opt/anaconda3"]
        .iter().map(|r| home.join(r))
        .chain(["/opt/homebrew/Caskroom/miniforge/base", "/opt/homebrew/Caskroom/miniconda/base", "/opt/miniconda3", "/opt/anaconda3", "/usr/local/miniconda3", "/usr/local/anaconda3"].iter().map(PathBuf::from));
    for root in roots {
        paths.push(root.clone());
        if let Ok(rd) = std::fs::read_dir(root.join("envs")) {
            paths.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()));
        }
    }
    let mut out: Vec<CondaEnv> = Vec::new();
    for p in paths {
        if !p.join("conda-meta").is_dir() { continue; }
        let path = p.to_string_lossy().into_owned();
        if out.iter().any(|e| e.path == path) { continue; }
        let name = if p.parent().and_then(|d| d.file_name()).is_some_and(|n| n == "envs") {
            p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        } else { "base".into() };
        out.push(CondaEnv { name, path });
    }
    out
}

/// Lowest Node.js major the template's ACP adapter runs on (from its package `engines`).
pub fn node_min(id: &str) -> Option<u32> {
    match id {
        "claude-code" => Some(22), // @agentclientprotocol/claude-agent-acp
        "pi" => Some(20),          // pi-acp
        _ => None,
    }
}
