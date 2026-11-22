mod api;
mod cluster;
mod manifests;
mod tokens;

use std::error::Error;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const CLUSTER: &str = "hubdouble-smoke";
const CONTEXT: &str = "kind-hubdouble-smoke";

pub(super) fn run(root: &Path) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(smoke(root))
}

async fn smoke(root: &Path) -> Result<()> {
    let target = root.join("target");
    std::fs::create_dir_all(&target)?;
    let scratch = tempfile::Builder::new()
        .prefix("hubdouble-smoke-")
        .tempdir_in(target)?;
    let mut cluster = cluster::Cluster::new(root, scratch.path());
    let outcome = tokio::select! {
        biased;
        signal = interrupt() => signal.and(Err("kind smoke interrupted".into())),
        result = exercise(&mut cluster) => result,
    };
    let cleanup = cluster.cleanup().await;
    let scratch = scratch.close().map_err(Into::into);
    let errors: Vec<_> = [outcome, cleanup, scratch]
        .into_iter()
        .filter_map(Result::err)
        .map(|error| error.to_string())
        .collect();
    require(errors.is_empty(), &errors.join("\n"))?;
    eprintln!("Kind smoke passed and removed its cluster and images");
    Ok(())
}

async fn exercise(cluster: &mut cluster::Cluster<'_>) -> Result<()> {
    cluster.start().await?;
    let result = async {
        let key = tokens::AppKey::generate()?;
        let data = manifests::Credentials {
            admin: format!(
                "{:032x}{:032x}",
                rand::random::<u128>(),
                rand::random::<u128>()
            ),
            user: format!(
                "ghp_{:032x}{:04x}",
                rand::random::<u128>(),
                rand::random::<u16>()
            ),
            webhook: format!(
                "{:032x}{:032x}",
                rand::random::<u128>(),
                rand::random::<u128>()
            ),
        };
        cluster.deploy(&data, &key.public_pem).await?;
        let port = cluster.forward().await?;
        api::check(port, &key, &data).await
    }
    .await;
    if result.is_err() {
        cluster.diagnose().await;
    }
    result
}

#[cfg(unix)]
async fn interrupt() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => {},
        _ = terminate.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
async fn interrupt() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn require(condition: bool, error: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(error.to_owned().into())
    }
}
