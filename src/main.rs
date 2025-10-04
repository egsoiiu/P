use std::time::Duration;
use std::env;

use anyhow::{Context, Result};
use bot::Bot;
use dotenv::dotenv;
use grammers_client::{Client, Config, InitParams};
use grammers_mtsender::{FixedReconnect, ReconnectionPolicy};
use grammers_session::Session;
use log::{info, error};
use simplelog::{TermLogger, ConfigBuilder, TerminalMode, ColorChoice};
use tokio::net::TcpListener;
use tokio::io::{AsyncWriteExt, AsyncReadExt}; // Import for socket read/write

mod bot;
mod command;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    TermLogger::init(
        log::LevelFilter::Info,
        ConfigBuilder::new()
            .set_time_format_rfc3339()
            .build(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .expect("error initializing termlogger");

    // Load environment variables with better error handling
    let api_id_str = env::var("API_ID").context("API_ID env is not set")?;
    let api_id = api_id_str.trim().parse::<i32>()
        .with_context(|| format!("Failed to parse API_ID: '{}'", api_id_str))?;
    let api_hash = env::var("API_HASH").context("API_HASH env is not set")?;
    let bot_token = env::var("BOT_TOKEN").context("BOT_TOKEN env is not set")?;

    info!("Bot starting with API_ID: {}", api_id);

    static RECONNECTION_POLICY: &dyn ReconnectionPolicy = &FixedReconnect {
        attempts: 3,
        delay: Duration::from_secs(5),
    };

    // Use /tmp for session file since Render's filesystem is ephemeral
    let config = Config {
        api_id,
        api_hash: api_hash.clone(),
        session: Session::load_file_or_create("/tmp/session.bin")?,
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

    client.session().save_to_file("/tmp/session.bin")?;
    info!("Bot authorized successfully");

    let bot = Bot::new(client).await?;

    // Start HTTP health check server for Render
    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let port = port.parse::<u16>().unwrap_or(8080);

    let listener = TcpListener::bind(("0.0.0.0", port)).await
        .with_context(|| format!("Failed to bind to port {}", port))?;

    info!("Health check server listening on port {}", port);

    // Run the bot asynchronously in a spawned task
    let bot_runner = tokio::spawn(async move {
        if let Err(e) = bot.run().await {
            error!("Bot runner error: {}", e);
        }
    });

    // Minimal HTTP server for health checks (to keep Render happy)
    let http_server = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut socket, addr)) => {
                    info!("Health check from {}", addr);

                    tokio::spawn(async move {
                        let response =
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: text/plain\r\n\
                             Content-Length: 2\r\n\
                             \r\n\
                             OK";

                        if let Err(e) = socket.write_all(response.as_bytes()).await {
                            error!("Failed to write health response to {}: {}", addr, e);
                        }
                    });
                }
                Err(e) => {
                    error!("Accept error: {}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    tokio::select! {
        result = bot_runner => {
            if let Err(e) = result {
                error!("Bot task failed: {}", e);
            }
        }
        result = http_server => {
            if let Err(e) = result {
                error!("HTTP server task failed: {}", e);
            }
        }
    }

    info!("Bot shutting down");
    Ok(())
}
