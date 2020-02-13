use crate::{
    cluster_query::*,
    display::{println_name_value, println_signers},
    nonce::{self, *},
    offline::*,
    stake::*,
    storage::*,
    validator_info::*,
    vote::*,
};
use chrono::prelude::*;
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};
use log::*;
use num_traits::FromPrimitive;
use serde_json::{self, json, Value};
use solana_budget_program::budget_instruction::{self, BudgetError};
use solana_clap_utils::{input_parsers::*, input_validators::*, ArgConstant};
use solana_client::{client_error::ClientError, rpc_client::RpcClient};
#[cfg(not(test))]
use solana_faucet::faucet::request_airdrop_transaction;
#[cfg(test)]
use solana_faucet::faucet_mock::request_airdrop_transaction;
use solana_remote_wallet::{
    ledger::get_ledger_from_info,
    remote_wallet::{DerivationPath, RemoteWallet, RemoteWalletInfo},
};
use solana_sdk::{
    bpf_loader,
    clock::{Epoch, Slot},
    commitment_config::CommitmentConfig,
    fee_calculator::FeeCalculator,
    hash::Hash,
    instruction::InstructionError,
    instruction_processor_utils::DecodeError,
    loader_instruction,
    message::Message,
    native_token::lamports_to_sol,
    pubkey::Pubkey,
    signature::{keypair_from_seed, Keypair, KeypairUtil, Signature},
    system_instruction::{self, create_address_with_seed, SystemError, MAX_ADDRESS_SEED_LEN},
    system_transaction,
    transaction::{Transaction, TransactionError},
};
use solana_stake_program::stake_state::{Lockup, StakeAuthorize};
use solana_storage_program::storage_instruction::StorageAccountType;
use solana_vote_program::vote_state::VoteAuthorize;
use std::{
    fs::File,
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    thread::sleep,
    time::Duration,
    {error, fmt},
};

const USERDATA_CHUNK_SIZE: usize = 229; // Keep program chunks under PACKET_DATA_SIZE

pub const FEE_PAYER_ARG: ArgConstant<'static> = ArgConstant {
    name: "fee_payer",
    long: "fee-payer",
    help: "Specify the fee-payer account. This may be a keypair file, the ASK keyword \n\
           or the pubkey of an offline signer, provided an appropriate --signer argument \n\
           is also passed. Defaults to the client keypair.",
};

pub fn fee_payer_arg<'a, 'b>() -> Arg<'a, 'b> {
    Arg::with_name(FEE_PAYER_ARG.name)
        .long(FEE_PAYER_ARG.long)
        .takes_value(true)
        .value_name("KEYPAIR or PUBKEY")
        .validator(is_pubkey_or_keypair_or_ask_keyword)
        .help(FEE_PAYER_ARG.help)
}

#[derive(Debug)]
pub struct KeypairEq(Keypair);

impl From<Keypair> for KeypairEq {
    fn from(keypair: Keypair) -> Self {
        Self(keypair)
    }
}

impl PartialEq for KeypairEq {
    fn eq(&self, other: &Self) -> bool {
        self.pubkey() == other.pubkey()
    }
}

impl std::ops::Deref for KeypairEq {
    type Target = Keypair;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub enum SigningAuthority {
    Online(Keypair),
    // We hold a random keypair alongside our legit pubkey in order
    // to generate a placeholder signature in the transaction
    Offline(Pubkey, Keypair),
}

impl SigningAuthority {
    pub fn new_from_matches(
        matches: &ArgMatches<'_>,
        name: &str,
        signers: Option<&[(Pubkey, Signature)]>,
    ) -> Result<Option<Self>, CliError> {
        if matches.is_present(name) {
            keypair_of(matches, name)
                .map(|keypair| keypair.into())
                .or_else(|| {
                    pubkey_of(matches, name)
                        .filter(|pubkey| {
                            signers
                                .and_then(|signers| {
                                    signers.iter().find(|(signer, _sig)| *signer == *pubkey)
                                })
                                .is_some()
                        })
                        .map(|pubkey| pubkey.into())
                })
                .ok_or_else(|| CliError::BadParameter("Invalid authority".to_string()))
                .map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn keypair(&self) -> &Keypair {
        match self {
            SigningAuthority::Online(keypair) => keypair,
            SigningAuthority::Offline(_pubkey, keypair) => keypair,
        }
    }

    pub fn pubkey(&self) -> Pubkey {
        match self {
            SigningAuthority::Online(keypair) => keypair.pubkey(),
            SigningAuthority::Offline(pubkey, _keypair) => *pubkey,
        }
    }
}

impl From<Keypair> for SigningAuthority {
    fn from(keypair: Keypair) -> Self {
        SigningAuthority::Online(keypair)
    }
}

impl From<Pubkey> for SigningAuthority {
    fn from(pubkey: Pubkey) -> Self {
        SigningAuthority::Offline(pubkey, keypair_from_seed(pubkey.as_ref()).unwrap())
    }
}

impl PartialEq for SigningAuthority {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SigningAuthority::Online(keypair1), SigningAuthority::Online(keypair2)) => {
                keypair1.pubkey() == keypair2.pubkey()
            }
            (SigningAuthority::Online(keypair), SigningAuthority::Offline(pubkey, _))
            | (SigningAuthority::Offline(pubkey, _), SigningAuthority::Online(keypair)) => {
                keypair.pubkey() == *pubkey
            }
            (SigningAuthority::Offline(pubkey1, _), SigningAuthority::Offline(pubkey2, _)) => {
                pubkey1 == pubkey2
            }
        }
    }
}

pub fn nonce_authority_arg<'a, 'b>() -> Arg<'a, 'b> {
    nonce::nonce_authority_arg().requires(NONCE_ARG.name)
}

#[derive(Default, Debug, PartialEq)]
pub struct PayCommand {
    pub lamports: u64,
    pub to: Pubkey,
    pub timestamp: Option<DateTime<Utc>>,
    pub timestamp_pubkey: Option<Pubkey>,
    pub witnesses: Option<Vec<Pubkey>>,
    pub cancelable: bool,
    pub sign_only: bool,
    pub signers: Option<Vec<(Pubkey, Signature)>>,
    pub blockhash_query: BlockhashQuery,
    pub nonce_account: Option<Pubkey>,
    pub nonce_authority: Option<SigningAuthority>,
}

#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum CliCommand {
    // Cluster Query Commands
    Catchup {
        node_pubkey: Pubkey,
    },
    ClusterVersion,
    CreateAddressWithSeed {
        from_pubkey: Option<Pubkey>,
        seed: String,
        program_id: Pubkey,
    },
    Fees,
    GetBlockTime {
        slot: Slot,
    },
    GetEpochInfo {
        commitment_config: CommitmentConfig,
    },
    GetGenesisHash,
    GetSlot {
        commitment_config: CommitmentConfig,
    },
    GetTransactionCount {
        commitment_config: CommitmentConfig,
    },
    LeaderSchedule,
    Ping {
        lamports: u64,
        interval: Duration,
        count: Option<u64>,
        timeout: Duration,
        commitment_config: CommitmentConfig,
    },
    ShowBlockProduction {
        epoch: Option<Epoch>,
        slot_limit: Option<u64>,
    },
    ShowGossip,
    ShowStakes {
        use_lamports_unit: bool,
        vote_account_pubkeys: Option<Vec<Pubkey>>,
    },
    ShowValidators {
        use_lamports_unit: bool,
    },
    // Nonce commands
    AuthorizeNonceAccount {
        nonce_account: Pubkey,
        nonce_authority: Option<SigningAuthority>,
        new_authority: Pubkey,
    },
    CreateNonceAccount {
        nonce_account: KeypairEq,
        seed: Option<String>,
        nonce_authority: Option<Pubkey>,
        lamports: u64,
    },
    GetNonce(Pubkey),
    NewNonce {
        nonce_account: Pubkey,
        nonce_authority: Option<SigningAuthority>,
    },
    ShowNonceAccount {
        nonce_account_pubkey: Pubkey,
        use_lamports_unit: bool,
    },
    WithdrawFromNonceAccount {
        nonce_account: Pubkey,
        nonce_authority: Option<SigningAuthority>,
        destination_account_pubkey: Pubkey,
        lamports: u64,
    },
    // Program Deployment
    Deploy(String),
    // Stake Commands
    CreateStakeAccount {
        stake_account: SigningAuthority,
        seed: Option<String>,
        staker: Option<Pubkey>,
        withdrawer: Option<Pubkey>,
        lockup: Lockup,
        lamports: u64,
        sign_only: bool,
        signers: Option<Vec<(Pubkey, Signature)>>,
        blockhash_query: BlockhashQuery,
        nonce_account: Option<Pubkey>,
        nonce_authority: Option<SigningAuthority>,
        fee_payer: Option<SigningAuthority>,
        from: Option<SigningAuthority>,
    },
    DeactivateStake {
        stake_account_pubkey: Pubkey,
        stake_authority: Option<SigningAuthority>,
        sign_only: bool,
        signers: Option<Vec<(Pubkey, Signature)>>,
        blockhash_query: BlockhashQuery,
        nonce_account: Option<Pubkey>,
        nonce_authority: Option<SigningAuthority>,
        fee_payer: Option<SigningAuthority>,
    },
    DelegateStake {
        stake_account_pubkey: Pubkey,
        vote_account_pubkey: Pubkey,
        stake_authority: Option<SigningAuthority>,
        force: bool,
        sign_only: bool,
        signers: Option<Vec<(Pubkey, Signature)>>,
        blockhash_query: BlockhashQuery,
        nonce_account: Option<Pubkey>,
        nonce_authority: Option<SigningAuthority>,
        fee_payer: Option<SigningAuthority>,
    },
    SplitStake {
        stake_account_pubkey: Pubkey,
        stake_authority: Option<SigningAuthority>,
        sign_only: bool,
        signers: Option<Vec<(Pubkey, Signature)>>,
        blockhash_query: BlockhashQuery,
        nonce_account: Option<Pubkey>,
        nonce_authority: Option<SigningAuthority>,
        split_stake_account: KeypairEq,
        seed: Option<String>,
        lamports: u64,
        fee_payer: Option<SigningAuthority>,
    },
    ShowStakeHistory {
        use_lamports_unit: bool,
    },
    ShowStakeAccount {
        pubkey: Pubkey,
        use_lamports_unit: bool,
    },
    StakeAuthorize {
        stake_account_pubkey: Pubkey,
        new_authorized_pubkey: Pubkey,
        stake_authorize: StakeAuthorize,
        authority: Option<SigningAuthority>,
        sign_only: bool,
        signers: Option<Vec<(Pubkey, Signature)>>,
        blockhash_query: BlockhashQuery,
        nonce_account: Option<Pubkey>,
        nonce_authority: Option<SigningAuthority>,
        fee_payer: Option<SigningAuthority>,
    },
    StakeSetLockup {
        stake_account_pubkey: Pubkey,
        lockup: Lockup,
        custodian: Option<SigningAuthority>,
        sign_only: bool,
        signers: Option<Vec<(Pubkey, Signature)>>,
        blockhash_query: BlockhashQuery,
        nonce_account: Option<Pubkey>,
        nonce_authority: Option<SigningAuthority>,
        fee_payer: Option<SigningAuthority>,
    },
    WithdrawStake {
        stake_account_pubkey: Pubkey,
        destination_account_pubkey: Pubkey,
        lamports: u64,
        withdraw_authority: Option<SigningAuthority>,
        sign_only: bool,
        signers: Option<Vec<(Pubkey, Signature)>>,
        blockhash_query: BlockhashQuery,
        nonce_account: Option<Pubkey>,
        nonce_authority: Option<SigningAuthority>,
        fee_payer: Option<SigningAuthority>,
    },
    // Storage Commands
    CreateStorageAccount {
        account_owner: Pubkey,
        storage_account: KeypairEq,
        account_type: StorageAccountType,
    },
    ClaimStorageReward {
        node_account_pubkey: Pubkey,
        storage_account_pubkey: Pubkey,
    },
    ShowStorageAccount(Pubkey),
    // Validator Info Commands
    GetValidatorInfo(Option<Pubkey>),
    SetValidatorInfo {
        validator_info: Value,
        force_keybase: bool,
        info_pubkey: Option<Pubkey>,
    },
    // Vote Commands
    CreateVoteAccount {
        vote_account: KeypairEq,
        seed: Option<String>,
        node_pubkey: Pubkey,
        authorized_voter: Option<Pubkey>,
        authorized_withdrawer: Option<Pubkey>,
        commission: u8,
    },
    ShowVoteAccount {
        pubkey: Pubkey,
        use_lamports_unit: bool,
    },
    VoteAuthorize {
        vote_account_pubkey: Pubkey,
        new_authorized_pubkey: Pubkey,
        vote_authorize: VoteAuthorize,
    },
    VoteUpdateValidator {
        vote_account_pubkey: Pubkey,
        new_identity_pubkey: Pubkey,
        authorized_voter: KeypairEq,
    },
    // Wallet Commands
    Address,
    Airdrop {
        faucet_host: Option<IpAddr>,
        faucet_port: u16,
        lamports: u64,
        use_lamports_unit: bool,
    },
    Balance {
        pubkey: Option<Pubkey>,
        use_lamports_unit: bool,
    },
    Cancel(Pubkey),
    Confirm(Signature),
    Pay(PayCommand),
    ShowAccount {
        pubkey: Pubkey,
        output_file: Option<String>,
        use_lamports_unit: bool,
    },
    TimeElapsed(Pubkey, Pubkey, DateTime<Utc>), // TimeElapsed(to, process_id, timestamp)
    Transfer {
        lamports: u64,
        to: Pubkey,
        from: Option<SigningAuthority>,
        sign_only: bool,
        signers: Option<Vec<(Pubkey, Signature)>>,
        blockhash_query: BlockhashQuery,
        nonce_account: Option<Pubkey>,
        nonce_authority: Option<SigningAuthority>,
        fee_payer: Option<SigningAuthority>,
    },
    Witness(Pubkey, Pubkey), // Witness(to, process_id)
}

