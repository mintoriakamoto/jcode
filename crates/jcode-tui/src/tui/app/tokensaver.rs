//! `/tokensaverstats` and `/tokensaver` commands: surface Token Saver token
//! savings measured by the `rtk` CLI and activate portal sync.
//!
//! `/tokensaverstats` runs `rtk gain` (plus `rtk gain --history`) and renders
//! the result as a "Token Saver" block. `/tokensaver <key>` verifies the key
//! against the portal's `POST /api/v1/activate` endpoint over HTTP and, on
//! success, stores `~/.config/tokensaver/credentials.json` (mode 0600, the
//! same file the portal's own tooling reads) — activation does not need the
//! rtk CLI at all; only the stats command does. Both the rtk invocation and
//! the HTTP call run off the UI thread; results are delivered back via
//! [`BusEvent::TokensaverCommandCompleted`].

use super::*;
use crate::bus::{Bus, BusEvent, TokensaverCommandCompleted, TokensaverCommandPayload};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Default Token Saver portal URL; overridable with `TOKENSAVER_PORTAL_URL`.
const DEFAULT_PORTAL_URL: &str = "https://portal.tokensaver.dev";

/// Hard cap on each rtk subprocess so a wedged binary never strands the
/// in-flight flag.
const RTK_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on the portal activation HTTP round-trip, mirroring the rtk
/// subprocess timeout discipline.
const PORTAL_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Handle `/tokensaverstats`: show how much Token Saver (via rtk) saved.
pub(super) fn handle_tokensaverstats_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed != "/tokensaverstats" {
        return false;
    }

    if app.tokensaver_command_running {
        app.set_status_notice("Token Saver command already running…");
        return true;
    }
    app.tokensaver_command_running = true;

    app.push_display_message(DisplayMessage::system(
        "Fetching your Token Saver token savings…".to_string(),
    ));
    app.set_status_notice("Token Saver → fetching savings");

    let session_id = app.session.id.clone();
    std::thread::spawn(move || {
        let result = run_tokensaverstats();
        Bus::global().publish(BusEvent::TokensaverCommandCompleted(
            TokensaverCommandCompleted { session_id, result },
        ));
    });

    true
}

/// Handle `/tokensaver [activate] <key>`: activate Token Saver savings sync.
pub(super) fn handle_tokensaver_command(app: &mut App, trimmed: &str) -> bool {
    let rest = if trimmed == "/tokensaver" {
        ""
    } else if let Some(rest) = trimmed.strip_prefix("/tokensaver ") {
        rest
    } else {
        return false;
    };

    let key = match parse_tokensaver_key(rest) {
        Ok(None) => {
            app.push_display_message(DisplayMessage::system(tokensaver_usage().to_string()));
            return true;
        }
        Ok(Some(key)) => key.to_string(),
        Err(message) => {
            app.push_display_message(DisplayMessage::error(message));
            return true;
        }
    };

    if app.tokensaver_command_running {
        app.set_status_notice("Token Saver command already running…");
        return true;
    }
    app.tokensaver_command_running = true;

    app.push_display_message(DisplayMessage::system(format!(
        "Activating Token Saver with key {}…",
        mask_key(&key)
    )));
    app.set_status_notice("Token Saver → activating");

    let session_id = app.session.id.clone();
    std::thread::spawn(move || {
        let result = run_tokensaver_activation(&key);
        Bus::global().publish(BusEvent::TokensaverCommandCompleted(
            TokensaverCommandCompleted { session_id, result },
        ));
    });

    true
}

impl App {
    pub(super) fn handle_tokensaver_command_completed(
        &mut self,
        event: TokensaverCommandCompleted,
    ) {
        if event.session_id != self.session.id {
            return;
        }
        self.tokensaver_command_running = false;

        match event.result {
            Ok(payload) => {
                self.push_display_message(DisplayMessage::system(payload.message));
                self.set_status_notice(payload.notice);
            }
            Err(message) => {
                self.push_display_message(DisplayMessage::error(message));
                self.set_status_notice("Token Saver command failed");
            }
        }
    }
}

