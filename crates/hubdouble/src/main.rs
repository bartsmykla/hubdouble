use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::process::ExitCode;
use std::sync::Arc;

use hubdouble_model::SystemClock;
use hubdouble_rest::Registry;
use hubdouble_server::{Registries, Server, config_from_env};
use mimalloc::MiMalloc;

#[global_allocator]
static ALLOCATOR: MiMalloc = MiMalloc;

const USAGE: &str =
    "usage: hubdouble [serve]\n       hubdouble coverage [--inventory <rest-routes.txt>]\n";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("serve") if args.len() <= 1 => serve(),
        Some("coverage") => coverage(&args[1..]),
        Some("help" | "-h" | "--help") => write_out(USAGE).map(|()| true),
        _ => Err(format!("unknown arguments {args:?}\n{USAGE}")),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("hubdouble: {e}");
            ExitCode::FAILURE
        }
    }
}

fn serve() -> Result<bool, String> {
    let env = |name: &str| std::env::var(name).ok();
    let config = config_from_env(&env).map_err(|e| e.to_string())?;
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    if let Some(n) = config.workers {
        runtime.worker_threads(n);
    }
    let runtime = runtime.enable_all().build().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let registries = Registries::new(hubdouble_api::register_all);
        let server = Server::start(config, Arc::new(SystemClock), &env, registries)
            .map_err(|e| e.to_string())?;
        let listener = server.bind().await.map_err(|e| e.to_string())?;
        if let Ok(addr) = listener.local_addr() {
            eprintln!("hubdouble: listening on {addr}");
        }
        server.serve(listener, shutdown_signal()).await;
        Ok(true)
    })
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn coverage(args: &[String]) -> Result<bool, String> {
    let inventory = match args {
        [] => None,
        [flag, path] if flag == "--inventory" => Some(path),
        _ => return Err(USAGE.to_owned()),
    };
    let registries = Registries::new(hubdouble_api::register_all);
    let rest = &registries.rest;
    let mut report = table_report(rest);
    let mut complete = true;
    if let Some(path) = inventory {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        let (lines, missing) = inventory_report(rest, &text)?;
        complete = missing == 0;
        report.push_str(&lines);
    }
    write_out(&report)?;
    Ok(complete)
}

fn table_report(rest: &Registry) -> String {
    let mut by_category: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let (mut registered, mut total, mut extras) = (0, 0, 0);
    for op in rest.operations() {
        if op.extra {
            extras += usize::from(op.registered);
            continue;
        }
        let entry = by_category.entry(op.category).or_default();
        entry.1 += 1;
        total += 1;
        if op.registered {
            entry.0 += 1;
            registered += 1;
        }
    }
    let mut out = format!(
        "operations: {registered} of {total} registered ({})\n",
        percent(registered, total)
    );
    for (category, (done, all)) in by_category {
        let _ = writeln!(out, "  {category}: {done}/{all}");
    }
    let _ = writeln!(out, "extra routes: {extras}");
    out
}

fn inventory_report(rest: &Registry, text: &str) -> Result<(String, usize), String> {
    let mut lines = String::new();
    let (mut total, mut missing) = (0, 0);
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let Some((method, template)) = line.split_once(' ') else {
            return Err(format!("inventory line {line:?} is not \"METHOD /path\""));
        };
        let method = method
            .parse::<http::Method>()
            .map_err(|e| format!("{line:?}: {e}"))?;
        total += 1;
        if method == http::Method::POST && template.trim() == "/graphql" {
            continue;
        }
        let route = rest.route(&method, &sample_path(template.trim()));
        match route {
            Some(r) if r.entry.is_some() => {}
            Some(r) => {
                missing += 1;
                let _ = writeln!(lines, "  missing {line} ({})", r.operation_id);
            }
            None => {
                missing += 1;
                let _ = writeln!(lines, "  missing {line} (no operation)");
            }
        }
    }
    let head = format!(
        "inventory: {} of {total} routes registered ({})\n",
        total - missing,
        percent(total - missing, total)
    );
    Ok((head + &lines, missing))
}

fn sample_path(template: &str) -> String {
    template
        .split('/')
        .map(|s| {
            if s.starts_with('{') && s.ends_with('}') {
                "sample"
            } else {
                s
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn percent(part: usize, all: usize) -> String {
    let tenths = (part * 1000).checked_div(all).unwrap_or(0);
    format!("{}.{}%", tenths / 10, tenths % 10)
}

fn write_out(text: &str) -> Result<(), String> {
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_inventory_is_fully_served() {
        // Given
        let registries = Registries::new(hubdouble_api::register_all);
        let inventory = include_str!("../../../docs/inventory/rest-routes.txt");

        // When
        let (report, missing) = inventory_report(&registries.rest, inventory).unwrap();

        // Then
        assert_eq!(missing, 0, "{report}");
    }

    #[test]
    fn an_unregistered_route_is_missing() {
        // Given
        let registries = Registries::new(hubdouble_api::register_all);

        // When
        let (_, missing) =
            inventory_report(&registries.rest, "GET /rate_limit\nPOST /graphql\n").unwrap();

        // Then
        assert_eq!(missing, 1);
    }
}
