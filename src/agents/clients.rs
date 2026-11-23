//! What connects an MCP client to Recouvrance: its config, and its command where it has
//! one that takes a header. Each carries the server's URL and the agent's token, and a
//! tool timeout long enough for an approval where the client has that setting. The
//! formats are the ones each client documents (November 2026).

/// The server's name in the clients' configs.
const SERVER: &str = "recouvrance";

/// How long a client should let one tool call run, in seconds. Zed allows 10 minutes at
/// most, and Gemini CLI waits that long by default.
pub const TOOL_TIMEOUT_SECS: u64 = 600;

// An approval waits up to 2 minutes, then `wait_for` or `rollout_status` can wait up to 5
// more: a client that stops the call sooner reports a failure for a call that went ahead.
const _: () = assert!(TOOL_TIMEOUT_SECS >= crate::mcp::APPROVAL_TIMEOUT.as_secs() + 300);

/// Shown in place of the token: copying puts the real one in.
pub const MASKED_TOKEN: &str = "kv_••••••••";

/// The clients the agent's page has a config for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Client {
    ClaudeCode,
    Codex,
    Cursor,
    VsCode,
    Gemini,
    Zed,
}

/// One way to connect a client.
pub struct Snippet {
    /// A command to run, rather than text for a config file.
    pub command: bool,
    /// Where it goes.
    pub hint: &'static str,
    pub text: String,
}

pub fn server_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

impl Client {
    pub const ALL: [Self; 6] = [Self::ClaudeCode, Self::Codex, Self::Cursor, Self::VsCode, Self::Gemini, Self::Zed];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::VsCode => "VS Code",
            Self::Gemini => "Gemini CLI",
            Self::Zed => "Zed",
        }
    }

    /// Its config, then its command. Claude Code has only the command: it keeps its config
    /// itself. Codex's command can't set a header or a timeout. The commands need no
    /// quoting beyond theirs: the URL is ours and a token is `kv_` and hex digits.
    pub fn snippets(self, url: &str, token: &str) -> Vec<Snippet> {
        let bearer = format!("Bearer {token}");
        let headers = format!("{{ \"Authorization\": {} }}", json_str(&bearer));
        let millis = TOOL_TIMEOUT_SECS * 1000;
        match self {
            Self::ClaudeCode => vec![command(
                "Run once:",
                format!(
                    "claude mcp add --scope user --transport http {SERVER} {url} --header \"Authorization: {bearer}\""
                ),
            )],
            Self::Codex => vec![config(
                "Add to ~/.codex/config.toml:",
                format!(
                    "[mcp_servers.{SERVER}]\nurl = {}\nhttp_headers = {{ Authorization = {} }}\ntool_timeout_sec = {TOOL_TIMEOUT_SECS}",
                    toml_str(url),
                    toml_str(&bearer)
                ),
            )],
            Self::Cursor => vec![config(
                "Add to ~/.cursor/mcp.json:",
                json_file("mcpServers", &[("url", json_str(url)), ("headers", headers)]),
            )],
            Self::VsCode => vec![config(
                "Add to ~/.copilot/mcp-config.json, the file VS Code's Copilot agent reads:",
                json_file(
                    "mcpServers",
                    &[
                        ("type", json_str("http")),
                        ("url", json_str(url)),
                        ("headers", headers),
                        ("tools", "[\"*\"]".into()),
                    ],
                ),
            )],
            Self::Gemini => vec![
                config(
                    "Add to ~/.gemini/settings.json:",
                    json_file(
                        "mcpServers",
                        &[("httpUrl", json_str(url)), ("headers", headers), ("timeout", millis.to_string())],
                    ),
                ),
                command(
                    "Or run once:",
                    format!(
                        "gemini mcp add --scope user --transport http --timeout {millis} --header \"Authorization: {bearer}\" {SERVER} {url}"
                    ),
                ),
            ],
            Self::Zed => vec![config(
                "Add to Zed's settings file (zed: open settings file):",
                json_file(
                    "context_servers",
                    &[("url", json_str(url)), ("headers", headers), ("timeout", TOOL_TIMEOUT_SECS.to_string())],
                ),
            )],
        }
    }
}

fn command(hint: &'static str, text: String) -> Snippet {
    Snippet { command: true, hint, text }
}

fn config(hint: &'static str, text: String) -> Snippet {
    Snippet { command: false, hint, text }
}

