//! X11 forwarding (`ssh -X` equivalent).
//!
//! The server opens one SSH `x11` channel per remote X client. Each channel is
//! spliced to the real local X server. The first packet is a connection setup
//! carrying the cookie that sshd installed remotely; the local server expects
//! the cookie from `xauth`, so the setup is rewritten before it is forwarded.
//!
//! Nothing here listens on a socket: the app only forwards channels that the
//! server opens, which is what makes this untrusted forwarding.

use std::io;
use std::process::Command;

use russh::client::Msg;
use russh::Channel;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};

/// Length of an MIT-MAGIC-COOKIE-1 value.
pub const COOKIE_BYTES: usize = 16;

/// A parsed `$DISPLAY` value (`:0`, `host:10.0`, `unix:0`, `host/unix:0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub host: String,
    /// Transport named after the host (`unix` for `host/unix:0`), if any.
    pub transport: Option<String>,
    pub number: u32,
    pub screen: u32,
}

impl Display {
    pub fn parse(value: &str) -> Option<Self> {
        let (host, transport, number, screen) = parse_display_name(value.trim())?;
        Some(Self {
            host,
            transport,
            number,
            screen: screen.unwrap_or(0),
        })
    }

    fn prefers_unix_socket(&self) -> bool {
        self.host.is_empty()
            || self.host == "unix"
            || matches!(self.host.as_str(), "localhost" | "127.0.0.1")
            || self.transport.as_deref() == Some("unix")
    }
}

/// Split a display or xauth entry name into host, transport, number and screen.
///
/// The grammar is `[host][/transport]:number[.screen]`. The transport is only
/// recognised when a host precedes the slash, so a launchd socket path such as
/// `/tmp/launch-x/org.xquartz:0` keeps its full path as the host.
fn parse_display_name(value: &str) -> Option<(String, Option<String>, u32, Option<u32>)> {
    if value.is_empty() {
        return None;
    }
    let (head, rest) = value.rsplit_once(':')?;
    let (host, transport) = match head.split_once('/') {
        Some((host, transport)) if !host.is_empty() && !transport.is_empty() => {
            (host.to_string(), Some(transport.to_string()))
        }
        _ => (head.to_string(), None),
    };
    let (number, screen) = match rest.split_once('.') {
        Some((number, screen)) => (number.parse().ok()?, Some(screen.parse().ok()?)),
        None => (rest.parse().ok()?, None),
    };
    Some((host, transport, number, screen))
}

/// What a proxy needs to reach the local X server.
#[derive(Clone, Debug)]
pub struct X11Config {
    pub display: Display,
    /// MIT-MAGIC-COOKIE-1 from `xauth`, when the local display uses auth.
    pub auth: Option<(String, Vec<u8>)>,
}

/// The local display named by `$DISPLAY`, if there is one.
pub fn local_display() -> Option<Display> {
    Display::parse(&std::env::var("DISPLAY").ok()?)
}

/// Ask `xauth` for the MIT-MAGIC-COOKIE-1 of the local display.
///
/// Returns `None` when `xauth` is missing or the display is unauthenticated
/// (for example `Xvfb -ac`), in which case the setup is forwarded unchanged.
pub fn local_auth(display: &Display) -> Option<(String, Vec<u8>)> {
    let output = Command::new("xauth").arg("list").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(protocol) = parts.next() else {
            continue;
        };
        let Some(cookie) = parts.next() else {
            continue;
        };
        if protocol != "MIT-MAGIC-COOKIE-1" {
            continue;
        }
        if !auth_entry_matches(name, display) {
            continue;
        }
        if let Some(bytes) = decode_hex(cookie) {
            return Some((protocol.to_string(), bytes));
        }
    }
    None
}

