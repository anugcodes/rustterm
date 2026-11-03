use tokio::net::TcpListener;
use super::*;

/// Look for a VNC session on the server and read the login shell.
///
/// The shell is detected on the same round trip the tunnel needs, so the bar
/// already knows whether to type `setenv` or `DISPLAY=` by the time it can be
/// clicked.
pub(crate) async fn probe_vnc(session: &Handle<ClientHandler>, auto_create: bool) -> Result<VncProbe, String> {
    let shell = probe_login_shell(session).await;
    let target = probe_vnc_target(session, auto_create).await?;
    Ok(VncProbe { target, shell })
}

/// Look for a VNC session on the server, in tiers.
///
/// 1. `vncserver -list` — the user's own VNC sessions (TigerVNC/TightVNC/
///    TurboVNC all provide this). If none exists and `auto_create` is set,
///    `vncserver` creates one.
/// 2. `ss`/`netstat` — any listener in the 5900-5999 range, for servers that
///    run VNC without the `vncserver` wrapper.
///
/// Returns `None` when nothing was found; the caller then falls back to the
/// configured address.
pub(crate) async fn probe_vnc_target(
    session: &Handle<ClientHandler>,
    auto_create: bool,
) -> Result<Option<VncTarget>, String> {
    let mut first_error: Option<String> = None;

    // Tier 1: the `vncserver` family.
    if let Some(output) = run_remote_script(session, "vncserver -list 2>/dev/null", 10).await {
        if let Some(display) = parse_vncserver_list(&output) {
            return Ok(Some(vnc_target_for_display(display)));
        }
        if auto_create {
            // `vncserver` blocks on a password prompt when ~/.vnc/passwd is
            // missing, so check for it first.
            let output = run_remote_script(
                session,
                r#"if [ -f "$HOME/.vnc/passwd" ]; then vncserver 2>&1; else echo "__RUSTTERM_NO_PASSWD__"; fi"#,
                20,
            )
            .await
            .unwrap_or_default();
            if output.contains("__RUSTTERM_NO_PASSWD__") {
                first_error = Some(
                    "no VNC session found and ~/.vnc/passwd is missing — run `vncpasswd` on the server first"
                        .to_string(),
                );
            } else if let Some(display) = parse_vncserver_create(&output) {
                return Ok(Some(vnc_target_for_display(display)));
            } else {
                log::debug!("could not parse vncserver output: {}", output.trim());
            }
        }
    }

    // Tier 2: any listening VNC port.
    if let Some(output) = run_remote_script(
        session,
        "ss -ltn 2>/dev/null || netstat -ltn 2>/dev/null || netstat -an 2>/dev/null",
        10,
    )
    .await
    {
        if let Some(target) = parse_vnc_listener(&output) {
            return Ok(Some(target));
        }
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(None),
    }
}

/// Read the account's login shell through `/bin/sh`, falling back to `$SHELL`.
///
/// `getent` is not on every Unix, and a `.cshrc` may print a banner, so the
/// output is parsed rather than trusted as a single line.
pub(crate) async fn probe_login_shell(session: &Handle<ClientHandler>) -> Option<String> {
    let output = run_remote_script(
        session,
        r#"s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7); printf "%s\n" "${s:-$SHELL}""#,
        8,
    )
    .await?;
    parse_login_shell(&output)
}

/// Take the last path-like line of the shell lookup output.
///
/// Startup-file output may precede the answer, and only the answer starts with
/// `/` (an absolute shell path).
pub(crate) fn parse_login_shell(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        // The answer is the last absolute path; startup output may precede it.
        .rfind(|line| line.starts_with('/'))
        .map(str::to_string)
}

/// Run a shell snippet on the server and return its output.
///
/// The snippet is executed through `/bin/sh` so it works when the login shell
/// is csh and on very old systems. A timeout is not fatal: whatever arrived is
/// returned so the caller can still parse it.
pub(crate) async fn run_remote_script(
    session: &Handle<ClientHandler>,
    script: &str,
    timeout_secs: u64,
) -> Option<String> {
    let channel = session.channel_open_session().await.ok()?;
    channel
        .exec(true, format!("/bin/sh -c '{script}'"))
        .await
        .ok()?;
    let mut stream = channel.into_stream();
    let mut output = Vec::new();
    // A timeout or a read error is not fatal: whatever arrived is returned so
    // the caller can still parse it.
    let _ = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        stream.read_to_end(&mut output),
    )
    .await;
    Some(String::from_utf8_lossy(&output).into_owned())
}

