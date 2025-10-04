use std::{sync::Arc, time::Duration};
use anyhow::Result;
use async_read_progress::TokioAsyncReadProgressExt;
use dashmap::{DashMap, DashSet};
use futures::{stream::once, StreamExt, TryStreamExt};
use grammers_client::{
    button, reply_markup,
    types::{CallbackQuery, Chat, Message, User},
    Client, InputMessage, Update,
};
use log::{error, info, warn};
use reqwest::Url;
use scopeguard::defer;
use stream_cancel::{Trigger, Valved};
use tokio::sync::Mutex;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tokio_util::io::StreamReader;
use bytes::Bytes;

use crate::command::{parse_command, Command};

#[derive(Debug)]
pub struct Bot {
    client: Client,
    me: User,
    http: reqwest::Client,
    locks: Arc<DashSet<i64>>,
    started_by: Arc<DashMap<i64, i64>>,
    triggers: Arc<DashMap<i64, Trigger>>,
}

impl Bot {
    pub async fn new(client: Client) -> Result<Arc<Self>> {
        let me = client.get_me().await?;
        Ok(Arc::new(Self {
            client,
            me,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                            (KHTML, like Gecko) Chrome/110.0.0.0 Safari/537.36")
                .redirect(reqwest::redirect::Policy::limited(10)) // Enable redirects
                .build()?,
            locks: Arc::new(DashSet::new()),
            started_by: Arc::new(DashMap::new()),
            triggers: Arc::new(DashMap::new()),
        }))
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C, exiting");
                    break;
                }