/// Parse the argument tail of `/tokensaver`, accepting an optional `activate`
/// verb. Returns `Ok(None)` when no key was given (show instructions),
/// `Ok(Some(key))` for a single-token key, and `Err(usage)` otherwise.
fn parse_tokensaver_key(rest: &str) -> Result<Option<&str>, String> {
    let rest = rest.trim();
    // Only strip a whole `activate` verb, never a key that merely starts with
    // the word (e.g. `activatefoo`).
    let rest = if rest == "activate" {
        ""
    } else {
        rest.strip_prefix("activate ")
            .map(str::trim)
            .unwrap_or(rest)
    };
    if rest.is_empty() {
        return Ok(None);
    }
    let mut parts = rest.split_whitespace();
    let key = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return Err("Usage: /tokensaver <key> (a single portal key, no spaces)".to_string());
    }
    Ok(Some(key))
}

fn tokensaver_usage() -> &'static str {
    "Token Saver activation\n\
     \n\
     Grab your key from the Token Saver portal Settings page\n\
     (https://portal.tokensaver.dev), then paste it here:\n\
     \n\
     /tokensaver <key>\n\
     \n\
     Once activated, run /tokensaverstats to see your savings."
}

/// Friendly install pointer shown instead of an error when `rtk` is not on
/// PATH. Only `/tokensaverstats` needs rtk; activation talks to the portal
/// directly over HTTP.
fn rtk_missing_message() -> String {
    "Token Saver stats need the rtk CLI, which is not installed yet.\n\
     \n\
     Install it with:\n\
     \n\
     curl -fsSL https://raw.githubusercontent.com/mintoriakamoto/rtk/refs/heads/master/install.sh | sh\n\
     \n\
     Then run /tokensaverstats again."
        .to_string()
}

/// Mask a portal key for display: everything but the last 4 characters.
fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let visible = if chars.len() > 4 { 4 } else { 0 };
    let masked = chars.len() - visible;
    let mut out = String::with_capacity(chars.len());
    for (i, c) in chars.iter().enumerate() {
        out.push(if i < masked { '*' } else { *c });
    }
    out
}

/// Replace every occurrence of `secret` in `text` with its masked form so
/// captured rtk output can never echo a full portal key back to the screen.
fn scrub_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, &mask_key(secret))
}

/// Format the `/tokensaverstats` transcript block.
fn format_stats_block(gain: &str, history: Option<&str>) -> String {
    let mut out = String::from("Token Saver\n===========\n\n");
    let gain = gain.trim();
    if gain.is_empty() {
        out.push_str("No savings recorded yet - rtk starts metering as soon as jcode routes commands through it.");
    } else {
        out.push_str(gain);
    }
    if let Some(history) = history {
        let history = history.trim();
        if !history.is_empty() {
            out.push_str("\n\nRecent history\n--------------\n");
            out.push_str(history);
        }
    }
    out
}

struct RtkOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Whether a spawn error means "rtk is not installed" (vs. a real failure).
enum RtkError {
    NotFound,
    Failed(String),
}

/// Run `rtk <args>` with a hard timeout, capturing stdout/stderr.
fn run_rtk(args: &[&str]) -> Result<RtkOutput, RtkError> {
    let mut cmd = std::process::Command::new("rtk");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RtkError::NotFound
        } else {
            RtkError::Failed(format!("Failed to start rtk: {error}"))
        }
    })?;

    let stdout = drain_stream(child.stdout.take());
    let stderr = drain_stream(child.stderr.take());

    let deadline = Instant::now() + RTK_COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout.join();
                    let _ = stderr.join();
                    return Err(RtkError::Failed(format!(
                        "rtk {} timed out after {}s",
                        args.join(" "),
                        RTK_COMMAND_TIMEOUT.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(RtkError::Failed(format!("Failed to wait for rtk: {error}")));
            }
        }
    };

    Ok(RtkOutput {
        success: status.success(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn drain_stream<R: Read + Send + 'static>(stream: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_string(&mut buf);
        }
        buf
    })
}