/// The target for a VNC display number (`:1` is port 5901 on the server).
pub(crate) fn vnc_target_for_display(display: u16) -> VncTarget {
    VncTarget {
        host: "127.0.0.1".to_string(),
        port: 5900 + display,
    }
}

/// Parse `vncserver -list` output and return the lowest display number.
///
/// TigerVNC prints a table whose first column is the display; TightVNC and
/// TurboVNC use the same shape.
pub(crate) fn parse_vncserver_list(text: &str) -> Option<u16> {
    let mut best: Option<u16> = None;
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(':') else {
            continue;
        };
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Ok(display) = digits.parse::<u16>() else {
            continue;
        };
        if display == 0 || display > 99 {
            continue;
        }
        best = Some(match best {
            Some(current) if current <= display => current,
            _ => display,
        });
    }
    best
}

/// Parse the output of a `vncserver` start and return the new display number.
///
/// TigerVNC prints `New 'host:1 (user)' desktop is host:1.`
pub(crate) fn parse_vncserver_create(text: &str) -> Option<u16> {
    for line in text.lines() {
        let Some(index) = line.find("desktop is ") else {
            continue;
        };
        let rest = &line[index + "desktop is ".len()..];
        // `host:1` or `host:1 (user)` — the display number follows the colon.
        let Some(colon) = rest.rfind(':') else {
            continue;
        };
        let digits: String = rest[colon + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(display) = digits.parse::<u16>() {
            if display > 0 && display <= 99 {
                return Some(display);
            }
        }
    }
    None
}

/// Find a listening VNC port (5900-5999) in `ss`/`netstat` output.
pub(crate) fn parse_vnc_listener(text: &str) -> Option<VncTarget> {
    let mut found: Option<VncTarget> = None;
    for line in text.lines() {
        // `-ltn` output is already filtered; `-an` mixes states.
        if line.contains("ESTABLISHED") || line.contains("TIME_WAIT") {
            continue;
        }
        let Some(target) = vnc_target_from_line(line) else {
            continue;
        };
        // Prefer display :0, otherwise keep the lowest display found.
        if found.as_ref().is_none_or(|best| target.port < best.port) {
            found = Some(target);
        }
    }
    found
}

pub(crate) fn vnc_target_from_line(line: &str) -> Option<VncTarget> {
    for token in line.split_whitespace() {
        let Some(colon) = token.rfind(':') else {
            continue;
        };
        let digits: String = token[colon + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let Ok(port) = digits.parse::<u16>() else {
            continue;
        };
        if !(5900..=5999).contains(&port) {
            continue;
        }
        let mut host = token[..colon].trim_matches(['[', ']']);
        if let Some((base, _)) = host.split_once('%') {
            host = base;
        }
        let host = if host.is_empty() || matches!(host, "*" | "0.0.0.0" | "::") {
            "127.0.0.1".to_string()
        } else {
            host.to_string()
        };
        return Some(VncTarget { host, port });
    }
    None
}

/// Open a loopback listener; each accepted viewer connection gets its own
/// `direct-tcpip` channel to the VNC server.
pub(crate) async fn open_vnc(
    session: &Arc<Handle<ClientHandler>>,
    host: &str,
    port: u16,
    clients: Arc<AtomicUsize>,
) -> Result<(u16, tokio::task::JoinHandle<()>), String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|err| format!("could not open a local VNC port: {err}"))?;
    let local = listener
        .local_addr()
        .map_err(|err| format!("could not read the local VNC port: {err}"))?
        .port();
    let session = Arc::clone(session);
    let host = host.to_string();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            if clients.load(Ordering::Relaxed) >= VNC_MAX_CLIENTS {
                // A viewer that reconnects in a loop must not spawn tasks
                // without bound; dropping the socket closes it.
                drop(socket);
                continue;
            }
            let session = Arc::clone(&session);
            let host = host.clone();
            let clients = Arc::clone(&clients);
            tokio::spawn(async move {
                clients.fetch_add(1, Ordering::Relaxed);
                if let Ok(channel) = session
                    .channel_open_direct_tcpip(host, port as u32, "127.0.0.1", 0)
                    .await
                {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                }
                clients.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });
    Ok((local, handle))
}

