//! `/tokensaverstats` and `/tokensaver` commands: surface Token Saver token
//! savings measured by the `rtk` CLI and activate portal sync.
//!
//! `/tokensaverstats` runs `rtk gain` (plus `rtk gain --history`) and renders
//! the result as a "Token Saver" block. `/tokensaver <key>` stores
//! portal credentials via `rtk portal login` and confirms them with
//! `rtk portal sync --dry-run`. The rtk invocations run off the UI thread;
//! results are delivered back via [`BusEvent::TokensaverCommandCompleted`].

use super::*;
use crate::bus::{Bus, BusEvent, TokensaverCommandCompleted, TokensaverCommandPayload};
use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Default Token Saver portal URL; overridable with `TOKENSAVER_PORTAL_URL`.
const DEFAULT_PORTAL_URL: &str = "https://portal.tokensaver.dev";

/// Hard cap on each rtk subprocess so a wedged binary never strands the
/// in-flight flag.
const RTK_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

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
/// PATH.
fn rtk_missing_message() -> String {
    "Token Saver needs the rtk CLI, which is not installed yet.\n\
     \n\
     Install it with:\n\
     \n\
     curl -fsSL https://raw.githubusercontent.com/mintoriakamoto/rtk/refs/heads/master/install.sh | sh\n\
     \n\
     Then run the command again."
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

/// Off-thread body of `/tokensaver <key>`.
fn run_tokensaver_activation(key: &str) -> Result<TokensaverCommandPayload, String> {
    let portal_url =
        std::env::var("TOKENSAVER_PORTAL_URL").unwrap_or_else(|_| DEFAULT_PORTAL_URL.to_string());

    let login = match run_rtk(&["portal", "login", "--url", &portal_url, "--token", key]) {
        Ok(output) => output,
        Err(RtkError::NotFound) => {
            return Ok(TokensaverCommandPayload {
                message: rtk_missing_message(),
                notice: "rtk not installed".to_string(),
            });
        }
        Err(RtkError::Failed(message)) => return Err(scrub_secret(&message, key)),
    };

    if !login.success {
        let detail = if login.stderr.trim().is_empty() {
            login.stdout
        } else {
            login.stderr
        };
        return Err(scrub_secret(
            &format!("Token Saver login failed: {}", detail.trim()),
            key,
        ));
    }

    let sync = run_rtk(&["portal", "sync", "--dry-run"]).map_err(|error| match error {
        // rtk vanished between the two calls; treat as a plain failure.
        RtkError::NotFound => "rtk disappeared from PATH during activation".to_string(),
        RtkError::Failed(message) => scrub_secret(&message, key),
    })?;

    if !sync.success {
        let detail = if sync.stderr.trim().is_empty() {
            sync.stdout
        } else {
            sync.stderr
        };
        return Err(scrub_secret(
            &format!("Token Saver sync check failed: {}", detail.trim()),
            key,
        ));
    }

    Ok(TokensaverCommandPayload {
        message: format!(
            "Token Saver activated (key {}) — run /tokensaverstats to see your savings.",
            mask_key(key)
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
}
