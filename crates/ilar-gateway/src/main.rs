use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ilar_gateway::channel::Channel;
use ilar_gateway::config::{GatewayConfig, channel_names, channel_table, gateway_dir};
use ilar_gateway::deltachat::{DeltaChat, DeltaChatConfig};
use ilar_gateway::driver::log;
use ilar_gateway::gateway::Gateway;
use ilar_gateway::inbox::{self, InboxMessage};

#[derive(Parser)]
#[command(
    name = "ilar-gateway",
    version,
    about = "ilar as an always-on assistant"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Listen on the configured channels (the default).
    Run,
    /// Print the Delta Chat invite link the running gateway wrote.
    Invite,
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
        Command::Invite => {
            let settings: DeltaChatConfig = channel_table(&config, "deltachat")
                .unwrap_or_default()
                .try_into()
                .context("parsing [channels.deltachat] in ilar.toml")?;
            let path = DeltaChat::new(settings, &gateway_dir(&config)).invite_path();
            let invite = std::fs::read_to_string(&path).with_context(|| {
                format!("no invite at {} — is the gateway running?", path.display())
            })?;
            print!("{invite}");
            Ok(())
        }
        Command::Run => run(config, gateway).await,
    }
}

async fn run(config: ilar::config::Config, gateway: GatewayConfig) -> Result<()> {
    for warning in &config.warnings {
        log(warning);
    }
    let mut channels: Vec<Arc<dyn Channel>> = Vec::new();
    for name in channel_names(&config) {
        let table = channel_table(&config, &name).unwrap_or_default();
        match name.as_str() {
            "deltachat" => {
                let settings: DeltaChatConfig = table
                    .try_into()
                    .context("parsing [channels.deltachat] in ilar.toml")?;
                channels.push(DeltaChat::new(settings, &gateway_dir(&config)));
            }
            other => anyhow::bail!("channel {other:?} is not supported"),
        }
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