#[derive(Debug, PartialEq)]
pub struct CliCommandInfo {
    pub command: CliCommand,
    pub require_keypair: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CliError {
    BadParameter(String),
    CommandNotRecognized(String),
    InsufficientFundsForFee,
    InvalidNonce(CliNonceError),
    DynamicProgramError(String),
    RpcRequestError(String),
    KeypairFileNotFound(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid")
    }
}

impl error::Error for CliError {
    fn description(&self) -> &str {
        "invalid"
    }

    fn cause(&self) -> Option<&dyn error::Error> {
        // Generic error, underlying cause isn't tracked.
        None
    }
}

pub struct CliConfig {
    pub command: CliCommand,
    pub json_rpc_url: String,
    pub keypair: Keypair,
    pub keypair_path: Option<String>,
    pub derivation_path: Option<DerivationPath>,
    pub rpc_client: Option<RpcClient>,
    pub verbose: bool,
}

impl CliConfig {
    pub fn default_keypair_path() -> String {
        let mut keypair_path = dirs::home_dir().expect("home directory");
        keypair_path.extend(&[".config", "solana", "id.json"]);
        keypair_path.to_str().unwrap().to_string()
    }

    pub fn default_json_rpc_url() -> String {
        "http://127.0.0.1:8899".to_string()
    }

    pub(crate) fn pubkey(&self) -> Result<Pubkey, Box<dyn std::error::Error>> {
        if let Some(path) = &self.keypair_path {
            if path.starts_with("usb://") {
                let (remote_wallet_info, mut derivation_path) =
                    RemoteWalletInfo::parse_path(path.to_string())?;
                if let Some(derivation) = &self.derivation_path {
                    let derivation = derivation.clone();
                    derivation_path = derivation;
                }
                let ledger = get_ledger_from_info(remote_wallet_info)?;
                return Ok(ledger.get_pubkey(&derivation_path)?);
            }
        }
        Ok(self.keypair.pubkey())
    }
}

impl Default for CliConfig {
    fn default() -> CliConfig {
        CliConfig {
            command: CliCommand::Balance {
                pubkey: Some(Pubkey::default()),
                use_lamports_unit: false,
            },
            json_rpc_url: Self::default_json_rpc_url(),
            keypair: Keypair::new(),
            keypair_path: Some(Self::default_keypair_path()),
            derivation_path: None,
            rpc_client: None,
            verbose: false,
        }
    }
}

pub fn parse_command(matches: &ArgMatches<'_>) -> Result<CliCommandInfo, Box<dyn error::Error>> {
    let response = match matches.subcommand() {
        // Cluster Query Commands
        ("catchup", Some(matches)) => parse_catchup(matches),
        ("cluster-version", Some(_matches)) => Ok(CliCommandInfo {
            command: CliCommand::ClusterVersion,
            require_keypair: false,
        }),
        ("create-address-with-seed", Some(matches)) => parse_create_address_with_seed(matches),
        ("fees", Some(_matches)) => Ok(CliCommandInfo {
            command: CliCommand::Fees,
            require_keypair: false,
        }),
        ("block-time", Some(matches)) => parse_get_block_time(matches),
        ("epoch-info", Some(matches)) => parse_get_epoch_info(matches),
        ("genesis-hash", Some(_matches)) => Ok(CliCommandInfo {
            command: CliCommand::GetGenesisHash,
            require_keypair: false,
        }),
        ("slot", Some(matches)) => parse_get_slot(matches),
        ("transaction-count", Some(matches)) => parse_get_transaction_count(matches),
        ("leader-schedule", Some(_matches)) => Ok(CliCommandInfo {
            command: CliCommand::LeaderSchedule,
            require_keypair: false,
        }),
        ("ping", Some(matches)) => parse_cluster_ping(matches),
        ("block-production", Some(matches)) => parse_show_block_production(matches),
        ("gossip", Some(_matches)) => Ok(CliCommandInfo {
            command: CliCommand::ShowGossip,
            require_keypair: false,
        }),
        ("stakes", Some(matches)) => parse_show_stakes(matches),
        ("validators", Some(matches)) => parse_show_validators(matches),
        // Nonce Commands
        ("authorize-nonce-account", Some(matches)) => parse_authorize_nonce_account(matches),
        ("create-nonce-account", Some(matches)) => parse_nonce_create_account(matches),
        ("nonce", Some(matches)) => parse_get_nonce(matches),
        ("new-nonce", Some(matches)) => parse_new_nonce(matches),
        ("nonce-account", Some(matches)) => parse_show_nonce_account(matches),
        ("withdraw-from-nonce-account", Some(matches)) => {
            parse_withdraw_from_nonce_account(matches)
        }
        // Program Deployment
        ("deploy", Some(matches)) => Ok(CliCommandInfo {
            command: CliCommand::Deploy(matches.value_of("program_location").unwrap().to_string()),
            require_keypair: true,
        }),
        // Stake Commands
        ("create-stake-account", Some(matches)) => parse_stake_create_account(matches),
        ("delegate-stake", Some(matches)) => parse_stake_delegate_stake(matches),
        ("withdraw-stake", Some(matches)) => parse_stake_withdraw_stake(matches),
        ("deactivate-stake", Some(matches)) => parse_stake_deactivate_stake(matches),
        ("split-stake", Some(matches)) => parse_split_stake(matches),
        ("stake-authorize-staker", Some(matches)) => {
            parse_stake_authorize(matches, StakeAuthorize::Staker)
        }
        ("stake-authorize-withdrawer", Some(matches)) => {
            parse_stake_authorize(matches, StakeAuthorize::Withdrawer)
        }
        ("stake-set-lockup", Some(matches)) => parse_stake_set_lockup(matches),
        ("stake-account", Some(matches)) => parse_show_stake_account(matches),
        ("stake-history", Some(matches)) => parse_show_stake_history(matches),
        // Storage Commands
        ("create-archiver-storage-account", Some(matches)) => {
            parse_storage_create_archiver_account(matches)
        }
        ("create-validator-storage-account", Some(matches)) => {
            parse_storage_create_validator_account(matches)
        }
        ("claim-storage-reward", Some(matches)) => parse_storage_claim_reward(matches),
        ("storage-account", Some(matches)) => parse_storage_get_account_command(matches),
        // Validator Info Commands
        ("validator-info", Some(matches)) => match matches.subcommand() {
            ("publish", Some(matches)) => parse_validator_info_command(matches),
            ("get", Some(matches)) => parse_get_validator_info_command(matches),
            _ => unreachable!(),
        },
        // Vote Commands
        ("create-vote-account", Some(matches)) => parse_vote_create_account(matches),
        ("vote-update-validator", Some(matches)) => parse_vote_update_validator(matches),
        ("vote-authorize-voter", Some(matches)) => {
            parse_vote_authorize(matches, VoteAuthorize::Voter)
        }
        ("vote-authorize-withdrawer", Some(matches)) => {
            parse_vote_authorize(matches, VoteAuthorize::Withdrawer)
        }
        ("vote-account", Some(matches)) => parse_vote_get_account_command(matches),
        // Wallet Commands
        ("address", Some(_matches)) => Ok(CliCommandInfo {
            command: CliCommand::Address,
            require_keypair: true,
        }),
        ("airdrop", Some(matches)) => {
            let faucet_port = matches
                .value_of("faucet_port")
                .unwrap()
                .parse()
                .or_else(|err| {
                    Err(CliError::BadParameter(format!(
                        "Invalid faucet port: {:?}",
                        err
                    )))
                })?;

            let faucet_host = if let Some(faucet_host) = matches.value_of("faucet_host") {
                Some(solana_net_utils::parse_host(faucet_host).or_else(|err| {
                    Err(CliError::BadParameter(format!(
                        "Invalid faucet host: {:?}",
                        err
                    )))
                })?)
            } else {
                None
            };
            let lamports = required_lamports_from(matches, "amount", "unit")?;
            let use_lamports_unit = matches.value_of("unit") == Some("lamports");
            Ok(CliCommandInfo {
                command: CliCommand::Airdrop {
                    faucet_host,
                    faucet_port,
                    lamports,
                    use_lamports_unit,
                },
                require_keypair: true,
            })
        }
        ("balance", Some(matches)) => {
            let pubkey = pubkey_of(&matches, "pubkey");
            Ok(CliCommandInfo {
                command: CliCommand::Balance {
                    pubkey,
                    use_lamports_unit: matches.is_present("lamports"),
                },
                require_keypair: pubkey.is_none(),
            })
        }
        ("cancel", Some(matches)) => {
            let process_id = value_of(matches, "process_id").unwrap();
            Ok(CliCommandInfo {
                command: CliCommand::Cancel(process_id),
                require_keypair: true,
            })
        }
        ("confirm", Some(matches)) => match matches.value_of("signature").unwrap().parse() {
            Ok(signature) => Ok(CliCommandInfo {
                command: CliCommand::Confirm(signature),
                require_keypair: false,
            }),
            _ => {
                eprintln!("{}", matches.usage());
                Err(CliError::BadParameter("Invalid signature".to_string()))
            }
        },
        ("pay", Some(matches)) => {
            let lamports = required_lamports_from(matches, "amount", "unit")?;
            let to = pubkey_of(&matches, "to").unwrap();
            let timestamp = if matches.is_present("timestamp") {
                // Parse input for serde_json
                let date_string = if !matches.value_of("timestamp").unwrap().contains('Z') {
                    format!("\"{}Z\"", matches.value_of("timestamp").unwrap())
                } else {
                    format!("\"{}\"", matches.value_of("timestamp").unwrap())
                };
                Some(serde_json::from_str(&date_string)?)
            } else {
                None
            };
            let timestamp_pubkey = value_of(&matches, "timestamp_pubkey");
            let witnesses = values_of(&matches, "witness");
            let cancelable = matches.is_present("cancelable");
            let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
            let signers = pubkeys_sigs_of(&matches, SIGNER_ARG.name);
            let blockhash_query = BlockhashQuery::new_from_matches(&matches);
            let nonce_account = pubkey_of(&matches, NONCE_ARG.name);
            let nonce_authority = SigningAuthority::new_from_matches(
                &matches,
                NONCE_AUTHORITY_ARG.name,
                signers.as_deref(),
            )?;

            Ok(CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports,
                    to,
                    timestamp,
                    timestamp_pubkey,
                    witnesses,
                    cancelable,
                    sign_only,
                    signers,
                    blockhash_query,
                    nonce_account,
                    nonce_authority,
                }),
                require_keypair: true,
            })
        }
        ("account", Some(matches)) => {
            let account_pubkey = pubkey_of(matches, "account_pubkey").unwrap();
            let output_file = matches.value_of("output_file");
            let use_lamports_unit = matches.is_present("lamports");
            Ok(CliCommandInfo {
                command: CliCommand::ShowAccount {
                    pubkey: account_pubkey,
                    output_file: output_file.map(ToString::to_string),
                    use_lamports_unit,
                },
                require_keypair: false,
            })
        }
        ("send-signature", Some(matches)) => {
            let to = value_of(&matches, "to").unwrap();
            let process_id = value_of(&matches, "process_id").unwrap();
            Ok(CliCommandInfo {
                command: CliCommand::Witness(to, process_id),
                require_keypair: true,
            })
        }
        ("send-timestamp", Some(matches)) => {
            let to = value_of(&matches, "to").unwrap();
            let process_id = value_of(&matches, "process_id").unwrap();
            let dt = if matches.is_present("datetime") {
                // Parse input for serde_json
                let date_string = if !matches.value_of("datetime").unwrap().contains('Z') {
                    format!("\"{}Z\"", matches.value_of("datetime").unwrap())
                } else {
                    format!("\"{}\"", matches.value_of("datetime").unwrap())
                };
                serde_json::from_str(&date_string)?
            } else {
                Utc::now()
            };
            Ok(CliCommandInfo {
                command: CliCommand::TimeElapsed(to, process_id, dt),
                require_keypair: true,
            })
        }
        ("transfer", Some(matches)) => {
            let lamports = required_lamports_from(matches, "amount", "unit")?;
            let to = pubkey_of(&matches, "to").unwrap();
            let sign_only = matches.is_present(SIGN_ONLY_ARG.name);
            let signers = pubkeys_sigs_of(&matches, SIGNER_ARG.name);
            let blockhash_query = BlockhashQuery::new_from_matches(matches);
            let nonce_account = pubkey_of(&matches, NONCE_ARG.name);
            let nonce_authority = SigningAuthority::new_from_matches(
                &matches,
                NONCE_AUTHORITY_ARG.name,
                signers.as_deref(),
            )?;
            let fee_payer = SigningAuthority::new_from_matches(
                &matches,
                FEE_PAYER_ARG.name,
                signers.as_deref(),
            )?;
            let from = SigningAuthority::new_from_matches(&matches, "from", signers.as_deref())?;
            Ok(CliCommandInfo {
                command: CliCommand::Transfer {
                    lamports,
                    to,
                    from,
                    sign_only,
                    signers,
                    blockhash_query,
                    nonce_account,
                    nonce_authority,
                    fee_payer,
                },
                require_keypair: true,
            })
        }
        //
        ("", None) => {
            eprintln!("{}", matches.usage());
            Err(CliError::CommandNotRecognized(
                "no subcommand given".to_string(),
            ))
        }
        _ => unreachable!(),
    }?;
    Ok(response)
}

