//! What makes this build Wise Wren as far as vole-kit is concerned: names,
//! cookies, token prefixes, migrations, translations, API reference, the
//! settings tabs, the webhook's words and what the shared layouts show
//! (docs/design.md §3).

use rust_embed::Embed;
use vole_kit::config::KitConfig;
use vole_kit::http::assets::StaticFile;
use vole_kit::notify::webhook::Wording;
use vole_kit::page::{NavItem, SettingsTab, Ui};
use vole_kit::settings::GeneralSettings;

/// The product's version (`CARGO_PKG_VERSION`), reported by the OpenAPI
/// document and the MCP server.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The product's name.
pub const NAME: &str = "Wise Wren";
/// `User-Agent` of outgoing requests.
pub const USER_AGENT: &str = concat!("WiseWren/", env!("CARGO_PKG_VERSION"));
/// Name of the signed session cookie.
pub const SESSION_COOKIE: &str = "ww_session";
/// Prefix of API tokens.
pub const TOKEN_PREFIX: &str = "ww_";
/// Prefix of agent tokens (one per server).
pub const AGENT_TOKEN_PREFIX: &str = "wwa_";
/// The database file in the data directory.
pub const DB_FILE: &str = "wren.sqlite";

/// Wren's own static files: icons, the web app manifest and `wren.css`.
/// The design system (stylesheet, scripts, icon sprite, fonts) is the kit's.
#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

/// Looks up one of Wren's static files (for the kit's `/static/` handler).
pub fn static_file(path: &str) -> Option<StaticFile> {
    let file = StaticAssets::get(path)?;
    Some(StaticFile {
        sha256: file.metadata.sha256_hash(),
        data: file.data,
    })
}

pub static KIT: KitConfig = KitConfig {
    product: NAME,
    slug: "wise-wren",
    short_name: "Wren",
    session_cookie: SESSION_COOKIE,
    pending_2fa_cookie: "ww_2fa",
    token_prefix: TOKEN_PREFIX,
    other_token_prefixes: &[AGENT_TOKEN_PREFIX],
    version: VERSION,
    user_agent: USER_AGENT,
    backup_prefix: "wren-before-",
    flash_cookie: "ww_flash",
    env_prefix: "WW",
    // Agents post with their own token from anywhere, never with a cookie.
    csrf_exempt_prefixes: &["/api/v1", "/api/agent", "/mcp"],
    csrf_exempt_path: no_exempt_path,
    extra_common_passwords: &["wisewren", "wren1234"],
    migrations: &crate::db::MIGRATIONS,
    embeddable_prefixes: &[],
    static_file,
    machine_path_prefixes: &["/static/", "/api/"],
    machine_paths: &["/healthz", "/mcp"],
    browser_language_prefixes: &[],
    docs: &crate::api::DOCS,
    locales: &[
        ("en", include_str!("../locales/en.toml")),
        ("nb", include_str!("../locales/nb.toml")),
        ("ar", include_str!("../locales/ar.toml")),
        ("cs", include_str!("../locales/cs.toml")),
        ("da", include_str!("../locales/da.toml")),
        ("de", include_str!("../locales/de.toml")),
        ("es", include_str!("../locales/es.toml")),
        ("fa", include_str!("../locales/fa.toml")),
        ("fi", include_str!("../locales/fi.toml")),
        ("fr", include_str!("../locales/fr.toml")),
        ("he", include_str!("../locales/he.toml")),
        ("hu", include_str!("../locales/hu.toml")),
        ("it", include_str!("../locales/it.toml")),
        ("ja", include_str!("../locales/ja.toml")),
        ("ko", include_str!("../locales/ko.toml")),
        ("nl", include_str!("../locales/nl.toml")),
        ("pl", include_str!("../locales/pl.toml")),
        ("pt-BR", include_str!("../locales/pt-BR.toml")),
        ("pt-PT", include_str!("../locales/pt-PT.toml")),
        ("ro", include_str!("../locales/ro.toml")),
        ("ru", include_str!("../locales/ru.toml")),
        ("sv", include_str!("../locales/sv.toml")),
        ("tr", include_str!("../locales/tr.toml")),
        ("uk", include_str!("../locales/uk.toml")),
        ("zh-CN", include_str!("../locales/zh-CN.toml")),
        ("zh-TW", include_str!("../locales/zh-TW.toml")),
    ],
    ui: &Ui {
        logo: "/static/favicon.svg",
        favicon: "/static/favicon.svg",
        apple_touch_icon: "/static/apple-touch-icon.png",
        manifest: "/static/site.webmanifest",
        nav: &[
            NavItem {
                id: "overview",
                href: "/",
                label: "overview.title",
                short_label: "overview.title",
                icon: "dashboard",
            },
            NavItem {
                id: "updates",
                href: "/updates",
                label: "updates.title",
                short_label: "updates.title",
                icon: "download",
            },
            NavItem {
                id: "schedules",
                href: "/schedules",
                label: "schedules.title",
                short_label: "schedules.title",
                icon: "calendar",
            },
            NavItem {
                id: "servers",
                href: "/servers",
                label: "servers.title",
                short_label: "servers.title",
                icon: "server",
            },
            NavItem {
                id: "settings",
                href: "/settings",
                label: "nav.settings",
                short_label: "nav.settings",
                icon: "gear",
            },
        ],
        settings_tabs: &[
            SettingsTab::GENERAL,
            SettingsTab::USERS,
            SettingsTab::SECURITY,
            SettingsTab::TOKENS,
            crate::channels::TAB,
            crate::registries::TAB,
            crate::vv_pages::TAB,
        ],
        home_url: "https://hedgerow.tools/Wise-Wren/",
    },
    // Set on the General page; `WW_PUBLIC_URL` stores it at start-up.
    public_url_setting: Some("Settings → General"),
    // "target" would clash with the kit's own {{target}} placeholder.
    webhook: Wording {
        subject: "subject",
        duration: "duration",
        url_example: "https://example.com/hooks/wren…",
    },
    token_scopes: &[],
    // Site name, public URL, time zone and language.
    general: Some(&GeneralSettings::CORE_ONLY),
    // The signed-in footer links the running version to its release notes.
    releases_url: Some("https://github.com/Ferdinand99/Wise-Wren/releases/tag/v{version}"),
    features: &vole_kit::config::KitFeatures::DEFAULT,
};

/// Every exempt path is a prefix above.
fn no_exempt_path(_: &str) -> bool {
    false
}

/// Installs [`KIT`]. Call it first thing at start-up, and in test set-up.
pub fn install() {
    vole_kit::config::install(&KIT);
}