/// Whether an `xauth list` display field belongs to `display`.
///
/// Entries are `host/transport:number[.screen]`; the screen is optional and,
/// when omitted, applies to every screen of that display.
fn auth_entry_matches(name: &str, display: &Display) -> bool {
    let Some((host, transport, number, screen)) = parse_display_name(name) else {
        return false;
    };
    if number != display.number {
        return false;
    }
    if screen.is_some_and(|screen| screen != display.screen) {
        return false;
    }
    if let (Some(want), Some(got)) = (display.transport.as_deref(), transport.as_deref()) {
        if !want.eq_ignore_ascii_case(got) {
            return false;
        }
    }
    auth_host_matches(&host, transport.as_deref(), display)
}

fn auth_host_matches(host: &str, transport: Option<&str>, display: &Display) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let want = display.host.trim_start_matches('[').trim_end_matches(']');
    if !want.is_empty() && !want.eq_ignore_ascii_case("unix") {
        return host.eq_ignore_ascii_case(want);
    }
    // A local display has no host of its own: accept the machine's Unix
    // transport entry (the usual `hostname/unix:0`), a bare entry, or a
    // loopback name.
    host.is_empty()
        || transport == Some("unix")
        || host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("unix")
}

/// A fresh random cookie, sent to the server in the `x11-req`.
pub fn random_cookie_hex() -> String {
    let mut bytes = [0u8; COOKIE_BYTES];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read as _;
        if file.read_exact(&mut bytes).is_ok() {
            return encode_hex(&bytes);
        }
    }
    // `/dev/urandom` is present on every supported platform; this is only a
    // last resort so a missing node cannot disable forwarding entirely.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128);
    let mut state = seed as u64 | 1;
    for byte in &mut bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    encode_hex(&bytes)
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push((high << 4 | low) as u8);
    }
    Some(out)
}

/// Splice one server-opened X11 channel to the local X server.
pub async fn proxy(
    channel: Channel<Msg>,
    display: Display,
    auth: Option<(String, Vec<u8>)>,
) -> Result<(), String> {
    let mut remote = channel.into_stream();
    let mut local = connect_local(&display).await?;

    let setup = read_setup(&mut remote)
        .await
        .map_err(|err| format!("could not read the X11 setup packet: {err}"))?;
    let setup = match auth.as_ref().and_then(|auth| rewrite_setup(&setup, auth)) {
        Some(rewritten) => rewritten,
        None => setup,
    };
    local
        .write_all(&setup)
        .await
        .map_err(|err| format!("could not send the X11 setup packet: {err}"))?;

    // EOF or a reset here is the normal end of an X client; only setup and
    // connection failures above are worth reporting.
    let _ = tokio::io::copy_bidirectional(&mut remote, &mut local).await;
    Ok(())
}

async fn connect_local(display: &Display) -> Result<LocalX11, String> {
    if display.prefers_unix_socket() {
        let path = format!("/tmp/.X11-unix/X{}", display.number);
        if let Ok(socket) = UnixStream::connect(&path).await {
            return Ok(LocalX11::Unix(socket));
        }
    }
    let host = display
        .host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let host = if host.is_empty() || host == "unix" {
        "127.0.0.1".to_string()
    } else {
        host
    };
    let port = x11_tcp_port(display.number).ok_or_else(|| {
        format!(
            "display number {} cannot be reached over TCP",
            display.number
        )
    })?;
    let stream = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|err| format!("could not reach the X server at {host}:{port}: {err}"))?;
    // X11 is chatty; disable Nagle on the local leg too.
    let _ = stream.set_nodelay(true);
    Ok(LocalX11::Tcp(stream))
}

/// X display `n` listens on TCP port `6000 + n`.
fn x11_tcp_port(number: u32) -> Option<u16> {
    u16::try_from(number).ok()?.checked_add(6000)
}