pub type ProcessResult = Result<String, Box<dyn std::error::Error>>;

pub fn check_account_for_fee(
    rpc_client: &RpcClient,
    account_pubkey: &Pubkey,
    fee_calculator: &FeeCalculator,
    message: &Message,
) -> Result<(), Box<dyn error::Error>> {
    check_account_for_multiple_fees(rpc_client, account_pubkey, fee_calculator, &[message])
}

fn check_account_for_multiple_fees(
    rpc_client: &RpcClient,
    account_pubkey: &Pubkey,
    fee_calculator: &FeeCalculator,
    messages: &[&Message],
) -> Result<(), Box<dyn error::Error>> {
    let balance = rpc_client.retry_get_balance(account_pubkey, 5)?;
    if let Some(lamports) = balance {
        let fee = messages
            .iter()
            .map(|message| fee_calculator.calculate_fee(message))
            .sum();
        if lamports != 0 && lamports >= fee {
            return Ok(());
        }
    }
    Err(CliError::InsufficientFundsForFee.into())
}

pub fn check_unique_pubkeys(
    pubkey0: (&Pubkey, String),
    pubkey1: (&Pubkey, String),
) -> Result<(), CliError> {
    if pubkey0.0 == pubkey1.0 {
        Err(CliError::BadParameter(format!(
            "Identical pubkeys found: `{}` and `{}` must be unique",
            pubkey0.1, pubkey1.1
        )))
    } else {
        Ok(())
    }
}

pub fn get_blockhash_fee_calculator(
    rpc_client: &RpcClient,
    sign_only: bool,
    blockhash: Option<Hash>,
) -> Result<(Hash, FeeCalculator), Box<dyn std::error::Error>> {
    Ok(if let Some(blockhash) = blockhash {
        if sign_only {
            (blockhash, FeeCalculator::default())
        } else {
            (blockhash, rpc_client.get_recent_blockhash()?.1)
        }
    } else {
        rpc_client.get_recent_blockhash()?
    })
}

pub fn return_signers(tx: &Transaction) -> ProcessResult {
    println_signers(tx);
    let signers: Vec<_> = tx
        .signatures
        .iter()
        .zip(tx.message.account_keys.clone())
        .map(|(signature, pubkey)| format!("{}={}", pubkey, signature))
        .collect();

    Ok(json!({
        "blockhash": tx.message.recent_blockhash.to_string(),
        "signers": &signers,
    })
    .to_string())
}

pub fn replace_signatures(tx: &mut Transaction, signers: &[(Pubkey, Signature)]) -> ProcessResult {
    tx.replace_signatures(signers).map_err(|_| {
        CliError::BadParameter(
            "Transaction construction failed, incorrect signature or public key provided"
                .to_string(),
        )
    })?;
    Ok("".to_string())
}

pub fn parse_create_address_with_seed(
    matches: &ArgMatches<'_>,
) -> Result<CliCommandInfo, CliError> {
    let from_pubkey = pubkey_of(matches, "from");

    let require_keypair = from_pubkey.is_none();

    let program_id = match matches.value_of("program_id").unwrap() {
        "STAKE" => solana_stake_program::id(),
        "VOTE" => solana_vote_program::id(),
        "STORAGE" => solana_storage_program::id(),
        _ => pubkey_of(matches, "program_id").unwrap(),
    };

    let seed = matches.value_of("seed").unwrap().to_string();

    if seed.len() > MAX_ADDRESS_SEED_LEN {
        return Err(CliError::BadParameter(
            "Address seed must not be longer than 32 bytes".to_string(),
        ));
    }

    Ok(CliCommandInfo {
        command: CliCommand::CreateAddressWithSeed {
            from_pubkey,
            seed,
            program_id,
        },
        require_keypair,
    })
}

fn process_create_address_with_seed(
    config: &CliConfig,
    from_pubkey: Option<&Pubkey>,
    seed: &str,
    program_id: &Pubkey,
) -> ProcessResult {
    let config_pubkey = config.pubkey()?;
    let from_pubkey = from_pubkey.unwrap_or(&config_pubkey);
    let address = create_address_with_seed(from_pubkey, seed, program_id)?;
    Ok(address.to_string())
}

fn process_airdrop(
    rpc_client: &RpcClient,
    config: &CliConfig,
    faucet_addr: &SocketAddr,
    lamports: u64,
    use_lamports_unit: bool,
) -> ProcessResult {
    let pubkey = config.pubkey()?;
    println!(
        "Requesting airdrop of {} from {}",
        build_balance_message(lamports, use_lamports_unit, true),
        faucet_addr
    );
    let previous_balance = match rpc_client.retry_get_balance(&pubkey, 5)? {
        Some(lamports) => lamports,
        None => {
            return Err(CliError::RpcRequestError(
                "Received result of an unexpected type".to_string(),
            )
            .into())
        }
    };

    request_and_confirm_airdrop(&rpc_client, faucet_addr, &pubkey, lamports)?;

    let current_balance = rpc_client
        .retry_get_balance(&pubkey, 5)?
        .unwrap_or(previous_balance);

    Ok(build_balance_message(
        current_balance,
        use_lamports_unit,
        true,
    ))
}

fn process_balance(
    rpc_client: &RpcClient,
    config: &CliConfig,
    pubkey: &Option<Pubkey>,
    use_lamports_unit: bool,
) -> ProcessResult {
    let pubkey = pubkey.unwrap_or(config.pubkey()?);
    let balance = rpc_client.retry_get_balance(&pubkey, 5)?;
    match balance {
        Some(lamports) => Ok(build_balance_message(lamports, use_lamports_unit, true)),
        None => Err(
            CliError::RpcRequestError("Received result of an unexpected type".to_string()).into(),
        ),
    }
}

fn process_confirm(rpc_client: &RpcClient, signature: &Signature) -> ProcessResult {
    match rpc_client.get_signature_status(&signature.to_string()) {
        Ok(status) => {
            if let Some(result) = status {
                match result {
                    Ok(_) => Ok("Confirmed".to_string()),
                    Err(err) => Ok(format!("Transaction failed with error {:?}", err)),
                }
            } else {
                Ok("Not found".to_string())
            }
        }
        Err(err) => Err(CliError::RpcRequestError(format!("Unable to confirm: {:?}", err)).into()),
    }
}

fn process_show_account(
    rpc_client: &RpcClient,
    _config: &CliConfig,
    account_pubkey: &Pubkey,
    output_file: &Option<String>,
    use_lamports_unit: bool,
) -> ProcessResult {
    let account = rpc_client.get_account(account_pubkey)?;

    println!();
    println_name_value("Public Key:", &account_pubkey.to_string());
    println_name_value(
        "Balance:",
        &build_balance_message(account.lamports, use_lamports_unit, true),
    );
    println_name_value("Owner:", &account.owner.to_string());
    println_name_value("Executable:", &account.executable.to_string());

    if let Some(output_file) = output_file {
        let mut f = File::create(output_file)?;
        f.write_all(&account.data)?;
        println!();
        println!("Wrote account data to {}", output_file);
    } else if !account.data.is_empty() {
        use pretty_hex::*;
        println!("{:?}", account.data.hex_dump());
    }

    Ok("".to_string())
}

fn process_deploy(
    rpc_client: &RpcClient,
    config: &CliConfig,
    program_location: &str,
) -> ProcessResult {
    let program_id = Keypair::new();
    let mut file = File::open(program_location).map_err(|err| {
        CliError::DynamicProgramError(format!("Unable to open program file: {}", err))
    })?;
    let mut program_data = Vec::new();
    file.read_to_end(&mut program_data).map_err(|err| {
        CliError::DynamicProgramError(format!("Unable to read program file: {}", err))
    })?;

    // Build transactions to calculate fees
    let mut messages: Vec<&Message> = Vec::new();
    let (blockhash, fee_calculator) = rpc_client.get_recent_blockhash()?;
    let minimum_balance = rpc_client.get_minimum_balance_for_rent_exemption(program_data.len())?;
    let mut create_account_tx = system_transaction::create_account(
        &config.keypair,
        &program_id,
        blockhash,
        minimum_balance.max(1),
        program_data.len() as u64,
        &bpf_loader::id(),
    );
    messages.push(&create_account_tx.message);
    let signers = [&config.keypair, &program_id];
    let write_transactions: Vec<_> = program_data
        .chunks(USERDATA_CHUNK_SIZE)
        .zip(0..)
        .map(|(chunk, i)| {
            let instruction = loader_instruction::write(
                &program_id.pubkey(),
                &bpf_loader::id(),
                (i * USERDATA_CHUNK_SIZE) as u32,
                chunk.to_vec(),
            );
            let message = Message::new_with_payer(vec![instruction], Some(&signers[0].pubkey()));
            Transaction::new(&signers, message, blockhash)
        })
        .collect();
    for transaction in write_transactions.iter() {
        messages.push(&transaction.message);
    }

    let instruction = loader_instruction::finalize(&program_id.pubkey(), &bpf_loader::id());
    let message = Message::new_with_payer(vec![instruction], Some(&signers[0].pubkey()));
    let mut finalize_tx = Transaction::new(&signers, message, blockhash);
    messages.push(&finalize_tx.message);

    check_account_for_multiple_fees(
        rpc_client,
        &config.keypair.pubkey(),
        &fee_calculator,
        &messages,
    )?;

    trace!("Creating program account");
    let result = rpc_client.send_and_confirm_transaction(&mut create_account_tx, &signers);
    log_instruction_custom_error::<SystemError>(result)
        .map_err(|_| CliError::DynamicProgramError("Program allocate space failed".to_string()))?;

    trace!("Writing program data");
    rpc_client.send_and_confirm_transactions(write_transactions, &signers)?;

    trace!("Finalizing program account");
    rpc_client
        .send_and_confirm_transaction(&mut finalize_tx, &signers)
        .map_err(|_| {
            CliError::DynamicProgramError("Program finalize transaction failed".to_string())
        })?;

    Ok(json!({
        "programId": format!("{}", program_id.pubkey()),
    })
    .to_string())
}

