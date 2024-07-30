use std::{fs::File, io::stdout, path::PathBuf};

use clap::Parser;
use futures::FutureExt;
use hotshot_stake_table::config::STAKE_TABLE_CAPACITY;
use hotshot_state_prover::service::light_client_genesis;
use sequencer_utils::{
    deployer::{deploy, ContractGroup, Contracts, DeployedContracts},
    logging,
};
use url::Url;

/// Deploy contracts needed to run the sequencer.
///
/// This script deploys contracts needed to run the sequencer to an L1. It outputs a .env file
/// containing the addresses of the deployed contracts.
///
/// This script can also be used to do incremental deployments. The only contract addresses needed
/// to configure the sequencer network are ESPRESSO_SEQUENCER_HOTSHOT_ADDRESS and
/// ESPRESSO_SEQUENCER_LIGHT_CLIENT_PROXY_ADDRESS. These contracts, however, have dependencies, and
/// a full deployment may involve up to 5 total contracts. Some of these contracts, especially
/// libraries may already have been deployed, or perhaps one of the top-level contracts has been
/// deployed and we only need to deploy the other one.
///
/// It is possible to pass in the addresses of already deployed contracts, in which case those
/// addresses will be used in place of deploying a new contract wherever that contract is required
/// in the deployment process. The generated .env file will include all the addresses passed in as
/// well as those newly deployed.
#[derive(Clone, Debug, Parser)]
struct Options {
    /// A JSON-RPC endpoint for the L1 to deploy to.
    #[clap(
        short,
        long,
        env = "ESPRESSO_SEQUENCER_L1_PROVIDER",
        default_value = "http://localhost:8545"
    )]
    rpc_url: Url,

    /// URL of a sequencer node that is currently providing the HotShot config.
    /// This is used to initialize the stake table.
    #[clap(
        long,
        env = "ESPRESSO_SEQUENCER_URL",
        default_value = "http://localhost:24000"
    )]
    pub sequencer_url: Url,

    /// Mnemonic for an L1 wallet.
    ///
    /// This wallet is used to deploy the contracts, so the account indicated by ACCOUNT_INDEX must
    /// be funded with with ETH.
    #[clap(
        long,
        name = "MNEMONIC",
        env = "ESPRESSO_SEQUENCER_ETH_MNEMONIC",
        default_value = "test test test test test test test test test test test junk"
    )]
    mnemonic: String,
    /// Account index in the L1 wallet generated by MNEMONIC to use when deploying the contracts.
    #[clap(
        long,
        name = "ACCOUNT_INDEX",
        env = "ESPRESSO_DEPLOYER_ACCOUNT_INDEX",
        default_value = "0"
    )]
    account_index: u32,

    /// Only deploy the given groups of related contracts.
    #[clap(long, value_delimiter = ',')]
    only: Option<Vec<ContractGroup>>,

    /// Write deployment results to OUT as a .env file.
    ///
    /// If not provided, the results will be written to stdout.
    #[clap(short, long, name = "OUT", env = "ESPRESSO_DEPLOYER_OUT_PATH")]
    out: Option<PathBuf>,

    #[clap(flatten)]
    contracts: DeployedContracts,

    /// If toggled, launch a mock prover contract with a smaller verification key.
    #[clap(short, long)]
    pub use_mock_contract: bool,

    /// Stake table capacity for the prover circuit
    #[clap(short, long, env = "ESPRESSO_SEQUENCER_STAKE_TABLE_CAPACITY", default_value_t = STAKE_TABLE_CAPACITY)]
    pub stake_table_capacity: usize,

    #[clap(flatten)]
    logging: logging::Config,
}

#[async_std::main]
async fn main() -> anyhow::Result<()> {
    let opt = Options::parse();
    opt.logging.init();

    let contracts = Contracts::from(opt.contracts);

    let sequencer_url = opt.sequencer_url.clone();

    let genesis = light_client_genesis(&sequencer_url, opt.stake_table_capacity).boxed();

    let contracts = deploy(
        opt.rpc_url,
        opt.mnemonic,
        opt.account_index,
        opt.use_mock_contract,
        opt.only,
        genesis,
        contracts,
    )
    .await?;

    if let Some(out) = &opt.out {
        let file = File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .open(out)?;
        contracts.write(file)?;
    } else {
        contracts.write(stdout())?;
    }

    Ok(())
}