/// Off-thread body of `/tokensaverstats`.
fn run_tokensaverstats() -> Result<TokensaverCommandPayload, String> {
    let gain = match run_rtk(&["gain"]) {
        Ok(output) => output,
        Err(RtkError::NotFound) => {
            return Ok(TokensaverCommandPayload {
                message: rtk_missing_message(),
                notice: "rtk not installed".to_string(),
            });
        }
        Err(RtkError::Failed(message)) => return Err(message),
    };

    if !gain.success {
        let detail = if gain.stderr.trim().is_empty() {
            gain.stdout
        } else {
            gain.stderr
        };
        return Err(format!("rtk gain failed: {}", detail.trim()));
    }

    // History is a nice-to-have; ignore any failure so the headline stats
    // still render.
    let history = run_rtk(&["gain", "--history"])
        .ok()
        .filter(|output| output.success)
        .map(|output| output.stdout);

    Ok(TokensaverCommandPayload {
        message: format_stats_block(&gain.stdout, history.as_deref()),
        notice: "Token Saver stats ready".to_string(),
    })
}

/// Resolve the portal base URL from an optional `TOKENSAVER_PORTAL_URL`
/// override, falling back to [`DEFAULT_PORTAL_URL`]. Blank overrides are
/// ignored so `TOKENSAVER_PORTAL_URL=""` doesn't break activation.
fn portal_url_from(env_override: Option<String>) -> String {
    env_override
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PORTAL_URL.to_string())
}

/// Build the activation endpoint URL, tolerating a trailing slash on the
/// configured portal base URL.
fn activate_endpoint(portal_url: &str) -> String {
    format!("{}/api/v1/activate", portal_url.trim_end_matches('/'))
}

/// Where device credentials live: `$XDG_CONFIG_HOME/tokensaver/credentials.json`
/// (or `~/.config/...`). This file is shared with the portal's own tooling
/// (tokensaver-mcp), so path and JSON shape must match it exactly.
fn credentials_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"));
    base.join("tokensaver").join("credentials.json")
}

/// On-disk credential shape shared with tokensaver-mcp:
/// `{"portal_url": "...", "device_token": "..."}`.
#[derive(serde::Serialize)]
struct Credentials<'a> {
    portal_url: &'a str,
    device_token: &'a str,
}

/// Persist credentials with owner-only permissions: parent directories are
/// created as needed and the file is forced to mode 0600 on create *and* on
/// every rewrite, in case a previous process loosened it.
fn write_credentials(path: &Path, portal_url: &str, device_token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let creds = Credentials {
        portal_url,
        device_token,
    };
    let json = serde_json::to_string_pretty(&creds).map_err(std::io::Error::other)?;
    std::fs::write(path, format!("{json}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Pure mapping from the activate endpoint's HTTP status + body to either a
/// greeting (success) or an error message (failure). Kept free of any HTTP
/// client types so it is unit-testable without touching the network.
fn interpret_activate_response(status: u16, body: &str) -> Result<String, String> {
    let json: Option<serde_json::Value> = serde_json::from_str(body).ok();
    if (200..300).contains(&status) {
        let greeting = json
            .as_ref()
            .and_then(|value| value.get("message"))
            .and_then(|message| message.as_str())
            .unwrap_or("Token Saver activated — run /tokensaverstats to watch it work.");
        return Ok(greeting.to_string());
    }
    let detail = json
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(|error| error.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                format!("HTTP {status}")
            } else {
                trimmed.to_string()
            }
        });
    Err(match status {
        // 401: unknown/revoked key; 402: prepaid balance exhausted. The
        // portal's error strings already tell the user what to do.
        401 | 402 => detail,
        _ => format!("portal returned HTTP {status}: {detail}"),
    })
}

/// POST the key to the portal activate endpoint as a bearer token. Returns
/// `(status, body)`; transport-level failures come back as `Err(message)`.
fn post_activate(portal_url: &str, key: &str) -> Result<(u16, String), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(PORTAL_HTTP_TIMEOUT)
        .user_agent("jcode-tokensaver")
        .build()
        .map_err(|error| format!("Failed to build HTTP client: {error}"))?;
    let response = client
        .post(activate_endpoint(portal_url))
        .bearer_auth(key)
        .send()
        .map_err(|error| {
            format!("Could not reach the Token Saver portal at {portal_url}: {error}")
        })?;
    let status = response.status().as_u16();
    let body = response.text().unwrap_or_default();
    Ok((status, body))
}

