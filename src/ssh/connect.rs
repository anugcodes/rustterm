use super::*;

/// Keep russh's modern defaults first, but append two legacy fallbacks for old
/// OpenSSH servers. In particular, OpenSSH 5.3 only offers SHA-1 key exchange
/// (`diffie-hellman-group14-sha1`) and an `ssh-rsa` host key. Modern servers
/// still negotiate the preferred algorithms because the client's list order is
/// preserved.
pub(crate) fn client_config() -> client::Config {
    let mut preferred = russh::Preferred::DEFAULT.clone();

    let mut kex = preferred.kex.into_owned();
    kex.push(russh::kex::DH_G14_SHA1);
    preferred.kex = Cow::Owned(kex);

    let mut key = preferred.key.into_owned();
    key.push(russh::keys::key::SSH_RSA);
    preferred.key = Cow::Owned(key);

    // Do **not** enable compression. russh 0.45's outbound zlib compressor
    // reserves `input.len() + 10` bytes and stops as soon as `flate2` reports
    // `Ok`, even when the buffer filled up and input is still pending. The
    // tail of an incompressible packet (PDF, PNG, zip) is then dropped, the
    // server sees a malformed SSH packet and disconnects the whole connection.
    // Uploads of text worked only because their compressed output fit.
    // Upstream fixed this later (see russh >= 0.65's
    // `incompressible_packets_round_trip` test); until we upgrade, advertise
    // `none` so the broken path cannot be negotiated.
    preferred.compression = Cow::Owned(vec![compression::NONE]);

    client::Config {
        preferred,
        // Detect a wedged transport instead of letting the tab freeze forever.
        // russh sends keepalives and closes the session after three unanswered
        // ones (~45 s), which flips `SshSession::is_alive()` and shows the
        // "Connection lost" banner.
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        ..client::Config::default()
    }
}

pub(crate) async fn connect_and_auth(
    config: &SshConfig,
    prompt_tx: Option<std_mpsc::Sender<HostKeyPrompt>>,
    wake: Option<Wake>,
) -> Result<(
    Handle<ClientHandler>,
    Channel<ClientMsg>,
    Option<SftpSession>,
    Option<String>,
    Arc<X11Shared>,
)> {
    /// How long to wait for the TCP handshake before reporting a timeout.
    /// The OS default can be over two minutes on an unreachable host.
    const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

    let ssh_config = Arc::new(client_config());
    let addr = (config.host.as_str(), config.port);

    // The handler is moved into `client::connect`, so the reason for a
    // rejection has to live outside it.
    let failure: Arc<Mutex<Option<HostKeyFailure>>> = Arc::new(Mutex::new(None));
    let x11 = X11Shared::new(wake.clone());
    let handler = ClientHandler {
        host: config.host.clone(),
        port: config.port,
        prompt_tx,
        failure: Arc::clone(&failure),
        x11: Arc::clone(&x11),
    };

    // `client::connect` does not set TCP_NODELAY. On high-latency links Nagle's
    // algorithm then delays small keystrokes by hundreds of milliseconds waiting
    // for an ACK. OpenSSH sets this itself; do the same explicitly.
    let socket = tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s connecting to {}:{}",
                TCP_CONNECT_TIMEOUT.as_secs(),
                config.host,
                config.port
            )
        })?
        .context("TCP connection failed")?;
    socket
        .set_nodelay(true)
        .context("failed to enable TCP_NODELAY")?;

    let mut session = match client::connect_stream(ssh_config, socket, handler).await {
        Ok(session) => session,
        Err(err) => {
            let reason = failure.lock().ok().and_then(|mut slot| slot.take());
            return Err(match reason {
                Some(HostKeyFailure::Changed { fingerprint }) => anyhow!(
                    "HOST KEY CHANGED for {}:{} — the server presented a different \
                     key ({fingerprint}).\n\n\
                     This is what a man-in-the-middle attack looks like. If you know \
                     the host was reinstalled or rekeyed, remove its old entry from \
                     ~/.ssh/known_hosts and reconnect. Otherwise stop and investigate.",
                    config.host,
                    config.port,
                ),
                Some(HostKeyFailure::Rejected { fingerprint }) => anyhow!(
                    "host key for {}:{} was not accepted ({fingerprint})",
                    config.host,
                    config.port,
                ),
                Some(HostKeyFailure::Error { message }) => anyhow!(
                    "could not verify the host key for {}:{}: {message}",
                    config.host,
                    config.port,
                ),
                None => anyhow!("{err}").context("SSH handshake failed"),
            });
        }
    };

    authenticate(&mut session, config).await?;

    let mut channel = session
        .channel_open_session()
        .await
        .context("failed to open SSH channel")?;

    channel
        .request_pty(false, "xterm-256color", 80, 24, 0, 0, &[])
        .await
        .context("PTY request rejected")?;
    // X11 has to be requested before the shell starts: sshd only puts
    // `DISPLAY` in the child environment when the request is known at spawn
    // time. A refusal is reported in the bottom bar, never fatal.
    if config.x11 {
        request_x11_forwarding(&mut channel, &x11).await;
    }
    channel
        .request_shell(false)
        .await
        .context("shell request rejected")?;

    // SFTP is optional: a server may not provide the subsystem. Never fail the
    // whole connection because of it — the terminal is the primary feature.
    let (sftp, sftp_error) = match open_sftp(&session).await {
        Ok(sftp) => (Some(sftp), None),
        Err(err) => {
            log::warn!("SFTP unavailable: {err}");
            (None, Some(err))
        }
    };

    Ok((session, channel, sftp, sftp_error, x11))
}

