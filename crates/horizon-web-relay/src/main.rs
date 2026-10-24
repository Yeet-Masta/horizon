//! `horizon-web-relay`: WebSockets in, TCP connections out (web/README.md).
//!
//! ```text
//! horizon-web-relay --target localhost:25565            every page reaches this server
//! horizon-web-relay --allow play.example.net:25565      pages name it: ?target=play.example.net:25565
//! horizon-web-relay --allow '*:25565'                   any public server on port 25565
//! horizon-web-relay                                     only rooms for worlds shared from browsers
//! ```
//!
//! Rooms (`?host`, `?join=CODE`) introduce browsers to each other over WebRTC;
//! `--no-rooms` turns them off, `--max-rooms` (256) and `--max-guests` (16)
//! bound them.
//!
//! A relay that reaches servers also passes their resource packs on to pages
//! whose own download the pack's server refuses (`?download=URL`, from public
//! addresses only, at most 250 MiB): `--no-downloads` turns that off, and
//! `--downloads` turns it on for a relay with rooms only.
//!
//! `--listen` (127.0.0.1:8081 by default) is where WebSockets arrive; serve
//! them as `wss://` from behind a web server (web/apache.htaccess).
//! `--origin https://example.net` lets only that page in; `--max` caps
//! connections (64).
use horizon_web_relay::{Allow, Config, serve};
use std::{net::TcpListener, process::exit};

fn usage() -> ! {
    eprintln!(
        "usage: horizon-web-relay [--listen ADDR:PORT] [--target HOST:PORT | --allow HOST:PORT|*:PORT ...] \
         [--origin URL ...] [--max N] [--no-rooms] [--max-rooms N] [--max-guests N] \
         [--downloads | --no-downloads]"
    );
    exit(2);
}

fn main() {
    let mut config = Config::default();
    let mut listen = "127.0.0.1:8081".to_owned();
    let mut downloads = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--listen" => listen = value(),
            "--target" => {
                let target = value();
                let Some(Allow {
                    host: Some(host),
                    port,
                }) = Allow::parse(&target)
                else {
                    eprintln!("horizon-web-relay: --target needs HOST:PORT, not {target}");
                    exit(2);
                };
                config.target = Some((host, port));
            }
            "--allow" => {
                let allow = value();
                match Allow::parse(&allow) {
                    Some(a) => config.allow.push(a),
                    None => {
                        eprintln!(
                            "horizon-web-relay: --allow needs HOST:PORT or *:PORT, not {allow}"
                        );
                        exit(2);
                    }
                }
            }
            "--origin" => config
                .origins
                .push(value().trim_end_matches('/').to_owned()),
            "--max" => config.max_connections = value().parse().unwrap_or_else(|_| usage()),
            "--no-rooms" => config.rooms = false,
            "--max-rooms" => config.max_rooms = value().parse().unwrap_or_else(|_| usage()),
            "--max-guests" => config.max_guests = value().parse().unwrap_or_else(|_| usage()),
            "--downloads" => downloads = Some(true),
            "--no-downloads" => downloads = Some(false),
            "-h" | "--help" => usage(),
            _ => {
                eprintln!("horizon-web-relay: unknown option {arg}");
                usage();
            }
        }
    }
    if config.target.is_none() && config.allow.is_empty() && !config.rooms {
        eprintln!(
            "horizon-web-relay: with --no-rooms, name the servers it reaches (--target, --allow)"
        );
        usage();
    }
    config.downloads = downloads.unwrap_or(config.target.is_some() || !config.allow.is_empty());
    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| {
        eprintln!("horizon-web-relay: can't listen on {listen}: {e}");
        exit(1);
    });
    let reaches = match &config.target {
        Some((host, port)) => format!("{host}:{port}"),
        None if config.allow.is_empty() => "no servers".to_owned(),
        None => config
            .allow
            .iter()
            .map(|a| format!("{}:{}", a.host.as_deref().unwrap_or("*"), a.port))
            .collect::<Vec<_>>()
            .join(", "),
    };
    let on = |on: bool| if on { "on" } else { "off" };
    println!(
        "horizon-web-relay {} on ws://{listen}/, reaching {reaches}; rooms {}, downloads {}",
        env!("CARGO_PKG_VERSION"),
        on(config.rooms),
        on(config.downloads),
    );
    serve(listener, config);
}
