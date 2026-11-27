//! The egress agent.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use egress::agent::{self, Metrics};
use egress::crd;
use kube::CustomResourceExt;
use sliplib_kube::ApiTimeouts;
use sliplib_runtime::metrics::{Page, Rendering};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

/// Well inside the kubelet's default grace period of 30 s.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const API_TIMEOUTS: ApiTimeouts = ApiTimeouts {
    watch_secs: 60,
    read: Duration::from_secs(65),
};
/// Scrapes served at once; past it a client waits in the listen backlog.
const SCRAPES: NonZeroUsize = NonZeroUsize::new(4).unwrap();

#[derive(Parser)]
#[command(version, about = "The egress agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the CRD of `Egress`, as `deploy/` holds it.
    Crd,
    /// Resolve every `Egress` that names this node, the node named by `NODE_NAME`.
    Run {
        /// Where `/metrics` is served.
        #[arg(long)]
        metrics: SocketAddr,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Crd => print!("{}", sliplib_yaml::to_string(&crd::Egress::crd())?),
        Command::Run { metrics } => run(metrics)?,
    }
    Ok(())
}

#[tokio::main]
async fn run(listen: SocketAddr) -> Result<()> {
    sliplib_runtime::init_tracing();
    tracing::info!("{} {}", env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"));
    sliplib_kube::run(
        "egress",
        SHUTDOWN_TIMEOUT,
        API_TIMEOUTS,
        move |agent, shutdown| async move {
            let metrics = Arc::new(Metrics::default());
            let listener = sliplib_runtime::metrics::bind(listen)
                .with_context(|| format!("failed to bind the metrics socket to {listen}"))?;
            tracing::info!(%listen, "metrics endpoint listening");
            let pages = Arc::clone(&metrics);
            tokio::spawn(sliplib_runtime::metrics::serve(
                listener,
                SCRAPES,
                move |path| {
                    let metrics = Arc::clone(&pages);
                    (path == "/metrics").then(|| {
                        let page: Rendering<std::fmt::Error> =
                            Box::pin(async move { Page::openmetrics(&metrics.registry) });
                        page
                    })
                },
            ));
            agent::run(agent, shutdown, &API_TIMEOUTS, metrics).await
        },
    )
    .await
}