/// Off-thread body of `/tokensaver <key>`: verify the key against the portal
/// over HTTP, then persist credentials. Verification comes first so a
/// rejected key (401/402) or unreachable portal never gets written to disk.
fn run_tokensaver_activation(key: &str) -> Result<TokensaverCommandPayload, String> {
    let portal_url = portal_url_from(std::env::var("TOKENSAVER_PORTAL_URL").ok());

    let (status, body) =
        post_activate(&portal_url, key).map_err(|error| scrub_secret(&error, key))?;

    let greeting = interpret_activate_response(status, &body)
        .map_err(|error| scrub_secret(&format!("Token Saver activation failed: {error}"), key))?;

    let path = credentials_path();
    write_credentials(&path, &portal_url, key).map_err(|error| {
        scrub_secret(
            &format!(
                "Key verified, but storing credentials in {} failed: {error}",
                path.display()
            ),
            key,
        )
    })?;

    Ok(TokensaverCommandPayload {
        message: scrub_secret(
            &format!(
                "{greeting}\n\nKey {} stored in {}.",
                mask_key(key),
                path.display()
            ),
            key,
        ),
        notice: "Token Saver activated".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_key_keeps_only_last_four_chars() {
        assert_eq!(mask_key("cook-1234-abcd"), "**********abcd");
        assert_eq!(mask_key("abcde"), "*bcde");
    }

    #[test]
    fn mask_key_hides_short_keys_entirely() {
        assert_eq!(mask_key("abcd"), "****");
        assert_eq!(mask_key("ab"), "**");
        assert_eq!(mask_key(""), "");
    }

    #[test]
    fn scrub_secret_masks_every_occurrence() {
        let scrubbed = scrub_secret(
            "token cook-secret-9999 rejected (cook-secret-9999)",
            "cook-secret-9999",
        );
        assert!(!scrubbed.contains("cook-secret-9999"));
        assert_eq!(
            scrubbed,
            "token ************9999 rejected (************9999)"
        );
    }

    #[test]
    fn scrub_secret_with_empty_secret_is_identity() {
        assert_eq!(scrub_secret("unchanged", ""), "unchanged");
    }

    #[test]
    fn parse_tokensaver_key_accepts_bare_and_activate_forms() {
        assert_eq!(parse_tokensaver_key("my-key"), Ok(Some("my-key")));
        assert_eq!(parse_tokensaver_key("activate my-key"), Ok(Some("my-key")));
        assert_eq!(
            parse_tokensaver_key("  activate   my-key  "),
            Ok(Some("my-key"))
        );
        // A key that merely starts with the word `activate` stays intact.
        assert_eq!(
            parse_tokensaver_key("activate9999"),
            Ok(Some("activate9999"))
        );
    }

    #[test]
    fn parse_tokensaver_key_without_key_asks_for_instructions() {
        assert_eq!(parse_tokensaver_key(""), Ok(None));
        assert_eq!(parse_tokensaver_key("activate"), Ok(None));
        assert_eq!(parse_tokensaver_key("   "), Ok(None));
    }

    #[test]
    fn parse_tokensaver_key_rejects_extra_tokens() {
        assert!(parse_tokensaver_key("one two").is_err());
        assert!(parse_tokensaver_key("activate one two").is_err());
    }

    #[test]
    fn format_stats_block_includes_header_and_history() {
        let block = format_stats_block("saved 12000 tokens", Some("git log: 80%\n"));
        assert!(block.starts_with("Token Saver\n"));
        assert!(block.contains("saved 12000 tokens"));
        assert!(block.contains("Recent history"));
        assert!(block.contains("git log: 80%"));
    }

    #[test]
    fn format_stats_block_without_history_or_data() {
        let block = format_stats_block("  ", None);
        assert!(block.starts_with("Token Saver\n"));
        assert!(block.contains("No savings recorded yet"));
        assert!(!block.contains("Recent history"));
    }

    #[test]
    fn portal_url_from_prefers_override_and_falls_back() {
        assert_eq!(
            portal_url_from(Some("https://example.test".to_string())),
            "https://example.test"
        );
        assert_eq!(
            portal_url_from(Some("  https://example.test  ".to_string())),
            "https://example.test"
        );
        assert_eq!(portal_url_from(None), DEFAULT_PORTAL_URL);
        // Blank overrides are ignored, not honored as an empty base URL.
        assert_eq!(portal_url_from(Some(String::new())), DEFAULT_PORTAL_URL);
        assert_eq!(portal_url_from(Some("   ".to_string())), DEFAULT_PORTAL_URL);
    }

    #[test]
    fn activate_endpoint_tolerates_trailing_slash() {
        assert_eq!(
            activate_endpoint("https://portal.tokensaver.dev"),
            "https://portal.tokensaver.dev/api/v1/activate"
        );
        assert_eq!(
            activate_endpoint("https://portal.tokensaver.dev/"),
            "https://portal.tokensaver.dev/api/v1/activate"
        );
    }

    #[test]
    fn write_credentials_matches_shared_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Parent dirs are created on demand, like tokensaver-mcp does.
        let path = dir.path().join("nested").join("credentials.json");
        write_credentials(&path, "https://portal.tokensaver.dev", "cook-1234-abcd")
            .expect("write credentials");

        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(raw.ends_with('\n'), "file ends with a newline");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        assert_eq!(
            parsed,
            serde_json::json!({
                "portal_url": "https://portal.tokensaver.dev",
                "device_token": "cook-1234-abcd",
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_credentials_enforces_0600_on_create_and_rewrite() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials.json");

        write_credentials(&path, "https://portal.tokensaver.dev", "first-key")
            .expect("initial write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "created with 0600");

        // Loosen it, then rewrite: permissions must snap back to 0600.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod 644");
        write_credentials(&path, "https://portal.tokensaver.dev", "second-key").expect("rewrite");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "rewrite restores 0600");

        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(raw.contains("second-key"), "rewrite replaced the token");
    }

    #[test]
    fn interpret_activate_response_success_uses_portal_greeting() {
        let body = r#"{"ok":true,"activated":true,"first_activation":true,
            "org":"Acme","device":"laptop","plan":"pro","share_pct":20,
            "message":"Token Saver activated for Acme — you pay 20% of what we save you. Run /tokensaverstats to watch it work."}"#;
        let greeting = interpret_activate_response(200, body).expect("success");
        assert!(greeting.contains("Token Saver activated for Acme"));
    }

    #[test]
    fn interpret_activate_response_success_without_message_field() {
        let greeting = interpret_activate_response(200, r#"{"ok":true}"#).expect("success");
        assert!(greeting.contains("Token Saver activated"));
    }

    #[test]
    fn interpret_activate_response_surfaces_portal_errors() {
        let unauthorized = interpret_activate_response(
            401,
            r#"{"error":"Unknown or revoked key. Create one on the portal Settings page."}"#,
        )
        .expect_err("401 fails");
        assert!(unauthorized.contains("Unknown or revoked key"));

        let no_credits = interpret_activate_response(402, r#"{"error":"Out of credits."}"#)
            .expect_err("402 fails");
        assert_eq!(no_credits, "Out of credits.");

        let opaque = interpret_activate_response(500, "boom").expect_err("500 fails");
        assert!(opaque.contains("HTTP 500"));
        assert!(opaque.contains("boom"));

        let empty = interpret_activate_response(503, "").expect_err("503 fails");
        assert!(empty.contains("HTTP 503"));
    }
}
