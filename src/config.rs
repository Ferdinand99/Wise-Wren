//! Process configuration: command-line flags, each with a `WW_` environment
//! variable. Everything else lives in the database. `WW_TRUST_PROXY` and
//! `WW_SETUP_TOKEN` are read by vole-kit itself.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::Parser;

/// Wise Wren: keeps the Docker containers on your servers up to date.
#[derive(Debug, Clone, Parser)]
#[command(name = "wise-wren", version, about, long_about = None)]
pub struct Config {
    /// Directory for the database (wren.sqlite), the session key and
    /// pre-migration backups.
    #[arg(long, env = "WW_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    /// Address to listen on.
    #[arg(long, env = "WW_HOST", default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    pub host: IpAddr,

    /// Port to listen on.
    #[arg(long, env = "WW_PORT", default_value_t = 3003)]
    pub port: u16,

    /// The address people and agents use to reach this instance, like
    /// https://wren.example.com. Used in the agents' `docker run` line, API
    /// examples and the same-origin checks; without it, the address of each
    /// request is used.
    #[arg(long, env = "WW_PUBLIC_URL", value_parser = parse_public_url)]
    pub public_url: Option<String>,

    /// Let script stacks' `url` version sources reach private, loopback and
    /// other local addresses (a version file on your LAN). Off by default:
    /// anyone who controls an agent's agent.toml could then make this server
    /// fetch addresses on its own network.
    #[arg(
        long,
        env = "WW_ALLOW_PRIVATE_VERSION_URLS",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub allow_private_version_urls: bool,
}

impl Config {
    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

/// An http(s) URL without query or fragment, returned without a trailing slash.
fn parse_public_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let url = url::Url::parse(raw).map_err(|e| format!("not a URL ({e})"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("use an http:// or https:// address, like https://wren.example.com".into());
    }
    if url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() {
        return Err("leave out user names, queries and fragments".into());
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_and_defaults() {
        let c = Config::try_parse_from(["wise-wren"]).unwrap();
        assert_eq!(c.port, 3003);
        assert_eq!(c.bind_addr().to_string(), "0.0.0.0:3003");
        assert_eq!(c.public_url, None);
        assert!(!c.allow_private_version_urls);
        let c = Config::try_parse_from([
            "wise-wren",
            "--port",
            "8080",
            "--public-url",
            "https://wren.example.com/",
            "--allow-private-version-urls",
            "1",
        ])
        .unwrap();
        assert_eq!(c.port, 8080);
        assert_eq!(c.public_url.as_deref(), Some("https://wren.example.com"));
        assert!(c.allow_private_version_urls);
    }

    #[test]
    fn public_urls_are_checked() {
        assert!(parse_public_url("https://example.com/wren/").is_ok());
        for bad in ["example.com", "ftp://x", "https://x?y=1", "https://u@x"] {
            assert!(parse_public_url(bad).is_err(), "{bad}");
        }
    }
}
