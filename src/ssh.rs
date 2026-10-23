//! SSH session wrapper around russh.
//!
//! This module owns the SSH connection and exposes a channel for the terminal
//! to read bytes from and write bytes to.

use anyhow::{Context, Result};
use russh::client;
use russh::ChannelMsg;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

/// A live SSH session with an interactive shell.
pub struct SshSession {
    // Handle to send input to the remote shell.
    pub input_tx: mpsc::UnboundedSender<Vec<u8>>,
    // Receiver for output bytes from the remote shell.
    pub output_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    // Dimensions (cols, rows) — send resize events here.
    pub resize_tx: mpsc::UnboundedSender<(u32, u32)>,
}

struct Client;

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // TODO: verify against known_hosts for security.
        Ok(true)
    }
}

impl SshSession {
    /// Connect, authenticate, open a shell, and return a handle.
    ///
    /// This runs on a dedicated background thread with its own tokio runtime.
    pub fn connect(config: SshConfig) -> Result<Self> {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u32, u32)>();
        let (output_tx, output_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");

            rt.block_on(async move {
                if let Err(e) = run_session(config, &mut input_rx, &mut resize_rx, &output_tx).await
                {
                    eprintln!("SSH session error: {e:#}");
                }
            });
        });

        Ok(Self {
            input_tx,
            output_rx,
            resize_tx,
        })
    }

    pub fn write(&self, data: Vec<u8>) {
        let _ = self.input_tx.send(data);
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        let _ = self.resize_tx.send((cols, rows));
    }

    /// Drain any pending output bytes. Non-blocking.
    pub fn drain_output(&self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        while let Ok(chunk) = self.output_rx.try_recv() {
            chunks.push(chunk);
        }
        chunks
    }
}

async fn run_session(
    config: SshConfig,
    input_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    resize_rx: &mut mpsc::UnboundedReceiver<(u32, u32)>,
    output_tx: &std::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let ssh_config = Arc::new(client::Config::default());
    let addr = format!("{}:{}", config.host, config.port);

    let mut session = client::connect(ssh_config, addr.clone(), Client)
        .await
        .context("TCP/SSH connect")?;

    let auth = session
        .authenticate_password(&config.username, &config.password)
        .await
        .context("authenticate_password")?;

    if !auth.success() {
        anyhow::bail!("password authentication failed");
    }

    let mut channel = session
        .channel_open_session()
        .await
        .context("channel_open_session")?;

    channel
        .request_pty(false, "xterm-256color", 80, 24, 0, 0, &[])
        .await
        .context("request_pty")?;

    channel
        .request_shell(false)
        .await
        .context("request_shell")?;

    // Main event loop: forward input → channel, channel output → output_tx.
    loop {
        tokio::select! {
            // Input from the UI.
            Some(data) = input_rx.recv() => {
                channel.data(&data[..]).await.context("channel.data")?;
            }
            // Resize requests.
            Some((cols, rows)) = resize_rx.recv() => {
                channel.window_change(cols, rows, 0, 0).await.context("window_change")?;
            }
            // Output from the server.
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        output_tx.send(data.to_vec()).ok();
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        output_tx.send(data.to_vec()).ok();
                    }
                    Some(ChannelMsg::Eof) | None => {
                        output_tx.send(b"\r\n[connection closed]\r\n".to_vec()).ok();
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    session
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await
        .ok();

    Ok(())
}