#[allow(clippy::too_many_arguments)]
fn process_pay(
    rpc_client: &RpcClient,
    config: &CliConfig,
    lamports: u64,
    to: &Pubkey,
    timestamp: Option<DateTime<Utc>>,
    timestamp_pubkey: Option<Pubkey>,
    witnesses: &Option<Vec<Pubkey>>,
    cancelable: bool,
    sign_only: bool,
    signers: &Option<Vec<(Pubkey, Signature)>>,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<Pubkey>,
    nonce_authority: Option<&SigningAuthority>,
) -> ProcessResult {
    check_unique_pubkeys(
        (&config.keypair.pubkey(), "cli keypair".to_string()),
        (to, "to".to_string()),
    )?;

    let (blockhash, fee_calculator) = blockhash_query.get_blockhash_fee_calculator(rpc_client)?;

    let cancelable = if cancelable {
        Some(config.keypair.pubkey())
    } else {
        None
    };

    if timestamp == None && *witnesses == None {
        let mut tx = if let Some(nonce_account) = &nonce_account {
            let nonce_authority: &Keypair = nonce_authority
                .map(|authority| authority.keypair())
                .unwrap_or(&config.keypair);
            system_transaction::nonced_transfer(
                &config.keypair,
                to,
                lamports,
                nonce_account,
                nonce_authority,
                blockhash,
            )
        } else {
            system_transaction::transfer(&config.keypair, to, lamports, blockhash)
        };

        if let Some(signers) = signers {
            replace_signatures(&mut tx, &signers)?;
        }

        if sign_only {
            return_signers(&tx)
        } else {
            if let Some(nonce_account) = &nonce_account {
                let nonce_authority: Pubkey = nonce_authority
                    .map(|authority| authority.pubkey())
                    .unwrap_or_else(|| config.keypair.pubkey());
                let nonce_account = rpc_client.get_account(nonce_account)?;
                check_nonce_account(&nonce_account, &nonce_authority, &blockhash)?;
            }
            check_account_for_fee(
                rpc_client,
                &config.keypair.pubkey(),
                &fee_calculator,
                &tx.message,
            )?;
            let result = rpc_client.send_and_confirm_transaction(&mut tx, &[&config.keypair]);
            log_instruction_custom_error::<SystemError>(result)
        }
    } else if *witnesses == None {
        let dt = timestamp.unwrap();
        let dt_pubkey = match timestamp_pubkey {
            Some(pubkey) => pubkey,
            None => config.keypair.pubkey(),
        };

        let contract_state = Keypair::new();

        // Initializing contract
        let ixs = budget_instruction::on_date(
            &config.keypair.pubkey(),
            to,
            &contract_state.pubkey(),
            dt,
            &dt_pubkey,
            cancelable,
            lamports,
        );
        let mut tx = Transaction::new_signed_instructions(
            &[&config.keypair, &contract_state],
            ixs,
            blockhash,
        );
        if let Some(signers) = signers {
            replace_signatures(&mut tx, &signers)?;
        }
        if sign_only {
            return_signers(&tx)
        } else {
            check_account_for_fee(
                rpc_client,
                &config.keypair.pubkey(),
                &fee_calculator,
                &tx.message,
            )?;
            let result = rpc_client
                .send_and_confirm_transaction(&mut tx, &[&config.keypair, &contract_state]);
            let signature_str = log_instruction_custom_error::<BudgetError>(result)?;

            Ok(json!({
                "signature": signature_str,
                "processId": format!("{}", contract_state.pubkey()),
            })
            .to_string())
        }
    } else if timestamp == None {
        let witness = if let Some(ref witness_vec) = *witnesses {
            witness_vec[0]
        } else {
            return Err(CliError::BadParameter(
                "Could not parse required signature pubkey(s)".to_string(),
            )
            .into());
        };

        let contract_state = Keypair::new();

        // Initializing contract
        let ixs = budget_instruction::when_signed(
            &config.keypair.pubkey(),
            to,
            &contract_state.pubkey(),
            &witness,
            cancelable,
            lamports,
        );
        let mut tx = Transaction::new_signed_instructions(
            &[&config.keypair, &contract_state],
            ixs,
            blockhash,
        );
        if let Some(signers) = signers {
            replace_signatures(&mut tx, &signers)?;
        }
        if sign_only {
            return_signers(&tx)
        } else {
            let result = rpc_client
                .send_and_confirm_transaction(&mut tx, &[&config.keypair, &contract_state]);
            check_account_for_fee(
                rpc_client,
                &config.keypair.pubkey(),
                &fee_calculator,
                &tx.message,
            )?;
            let signature_str = log_instruction_custom_error::<BudgetError>(result)?;

            Ok(json!({
                "signature": signature_str,
                "processId": format!("{}", contract_state.pubkey()),
            })
            .to_string())
        }
    } else {
        Ok("Combo transactions not yet handled".to_string())
    }
}

fn process_cancel(rpc_client: &RpcClient, config: &CliConfig, pubkey: &Pubkey) -> ProcessResult {
    let (blockhash, fee_calculator) = rpc_client.get_recent_blockhash()?;
    let ix = budget_instruction::apply_signature(
        &config.keypair.pubkey(),
        pubkey,
        &config.keypair.pubkey(),
    );
    let mut tx = Transaction::new_signed_instructions(&[&config.keypair], vec![ix], blockhash);
    check_account_for_fee(
        rpc_client,
        &config.keypair.pubkey(),
        &fee_calculator,
        &tx.message,
    )?;
    let result = rpc_client.send_and_confirm_transaction(&mut tx, &[&config.keypair]);
    log_instruction_custom_error::<BudgetError>(result)
}

fn process_time_elapsed(
    rpc_client: &RpcClient,
    config: &CliConfig,
    to: &Pubkey,
    pubkey: &Pubkey,
    dt: DateTime<Utc>,
) -> ProcessResult {
    let (blockhash, fee_calculator) = rpc_client.get_recent_blockhash()?;

    let ix = budget_instruction::apply_timestamp(&config.keypair.pubkey(), pubkey, to, dt);
    let mut tx = Transaction::new_signed_instructions(&[&config.keypair], vec![ix], blockhash);
    check_account_for_fee(
        rpc_client,
        &config.keypair.pubkey(),
        &fee_calculator,
        &tx.message,
    )?;
    let result = rpc_client.send_and_confirm_transaction(&mut tx, &[&config.keypair]);
    log_instruction_custom_error::<BudgetError>(result)
}

#[allow(clippy::too_many_arguments)]
fn process_transfer(
    rpc_client: &RpcClient,
    config: &CliConfig,
    lamports: u64,
    to: &Pubkey,
    from: Option<&SigningAuthority>,
    sign_only: bool,
    signers: Option<&Vec<(Pubkey, Signature)>>,
    blockhash_query: &BlockhashQuery,
    nonce_account: Option<&Pubkey>,
    nonce_authority: Option<&SigningAuthority>,
    fee_payer: Option<&SigningAuthority>,
) -> ProcessResult {
    let (from_pubkey, from) = from
        .map(|f| (f.pubkey(), f.keypair()))
        .unwrap_or((config.keypair.pubkey(), &config.keypair));

    check_unique_pubkeys(
        (&from_pubkey, "cli keypair".to_string()),
        (to, "to".to_string()),
    )?;

    let (recent_blockhash, fee_calculator) =
        blockhash_query.get_blockhash_fee_calculator(rpc_client)?;
    let ixs = vec![system_instruction::transfer(&from.pubkey(), to, lamports)];

    let (nonce_authority_pubkey, nonce_authority) = nonce_authority
        .map(|authority| (authority.pubkey(), authority.keypair()))
        .unwrap_or((config.keypair.pubkey(), &config.keypair));
    let fee_payer = fee_payer.map(|fp| fp.keypair()).unwrap_or(&config.keypair);
    let mut tx = if let Some(nonce_account) = &nonce_account {
        Transaction::new_signed_with_nonce(
            ixs,
            Some(&fee_payer.pubkey()),
            &[fee_payer, from, nonce_authority],
            nonce_account,
            &nonce_authority.pubkey(),
            recent_blockhash,
        )
    } else {
        Transaction::new_signed_with_payer(
            ixs,
            Some(&fee_payer.pubkey()),
            &[fee_payer, from],
            recent_blockhash,
        )
    };

    if let Some(signers) = signers {
        replace_signatures(&mut tx, &signers)?;
    }

    if sign_only {
        return_signers(&tx)
    } else {
        if let Some(nonce_account) = &nonce_account {
            let nonce_account = rpc_client.get_account(nonce_account)?;
            check_nonce_account(&nonce_account, &nonce_authority_pubkey, &recent_blockhash)?;
        }
        check_account_for_fee(
            rpc_client,
            &tx.message.account_keys[0],
            &fee_calculator,
            &tx.message,
        )?;
        let result = rpc_client.send_and_confirm_transaction(&mut tx, &[&config.keypair]);
        log_instruction_custom_error::<SystemError>(result)
    }
}

fn process_witness(
    rpc_client: &RpcClient,
    config: &CliConfig,
    to: &Pubkey,
    pubkey: &Pubkey,
) -> ProcessResult {
    let (blockhash, fee_calculator) = rpc_client.get_recent_blockhash()?;

    let ix = budget_instruction::apply_signature(&config.keypair.pubkey(), pubkey, to);
    let mut tx = Transaction::new_signed_instructions(&[&config.keypair], vec![ix], blockhash);
    check_account_for_fee(
        rpc_client,
        &config.keypair.pubkey(),
        &fee_calculator,
        &tx.message,
    )?;
    let result = rpc_client.send_and_confirm_transaction(&mut tx, &[&config.keypair]);
    log_instruction_custom_error::<BudgetError>(result)
}

