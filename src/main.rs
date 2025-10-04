use std::time::Duration;
use std::env;

use anyhow::{Context, Result};
use bot::Bot;
use dotenv::dotenv;
use grammers_client::{Client, Config, InitParams};
use grammers_mtsender::{FixedReconnect, ReconnectionPolicy};
use grammers_session::Session;
use log::info;
use simplelog::TermLogger;
use tokio::net::TcpListener;

mod bot;
mod command;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    TermLogger::init(
        log::LevelFilter::Info,
        simplelog::ConfigBuilder::new()
            .set_time_format_rfc3339()
            .build(),
        simplelog::TerminalMode::Mixed,
        simplelog::ColorChoice::Auto,
    )
    .expect("error initializing termlogger");

    // Parse environment variables with trimming to avoid parsing errors
    let api_id_str = env::var("API_ID").context("API_ID env is not set")?;
    let api_id = api_id_str.trim().parse::<i32>()
        .with_context(|| format!("Failed to parse API_ID: '{}'", api_id_str))?;
    let api_hash = env::var("API_HASH").context("API_HASH env is not set")?;
    let bot_token = env::var("BOT_TOKEN").context("BOT_TOKEN env is not set")?;

    static RECONNECTION_POLICY: &dyn ReconnectionPolicy = &FixedReconnect {
        attempts: 3,
        delay: Duration::from_secs(5),
    };

    let config = Config {
        api_id,
        api_hash: api_hash.clone(),
        session: Session::load_file_or_create("session.bin")?,
        params: InitParams {
            reconnection_policy: RECONNECTION_POLICY,
            ..Default::default()
        },
    };

    let client = Client::connect(config).await?;

    if !client.is_authorized().await? {
        info!("Not authorized, signing in");
        client.bot_sign_in(&bot_token).await?;
    }

    client.session().save_to_file("session.bin")?;

    let bot = Bot::new(client).await?;

    // Bind an HTTP server to port from env or default 8080 (Render requirement)
    let port_env = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let port = port_env.parse::<u16>().unwrap_or(8080);
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    info!("Listening on HTTP port {}", port);

    let bot_runner = tokio::spawn(bot.run());

    // Minimal HTTP server to respond 200 OK for any connection (Render health check)
    let server = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut socket, _addr)) => {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0; 1024];
                        let _ = socket.read(&mut buf).await;
                        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
                Err(e) => {
                    log::error!("accept failed: {}", e);
                }
            }
        }
    });

    // Wait for bot or server tasks (server runs forever)
    tokio::select! {
        _ = bot_runner => {},
        _ = server => {},
    }

    Ok(())
}
