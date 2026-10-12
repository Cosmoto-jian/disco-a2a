//! M0-1/2: connect Gateway, read READY, find a text channel, post as two webhook
//! personas, open a private thread, and measure latencies. Token from DISCORD_TOKEN.
use std::time::Instant;

use twilight_gateway::{EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::Client;
use twilight_model::channel::ChannelType;
use twilight_model::gateway::event::Event;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Several deps enable different rustls backends; pin ring process-wide.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let token = std::env::var("DISCORD_TOKEN")?;
    // HARK_INJECTED=1: the sandbox proxy adds the Authorization header itself.
    let http = if std::env::var_os("HARK_INJECTED").is_some() {
        Client::builder().build()
    } else {
        Client::new(token.clone())
    };
    let intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
    let mut shard = Shard::new(ShardId::ONE, token, intents);

    let t0 = Instant::now();
    let mut ready = false;
    let gw = async {
        while let Some(item) = shard.next_event(EventTypeFlags::all()).await {
            match item {
                Ok(Event::Ready(r)) => {
                    println!("READY in {:?} as {} ({} guilds)", t0.elapsed(), r.user.name, r.guilds.len());
                    ready = true;
                }
                Ok(Event::GuildCreate(_)) => break,
                Ok(Event::GatewayClose(f)) => { println!("gateway closed: {f:?}"); break }
                Ok(_) => {}
                Err(e) => { println!("gateway error: {e}"); break }
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(20), gw).await;
    if !ready { println!("gateway: no READY (continuing with REST)"); }
    let guild = http.current_user_guilds().await?.model().await?.first().ok_or("bot is in no guild")?.id;
    let channels = http.guild_channels(guild).await?.model().await?;
    let ch = channels.iter().find(|c| c.kind == ChannelType::GuildText).ok_or("no text channel")?;
    println!("guild {guild} channel #{}", ch.name.as_deref().unwrap_or("?"));

    let t = Instant::now();
    let hook = http.create_webhook(ch.id, "disco-spike").await?.model().await?;
    let hook_token = hook.token.ok_or("webhook without token")?;
    println!("webhook created in {:?}", t.elapsed());
    for persona in ["Planner", "Coder"] {
        let t = Instant::now();
        http.execute_webhook(hook.id, &hook_token)
            .username(persona)
            .content(&format!("hello from {persona} (Disco M0 spike)"))
            .await?;
        println!("post as {persona} in {:?}", t.elapsed());
    }
    let t = Instant::now();
    let thread = http.create_thread(ch.id, "disco-dm-spike", ChannelType::PrivateThread).await?.model().await?;
    http.create_message(thread.id).content("private thread OK").await?;
    println!("private thread {} in {:?}", thread.id, t.elapsed());
    http.delete_webhook(hook.id).await?;
    println!("done");
    Ok(())
}