                Ok(update) = self.client.next_update() => {
                    let self_ = self.clone();
                    tokio::spawn(async move {
                        if let Err(err) = self_.handle_update(update).await {
                            error!("Error handling update: {}", err);
                        }
                    });
                }
            }
        }
    }

    async fn handle_update(&self, update: Update) -> Result<()> {
        match update {
            Update::NewMessage(msg) => self.handle_message(msg).await,
            Update::CallbackQuery(query) => self.handle_callback(query).await,
            _ => Ok(()),
        }
    }

    async fn handle_message(&self, msg: Message) -> Result<()> {
        match msg.chat() {
            Chat::User(_) | Chat::Group(_) => {}
            _ => return Ok(()),
        };

        let command = parse_command(msg.text());
        if let Some(command) = command {
            if let Some(via) = &command.via {
                if via.to_lowercase() != self.me.username().unwrap_or_default().to_lowercase() {
                    warn!("Ignoring command for unknown bot: {}", via);
                    return Ok(());
                }
            }

            if let Chat::Group(_) = msg.chat() {
                if command.name == "start" && command.via.is_none() {
                    return Ok(());
                }
            }

            info!("Received command: {:?}", command);
            match command.name.as_str() {
                "start" => {
                    return self.handle_start(msg).await;
                }
                "upload" => {
                    return self.handle_upload(msg, command).await;
                }
                _ => {}
            }
        }

        if let Chat::User(_) = msg.chat() {
            if let Ok(url) = Url::parse(msg.text()) {
                return self.handle_url(msg, url).await;
            }
        }

        Ok(())
    }

    async fn handle_start(&self, msg: Message) -> Result<()> {
        msg.reply(InputMessage::html(
            "📁 <b>Hi! Need a file uploaded? Just send the link!</b>\n\
            In groups, use <code>/upload &lt;url&gt;</code>\n\
            \n\
            🌟 <b>Features:</b>\n\
            \u{2022} Free & fast\n\
            \u{2022} <a href=\"https://github.com/altfoxie/url-uploader\">Open source</a>\n\
            \u{2022} Uploads files up to 2GB\n\
            \u{2022} Redirect-friendly",
        ))
        .await?;
        Ok(())
    }

    async fn handle_upload(&self, msg: Message, cmd: Command) -> Result<()> {
        let url = match cmd.arg {
            Some(url) => url,
            None => {
                msg.reply("Please specify a URL").await?;
                return Ok(());
            }
        };

        let url = match Url::parse(&url) {
            Ok(url) => url,
            Err(err) => {
                msg.reply(format!("Invalid URL: {}", err)).await?;
                return Ok(());
            }
        };

        self.handle_url(msg, url).await
    }

    async fn handle_url(&self, msg: Message, url: Url) -> Result<()> {
        let sender = match msg.sender() {
            Some(sender) => sender,
            None => return Ok(()),
        };

        info!("Locking chat {}", msg.chat().id());
        let _lock = self.locks.insert(msg.chat().id());
        if !_lock {
            msg.reply("✋ Whoa, slow down! There's already an active upload in this chat.")
                .await?;
            return Ok(());
        }
        self.started_by.insert(msg.chat().id(), sender.id());

        defer! {
            info!("Unlocking chat {}", msg.chat().id());
            self.locks.remove(&msg.chat().id());
            self.started_by.remove(&msg.chat().id());
        };

        info!("Downloading file from {}", url);

        // Send request and await response
        let response = self.http.get(url.clone()).send().await?;

        if !response.status().is_success() {
            msg.reply(format!("Failed to download file: HTTP {}", response.status()))
                .await?;
            return Ok(());
        }

        let headers = response.headers().clone();
        let final_url = response.url().clone();
        let content_length = response.content_length().unwrap_or(0) as usize;
        let content_disposition = headers.get("content-disposition").cloned();
        let content_type = headers
            .get("content-type")
            .and_then(|ct| ct.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();

        let name = match content_disposition
            .as_ref()
            .and_then(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|v| v.split(';').map(|v| v.trim()).find(|v| v.starts_with("filename=")))
                    .map(|v| v.trim_start_matches("filename="))
                    .map(|v| v.trim_matches('"'))
            }) {
            Some(name) => name.to_string(),
            None => final_url
                .path_segments()
                .and_then(|segments| segments.last())
                .and_then(|name| {
                    if name.contains('.') {
                        Some(name.to_string())
                    } else {
                        mime_guess::get_mime_extensions_str(&content_type)
                            .and_then(|exts| exts.first())
                            .map(|ext| format!("{}.{}", name, ext))
                    }
                })
                .unwrap_or_else(|| "file.bin".to_string()),
        };

        let name = percent_encoding::percent_decode_str(&name).decode_utf8()?.to_string();

        let is_video = content_type.starts_with("video/mp4") || name.to_lowercase().ends_with(".mp4");

        info!("File {} ({} bytes, video: {})", name, content_length, is_video);

        // If content_length is 0, read the whole body bytes to check emptiness and create stream from it
        let (stream, length) = if content_length == 0 {
            let body_bytes = response.bytes().await?;
            if body_bytes.is_empty() {
                msg.reply("⚠️ File is empty").await?;
                return Ok(());
            }
            // Create a stream from the bytes for upload
            (once(async { Ok::<_, std::io::Error>(body_bytes) }).boxed(), body_bytes.len())
        } else {
            // Use the bytes_stream for streaming if content_length>0
            (
                response
                    .bytes_stream()
                    .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))
                    .boxed(),
                content_length,
            )
        };

        if length > 2 * 1024 * 1024 * 1024 {
            msg.reply("⚠️ File is too large").await?;
            return Ok(());
        }

        let (trigger, valved_stream) = Valved::new(stream);
        self.triggers.insert(msg.chat().id(), trigger);

        defer! {
            self.triggers.remove(&msg.chat().id());
        };

        let reply_markup = Arc::new(reply_markup::inline(vec![vec![button::inline(
            "⛔ Cancel",
            "cancel",
        )]]));

        let status = Arc::new(Mutex::new(
            msg.reply(
                InputMessage::html(format!("🚀 Starting upload of <code>{}</code>...", name))
                    .reply_markup(reply_markup.clone().as_ref()),
            )
            .await?,
        ));

        let mut async_reader = StreamReader::new(valved_stream).compat();

        let mut stream = async_reader.report_progress(Duration::from_secs(3), |progress| {
            let status = status.clone();
            let name = name.clone();
            let reply_markup = reply_markup.clone();
            tokio::spawn(async move {
                status
                    .lock()
                    .await
                    .edit(
                        InputMessage::html(format!(
                            "⏳ Uploading <code>{}</code> <b>({:.2}%)</b>\n\
                             <i>{} / {}</i>",
                            name,
                            progress as f64 / length as f64 * 100.0,
                            bytesize::to_string(progress as u64, true),
                            bytesize::to_string(length as u64, true),
                        ))
                        .reply_markup(reply_markup.as_ref()),
                    )
                    .await
                    .ok();
            });
        });

        let start_time = chrono::Utc::now();
        let file = self
            .client
            .upload_stream(&mut stream, length, name.clone())
            .await?;

        let elapsed = chrono::Utc::now() - start_time;
        info!("Uploaded file {} ({} bytes) in {}", name, length, elapsed);

        let mut input_msg = InputMessage::html(format!(
            "Uploaded in <b>{:.2} secs</b>",
            elapsed.num_milliseconds() as f64 / 1000.0
        ));
        input_msg = input_msg.document(file);
        if is_video {
            input_msg = input_msg.attribute(grammers_client::types::Attribute::Video {
                supports_streaming: false,
                duration: Duration::ZERO,
                w: 0,
                h: 0,
                round_message: false,
            });
        }
        msg.reply(input_msg).await?;

        status.lock().await.delete().await?;

        Ok(())
    }

    async fn handle_callback(&self, query: CallbackQuery) -> Result<()> {
        match query.data() {
            b"cancel" => self.handle_cancel(query).await,
            _ => Ok(()),
        }
    }

    async fn handle_cancel(&self, query: CallbackQuery) -> Result<()> {
        let started_by_user_id = match self.started_by.get(&query.chat().id()) {
            Some(id) => *id,
            None => return Ok(()),
        };

        if started_by_user_id != query.sender().id() {
            info!(
                "User ID {} tried to cancel another user's upload in chat {}",
                query.sender().id(),
                query.chat().id()
            );

            query
                .answer()
                .alert("⚠️ You can't cancel another user's upload")
                .cache_time(Duration::ZERO)
                .send()
                .await?;

            return Ok(());
        }

        if let Some((chat_id, trigger)) = self.triggers.remove(&query.chat().id()) {
            info!("Cancelling upload in chat {}", chat_id);
            drop(trigger);
            self.started_by.remove(&chat_id);

            query
                .load_message()
                .await?
                .edit("⛔ Upload cancelled")
                .await?;

            query.answer().send().await?;
        }
        Ok(())
    }
}
