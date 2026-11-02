use super::*;

/// Channels owned by one session thread: terminal input/output plus the SFTP
/// request queue. Bundled so `run_session` keeps a readable signature.
pub(crate) struct SessionIo {
    pub(crate) input_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    pub(crate) resize_rx: mpsc::UnboundedReceiver<(u16, u16)>,
    pub(crate) disconnect_rx: mpsc::UnboundedReceiver<()>,
    pub(crate) sftp_rx: mpsc::UnboundedReceiver<SftpRequest>,
    pub(crate) display_rx: mpsc::UnboundedReceiver<DisplayRequest>,
    pub(crate) output_tx: std_mpsc::Sender<Vec<u8>>,
    pub(crate) output_backlog: Arc<AtomicUsize>,
    pub(crate) output_space: Arc<Notify>,
}

pub(crate) async fn run_session(
    session: Arc<Handle<ClientHandler>>,
    mut channel: Channel<ClientMsg>,
    sftp: Option<SftpSession>,
    vnc_clients: Arc<AtomicUsize>,
    io: SessionIo,
    wake: Option<Wake>,
) {
    let SessionIo {
        mut input_rx,
        mut resize_rx,
        mut disconnect_rx,
        mut sftp_rx,
        mut display_rx,
        output_tx,
        output_backlog,
        output_space,
    } = io;
    // The client is behind a lock so a reconnect can swap in a fresh SFTP
    // channel while the terminal loop keeps running. Locks are only held long
    // enough to clone the `Arc`; all I/O happens without one.
    let sftp: SharedSftp = Arc::new(Mutex::new(sftp.map(|sftp| Arc::new(SftpClient::new(sftp)))));
    // At most one VNC listener per session; stopping aborts its accept loop.
    let mut vnc: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            maybe_bytes = input_rx.recv() => {
                match maybe_bytes {
                    Some(bytes) => {
                        // russh 0.45's Channel::data takes AsyncRead; a byte slice satisfies it.
                        if channel.data(&bytes[..]).await.is_err() {
                            break;
                        }
                    }
                    None => break, // app dropped the sender
                }
            }
            maybe_size = resize_rx.recv() => {
                match maybe_size {
                    Some((cols, rows)) => {
                        let _ = channel
                            .window_change(cols as u32, rows as u32, 0, 0)
                            .await;
                    }
                    None => break,
                }
            }
            // Fires on explicit disconnect AND on sender drop. Both mean stop.
            _ = disconnect_rx.recv() => break,
            maybe_request = sftp_rx.recv() => {
                let Some(request) = maybe_request else { break };
                let SftpRequest { action, wake: request_wake } = request;
                match action {
                    SftpAction::Reconnect { reply } => {
                        let session = session.clone();
                        let shared = Arc::clone(&sftp);
                        // Open the new channel off the terminal loop: the SFTP
                        // handshake must not freeze keystrokes or output.
                        tokio::spawn(async move {
                            let result = reconnect_sftp(&session, &shared).await;
                            let _ = reply.send(result);
                            if let Some(wake) = request_wake {
                                wake();
                            }
                        });
                    }
                    action => {
                        let shared = Arc::clone(&sftp);
                        tokio::spawn(async move {
                            let client = shared.lock().ok().and_then(|slot| slot.clone());
                            match client {
                                Some(client) => action.run(&client).await,
                                None => {
                                    action.fail("SFTP support is not available on this server")
                                }
                            }
                            if let Some(wake) = request_wake {
                                wake();
                            }
                        });
                    }
                }
            }
            maybe_display = display_rx.recv() => {
                let Some(request) = maybe_display else { break };
                let DisplayRequest { action, wake: request_wake } = request;
                match action {
                    DisplayAction::Probe { auto_create, reply } => {
                        let session = Arc::clone(&session);
                        tokio::spawn(async move {
                            let result = probe_vnc(&session, auto_create).await;
                            let _ = reply.send(result);
                            if let Some(wake) = request_wake {
                                wake();
                            }
                        });
                    }
                    DisplayAction::Open { host, port, reply } => {
                        if let Some(tunnel) = vnc.take() {
                            tunnel.abort();
                        }
                        match open_vnc(&session, &host, port, Arc::clone(&vnc_clients)).await {
                            Ok((local_port, handle)) => {
                                vnc = Some(handle);
                                let _ = reply.send(Ok(local_port));
                            }
                            Err(err) => {
                                let _ = reply.send(Err(err));
                            }
                        }
                        if let Some(wake) = request_wake {
                            wake();
                        }
                    }
                    DisplayAction::Close { reply } => {
                        if let Some(tunnel) = vnc.take() {
                            tunnel.abort();
                        }
                        let _ = reply.send(());
                        if let Some(wake) = request_wake {
                            wake();
                        }
                    }
                }
            }
            // Stop reading the SSH channel while the UI is behind. The remote
            // sees a full window and pauses, which bounds memory without
            // dropping a single byte.
            msg = channel.wait(), if output_backlog.load(Ordering::Relaxed) < OUTPUT_HIGH_WATER => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        output_backlog.fetch_add(data.len(), Ordering::Relaxed);
                        if output_tx.send(data.to_vec()).is_err() {
                            break;
                        }
                        if let Some(wake) = wake.as_ref() {
                            wake();
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        output_backlog.fetch_add(data.len(), Ordering::Relaxed);
                        if output_tx.send(data.to_vec()).is_err() {
                            break;
                        }
                        if let Some(wake) = wake.as_ref() {
                            wake();
                        }
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
            // The UI freed space; loop around so the gate above reopens.
            _ = output_space.notified() => {}
        }
    }

    // Stop the VNC listener; aborting drops the task and its socket.
    if let Some(tunnel) = vnc.take() {
        tunnel.abort();
    }

    // Best-effort graceful disconnect on every exit path.
    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    if let Some(wake) = wake.as_ref() {
        wake();
    }
}
