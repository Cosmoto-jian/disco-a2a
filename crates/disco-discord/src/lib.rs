//! Discord adapter (Twilight). Gateway + REST live here; nothing else in Disco
//! imports twilight directly.
#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use twilight_gateway::{EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_model::gateway::event::Event;

/// Several dependencies enable different rustls backends; pin one process-wide.
/// Call once at startup before any TLS.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[derive(Debug, Clone)]
pub struct GatewayReport {
    pub bot_name: String,
    pub guilds: usize,
    pub ready_after: Duration,
}

/// M0-1: connect, IDENTIFY, wait for READY, then disconnect.
pub async fn gateway_check(token: String, timeout: Duration) -> Result<GatewayReport, String> {
    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, token, intents);
    let t0 = Instant::now();
    let wait = async {
        while let Some(item) = shard.next_event(EventTypeFlags::all()).await {
            match item {
                Ok(Event::Ready(r)) => {
                    return Ok(GatewayReport { bot_name: r.user.name, guilds: r.guilds.len(), ready_after: t0.elapsed() });
                }
                Ok(Event::GatewayClose(f)) => {
                    return Err(match f {
                        Some(f) if f.code == 4004 => "token 无效（4004）".to_owned(),
                        Some(f) if f.code == 4014 => "缺少特权 intent：请在开发者后台打开 Message Content Intent（4014）".to_owned(),
                        Some(f) => format!("gateway 关闭 {}: {}", f.code, f.reason),
                        None => "gateway 关闭".to_owned(),
                    });
                }
                Ok(_) => {}
                Err(e) => return Err(format!("gateway 错误: {e}")),
            }
        }
        Err("gateway 流结束".to_owned())
    };
    tokio::time::timeout(timeout, wait).await.map_err(|_| "等待 READY 超时".to_owned())?
}

// ---------------------------------------------------------------- live bridge

use std::sync::Arc;

use tokio::sync::mpsc;
use twilight_http::Client;
use twilight_model::channel::ChannelType;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, WebhookMarker};

/// A human message seen in a guild text channel.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub guild_id: String,
    pub channel_id: String,
    pub author: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub guild_id: String,
    pub guild_name: String,
    pub channel_id: String,
    pub channel_name: String,
}

/// Discord's hard per-message limit.
const MAX_LEN: usize = 2000;

/// Split on line boundaries where possible, never inside a UTF-8 char.
pub fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if cur.len() + line.len() > MAX_LEN && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if line.len() > MAX_LEN {
            let mut rest = line;
            while rest.len() > MAX_LEN {
                let mut cut = MAX_LEN;
                while !rest.is_char_boundary(cut) { cut -= 1; }
                out.push(rest[..cut].to_owned());
                rest = &rest[cut..];
            }
            cur.push_str(rest);
        } else {
            cur.push_str(line);
        }
    }
    if !cur.trim().is_empty() { out.push(cur); }
    out
}

fn id<T>(s: &str) -> Result<Id<T>, String> {
    s.parse::<u64>().ok().and_then(Id::new_checked).ok_or_else(|| format!("bad id {s}"))
}

/// REST side, cheap to clone.
#[derive(Clone)]
pub struct Rest(Arc<Client>);

impl Rest {
    pub fn new(token: String) -> Self {
        Self(Arc::new(Client::new(token)))
    }

    /// Every text channel in every guild the bot is in.
    pub async fn text_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let guilds = self.0.current_user_guilds().await.map_err(err)?.model().await.map_err(err)?;
        let mut out = Vec::new();
        for g in guilds {
            let chans = self.0.guild_channels(g.id).await.map_err(err)?.model().await.map_err(err)?;
            out.extend(chans.into_iter().filter(|c| c.kind == ChannelType::GuildText).map(|c| ChannelInfo {
                guild_id: g.id.to_string(),
                guild_name: g.name.clone(),
                channel_id: c.id.to_string(),
                channel_name: c.name.unwrap_or_default(),
            }));
        }
        Ok(out)
    }

    /// Create the channel webhook used for agent personas. Returns "id\ntoken".
    pub async fn create_webhook(&self, channel: &str) -> Result<String, String> {
        let hook = self.0.create_webhook(id::<ChannelMarker>(channel)?, "Disco").await.map_err(err)?.model().await.map_err(err)?;
        Ok(format!("{}\n{}", hook.id, hook.token.ok_or("webhook without token")?))
    }

    /// Post as an agent persona through the channel webhook.
    pub async fn post_as(&self, webhook: &str, username: &str, text: &str) -> Result<(), String> {
        let (wid, token) = webhook.split_once('\n').ok_or("bad webhook ref")?;
        let wid = id::<WebhookMarker>(wid)?;
        for part in chunk(text) {
            self.0.execute_webhook(wid, token).username(username).content(&part).await.map_err(err)?;
        }
        Ok(())
    }

    /// Post as the bot itself (used for messages typed in the app).
    pub async fn post(&self, channel: &str, text: &str) -> Result<(), String> {
        let ch = id::<ChannelMarker>(channel)?;
        for part in chunk(text) {
            self.0.create_message(ch).content(&part).await.map_err(err)?;
        }
        Ok(())
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Run the Gateway until `stop` fires, forwarding human guild messages.
/// Bots and webhooks (including our own personas) are dropped here, which is
/// the first line of loop protection.
pub async fn run_gateway(
    token: String,
    tx: mpsc::Sender<Incoming>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    on_status: impl Fn(String) + Send + 'static,
) {
    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, token, intents);
    let flags = EventTypeFlags::READY | EventTypeFlags::MESSAGE_CREATE;
    loop {
        tokio::select! {
            _ = stop.changed() => { on_status("已断开".into()); return; }
            item = shard.next_event(flags) => match item {
                None => { on_status("连接已结束".into()); return; }
                Some(Err(e)) => tracing::warn!("gateway: {e}"),
                Some(Ok(Event::Ready(r))) => on_status(format!("在线 · {} · {} 个服务器", r.user.name, r.guilds.len())),
                Some(Ok(Event::MessageCreate(m))) => {
                    if m.author.bot || m.webhook_id.is_some() || m.content.is_empty() { continue; }
                    let Some(guild) = m.guild_id else { continue };
                    let msg = Incoming {
                        guild_id: guild.to_string(),
                        channel_id: m.channel_id.to_string(),
                        author: m.author.global_name.clone().unwrap_or_else(|| m.author.name.clone()),
                        content: m.content.clone(),
                    };
                    if tx.send(msg).await.is_err() { return; }
                }
                Some(Ok(_)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::chunk;

    #[test]
    fn chunks_respect_limit_and_utf8() {
        let s = "好".repeat(1500); // 4500 bytes, one line
        let parts = chunk(&s);
        assert!(parts.iter().all(|p| p.len() <= 2000));
        assert_eq!(parts.concat(), s);
        let lines = "a\n".repeat(1500);
        assert_eq!(chunk(&lines).concat(), lines);
        assert!(chunk("  \n").is_empty());
    }
}
