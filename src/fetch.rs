//! Fetches of URLs that someone other than an admin can influence (design
//! §8): a script stack's version URL comes from an agent's inventory, so a
//! stolen agent token must not turn Wren into a proxy into its own network.
//!
//! - Only public addresses: every address the host resolves to is checked
//!   ([`is_public`]), and the connection goes to the checked address (DNS is
//!   pinned with `ClientBuilder::resolve`, no proxy), so a second lookup
//!   can't swap in a private one.
//! - Redirects are followed by hand, at most [`MAX_REDIRECTS`], each hop
//!   checked like the first.
//! - The whole fetch has one time limit, and at most `max_body` bytes are
//!   read; callers keep what they extract, never the body.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::Url;
use reqwest::header::LOCATION;

/// Redirects followed per fetch.
pub const MAX_REDIRECTS: usize = 3;

/// Which addresses a fetch may connect to.
pub type Allow = fn(IpAddr) -> bool;

/// Any address (tests, which serve on loopback).
pub fn any(_: IpAddr) -> bool {
    true
}

/// A public unicast address: not loopback, private, link-local, CGNAT,
/// multicast, broadcast, unspecified, reserved, documentation or
/// benchmarking, for IPv4 and IPv6, and IPv4 inside IPv6 (mapped,
/// compatible, NAT64, 6to4) judged as the IPv4 it carries.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(o[0] == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || ip.is_documentation()
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || ip.is_multicast()
        || o[0] >= 240)
}

/// The IPv4 address an IPv6 address carries, if it is one of the forms
/// that reach IPv4 hosts.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let v4 = |hi: u16, lo: u16| {
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    // ::ffff:a.b.c.d (mapped) and ::a.b.c.d (compatible; also :: and ::1).
    if s[..5] == [0; 5] && (s[5] == 0xffff || s[5] == 0) {
        return Some(v4(s[6], s[7]));
    }
    // 64:ff9b::/96 (NAT64).
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return Some(v4(s[6], s[7]));
    }
    // 2002::/16 (6to4).
    if s[0] == 0x2002 {
        return Some(v4(s[1], s[2]));
    }
    None
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = embedded_v4(ip) {
        return public_v4(v4);
    }
    let s = ip.segments();
    !(ip.is_multicast()
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        || (s[0] == 0x2001 && s[1] == 0)
        || (s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1)
        || s[..4] == [0x100, 0, 0, 0])
}

/// The address to connect to for `url`: every address of its host must be
/// allowed (a mix is refused, so the answer can't depend on which one a
/// resolver lists first).
async fn resolve(url: &Url, allow: Allow) -> Result<SocketAddr, String> {
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs: Vec<SocketAddr> = match url.host() {
        Some(url::Host::Ipv4(ip)) => vec![SocketAddr::new(ip.into(), port)],
        Some(url::Host::Ipv6(ip)) => vec![SocketAddr::new(ip.into(), port)],
        Some(url::Host::Domain(d)) => tokio::net::lookup_host((d, port))
            .await
            .map_err(|_| format!("{d} can't be resolved"))?
            .collect(),
        None => return Err("the URL has no host".into()),
    };
    let first = *addrs.first().ok_or("the URL's host has no address")?;
    if addrs.iter().any(|a| !allow(a.ip())) {
        return Err(
            "the URL points to a private or local address; Wren only fetches public addresses"
                .into(),
        );
    }
    Ok(first)
}

/// `GET url` under the rules above: the first `max_body` bytes of a `2xx`
/// answer, within `timeout` for the whole fetch.
pub async fn get_limited(
    raw: &str,
    allow: Allow,
    max_body: usize,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, get(raw, allow, max_body))
        .await
        .map_err(|_| "the URL took too long to answer".to_string())?
}