/// A JSON file holding this one server under `key`, with these fields (their values
/// already JSON) in the order the client's docs show them.
fn json_file(key: &str, fields: &[(&str, String)]) -> String {
    let fields: Vec<String> = fields.iter().map(|(name, value)| format!("      \"{name}\": {value}")).collect();
    format!("{{\n  \"{key}\": {{\n    \"{SERVER}\": {{\n{}\n    }}\n  }}\n}}", fields.join(",\n"))
}

fn json_str(s: &str) -> String {
    serde_json::Value::from(s).to_string()
}

/// A TOML basic string: quotes, backslashes and control characters escaped.
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::*;

    const URL: &str = "http://127.0.0.1:47070/mcp";
    const TOKEN: &str = "kv_0123456789abcdef";

    fn texts(client: Client) -> Vec<String> {
        client.snippets(URL, TOKEN).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn every_snippet_holds_the_url_and_the_token() {
        for client in Client::ALL {
            let snippets = client.snippets(URL, TOKEN);
            assert!(!snippets.is_empty(), "{client:?}");
            for s in snippets {
                assert!(s.text.contains(URL), "{client:?}: {}", s.text);
                assert!(s.text.contains(&format!("Bearer {TOKEN}")), "{client:?}: {}", s.text);
            }
        }
    }

    #[test]
    fn the_masked_token_shows_where_the_token_goes() {
        for client in Client::ALL {
            for (masked, real) in client.snippets(URL, MASKED_TOKEN).into_iter().zip(client.snippets(URL, TOKEN)) {
                assert_eq!(masked.text.replace(MASKED_TOKEN, TOKEN), real.text);
            }
        }
    }

    #[test]
    fn claude_code_is_registered_for_the_user() {
        assert_eq!(
            texts(Client::ClaudeCode),
            [
                "claude mcp add --scope user --transport http recouvrance http://127.0.0.1:47070/mcp --header \"Authorization: Bearer kv_0123456789abcdef\""
            ]
        );
    }

    #[test]
    fn codex_waits_longer_than_its_60_s_default() {
        assert_eq!(
            texts(Client::Codex),
            ["[mcp_servers.recouvrance]\n\
              url = \"http://127.0.0.1:47070/mcp\"\n\
              http_headers = { Authorization = \"Bearer kv_0123456789abcdef\" }\n\
              tool_timeout_sec = 600"]
        );
    }

    #[test]
    fn gemini_command_sets_what_its_config_sets() {
        assert_eq!(
            texts(Client::Gemini)[1],
            "gemini mcp add --scope user --transport http --timeout 600000 --header \"Authorization: Bearer kv_0123456789abcdef\" recouvrance http://127.0.0.1:47070/mcp"
        );
    }

    #[test]
    fn toml_strings_escape_what_toml_requires() {
        assert_eq!(toml_str(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(toml_str("a\tb\nc\u{7f}"), r#""a\u0009b\u000Ac\u007F""#);
        assert_eq!(toml_str("kv_•é"), "\"kv_•é\"");
    }

    /// Where a JSON config keeps the server, the key of its URL, and its timeout in the
    /// unit the client takes.
    fn json_layout(client: Client) -> Option<(&'static str, &'static str, Option<u64>)> {
        match client {
            Client::Cursor | Client::VsCode => Some(("mcpServers", "url", None)),
            Client::Gemini => Some(("mcpServers", "httpUrl", Some(TOOL_TIMEOUT_SECS * 1000))),
            Client::Zed => Some(("context_servers", "url", Some(TOOL_TIMEOUT_SECS))),
            Client::ClaudeCode | Client::Codex => None,
        }
    }

    proptest! {
        /// Whatever the URL and token, each JSON config parses and gives them back where
        /// the client reads them, with the timeout where it takes one.
        #[test]
        fn json_configs_hold_the_url_token_and_timeout(url in any::<String>(), token in any::<String>()) {
            for client in Client::ALL {
                let Some((key, url_key, timeout)) = json_layout(client) else { continue };
                let snippets = client.snippets(&url, &token);
                let config: Value = serde_json::from_str(&snippets[0].text).expect("valid JSON");
                let server = &config[key][SERVER];
                prop_assert_eq!(&server[url_key], &json!(url));
                prop_assert_eq!(&server["headers"]["Authorization"], &json!(format!("Bearer {token}")));
                let timeout = timeout.map(|t| json!(t));
                prop_assert_eq!(server.get("timeout"), timeout.as_ref());
                if client == Client::VsCode {
                    prop_assert_eq!(&server["type"], &json!("http"));
                    prop_assert_eq!(&server["tools"], &json!(["*"]));
                }
            }
        }
    }
}
