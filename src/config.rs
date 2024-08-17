use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Name of the person to greet
    #[arg(long)]
    port: u16,

    #[arg(long)]
    fork_url: String,

    #[arg(long)]
    etherscan_key: Option<String>,

    #[arg(long)]
    api_key: Option<String>,

    #[arg(long)]
    max_request_size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub fork_url: String,
    pub etherscan_key: Option<String>,
    pub api_key: Option<String>,
    pub max_request_size: u64,
}

pub fn config() -> Config {
    let args = Args::parse();

    Config {
        port: args.port,
        fork_url: args.fork_url,
        etherscan_key: args.etherscan_key,
        api_key: args.api_key,
        max_request_size: args.max_request_size.unwrap_or(16) * 1024,
    }
}