pub fn process_command(config: &CliConfig) -> ProcessResult {
    if config.verbose {
        println_name_value("RPC URL:", &config.json_rpc_url);
        if let Some(keypair_path) = &config.keypair_path {
            println_name_value("Keypair Path:", keypair_path);
            if keypair_path.starts_with("usb://") {
                println_name_value("Pubkey:", &format!("{:?}", config.pubkey()?));
            }
        }
    }

    let mut _rpc_client;
    let rpc_client = if config.rpc_client.is_none() {
        _rpc_client = RpcClient::new(config.json_rpc_url.to_string());
        &_rpc_client
    } else {
        // Primarily for testing
        config.rpc_client.as_ref().unwrap()
    };

    match &config.command {
        // Cluster Query Commands
        // Get address of this client
        CliCommand::Address => Ok(format!("{}", config.pubkey()?)),

        // Return software version of solana-cli and cluster entrypoint node
        CliCommand::Catchup { node_pubkey } => process_catchup(&rpc_client, node_pubkey),
        CliCommand::ClusterVersion => process_cluster_version(&rpc_client),
        CliCommand::CreateAddressWithSeed {
            from_pubkey,
            seed,
            program_id,
        } => process_create_address_with_seed(config, from_pubkey.as_ref(), &seed, &program_id),
        CliCommand::Fees => process_fees(&rpc_client),
        CliCommand::GetBlockTime { slot } => process_get_block_time(&rpc_client, *slot),
        CliCommand::GetGenesisHash => process_get_genesis_hash(&rpc_client),
        CliCommand::GetEpochInfo { commitment_config } => {
            process_get_epoch_info(&rpc_client, commitment_config)
        }
        CliCommand::GetSlot { commitment_config } => {
            process_get_slot(&rpc_client, commitment_config)
        }
        CliCommand::GetTransactionCount { commitment_config } => {
            process_get_transaction_count(&rpc_client, commitment_config)
        }
        CliCommand::LeaderSchedule => process_leader_schedule(&rpc_client),
        CliCommand::Ping {
            lamports,
            interval,
            count,
            timeout,
            commitment_config,
        } => process_ping(
            &rpc_client,
            config,
            *lamports,
            interval,
            count,
            timeout,
            commitment_config,
        ),
        CliCommand::ShowBlockProduction { epoch, slot_limit } => {
            process_show_block_production(&rpc_client, config, *epoch, *slot_limit)
        }
        CliCommand::ShowGossip => process_show_gossip(&rpc_client),
        CliCommand::ShowStakes {
            use_lamports_unit,
            vote_account_pubkeys,
        } => process_show_stakes(
            &rpc_client,
            *use_lamports_unit,
            vote_account_pubkeys.as_deref(),
        ),
        CliCommand::ShowValidators { use_lamports_unit } => {
            process_show_validators(&rpc_client, *use_lamports_unit)
        }

        // Nonce Commands

        // Assign authority to nonce account
        CliCommand::AuthorizeNonceAccount {
            nonce_account,
            ref nonce_authority,
            new_authority,
        } => process_authorize_nonce_account(
            &rpc_client,
            config,
            nonce_account,
            nonce_authority.as_ref(),
            new_authority,
        ),
        // Create nonce account
        CliCommand::CreateNonceAccount {
            nonce_account,
            seed,
            nonce_authority,
            lamports,
        } => process_create_nonce_account(
            &rpc_client,
            config,
            nonce_account,
            seed.clone(),
            *nonce_authority,
            *lamports,
        ),
        // Get the current nonce
        CliCommand::GetNonce(nonce_account_pubkey) => {
            process_get_nonce(&rpc_client, &nonce_account_pubkey)
        }
        // Get a new nonce
        CliCommand::NewNonce {
            nonce_account,
            ref nonce_authority,
        } => process_new_nonce(&rpc_client, config, nonce_account, nonce_authority.as_ref()),
        // Show the contents of a nonce account
        CliCommand::ShowNonceAccount {
            nonce_account_pubkey,
            use_lamports_unit,
        } => process_show_nonce_account(&rpc_client, &nonce_account_pubkey, *use_lamports_unit),
        // Withdraw lamports from a nonce account
        CliCommand::WithdrawFromNonceAccount {
            nonce_account,
            ref nonce_authority,
            destination_account_pubkey,
            lamports,
        } => process_withdraw_from_nonce_account(
            &rpc_client,
            config,
            &nonce_account,
            nonce_authority.as_ref(),
            &destination_account_pubkey,
            *lamports,
        ),

        // Program Deployment

        // Deploy a custom program to the chain
        CliCommand::Deploy(ref program_location) => {
            process_deploy(&rpc_client, config, program_location)
        }

        // Stake Commands

        // Create stake account
        CliCommand::CreateStakeAccount {
            ref stake_account,
            seed,
            staker,
            withdrawer,
            lockup,
            lamports,
            sign_only,
            ref signers,
            blockhash_query,
            ref nonce_account,
            ref nonce_authority,
            ref fee_payer,
            ref from,
        } => process_create_stake_account(
            &rpc_client,
            config,
            stake_account,
            seed,
            staker,
            withdrawer,
            lockup,
            *lamports,
            *sign_only,
            signers.as_ref(),
            blockhash_query,
            nonce_account.as_ref(),
            nonce_authority.as_ref(),
            fee_payer.as_ref(),
            from.as_ref(),
        ),
        CliCommand::DeactivateStake {
            stake_account_pubkey,
            ref stake_authority,
            sign_only,
            ref signers,
            blockhash_query,
            nonce_account,
            ref nonce_authority,
            ref fee_payer,
        } => process_deactivate_stake_account(
            &rpc_client,
            config,
            &stake_account_pubkey,
            stake_authority.as_ref(),
            *sign_only,
            signers,
            blockhash_query,
            *nonce_account,
            nonce_authority.as_ref(),
            fee_payer.as_ref(),
        ),
        CliCommand::DelegateStake {
            stake_account_pubkey,
            vote_account_pubkey,
            ref stake_authority,
            force,
            sign_only,
            ref signers,
            blockhash_query,
            nonce_account,
            ref nonce_authority,
            ref fee_payer,
        } => process_delegate_stake(
            &rpc_client,
            config,
            &stake_account_pubkey,
            &vote_account_pubkey,
            stake_authority.as_ref(),
            *force,
            *sign_only,
            signers,
            blockhash_query,
            *nonce_account,
            nonce_authority.as_ref(),
            fee_payer.as_ref(),
        ),
        CliCommand::SplitStake {
            stake_account_pubkey,
            ref stake_authority,
            sign_only,
            ref signers,
            blockhash_query,
            nonce_account,
            ref nonce_authority,
            split_stake_account,
            seed,
            lamports,
            ref fee_payer,
        } => process_split_stake(
            &rpc_client,
            config,
            &stake_account_pubkey,
            stake_authority.as_ref(),
            *sign_only,
            signers,
            blockhash_query,
            *nonce_account,
            nonce_authority.as_ref(),
            split_stake_account,
            seed,
            *lamports,
            fee_payer.as_ref(),
        ),
        CliCommand::ShowStakeAccount {
            pubkey: stake_account_pubkey,
            use_lamports_unit,
        } => process_show_stake_account(
            &rpc_client,
            config,
            &stake_account_pubkey,
            *use_lamports_unit,
        ),
        CliCommand::ShowStakeHistory { use_lamports_unit } => {
            process_show_stake_history(&rpc_client, config, *use_lamports_unit)
        }
        CliCommand::StakeAuthorize {
            stake_account_pubkey,
            new_authorized_pubkey,
            stake_authorize,
            ref authority,
            sign_only,
            ref signers,
            blockhash_query,
            nonce_account,
            ref nonce_authority,
            ref fee_payer,
        } => process_stake_authorize(
            &rpc_client,
            config,
            &stake_account_pubkey,
            &new_authorized_pubkey,
            *stake_authorize,
            authority.as_ref(),
            *sign_only,
            signers,
            blockhash_query,
            *nonce_account,
            nonce_authority.as_ref(),
            fee_payer.as_ref(),
        ),
        CliCommand::StakeSetLockup {
            stake_account_pubkey,
            mut lockup,
            ref custodian,
            sign_only,
            ref signers,
            blockhash_query,
            nonce_account,
            ref nonce_authority,
            ref fee_payer,
        } => process_stake_set_lockup(
            &rpc_client,
            config,
            &stake_account_pubkey,
            &mut lockup,
            custodian.as_ref(),
            *sign_only,
            signers,
            blockhash_query,
            *nonce_account,
            nonce_authority.as_ref(),
            fee_payer.as_ref(),
        ),
        CliCommand::WithdrawStake {
            stake_account_pubkey,
            destination_account_pubkey,
            lamports,
            ref withdraw_authority,
            sign_only,
            ref signers,
            blockhash_query,
            ref nonce_account,
            ref nonce_authority,
            ref fee_payer,
        } => process_withdraw_stake(
            &rpc_client,
            config,
            &stake_account_pubkey,
            &destination_account_pubkey,
            *lamports,
            withdraw_authority.as_ref(),
            *sign_only,
            signers.as_ref(),
            blockhash_query,
            nonce_account.as_ref(),
            nonce_authority.as_ref(),
            fee_payer.as_ref(),
        ),

        // Storage Commands

        // Create storage account
        CliCommand::CreateStorageAccount {
            account_owner,
            storage_account,
            account_type,
        } => process_create_storage_account(
            &rpc_client,
            config,
            &account_owner,
            storage_account,
            *account_type,
        ),
        CliCommand::ClaimStorageReward {
            node_account_pubkey,
            storage_account_pubkey,
        } => process_claim_storage_reward(
            &rpc_client,
            config,
            node_account_pubkey,
            &storage_account_pubkey,
        ),
        CliCommand::ShowStorageAccount(storage_account_pubkey) => {
            process_show_storage_account(&rpc_client, config, &storage_account_pubkey)
        }

        // Validator Info Commands

        // Return all or single validator info
        CliCommand::GetValidatorInfo(info_pubkey) => {
            process_get_validator_info(&rpc_client, *info_pubkey)
        }
        // Publish validator info
        CliCommand::SetValidatorInfo {
            validator_info,
            force_keybase,
            info_pubkey,
        } => process_set_validator_info(
            &rpc_client,
            config,
            &validator_info,
            *force_keybase,
            *info_pubkey,
        ),

        // Vote Commands

        // Create vote account
        CliCommand::CreateVoteAccount {
            vote_account,
            seed,
            node_pubkey,
            authorized_voter,
            authorized_withdrawer,
            commission,
        } => process_create_vote_account(
            &rpc_client,
            config,
            vote_account,
            seed,
            &node_pubkey,
            authorized_voter,
            authorized_withdrawer,
            *commission,
        ),
        CliCommand::ShowVoteAccount {
            pubkey: vote_account_pubkey,
            use_lamports_unit,
        } => process_show_vote_account(
            &rpc_client,
            config,
            &vote_account_pubkey,
            *use_lamports_unit,
        ),
        CliCommand::VoteAuthorize {
            vote_account_pubkey,
            new_authorized_pubkey,
            vote_authorize,
        } => process_vote_authorize(
            &rpc_client,
            config,
            &vote_account_pubkey,
            &new_authorized_pubkey,
            *vote_authorize,
        ),
        CliCommand::VoteUpdateValidator {
            vote_account_pubkey,
            new_identity_pubkey,
            authorized_voter,
        } => process_vote_update_validator(
            &rpc_client,
            config,
            &vote_account_pubkey,
            &new_identity_pubkey,
            authorized_voter,
        ),

        // Wallet Commands

        // Request an airdrop from Solana Faucet;
        CliCommand::Airdrop {
            faucet_host,
            faucet_port,
            lamports,
            use_lamports_unit,
        } => {
            let faucet_addr = SocketAddr::new(
                faucet_host.unwrap_or_else(|| {
                    let faucet_host = url::Url::parse(&config.json_rpc_url)
                        .unwrap()
                        .host()
                        .unwrap()
                        .to_string();
                    solana_net_utils::parse_host(&faucet_host).unwrap_or_else(|err| {
                        panic!("Unable to resolve {}: {}", faucet_host, err);
                    })
                }),
                *faucet_port,
            );

            process_airdrop(
                &rpc_client,
                config,
                &faucet_addr,
                *lamports,
                *use_lamports_unit,
            )
        }
        // Check client balance
        CliCommand::Balance {
            pubkey,
            use_lamports_unit,
        } => process_balance(&rpc_client, config, &pubkey, *use_lamports_unit),
        // Cancel a contract by contract Pubkey
        CliCommand::Cancel(pubkey) => process_cancel(&rpc_client, config, &pubkey),
        // Confirm the last client transaction by signature
        CliCommand::Confirm(signature) => process_confirm(&rpc_client, signature),
        // If client has positive balance, pay lamports to another address
        CliCommand::Pay(PayCommand {
            lamports,
            to,
            timestamp,
            timestamp_pubkey,
            ref witnesses,
            cancelable,
            sign_only,
            ref signers,
            blockhash_query,
            nonce_account,
            ref nonce_authority,
        }) => process_pay(
            &rpc_client,
            config,
            *lamports,
            &to,
            *timestamp,
            *timestamp_pubkey,
            witnesses,
            *cancelable,
            *sign_only,
            signers,
            blockhash_query,
            *nonce_account,
            nonce_authority.as_ref(),
        ),
        CliCommand::ShowAccount {
            pubkey,
            output_file,
            use_lamports_unit,
        } => process_show_account(
            &rpc_client,
            config,
            &pubkey,
            &output_file,
            *use_lamports_unit,
        ),
        // Apply time elapsed to contract
        CliCommand::TimeElapsed(to, pubkey, dt) => {
            process_time_elapsed(&rpc_client, config, &to, &pubkey, *dt)
        }
        CliCommand::Transfer {
            lamports,
            to,
            ref from,
            sign_only,
            ref signers,
            ref blockhash_query,
            ref nonce_account,
            ref nonce_authority,
            ref fee_payer,
        } => process_transfer(
            &rpc_client,
            config,
            *lamports,
            to,
            from.as_ref(),
            *sign_only,
            signers.as_ref(),
            blockhash_query,
            nonce_account.as_ref(),
            nonce_authority.as_ref(),
            fee_payer.as_ref(),
        ),
        // Apply witness signature to contract
        CliCommand::Witness(to, pubkey) => process_witness(&rpc_client, config, &to, &pubkey),
    }
}

// Quick and dirty Keypair that assumes the client will do retries but not update the
// blockhash. If the client updates the blockhash, the signature will be invalid.
struct FaucetKeypair {
    transaction: Transaction,
}

impl FaucetKeypair {
    fn new_keypair(
        faucet_addr: &SocketAddr,
        to_pubkey: &Pubkey,
        lamports: u64,
        blockhash: Hash,
    ) -> Result<Self, Box<dyn error::Error>> {
        let transaction = request_airdrop_transaction(faucet_addr, to_pubkey, lamports, blockhash)?;
        Ok(Self { transaction })
    }

    fn airdrop_transaction(&self) -> Transaction {
        self.transaction.clone()
    }
}

impl KeypairUtil for FaucetKeypair {
    /// Return the public key of the keypair used to sign votes
    fn pubkey(&self) -> Pubkey {
        self.transaction.message().account_keys[0]
    }

    fn sign_message(&self, _msg: &[u8]) -> Signature {
        self.transaction.signatures[0]
    }
}

pub fn request_and_confirm_airdrop(
    rpc_client: &RpcClient,
    faucet_addr: &SocketAddr,
    to_pubkey: &Pubkey,
    lamports: u64,
) -> ProcessResult {
    let (blockhash, _fee_calculator) = rpc_client.get_recent_blockhash()?;
    let keypair = {
        let mut retries = 5;
        loop {
            let result = FaucetKeypair::new_keypair(faucet_addr, to_pubkey, lamports, blockhash);
            if result.is_ok() || retries == 0 {
                break result;
            }
            retries -= 1;
            sleep(Duration::from_secs(1));
        }
    }?;
    let mut tx = keypair.airdrop_transaction();
    let result = rpc_client.send_and_confirm_transaction(&mut tx, &[&keypair]);
    log_instruction_custom_error::<SystemError>(result)
}

pub fn log_instruction_custom_error<E>(result: Result<String, ClientError>) -> ProcessResult
where
    E: 'static + std::error::Error + DecodeError<E> + FromPrimitive,
{
    match result {
        Err(err) => {
            if let ClientError::TransactionError(TransactionError::InstructionError(
                _,
                InstructionError::CustomError(code),
            )) = err
            {
                if let Some(specific_error) = E::decode_custom_error_to_enum(code) {
                    error!("{}::{:?}", E::type_of(), specific_error);
                    return Err(specific_error.into());
                }
            }
            error!("{:?}", err);
            Err(err.into())
        }
        Ok(sig) => Ok(sig),
    }
}

// If clap arg `name` is_required, and specifies an amount of either lamports or SOL, the only way
// `amount_of()` can return None is if `name` is an f64 and `unit`== "lamports". This method
// catches that case and converts it to an Error.
pub(crate) fn required_lamports_from(
    matches: &ArgMatches<'_>,
    name: &str,
    unit: &str,
) -> Result<u64, CliError> {
    amount_of(matches, name, unit).ok_or_else(|| {
        CliError::BadParameter(format!(
            "Lamports cannot be fractional: {}",
            matches.value_of("amount").unwrap()
        ))
    })
}

pub(crate) fn build_balance_message(
    lamports: u64,
    use_lamports_unit: bool,
    show_unit: bool,
) -> String {
    if use_lamports_unit {
        let ess = if lamports == 1 { "" } else { "s" };
        let unit = if show_unit {
            format!(" lamport{}", ess)
        } else {
            "".to_string()
        };
        format!("{:?}{}", lamports, unit)
    } else {
        let sol = lamports_to_sol(lamports);
        let sol_str = format!("{:.9}", sol);
        let pretty_sol = sol_str.trim_end_matches('0').trim_end_matches('.');
        let unit = if show_unit { " SOL" } else { "" };
        format!("{}{}", pretty_sol, unit)
    }
}