/// Ask the server to forward X11, then wait for its answer.
///
/// The cookie sent here is generated locally; sshd installs it for the remote
/// display. The real local cookie is looked up separately when a channel is
/// proxied, so the remote side never sees it.
pub(crate) async fn request_x11_forwarding(channel: &mut Channel<ClientMsg>, shared: &X11Shared) {
    const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

    let Some(display) = x11::local_display() else {
        shared.mark_forwarding(
            false,
            Some("no local X display (DISPLAY is unset)".to_string()),
        );
        return;
    };
    // `xauth` is a blocking subprocess; keep it off the session runtime.
    let auth = {
        let display = display.clone();
        tokio::task::spawn_blocking(move || x11::local_auth(&display))
            .await
            .ok()
            .flatten()
    };
    let cookie = x11::random_cookie_hex();
    if let Err(err) = channel
        .request_x11(true, false, "MIT-MAGIC-COOKIE-1", cookie, display.screen)
        .await
    {
        shared.mark_forwarding(
            false,
            Some(format!("could not request X11 forwarding: {err}")),
        );
        return;
    }

    let reply = tokio::time::timeout(REPLY_TIMEOUT, async {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => return Ok(()),
                Some(ChannelMsg::Failure) => {
                    return Err(
                        "the server refused X11 forwarding (check X11Forwarding on the server)"
                            .to_string(),
                    )
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    return Err("the session closed while asking for X11 forwarding".to_string())
                }
                // Nothing else is expected before the shell starts.
                Some(_) => continue,
            }
        }
    })
    .await;

    match reply {
        Ok(Ok(())) => {
            shared.set_config(Some(x11::X11Config { display, auth }));
            shared.mark_forwarding(true, None);
        }
        Ok(Err(err)) => shared.mark_forwarding(false, Some(err)),
        Err(_) => shared.mark_forwarding(
            false,
            Some("timed out waiting for the server to confirm X11 forwarding".to_string()),
        ),
    }
}

/// Open a second channel and start the SFTP client handshake (`SSH_FXP_INIT`).
pub(crate) async fn open_sftp(session: &Handle<ClientHandler>) -> Result<SftpSession, String> {
    let channel = session
        .channel_open_session()
        .await
        .map_err(|err| format!("could not open an SFTP channel: {err}"))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|err| format!("could not request the SFTP subsystem: {err}"))?;

    SftpSession::new_with_config(
        channel.into_stream(),
        SftpConfig {
            // Network filesystems (NFS/GPFS mounts such as /remote/...) can
            // take well over ten seconds to answer a cold `stat`. The old 10 s
            // limit surfaced as "could not inspect '...': Timeout" on the first
            // click even though the channel was healthy.
            request_timeout_secs: 30,
            ..SftpConfig::default()
        },
    )
    .await
    .map_err(|err| format!("SFTP handshake failed: {err}"))
}