async fn get(raw: &str, allow: Allow, max_body: usize) -> Result<Vec<u8>, String> {
    let mut url = Url::parse(raw.trim()).map_err(|_| "not a URL".to_string())?;
    for _ in 0..=MAX_REDIRECTS {
        if !matches!(url.scheme(), "http" | "https") {
            return Err("the URL must use http or https".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("the URL must not carry a user name or password".into());
        }
        let addr = resolve(&url, allow).await?;
        let mut builder = reqwest::Client::builder()
            .user_agent(crate::product::USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if let Some(url::Host::Domain(d)) = url.host() {
            builder = builder.resolve(d, addr);
        }
        let client = builder.build().map_err(|_| "no HTTP client".to_string())?;
        let mut res = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| "the URL can't be reached".to_string())?;
        let status = res.status();
        if status.is_redirection() {
            let next = res
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url.join(l).ok())
                .ok_or_else(|| format!("the URL answered {status} without a valid Location"))?;
            url = next;
            continue;
        }
        if !status.is_success() {
            return Err(format!("the URL answered {status}"));
        }
        let mut body = Vec::new();
        while body.len() < max_body {
            match res.chunk().await {
                Ok(Some(chunk)) => {
                    let room = max_body - body.len();
                    body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                }
                Ok(None) => break,
                Err(_) => return Err("the URL sent nothing readable".into()),
            }
        }
        return Ok(body);
    }
    Err("the URL redirected too often".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn blocks_private_and_special_addresses() {
        for blocked in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.254",
            "192.0.0.8",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "2002:7f00:1::",
            "2002:c0a8:101::1",
            "fe80::1",
            "fec0::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "100::1",
        ] {
            let ip: IpAddr = blocked.parse().unwrap();
            assert!(!is_public(ip), "{blocked} must be blocked");
        }
        for open in [
            "1.1.1.1",
            "8.8.8.8",
            "140.82.112.3",
            "100.63.255.255",
            "100.128.0.1",
            "172.32.0.1",
            "2606:4700:4700::1111",
            "::ffff:1.1.1.1",
            "64:ff9b::808:808",
            "2002:0808:0808::1",
        ] {
            let ip: IpAddr = open.parse().unwrap();
            assert!(is_public(ip), "{open} is public");
        }
    }

    /// A one-shot HTTP server on loopback answering `reply` to every request.
    async fn serve(reply: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let reply = reply.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(&reply).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn only_loopback(ip: IpAddr) -> bool {
        ip == IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    const SECOND: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn version_url_on_loopback_is_refused() {
        let base = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi".to_vec()).await;
        let err = get_limited(&format!("{base}/v"), is_public, 1024, SECOND)
            .await
            .unwrap_err();
        assert!(err.contains("private or local"), "{err}");
        for url in [
            "http://[::1]:9/",
            "http://localhost:9/",
            "http://169.254.169.254/latest/meta-data/",
        ] {
            let err = get_limited(url, is_public, 1024, SECOND).await.unwrap_err();
            assert!(err.contains("private or local"), "{url}: {err}");
        }
        assert!(
            get_limited("file:///etc/passwd", any, 1024, SECOND)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn redirect_to_a_private_address_is_refused() {
        let base = serve(
            b"HTTP/1.1 302 Found\r\nLocation: http://10.255.255.1/x\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
        )
        .await;
        let err = get_limited(&format!("{base}/v"), only_loopback, 1024, SECOND)
            .await
            .unwrap_err();
        assert!(err.contains("private or local"), "{err}");
        // A redirect loop ends.
        let looped =
            serve(b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\n\r\n".to_vec())
                .await;
        let err = get_limited(&format!("{looped}/v"), only_loopback, 1024, SECOND)
            .await
            .unwrap_err();
        assert!(err.contains("too often"), "{err}");
    }

    #[tokio::test]
    async fn body_is_capped() {
        let mut reply = b"HTTP/1.1 200 OK\r\nContent-Length: 300000\r\n\r\n".to_vec();
        reply.extend(std::iter::repeat_n(b'x', 300_000));
        let base = serve(reply).await;
        let body = get_limited(&format!("{base}/big"), only_loopback, 1000, SECOND)
            .await
            .unwrap();
        assert_eq!(body.len(), 1000);
    }
}
