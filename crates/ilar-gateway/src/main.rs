use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ilar_gateway::channel::Channel;
use ilar_gateway::config::{GatewayConfig, channel_names, gateway_dir};
use ilar_gateway::driver::log;
use ilar_gateway::gateway::Gateway;
use ilar_gateway::inbox::{self, InboxMessage};

#[derive(Parser)]
#[command(name = "ilar-gateway", about = "ilar as an always-on assistant")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Listen on the configured channels (the default).
    Run,
    /// Send a message to the running gateway from a script.
    Notify {
        text: String,
        /// Who is sending; one message per source per interval.
        #[arg(long, default_value = "notify")]
        source: String,
        /// A session key (`<channel>:<chat>`); the last active chat otherwise.
        #[arg(long)]
        to: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = ilar::config::load().resolve()?;
    let gateway = GatewayConfig::from_core(&config)?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Notify { text, source, to } => {
            let path = inbox::write(
                &gateway_dir(&config).join("inbox"),
                &InboxMessage { source, text, to },
            )?;
            println!("{}", path.display());
            Ok(())
        }
        Command::Run => run(config, gateway).await,
    }
}

async fn run(config: ilar::config::Config, gateway: GatewayConfig) -> Result<()> {
    for warning in &config.warnings {
        log(warning);
    }
    let channels: Vec<Arc<dyn Channel>> = Vec::new();
    if let Some(name) = channel_names(&config).first() {
        anyhow::bail!("channel {name:?} is not supported yet");
    }
    if channels.is_empty() {
        log("no channels configured; listening to the inbox only");
    }
    let resolver: Arc<dyn ilar::provider::ProviderResolver> = Arc::new(config.clone());
    let gateway =
        Gateway::new(config, gateway, resolver, channels).context("starting the gateway")?;
    let stopper = gateway.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            log("stopping");
            stopper.cancel();
        }
    });
    gateway.run().await
}