/// Authenticate in the same order `ssh` users expect: key file(s), then
/// ssh-agent identities, then the typed password.
///
/// Every method is optional. Failures are accumulated so the UI can explain
/// *why* nothing worked (missing key, encrypted key, agent not running, …)
/// instead of a bare "authentication rejected". The password is never included
/// in any message.
pub(crate) async fn authenticate(
    session: &mut Handle<ClientHandler>,
    config: &SshConfig,
) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();

    if config.use_key {
        let candidates = key_candidates(&config.key_path);
        if candidates.is_empty() {
            failures.push(if config.key_path.trim().is_empty() {
                "no key found in ~/.ssh (id_ed25519, id_ecdsa, id_rsa, id_dsa)".to_string()
            } else {
                format!("key file not found: {}", config.key_path.trim())
            });
        }

        for path in candidates {
            let display = path.display();
            let key = match load_secret_key(&path, None) {
                Ok(key) => key,
                Err(russh::keys::Error::KeyIsEncrypted) => {
                    failures.push(format!(
                        "{display}: key is passphrase-protected; add it to ssh-agent \
                         (ssh-add) and enable the agent option"
                    ));
                    continue;
                }
                Err(err) => {
                    failures.push(format!("{display}: {err}"));
                    continue;
                }
            };

            match session
                .authenticate_publickey(&config.user, Arc::new(key))
                .await
            {
                Ok(true) => return Ok(()),
                Ok(false) => failures.push(format!("{display}: key rejected by server")),
                Err(err) => failures.push(format!("{display}: {err}")),
            }
        }
    }

    if config.use_agent {
        match authenticate_with_agent(session, &config.user).await {
            Ok(true) => return Ok(()),
            Ok(false) => failures.push("ssh-agent: no identity was accepted".to_string()),
            Err(err) => failures.push(format!("ssh-agent: {err}")),
        }
    }

    if !config.password.is_empty() {
        match session
            .authenticate_password(&config.user, &config.password)
            .await
        {
            Ok(true) => return Ok(()),
            Ok(false) => failures.push("password: rejected by server".to_string()),
            Err(err) => return Err(anyhow!("password authentication request failed: {err}")),
        }
    }

    if failures.is_empty() {
        return Err(anyhow!(
            "no authentication method selected: enable a key or ssh-agent, or type a password"
        ));
    }
    Err(anyhow!("authentication failed: {}", failures.join("; ")))
}

/// Try every identity currently loaded in `$SSH_AUTH_SOCK`.
///
/// `authenticate_future` consumes the agent connection and hands it back, so a
/// single connection is reused for all identities — the agent never has to be
/// re-opened after a rejected key.
pub(crate) async fn authenticate_with_agent(
    session: &mut Handle<ClientHandler>,
    user: &str,
) -> Result<bool> {
    let mut agent = AgentClient::connect_env()
        .await
        .context("could not connect to $SSH_AUTH_SOCK (is ssh-agent running?)")?;
    let identities = agent
        .request_identities()
        .await
        .context("could not list ssh-agent identities")?;
    if identities.is_empty() {
        return Err(anyhow!("the agent has no identities (run ssh-add)"));
    }

    for key in identities {
        let (returned, result) = session.authenticate_future(user, key, agent).await;
        agent = returned;
        match result {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(err) => return Err(anyhow!("signing with an agent key failed: {err}")),
        }
    }
    Ok(false)
}

const DEFAULT_KEY_NAMES: [&str; 4] = ["id_ed25519", "id_ecdsa", "id_rsa", "id_dsa"];

/// Explicit path if configured, otherwise the usual `~/.ssh/id_*` files that
/// actually exist. Missing *default* keys are simply skipped; an explicit path
/// is always returned so its load error is reported.
pub(crate) fn key_candidates(configured: &str) -> Vec<PathBuf> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return vec![expand_tilde(configured)];
    }

    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let ssh = home.join(".ssh");
    DEFAULT_KEY_NAMES
        .iter()
        .map(|name| ssh.join(name))
        .filter(|path| path.is_file())
        .collect()
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Expand a leading `~` / `~/`. `~user` is left untouched.
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}
