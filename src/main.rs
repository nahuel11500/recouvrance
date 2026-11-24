mod agents;
mod alerts;
mod app;
mod assistant;
mod certs;
mod cron;
mod details;
#[cfg(feature = "dev")]
mod dev;
mod diagnose;
#[cfg(all(test, feature = "dev"))]
mod e2e;
mod external_secrets;
mod favorites;
mod filter_syntax;
mod forwards;
mod glob;
mod helm;
mod hex;
mod history;
mod json;
mod k8s;
mod latest;
mod map;
mod mcp;
mod overview;
mod paths;
mod plugins;
mod plural;
mod private_files;
mod problems;
mod prom;
mod quantity;
mod rbac;
mod reachability;
mod schema;
mod settings;
mod smartdiff;
mod status;
mod table;
mod taints;
mod templates;
mod timeline;
mod ui;
mod updates;
mod upgrade;
mod yaml;

// Many threads allocate large, short-lived lists (huge clusters); mimalloc
// gives that memory back where glibc malloc keeps it fragmented.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const HELP: &str = concat!(
    "Recouvrance ",
    env!("CARGO_PKG_VERSION"),
    ": a native Kubernetes desktop client with a local MCP server for agents.

Usage:
  recouvrance                              Open the app
  recouvrance serve                        Run the MCP server without a window
  recouvrance agent-create NAME [CONTEXT…] Create an agent and print its token once
  recouvrance --version                    Print the version
  recouvrance --help                       Print this help

Settings live in {config}; see
https://github.com/nahuel11500/recouvrance/blob/main/docs/configuration.md
"
);

fn main() {
    #[cfg(feature = "dev")]
    if let Ok(ctx) = std::env::var("RECOUVRANCE_SMOKE") {
        return dev::smoke::run(ctx);
    }
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // Headless MCP server, without the window.
        Some("serve") => return serve(),
        // `Recouvrance agent-create <name> [context...]` prints the agent's token once.
        Some("agent-create") => return agent_create(&args[2..]),
        Some("--version" | "-V") => return println!("recouvrance {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h") => return print!("{}", HELP.replace("{config}", &paths::tilde(&paths::config_dir()))),
        _ => {}
    }
    #[cfg(feature = "dev")]
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "--snapshot") {
            let path = args.get(i + 1).map_or("snapshot.png", String::as_str);
            let secs = args.get(i + 2).and_then(|s| s.parse().ok()).unwrap_or(6);
            dev::snapshot::run(path, std::time::Duration::from_secs(secs)).expect("snapshot");
            return;
        }
    }
    let application = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);
    // macOS keeps the app running when its last window closes: its Dock icon brings one back.
    application.on_reopen(app::reopen);
    application.run(|cx| {
        gpui_kit::init(cx);
        app::bind_keys(cx);
        // After the keys: the macOS menu bar shows the keys bound when it is set.
        app::init_menus(cx);
        plugins::reload(cx);
        // Every window shares one store of favorites, read once here.
        let _ = favorites::shared(cx);
        app::load_zoom(cx);
        let mode = app::initial_theme(cx);
        app::apply_theme(mode, None, cx);
        app::open_main_window(cx);
        // Copies of pods made to read their files go with the app (a few seconds at most).
        cx.on_app_quit(|_| {
            k8s::pod_copy::remove_all_now(std::time::Duration::from_secs(5));
            async {}
        })
        .detach();
        cx.activate(true);
    });
}

