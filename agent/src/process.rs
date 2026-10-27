//! What every child process of the agent has in common: it never sees the
//! agent's own credentials. Script stacks run vendor installers downloaded
//! at run time, and Docker, `cp` and PHP run beside them; none of them has
//! a use for the token or Wren's address.

use tokio::process::Command;

/// The environment variables that carry the agent's credentials.
pub const SECRET_VARS: [&str; 2] = ["WW_AGENT_TOKEN", "WW_SERVER_URL"];

/// Removes [`SECRET_VARS`] from the child's environment, whether inherited
/// or set before (a script stack's `env` included). Call it last.
pub fn scrub(cmd: &mut Command) -> &mut Command {
    for var in SECRET_VARS {
        cmd.env_remove(var);
    }
    cmd
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    async fn shell(script: &str, scrubbed: bool) -> String {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            // As if the agent's own environment held them.
            .env("WW_AGENT_TOKEN", "wwa_secret")
            .env("WW_SERVER_URL", "https://wren.example.com")
            .env("WW_OTHER", "kept");
        if scrubbed {
            scrub(&mut cmd);
        }
        let out = cmd.output().await.unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn children_do_not_see_the_token_or_the_server() {
        let script = r#"echo "${WW_AGENT_TOKEN:-none} ${WW_SERVER_URL:-none} ${WW_OTHER:-none}""#;
        // The probe itself works: without the helper the child sees them.
        assert_eq!(
            shell(script, false).await,
            "wwa_secret https://wren.example.com kept"
        );
        assert_eq!(shell(script, true).await, "none none kept");
        assert!(!shell("env", true).await.contains("wwa_secret"));
    }

    #[tokio::test]
    async fn the_real_helpers_scrub_too() {
        // `snapshot::exec` and the script runners go through `scrub`; the
        // parent's own variables can't be changed in a test (no unsafe), so
        // check what a script stack's `env` can't bring back either.
        let env = std::collections::BTreeMap::from([(
            "WW_AGENT_TOKEN".to_string(),
            "wwa_from_toml".to_string(),
        )]);
        let out = crate::run::run_script(
            r#"echo "${WW_AGENT_TOKEN:-none}""#,
            std::path::Path::new("/"),
            &env,
            std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(out.trim(), "none");
        let out = crate::discover::run(
            r#"echo "${WW_AGENT_TOKEN:-none}""#,
            std::path::Path::new("/"),
            &env,
            std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(out, "none");
    }
}