enum LocalX11 {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl tokio::io::AsyncRead for LocalX11 {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut *self {
            LocalX11::Unix(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            LocalX11::Tcp(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for LocalX11 {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match &mut *self {
            LocalX11::Unix(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            LocalX11::Tcp(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut *self {
            LocalX11::Unix(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            LocalX11::Tcp(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut *self {
            LocalX11::Unix(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            LocalX11::Tcp(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// Read exactly one X11 connection-setup packet.
async fn read_setup<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 12];
    reader.read_exact(&mut header).await?;
    let big = header[0] == 0x42;
    let length = |high, low| {
        if big {
            u16::from_be_bytes([high, low]) as usize
        } else {
            u16::from_le_bytes([high, low]) as usize
        }
    };
    let name = pad4(length(header[6], header[7]));
    let data = pad4(length(header[8], header[9]));
    let mut setup = vec![0u8; 12 + name + data];
    setup[..12].copy_from_slice(&header);
    reader.read_exact(&mut setup[12..]).await?;
    Ok(setup)
}

/// Replace the setup's authentication name/data with the local cookie.
fn rewrite_setup(setup: &[u8], auth: &(String, Vec<u8>)) -> Option<Vec<u8>> {
    if setup.len() < 12 {
        return None;
    }
    let big = setup[0] == 0x42;
    let length = |high: u8, low: u8| {
        if big {
            u16::from_be_bytes([high, low]) as usize
        } else {
            u16::from_le_bytes([high, low]) as usize
        }
    };
    let old_name = pad4(length(setup[6], setup[7]));
    let old_data = pad4(length(setup[8], setup[9]));
    if setup.len() < 12 + old_name + old_data {
        return None;
    }
    let name = auth.0.as_bytes();
    let data = &auth.1;
    let name_pad = pad4(name.len());
    let data_pad = pad4(data.len());
    let encode = |value: usize| {
        if big {
            (value as u16).to_be_bytes()
        } else {
            (value as u16).to_le_bytes()
        }
    };

    let mut out = Vec::with_capacity(12 + name_pad + data_pad);
    out.extend_from_slice(&setup[..6]);
    out.extend_from_slice(&encode(name.len()));
    out.extend_from_slice(&encode(data.len()));
    out.extend_from_slice(&setup[10..12]);
    out.extend_from_slice(name);
    out.resize(12 + name_pad, 0);
    out.extend_from_slice(data);
    out.resize(12 + name_pad + data_pad, 0);
    // Any bytes after the original auth block (normally none) stay in order.
    out.extend_from_slice(&setup[12 + old_name + old_data..]);
    Some(out)
}

fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_and_remote_displays() {
        assert_eq!(
            Display::parse(":0"),
            Some(Display {
                host: String::new(),
                transport: None,
                number: 0,
                screen: 0
            })
        );
        assert_eq!(
            Display::parse(":10.1"),
            Some(Display {
                host: String::new(),
                transport: None,
                number: 10,
                screen: 1
            })
        );
        assert_eq!(
            Display::parse("localhost:10.0"),
            Some(Display {
                host: "localhost".into(),
                transport: None,
                number: 10,
                screen: 0
            })
        );
        assert_eq!(
            Display::parse("192.168.1.9:2"),
            Some(Display {
                host: "192.168.1.9".into(),
                transport: None,
                number: 2,
                screen: 0
            })
        );
        assert_eq!(
            Display::parse("workstation/unix:2"),
            Some(Display {
                host: "workstation".into(),
                transport: Some("unix".into()),
                number: 2,
                screen: 0
            })
        );
        assert_eq!(Display::parse(""), None);
        assert_eq!(Display::parse("nonsense"), None);
    }

    #[test]
    fn unix_transport_displays_are_local() {
        for name in [":0", "unix:0", "localhost:0", "workstation/unix:0"] {
            assert!(
                Display::parse(name).unwrap().prefers_unix_socket(),
                "{name} should use the Unix socket"
            );
        }
        for name in ["192.168.1.9:2", "tcp/workstation:2"] {
            assert!(
                !Display::parse(name).unwrap().prefers_unix_socket(),
                "{name} should use TCP"
            );
        }
    }

    #[test]
    fn matches_xauth_entries_by_host_number_and_screen() {
        let local = Display::parse(":0").unwrap();
        assert!(auth_entry_matches("box/unix:0", &local));
        assert!(auth_entry_matches(":0", &local));
        assert!(!auth_entry_matches("box/unix:1", &local));
        assert!(!auth_entry_matches("box/tcp:0", &local));

        let remote = Display::parse("workstation/unix:2").unwrap();
        assert!(auth_entry_matches("workstation/unix:2", &remote));
        assert!(!auth_entry_matches("otherhost/unix:2", &remote));
        assert!(!auth_entry_matches("workstation/unix:3", &remote));

        // An entry without a screen applies to every screen.
        let screen_one = Display::parse("workstation/unix:2.1").unwrap();
        assert!(auth_entry_matches("workstation/unix:2", &screen_one));
        assert!(auth_entry_matches("workstation/unix:2.1", &screen_one));
        assert!(!auth_entry_matches("workstation/unix:2.0", &screen_one));
    }

    #[test]
    fn tcp_ports_use_checked_arithmetic() {
        assert_eq!(x11_tcp_port(0), Some(6000));
        assert_eq!(x11_tcp_port(59_535), Some(65_535));
        assert_eq!(x11_tcp_port(59_536), None);
        assert_eq!(x11_tcp_port(u32::MAX), None);
    }

    #[test]
    fn hex_cookies_round_trip() {
        let text = random_cookie_hex();
        assert_eq!(text.len(), COOKIE_BYTES * 2);
        let bytes = decode_hex(&text).unwrap();
        assert_eq!(bytes.len(), COOKIE_BYTES);
        assert_eq!(encode_hex(&bytes), text);
        assert!(decode_hex("xyz").is_none());
    }

    #[test]
    fn rewrite_replaces_auth_and_keeps_order() {
        // A little-endian setup: name "MIT-MAGIC-COOKIE-1", 16-byte data.
        let old_name = b"MIT-MAGIC-COOKIE-1";
        let old_data = [0xAAu8; 16];
        let mut setup = vec![0x6c, 0];
        setup.extend_from_slice(&11u16.to_le_bytes());
        setup.extend_from_slice(&0u16.to_le_bytes());
        setup.extend_from_slice(&(old_name.len() as u16).to_le_bytes());
        setup.extend_from_slice(&(old_data.len() as u16).to_le_bytes());
        setup.extend_from_slice(&0u16.to_le_bytes());
        setup.extend_from_slice(old_name);
        setup.resize(12 + pad4(old_name.len()), 0);
        setup.extend_from_slice(&old_data);
        setup.resize(12 + pad4(old_name.len()) + 16, 0);

        let auth = ("MIT-MAGIC-COOKIE-1".to_string(), vec![0x55u8; 16]);
        let rewritten = rewrite_setup(&setup, &auth).unwrap();
        assert_eq!(rewritten.len(), setup.len());
        assert_eq!(&rewritten[setup.len() - 16..], &[0x55u8; 16]);
    }

    #[test]
    fn rewrite_accepts_an_empty_auth_block() {
        let mut setup = vec![0x42, 0];
        setup.extend_from_slice(&11u16.to_be_bytes());
        setup.extend_from_slice(&0u16.to_be_bytes());
        setup.extend_from_slice(&0u16.to_be_bytes());
        setup.extend_from_slice(&0u16.to_be_bytes());
        setup.extend_from_slice(&0u16.to_be_bytes());
        let auth = ("MIT-MAGIC-COOKIE-1".to_string(), vec![1u8; 16]);
        let rewritten = rewrite_setup(&setup, &auth).unwrap();
        assert_eq!(rewritten.len(), 12 + 20 + 16);
        assert_eq!(&rewritten[12 + 20..], &[1u8; 16]);
    }
}