fn serve() {
    let hub = mcp::Hub::load();
    hub.claim_config();
    // No window to approve in: Ask rules refuse at once instead of waiting two minutes.
    hub.set_headless(true);
    if let Err(e) = hub.apply_rules_file() {
        eprintln!("rules.yaml: {e:#}");
    }
    hub.watch_rules_file();
    // No window here: a context made read-only in the app, or by hand, counts within seconds.
    settings::watch_read_only();
    let mut events = hub.subscribe();
    let _server = mcp::server::start(hub.clone());
    eprintln!("Recouvrance MCP on http://127.0.0.1:{}/mcp", hub.port());
    futures::executor::block_on(async {
        use futures::StreamExt as _;
        let mut last_error = String::new();
        while let Some(ev) = events.next().await {
            match &*ev {
                mcp::HubEvent::Audit(e) => {
                    let client = e.client.as_ref().map_or_default(|c| format!(" ({c})"));
                    let revealed = if e.revealed.is_empty() {
                        String::new()
                    } else {
                        format!(" [shown: {}]", e.revealed.join(", "))
                    };
                    eprintln!(
                        "{}{client} {:?} {} {} {}ms {}{revealed}",
                        e.agent, e.decision, e.tool, e.args, e.duration_ms, e.detail
                    );
                }
                mcp::HubEvent::ServerStatus(Err(e)) => {
                    if *e != last_error {
                        eprintln!("server error: {e}");
                        last_error.clone_from(e);
                    }
                    // A busy port is tried again (`mcp::server`): keep waiting for it.
                    if !e.contains("trying again") {
                        return;
                    }
                }
                mcp::HubEvent::ServerStatus(Ok(port)) => eprintln!("listening on 127.0.0.1:{port}"),
                _ => {}
            }
        }
    });
}

fn agent_create(args: &[String]) {
    // The running app or `serve` would not see the agent, and would save over it.
    if mcp::config_in_use() {
        eprintln!("Recouvrance is running: create the agent on its Agents page, or quit it first.");
        std::process::exit(1);
    }
    let hub = mcp::Hub::load();
    // rules.yaml wins over agents.json: load it before writing it back with the new agent.
    if let Err(e) = hub.apply_rules_file() {
        eprintln!("rules.yaml: {e:#}");
    }
    let (name, contexts) = agent_create_args(args);
    let created = hub.create_agent(name).and_then(|(agent, token)| {
        hub.update_agent(&agent.id, |a| a.policy.rules[0].contexts = contexts)?;
        Ok(token)
    });
    match created {
        Ok(token) => println!("{token}"),
        Err(e) => {
            eprintln!("agent-create: {e:#}");
            std::process::exit(1);
        }
    }
}

/// `<name> [context...]`; both optional.
fn agent_create_args(args: &[String]) -> (String, Vec<String>) {
    let name = args.first().cloned().unwrap_or_else(|| "agent".into());
    (name, args.get(1..).unwrap_or_default().to_vec())
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_create_args_may_be_missing() {
        assert_eq!(super::agent_create_args(&[]), ("agent".to_string(), vec![]));
        let args = ["bot".to_string(), "c1".to_string(), "c2".to_string()];
        assert_eq!(super::agent_create_args(&args), ("bot".to_string(), vec!["c1".to_string(), "c2".to_string()]));
    }

    /// The project site builds its compare page from the table in `docs/COMPARISON.md`: from the
    /// line starting with `| Area` to the first one that doesn't start with `|`, it drops each
    /// line's first and last character and splits the rest on `|`. A `|` inside a cell, a missing
    /// cell or a space after the last `|` would shift the site's columns.
    #[test]
    fn the_comparison_table_keeps_five_cells_a_row() {
        let doc = include_str!("../docs/COMPARISON.md");
        let table: Vec<&str> =
            doc.lines().skip_while(|l| !l.starts_with("| Area")).take_while(|l| l.starts_with('|')).collect();
        let (head, rows) = table.split_at_checked(2).expect("docs/COMPARISON.md has a comparison table");
        assert_eq!(head, ["| Area | Aptakube | FreeLens | Headlamp | Recouvrance |", "|---|---|---|---|---|"]);
        assert!(!rows.is_empty(), "the comparison table has no rows");
        for row in rows {
            let inner = row.strip_prefix('|').and_then(|r| r.strip_suffix('|'));
            let inner = inner.unwrap_or_else(|| panic!("{row:?} must start and end with `|`"));
            let cells: Vec<&str> = inner.split('|').map(str::trim).collect();
            assert_eq!(cells.len(), 5, "{row:?} must have five cells");
            assert!(!cells.contains(&""), "{row:?} has an empty cell");
        }
    }
}