pub fn app<'ab, 'v>(name: &str, about: &'ab str, version: &'v str) -> App<'ab, 'v> {
    App::new(name)
        .about(about)
        .version(version)
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .subcommand(SubCommand::with_name("address").about("Get your public key"))
        .cluster_query_subcommands()
        .nonce_subcommands()
        .stake_subcommands()
        .storage_subcommands()
        .subcommand(
            SubCommand::with_name("airdrop")
                .about("Request lamports")
                .arg(
                    Arg::with_name("faucet_host")
                        .long("faucet-host")
                        .value_name("HOST")
                        .takes_value(true)
                        .help("Faucet host to use [default: the --url host]"),
                )
                .arg(
                    Arg::with_name("faucet_port")
                        .long("faucet-port")
                        .value_name("PORT")
                        .takes_value(true)
                        .default_value(solana_faucet::faucet::FAUCET_PORT_STR)
                        .help("Faucet port to use"),
                )
                .arg(
                    Arg::with_name("amount")
                        .index(1)
                        .value_name("AMOUNT")
                        .takes_value(true)
                        .validator(is_amount)
                        .required(true)
                        .help("The airdrop amount to request (default unit SOL)"),
                )
                .arg(
                    Arg::with_name("unit")
                        .index(2)
                        .value_name("UNIT")
                        .takes_value(true)
                        .possible_values(&["SOL", "lamports"])
                        .help("Specify unit to use for request and balance display"),
                ),
        )
        .subcommand(
            SubCommand::with_name("balance")
                .about("Get your balance")
                .arg(
                    Arg::with_name("pubkey")
                        .index(1)
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .validator(is_pubkey_or_keypair)
                        .help("The public key of the balance to check"),
                )
                .arg(
                    Arg::with_name("lamports")
                        .long("lamports")
                        .takes_value(false)
                        .help("Display balance in lamports instead of SOL"),
                ),
        )
        .subcommand(
            SubCommand::with_name("cancel")
                .about("Cancel a transfer")
                .arg(
                    Arg::with_name("process_id")
                        .index(1)
                        .value_name("PROCESS ID")
                        .takes_value(true)
                        .required(true)
                        .validator(is_pubkey)
                        .help("The process id of the transfer to cancel"),
                ),
        )
        .subcommand(
            SubCommand::with_name("confirm")
                .about("Confirm transaction by signature")
                .arg(
                    Arg::with_name("signature")
                        .index(1)
                        .value_name("SIGNATURE")
                        .takes_value(true)
                        .required(true)
                        .help("The transaction signature to confirm"),
                ),
        )
        .subcommand(
            SubCommand::with_name("create-address-with-seed")
                .about("Generate a derived account address with a seed")
                .arg(
                    Arg::with_name("seed")
                        .index(1)
                        .value_name("SEED_STRING")
                        .takes_value(true)
                        .required(true)
                        .help("The seed.  Must not take more than 32 bytes to encode as utf-8"),
                )
                .arg(
                    Arg::with_name("program_id")
                        .index(2)
                        .value_name("PROGRAM_ID")
                        .takes_value(true)
                        .required(true)
                        .help(
                            "The program_id that the address will ultimately be used for, \n\
                             or one of STAKE, VOTE, and STORAGE keywords",
                        ),
                )
                .arg(
                    Arg::with_name("from")
                        .long("from")
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .required(false)
                        .validator(is_pubkey_or_keypair)
                        .help("From (base) key, defaults to client keypair."),
                ),
        )
        .subcommand(
            SubCommand::with_name("deploy")
                .about("Deploy a program")
                .arg(
                    Arg::with_name("program_location")
                        .index(1)
                        .value_name("PATH TO BPF PROGRAM")
                        .takes_value(true)
                        .required(true)
                        .help("/path/to/program.o"),
                ),
        )
        .subcommand(
            SubCommand::with_name("pay")
                .about("Send a payment")
                .arg(
                    Arg::with_name("to")
                        .index(1)
                        .value_name("TO PUBKEY")
                        .takes_value(true)
                        .required(true)
                        .validator(is_pubkey_or_keypair)
                        .help("The pubkey of recipient"),
                )
                .arg(
                    Arg::with_name("amount")
                        .index(2)
                        .value_name("AMOUNT")
                        .takes_value(true)
                        .validator(is_amount)
                        .required(true)
                        .help("The amount to send (default unit SOL)"),
                )
                .arg(
                    Arg::with_name("unit")
                        .index(3)
                        .value_name("UNIT")
                        .takes_value(true)
                        .possible_values(&["SOL", "lamports"])
                        .help("Specify unit to use for request"),
                )
                .arg(
                    Arg::with_name("timestamp")
                        .long("after")
                        .value_name("DATETIME")
                        .takes_value(true)
                        .help("A timestamp after which transaction will execute"),
                )
                .arg(
                    Arg::with_name("timestamp_pubkey")
                        .long("require-timestamp-from")
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .requires("timestamp")
                        .validator(is_pubkey)
                        .help("Require timestamp from this third party"),
                )
                .arg(
                    Arg::with_name("witness")
                        .long("require-signature-from")
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .multiple(true)
                        .use_delimiter(true)
                        .validator(is_pubkey)
                        .help("Any third party signatures required to unlock the lamports"),
                )
                .arg(
                    Arg::with_name("cancelable")
                        .long("cancelable")
                        .takes_value(false),
                )
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg()),
        )
        .subcommand(
            SubCommand::with_name("send-signature")
                .about("Send a signature to authorize a transfer")
                .arg(
                    Arg::with_name("to")
                        .index(1)
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .required(true)
                        .validator(is_pubkey)
                        .help("The pubkey of recipient"),
                )
                .arg(
                    Arg::with_name("process_id")
                        .index(2)
                        .value_name("PROCESS ID")
                        .takes_value(true)
                        .required(true)
                        .help("The process id of the transfer to authorize"),
                ),
        )
        .subcommand(
            SubCommand::with_name("send-timestamp")
                .about("Send a timestamp to unlock a transfer")
                .arg(
                    Arg::with_name("to")
                        .index(1)
                        .value_name("PUBKEY")
                        .takes_value(true)
                        .required(true)
                        .validator(is_pubkey)
                        .help("The pubkey of recipient"),
                )
                .arg(
                    Arg::with_name("process_id")
                        .index(2)
                        .value_name("PROCESS ID")
                        .takes_value(true)
                        .required(true)
                        .help("The process id of the transfer to unlock"),
                )
                .arg(
                    Arg::with_name("datetime")
                        .long("date")
                        .value_name("DATETIME")
                        .takes_value(true)
                        .help("Optional arbitrary timestamp to apply"),
                ),
        )
        .subcommand(
            SubCommand::with_name("transfer")
                .about("Transfer funds between system accounts")
                .arg(
                    Arg::with_name("to")
                        .index(1)
                        .value_name("TO PUBKEY")
                        .takes_value(true)
                        .required(true)
                        .validator(is_pubkey_or_keypair)
                        .help("The pubkey of recipient"),
                )
                .arg(
                    Arg::with_name("amount")
                        .index(2)
                        .value_name("AMOUNT")
                        .takes_value(true)
                        .validator(is_amount)
                        .required(true)
                        .help("The amount to send (default unit SOL)"),
                )
                .arg(
                    Arg::with_name("unit")
                        .index(3)
                        .value_name("UNIT")
                        .takes_value(true)
                        .possible_values(&["SOL", "lamports"])
                        .help("Specify unit to use for request"),
                )
                .arg(
                    Arg::with_name("from")
                        .long("from")
                        .takes_value(true)
                        .value_name("KEYPAIR or PUBKEY")
                        .validator(is_pubkey_or_keypair_or_ask_keyword)
                        .help("Source account of funds (if different from client local account)"),
                )
                .offline_args()
                .arg(nonce_arg())
                .arg(nonce_authority_arg())
                .arg(fee_payer_arg()),
        )
        .subcommand(
            SubCommand::with_name("account")
                .about("Show the contents of an account")
                .alias("account")
                .arg(
                    Arg::with_name("account_pubkey")
                        .index(1)
                        .value_name("ACCOUNT PUBKEY")
                        .takes_value(true)
                        .required(true)
                        .validator(is_pubkey_or_keypair)
                        .help("Account pubkey"),
                )
                .arg(
                    Arg::with_name("output_file")
                        .long("output")
                        .short("o")
                        .value_name("FILE")
                        .takes_value(true)
                        .help("Write the account data to this file"),
                )
                .arg(
                    Arg::with_name("lamports")
                        .long("lamports")
                        .takes_value(false)
                        .help("Display balance in lamports instead of SOL"),
                ),
        )
        .validator_info_subcommands()
        .vote_subcommands()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use solana_client::{
        mock_rpc_client_request::SIGNATURE,
        rpc_request::RpcRequest,
        rpc_response::{Response, RpcAccount, RpcResponseContext},
    };
    use solana_sdk::{
        account::Account,
        nonce_state::{Meta as NonceMeta, NonceState},
        pubkey::Pubkey,
        signature::{keypair_from_seed, read_keypair_file, write_keypair_file},
        system_program,
        transaction::TransactionError,
    };
    use std::{collections::HashMap, path::PathBuf};

    fn make_tmp_path(name: &str) -> String {
        let out_dir = std::env::var("FARF_DIR").unwrap_or_else(|_| "farf".to_string());
        let keypair = Keypair::new();

        let path = format!("{}/tmp/{}-{}", out_dir, name, keypair.pubkey());

        // whack any possible collision
        let _ignored = std::fs::remove_dir_all(&path);
        // whack any possible collision
        let _ignored = std::fs::remove_file(&path);

        path
    }

    #[test]
    fn test_signing_authority_dummy_keypairs() {
        let signing_authority: SigningAuthority = Pubkey::new(&[1u8; 32]).into();
        assert_eq!(signing_authority, Pubkey::new(&[1u8; 32]).into());
        assert_ne!(signing_authority, Pubkey::new(&[2u8; 32]).into());
    }

    #[test]
    fn test_cli_parse_command() {
        let test_commands = app("test", "desc", "version");

        let pubkey = Pubkey::new_rand();
        let pubkey_string = format!("{}", pubkey);
        let witness0 = Pubkey::new_rand();
        let witness0_string = format!("{}", witness0);
        let witness1 = Pubkey::new_rand();
        let witness1_string = format!("{}", witness1);
        let dt = Utc.ymd(2018, 9, 19).and_hms(17, 30, 59);

        // Test Airdrop Subcommand
        let test_airdrop = test_commands
            .clone()
            .get_matches_from(vec!["test", "airdrop", "50", "lamports"]);
        assert_eq!(
            parse_command(&test_airdrop).unwrap(),
            CliCommandInfo {
                command: CliCommand::Airdrop {
                    faucet_host: None,
                    faucet_port: solana_faucet::faucet::FAUCET_PORT,
                    lamports: 50,
                    use_lamports_unit: true,
                },
                require_keypair: true,
            }
        );

        // Test Balance Subcommand, incl pubkey and keypair-file inputs
        let keypair_file = make_tmp_path("keypair_file");
        write_keypair_file(&Keypair::new(), &keypair_file).unwrap();
        let keypair = read_keypair_file(&keypair_file).unwrap();
        let test_balance = test_commands.clone().get_matches_from(vec![
            "test",
            "balance",
            &keypair.pubkey().to_string(),
        ]);
        assert_eq!(
            parse_command(&test_balance).unwrap(),
            CliCommandInfo {
                command: CliCommand::Balance {
                    pubkey: Some(keypair.pubkey()),
                    use_lamports_unit: false
                },
                require_keypair: false
            }
        );
        let test_balance = test_commands.clone().get_matches_from(vec![
            "test",
            "balance",
            &keypair_file,
            "--lamports",
        ]);
        assert_eq!(
            parse_command(&test_balance).unwrap(),
            CliCommandInfo {
                command: CliCommand::Balance {
                    pubkey: Some(keypair.pubkey()),
                    use_lamports_unit: true
                },
                require_keypair: false
            }
        );
        let test_balance =
            test_commands
                .clone()
                .get_matches_from(vec!["test", "balance", "--lamports"]);
        assert_eq!(
            parse_command(&test_balance).unwrap(),
            CliCommandInfo {
                command: CliCommand::Balance {
                    pubkey: None,
                    use_lamports_unit: true
                },
                require_keypair: true
            }
        );

        // Test Cancel Subcommand
        let test_cancel =
            test_commands
                .clone()
                .get_matches_from(vec!["test", "cancel", &pubkey_string]);
        assert_eq!(
            parse_command(&test_cancel).unwrap(),
            CliCommandInfo {
                command: CliCommand::Cancel(pubkey),
                require_keypair: true
            }
        );

        // Test Confirm Subcommand
        let signature = Signature::new(&vec![1; 64]);
        let signature_string = format!("{:?}", signature);
        let test_confirm =
            test_commands
                .clone()
                .get_matches_from(vec!["test", "confirm", &signature_string]);
        assert_eq!(
            parse_command(&test_confirm).unwrap(),
            CliCommandInfo {
                command: CliCommand::Confirm(signature),
                require_keypair: false
            }
        );
        let test_bad_signature = test_commands
            .clone()
            .get_matches_from(vec!["test", "confirm", "deadbeef"]);
        assert!(parse_command(&test_bad_signature).is_err());

        // Test CreateAddressWithSeed
        let from_pubkey = Some(Pubkey::new_rand());
        let from_str = from_pubkey.unwrap().to_string();
        for (name, program_id) in &[
            ("STAKE", solana_stake_program::id()),
            ("VOTE", solana_vote_program::id()),
            ("STORAGE", solana_storage_program::id()),
        ] {
            let test_create_address_with_seed = test_commands.clone().get_matches_from(vec![
                "test",
                "create-address-with-seed",
                "seed",
                name,
                "--from",
                &from_str,
            ]);
            assert_eq!(
                parse_command(&test_create_address_with_seed).unwrap(),
                CliCommandInfo {
                    command: CliCommand::CreateAddressWithSeed {
                        from_pubkey,
                        seed: "seed".to_string(),
                        program_id: *program_id
                    },
                    require_keypair: false
                }
            );
        }
        let test_create_address_with_seed = test_commands.clone().get_matches_from(vec![
            "test",
            "create-address-with-seed",
            "seed",
            "STAKE",
        ]);
        assert_eq!(
            parse_command(&test_create_address_with_seed).unwrap(),
            CliCommandInfo {
                command: CliCommand::CreateAddressWithSeed {
                    from_pubkey: None,
                    seed: "seed".to_string(),
                    program_id: solana_stake_program::id(),
                },
                require_keypair: true
            }
        );

        // Test Deploy Subcommand
        let test_deploy =
            test_commands
                .clone()
                .get_matches_from(vec!["test", "deploy", "/Users/test/program.o"]);
        assert_eq!(
            parse_command(&test_deploy).unwrap(),
            CliCommandInfo {
                command: CliCommand::Deploy("/Users/test/program.o".to_string()),
                require_keypair: true
            }
        );

        // Test Simple Pay Subcommand
        let test_pay = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
        ]);
        assert_eq!(
            parse_command(&test_pay).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Pay Subcommand w/ Witness
        let test_pay_multiple_witnesses = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--require-signature-from",
            &witness0_string,
            "--require-signature-from",
            &witness1_string,
        ]);
        assert_eq!(
            parse_command(&test_pay_multiple_witnesses).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    witnesses: Some(vec![witness0, witness1]),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );
        let test_pay_single_witness = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--require-signature-from",
            &witness0_string,
        ]);
        assert_eq!(
            parse_command(&test_pay_single_witness).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    witnesses: Some(vec![witness0]),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Pay Subcommand w/ Timestamp
        let test_pay_timestamp = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--after",
            "2018-09-19T17:30:59",
            "--require-timestamp-from",
            &witness0_string,
        ]);
        assert_eq!(
            parse_command(&test_pay_timestamp).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    timestamp: Some(dt),
                    timestamp_pubkey: Some(witness0),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Pay Subcommand w/ sign-only
        let blockhash = Hash::default();
        let blockhash_string = format!("{}", blockhash);
        let test_pay = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--blockhash",
            &blockhash_string,
            "--sign-only",
        ]);
        assert_eq!(
            parse_command(&test_pay).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    blockhash_query: BlockhashQuery::None(blockhash, FeeCalculator::default()),
                    sign_only: true,
                    ..PayCommand::default()
                }),
                require_keypair: true,
            }
        );

        // Test Pay Subcommand w/ signer
        let key1 = Pubkey::new_rand();
        let sig1 = Keypair::new().sign_message(&[0u8]);
        let signer1 = format!("{}={}", key1, sig1);
        let test_pay = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--blockhash",
            &blockhash_string,
            "--signer",
            &signer1,
        ]);
        assert_eq!(
            parse_command(&test_pay).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
                    signers: Some(vec![(key1, sig1)]),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Pay Subcommand w/ signers
        let key2 = Pubkey::new_rand();
        let sig2 = Keypair::new().sign_message(&[1u8]);
        let signer2 = format!("{}={}", key2, sig2);
        let test_pay = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--blockhash",
            &blockhash_string,
            "--signer",
            &signer1,
            "--signer",
            &signer2,
        ]);
        assert_eq!(
            parse_command(&test_pay).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
                    signers: Some(vec![(key1, sig1), (key2, sig2)]),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Pay Subcommand w/ Blockhash
        let test_pay = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--blockhash",
            &blockhash_string,
        ]);
        assert_eq!(
            parse_command(&test_pay).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Pay Subcommand w/ Nonce
        let blockhash = Hash::default();
        let blockhash_string = format!("{}", blockhash);
        let test_pay = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--blockhash",
            &blockhash_string,
            "--nonce",
            &pubkey_string,
        ]);
        assert_eq!(
            parse_command(&test_pay).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
                    nonce_account: Some(pubkey),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Pay Subcommand w/ Nonce and Nonce Authority
        let blockhash = Hash::default();
        let blockhash_string = format!("{}", blockhash);
        let keypair = read_keypair_file(&keypair_file).unwrap();
        let test_pay = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--blockhash",
            &blockhash_string,
            "--nonce",
            &pubkey_string,
            "--nonce-authority",
            &keypair_file,
        ]);
        assert_eq!(
            parse_command(&test_pay).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
                    nonce_account: Some(pubkey),
                    nonce_authority: Some(keypair.into()),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Pay Subcommand w/ Nonce and Offline Nonce Authority
        let keypair = read_keypair_file(&keypair_file).unwrap();
        let authority_pubkey = keypair.pubkey();
        let authority_pubkey_string = format!("{}", authority_pubkey);
        let sig = keypair.sign_message(&[0u8]);
        let signer_arg = format!("{}={}", authority_pubkey, sig);
        let test_pay = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--blockhash",
            &blockhash_string,
            "--nonce",
            &pubkey_string,
            "--nonce-authority",
            &authority_pubkey_string,
            "--signer",
            &signer_arg,
        ]);
        assert_eq!(
            parse_command(&test_pay).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
                    nonce_account: Some(pubkey),
                    nonce_authority: Some(authority_pubkey.into()),
                    signers: Some(vec![(authority_pubkey, sig)]),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Pay Subcommand w/ Nonce and Offline Nonce Authority
        // authority pubkey not in signers fails
        let keypair = read_keypair_file(&keypair_file).unwrap();
        let authority_pubkey = keypair.pubkey();
        let authority_pubkey_string = format!("{}", authority_pubkey);
        let sig = keypair.sign_message(&[0u8]);
        let signer_arg = format!("{}={}", Pubkey::new_rand(), sig);
        let test_pay = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--blockhash",
            &blockhash_string,
            "--nonce",
            &pubkey_string,
            "--nonce-authority",
            &authority_pubkey_string,
            "--signer",
            &signer_arg,
        ]);
        assert!(parse_command(&test_pay).is_err());

        // Test Send-Signature Subcommand
        let test_send_signature = test_commands.clone().get_matches_from(vec![
            "test",
            "send-signature",
            &pubkey_string,
            &pubkey_string,
        ]);
        assert_eq!(
            parse_command(&test_send_signature).unwrap(),
            CliCommandInfo {
                command: CliCommand::Witness(pubkey, pubkey),
                require_keypair: true
            }
        );
        let test_pay_multiple_witnesses = test_commands.clone().get_matches_from(vec![
            "test",
            "pay",
            &pubkey_string,
            "50",
            "lamports",
            "--after",
            "2018-09-19T17:30:59",
            "--require-signature-from",
            &witness0_string,
            "--require-timestamp-from",
            &witness0_string,
            "--require-signature-from",
            &witness1_string,
        ]);
        assert_eq!(
            parse_command(&test_pay_multiple_witnesses).unwrap(),
            CliCommandInfo {
                command: CliCommand::Pay(PayCommand {
                    lamports: 50,
                    to: pubkey,
                    timestamp: Some(dt),
                    timestamp_pubkey: Some(witness0),
                    witnesses: Some(vec![witness0, witness1]),
                    ..PayCommand::default()
                }),
                require_keypair: true
            }
        );

        // Test Send-Timestamp Subcommand
        let test_send_timestamp = test_commands.clone().get_matches_from(vec![
            "test",
            "send-timestamp",
            &pubkey_string,
            &pubkey_string,
            "--date",
            "2018-09-19T17:30:59",
        ]);
        assert_eq!(
            parse_command(&test_send_timestamp).unwrap(),
            CliCommandInfo {
                command: CliCommand::TimeElapsed(pubkey, pubkey, dt),
                require_keypair: true
            }
        );
        let test_bad_timestamp = test_commands.clone().get_matches_from(vec![
            "test",
            "send-timestamp",
            &pubkey_string,
            &pubkey_string,
            "--date",
            "20180919T17:30:59",
        ]);
        assert!(parse_command(&test_bad_timestamp).is_err());
    }

    #[test]
    fn test_cli_process_command() {
        // Success cases
        let mut config = CliConfig::default();
        config.rpc_client = Some(RpcClient::new_mock("succeeds".to_string()));

        let keypair = Keypair::new();
        let pubkey = keypair.pubkey().to_string();
        config.keypair = keypair;
        config.command = CliCommand::Address;
        assert_eq!(process_command(&config).unwrap(), pubkey);

        config.command = CliCommand::Balance {
            pubkey: None,
            use_lamports_unit: true,
        };
        assert_eq!(process_command(&config).unwrap(), "50 lamports");

        config.command = CliCommand::Balance {
            pubkey: None,
            use_lamports_unit: false,
        };
        assert_eq!(process_command(&config).unwrap(), "0.00000005 SOL");

        let process_id = Pubkey::new_rand();
        config.command = CliCommand::Cancel(process_id);
        assert_eq!(process_command(&config).unwrap(), SIGNATURE);

        let good_signature = Signature::new(&bs58::decode(SIGNATURE).into_vec().unwrap());
        config.command = CliCommand::Confirm(good_signature);
        assert_eq!(process_command(&config).unwrap(), "Confirmed");

        let bob_keypair = Keypair::new();
        let bob_pubkey = bob_keypair.pubkey();
        let node_pubkey = Pubkey::new_rand();
        config.command = CliCommand::CreateVoteAccount {
            vote_account: bob_keypair.into(),
            seed: None,
            node_pubkey,
            authorized_voter: Some(bob_pubkey),
            authorized_withdrawer: Some(bob_pubkey),
            commission: 0,
        };
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let new_authorized_pubkey = Pubkey::new_rand();
        config.command = CliCommand::VoteAuthorize {
            vote_account_pubkey: bob_pubkey,
            new_authorized_pubkey,
            vote_authorize: VoteAuthorize::Voter,
        };
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let new_identity_pubkey = Pubkey::new_rand();
        config.command = CliCommand::VoteUpdateValidator {
            vote_account_pubkey: bob_pubkey,
            new_identity_pubkey,
            authorized_voter: Keypair::new().into(),
        };
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let bob_keypair = Keypair::new();
        let bob_pubkey = bob_keypair.pubkey();
        let custodian = Pubkey::new_rand();
        config.command = CliCommand::CreateStakeAccount {
            stake_account: bob_keypair.into(),
            seed: None,
            staker: None,
            withdrawer: None,
            lockup: Lockup {
                epoch: 0,
                unix_timestamp: 0,
                custodian,
            },
            lamports: 1234,
            sign_only: false,
            signers: None,
            blockhash_query: BlockhashQuery::All,
            nonce_account: None,
            nonce_authority: None,
            fee_payer: None,
            from: None,
        };
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let stake_pubkey = Pubkey::new_rand();
        let to_pubkey = Pubkey::new_rand();
        config.command = CliCommand::WithdrawStake {
            stake_account_pubkey: stake_pubkey,
            destination_account_pubkey: to_pubkey,
            lamports: 100,
            withdraw_authority: None,
            sign_only: false,
            signers: None,
            blockhash_query: BlockhashQuery::All,
            nonce_account: None,
            nonce_authority: None,
            fee_payer: None,
        };
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let stake_pubkey = Pubkey::new_rand();
        config.command = CliCommand::DeactivateStake {
            stake_account_pubkey: stake_pubkey,
            stake_authority: None,
            sign_only: false,
            signers: None,
            blockhash_query: BlockhashQuery::default(),
            nonce_account: None,
            nonce_authority: None,
            fee_payer: None,
        };
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let stake_pubkey = Pubkey::new_rand();
        let split_stake_account = Keypair::new();
        config.command = CliCommand::SplitStake {
            stake_account_pubkey: stake_pubkey,
            stake_authority: None,
            sign_only: false,
            signers: None,
            blockhash_query: BlockhashQuery::default(),
            nonce_account: None,
            nonce_authority: None,
            split_stake_account: split_stake_account.into(),
            seed: None,
            lamports: 1234,
            fee_payer: None,
        };
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        config.command = CliCommand::GetSlot {
            commitment_config: CommitmentConfig::default(),
        };
        assert_eq!(process_command(&config).unwrap(), "0");

        config.command = CliCommand::GetTransactionCount {
            commitment_config: CommitmentConfig::default(),
        };
        assert_eq!(process_command(&config).unwrap(), "1234");

        config.command = CliCommand::Pay(PayCommand {
            lamports: 10,
            to: bob_pubkey,
            ..PayCommand::default()
        });
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let date_string = "\"2018-09-19T17:30:59Z\"";
        let dt: DateTime<Utc> = serde_json::from_str(&date_string).unwrap();
        config.command = CliCommand::Pay(PayCommand {
            lamports: 10,
            to: bob_pubkey,
            timestamp: Some(dt),
            timestamp_pubkey: Some(config.keypair.pubkey()),
            ..PayCommand::default()
        });
        let result = process_command(&config);
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(
            json.as_object()
                .unwrap()
                .get("signature")
                .unwrap()
                .as_str()
                .unwrap(),
            SIGNATURE.to_string()
        );

        let witness = Pubkey::new_rand();
        config.command = CliCommand::Pay(PayCommand {
            lamports: 10,
            to: bob_pubkey,
            witnesses: Some(vec![witness]),
            cancelable: true,
            ..PayCommand::default()
        });
        let result = process_command(&config);
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(
            json.as_object()
                .unwrap()
                .get("signature")
                .unwrap()
                .as_str()
                .unwrap(),
            SIGNATURE.to_string()
        );

        // Nonced pay
        let blockhash = Hash::default();
        let nonce_response = json!(Response {
            context: RpcResponseContext { slot: 1 },
            value: json!(RpcAccount::encode(
                Account::new_data(
                    1,
                    &NonceState::Initialized(NonceMeta::new(&config.keypair.pubkey()), blockhash),
                    &system_program::ID,
                )
                .unwrap()
            )),
        });
        let mut mocks = HashMap::new();
        mocks.insert(RpcRequest::GetAccountInfo, nonce_response);
        config.rpc_client = Some(RpcClient::new_mock_with_mocks("".to_string(), mocks));
        config.command = CliCommand::Pay(PayCommand {
            lamports: 10,
            to: bob_pubkey,
            nonce_account: Some(bob_pubkey),
            blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
            ..PayCommand::default()
        });
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        // Nonced pay w/ non-payer authority
        let bob_keypair = Keypair::new();
        let bob_pubkey = bob_keypair.pubkey();
        let blockhash = Hash::default();
        let nonce_authority_response = json!(Response {
            context: RpcResponseContext { slot: 1 },
            value: json!(RpcAccount::encode(
                Account::new_data(
                    1,
                    &NonceState::Initialized(NonceMeta::new(&bob_pubkey), blockhash),
                    &system_program::ID,
                )
                .unwrap()
            )),
        });
        let mut mocks = HashMap::new();
        mocks.insert(RpcRequest::GetAccountInfo, nonce_authority_response);
        config.rpc_client = Some(RpcClient::new_mock_with_mocks("".to_string(), mocks));
        config.command = CliCommand::Pay(PayCommand {
            lamports: 10,
            to: bob_pubkey,
            blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
            nonce_account: Some(bob_pubkey),
            nonce_authority: Some(bob_keypair.into()),
            ..PayCommand::default()
        });
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let process_id = Pubkey::new_rand();
        config.command = CliCommand::TimeElapsed(bob_pubkey, process_id, dt);
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let witness = Pubkey::new_rand();
        config.command = CliCommand::Witness(bob_pubkey, witness);
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        // Need airdrop cases
        config.command = CliCommand::Airdrop {
            faucet_host: None,
            faucet_port: 1234,
            lamports: 50,
            use_lamports_unit: true,
        };
        assert!(process_command(&config).is_ok());

        config.command = CliCommand::TimeElapsed(bob_pubkey, process_id, dt);
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        let witness = Pubkey::new_rand();
        config.command = CliCommand::Witness(bob_pubkey, witness);
        let signature = process_command(&config);
        assert_eq!(signature.unwrap(), SIGNATURE.to_string());

        // sig_not_found case
        config.rpc_client = Some(RpcClient::new_mock("sig_not_found".to_string()));
        let missing_signature = Signature::new(&bs58::decode("5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW").into_vec().unwrap());
        config.command = CliCommand::Confirm(missing_signature);
        assert_eq!(process_command(&config).unwrap(), "Not found");

        // Tx error case
        config.rpc_client = Some(RpcClient::new_mock("account_in_use".to_string()));
        let any_signature = Signature::new(&bs58::decode(SIGNATURE).into_vec().unwrap());
        config.command = CliCommand::Confirm(any_signature);
        assert_eq!(
            process_command(&config).unwrap(),
            format!(
                "Transaction failed with error {:?}",
                TransactionError::AccountInUse
            )
        );

        // Failure cases
        config.rpc_client = Some(RpcClient::new_mock("fails".to_string()));

        config.command = CliCommand::Airdrop {
            faucet_host: None,
            faucet_port: 1234,
            lamports: 50,
            use_lamports_unit: true,
        };
        assert!(process_command(&config).is_err());

        config.command = CliCommand::Balance {
            pubkey: None,
            use_lamports_unit: false,
        };
        assert!(process_command(&config).is_err());

        let bob_keypair = Keypair::new();
        config.command = CliCommand::CreateVoteAccount {
            vote_account: bob_keypair.into(),
            seed: None,
            node_pubkey,
            authorized_voter: Some(bob_pubkey),
            authorized_withdrawer: Some(bob_pubkey),
            commission: 0,
        };
        assert!(process_command(&config).is_err());

        config.command = CliCommand::VoteAuthorize {
            vote_account_pubkey: bob_pubkey,
            new_authorized_pubkey: bob_pubkey,
            vote_authorize: VoteAuthorize::Voter,
        };
        assert!(process_command(&config).is_err());

        config.command = CliCommand::VoteUpdateValidator {
            vote_account_pubkey: bob_pubkey,
            new_identity_pubkey: bob_pubkey,
            authorized_voter: Keypair::new().into(),
        };
        assert!(process_command(&config).is_err());

        config.command = CliCommand::GetSlot {
            commitment_config: CommitmentConfig::default(),
        };
        assert!(process_command(&config).is_err());

        config.command = CliCommand::GetTransactionCount {
            commitment_config: CommitmentConfig::default(),
        };
        assert!(process_command(&config).is_err());

        config.command = CliCommand::Pay(PayCommand {
            lamports: 10,
            to: bob_pubkey,
            ..PayCommand::default()
        });
        assert!(process_command(&config).is_err());

        config.command = CliCommand::Pay(PayCommand {
            lamports: 10,
            to: bob_pubkey,
            timestamp: Some(dt),
            timestamp_pubkey: Some(config.keypair.pubkey()),
            ..PayCommand::default()
        });
        assert!(process_command(&config).is_err());

        config.command = CliCommand::Pay(PayCommand {
            lamports: 10,
            to: bob_pubkey,
            witnesses: Some(vec![witness]),
            cancelable: true,
            ..PayCommand::default()
        });
        assert!(process_command(&config).is_err());

        config.command = CliCommand::TimeElapsed(bob_pubkey, process_id, dt);
        assert!(process_command(&config).is_err());
    }

    #[test]
    fn test_cli_deploy() {
        solana_logger::setup();
        let mut pathbuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        pathbuf.push("tests");
        pathbuf.push("fixtures");
        pathbuf.push("noop");
        pathbuf.set_extension("so");

        // Success case
        let mut config = CliConfig::default();
        config.rpc_client = Some(RpcClient::new_mock("deploy_succeeds".to_string()));

        config.command = CliCommand::Deploy(pathbuf.to_str().unwrap().to_string());
        let result = process_command(&config);
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        let program_id = json
            .as_object()
            .unwrap()
            .get("programId")
            .unwrap()
            .as_str()
            .unwrap();

        assert!(program_id.parse::<Pubkey>().is_ok());

        // Failure case
        config.command = CliCommand::Deploy("bad/file/location.so".to_string());
        assert!(process_command(&config).is_err());
    }

    #[test]
    fn test_parse_transfer_subcommand() {
        let test_commands = app("test", "desc", "version");

        //Test Transfer Subcommand, lamports
        let from_keypair = keypair_from_seed(&[0u8; 32]).unwrap();
        let from_pubkey = from_keypair.pubkey();
        let from_string = from_pubkey.to_string();
        let to_keypair = keypair_from_seed(&[1u8; 32]).unwrap();
        let to_pubkey = to_keypair.pubkey();
        let to_string = to_pubkey.to_string();
        let test_transfer = test_commands
            .clone()
            .get_matches_from(vec!["test", "transfer", &to_string, "42", "lamports"]);
        assert_eq!(
            parse_command(&test_transfer).unwrap(),
            CliCommandInfo {
                command: CliCommand::Transfer {
                    lamports: 42,
                    to: to_pubkey,
                    from: None,
                    sign_only: false,
                    signers: None,
                    blockhash_query: BlockhashQuery::All,
                    nonce_account: None,
                    nonce_authority: None,
                    fee_payer: None,
                },
                require_keypair: true,
            }
        );

        //Test Transfer Subcommand, SOL
        let test_transfer = test_commands
            .clone()
            .get_matches_from(vec!["test", "transfer", &to_string, "42"]);
        assert_eq!(
            parse_command(&test_transfer).unwrap(),
            CliCommandInfo {
                command: CliCommand::Transfer {
                    lamports: 42_000_000_000,
                    to: to_pubkey,
                    from: None,
                    sign_only: false,
                    signers: None,
                    blockhash_query: BlockhashQuery::All,
                    nonce_account: None,
                    nonce_authority: None,
                    fee_payer: None,
                },
                require_keypair: true,
            }
        );

        //Test Transfer Subcommand, offline sign
        let blockhash = Hash::new(&[1u8; 32]);
        let blockhash_string = blockhash.to_string();
        let test_transfer = test_commands.clone().get_matches_from(vec![
            "test",
            "transfer",
            &to_string,
            "42",
            "lamports",
            "--blockhash",
            &blockhash_string,
            "--sign-only",
        ]);
        assert_eq!(
            parse_command(&test_transfer).unwrap(),
            CliCommandInfo {
                command: CliCommand::Transfer {
                    lamports: 42,
                    to: to_pubkey,
                    from: None,
                    sign_only: true,
                    signers: None,
                    blockhash_query: BlockhashQuery::None(blockhash, FeeCalculator::default()),
                    nonce_account: None,
                    nonce_authority: None,
                    fee_payer: None,
                },
                require_keypair: true,
            }
        );

        //Test Transfer Subcommand, submit offline `from`
        let from_sig = from_keypair.sign_message(&[0u8]);
        let from_signer = format!("{}={}", from_pubkey, from_sig);
        let test_transfer = test_commands.clone().get_matches_from(vec![
            "test",
            "transfer",
            &to_string,
            "42",
            "lamports",
            "--from",
            &from_string,
            "--fee-payer",
            &from_string,
            "--signer",
            &from_signer,
            "--blockhash",
            &blockhash_string,
        ]);
        assert_eq!(
            parse_command(&test_transfer).unwrap(),
            CliCommandInfo {
                command: CliCommand::Transfer {
                    lamports: 42,
                    to: to_pubkey,
                    from: Some(from_pubkey.into()),
                    sign_only: false,
                    signers: Some(vec![(from_pubkey, from_sig)]),
                    blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
                    nonce_account: None,
                    nonce_authority: None,
                    fee_payer: Some(from_pubkey.into()),
                },
                require_keypair: true,
            }
        );

        //Test Transfer Subcommand, with nonce
        let nonce_address = Pubkey::new(&[1u8; 32]);
        let nonce_address_string = nonce_address.to_string();
        let nonce_authority = keypair_from_seed(&[2u8; 32]).unwrap();
        let nonce_authority_file = make_tmp_path("nonce_authority_file");
        write_keypair_file(&nonce_authority, &nonce_authority_file).unwrap();
        let test_transfer = test_commands.clone().get_matches_from(vec![
            "test",
            "transfer",
            &to_string,
            "42",
            "lamports",
            "--blockhash",
            &blockhash_string,
            "--nonce",
            &nonce_address_string,
            "--nonce-authority",
            &nonce_authority_file,
        ]);
        assert_eq!(
            parse_command(&test_transfer).unwrap(),
            CliCommandInfo {
                command: CliCommand::Transfer {
                    lamports: 42,
                    to: to_pubkey,
                    from: None,
                    sign_only: false,
                    signers: None,
                    blockhash_query: BlockhashQuery::FeeCalculator(blockhash),
                    nonce_account: Some(nonce_address.into()),
                    nonce_authority: Some(read_keypair_file(&nonce_authority_file).unwrap().into()),
                    fee_payer: None,
                },
                require_keypair: true,
            }
        );
    }
}
