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
