use alloy::eips::eip2930::AccessList;
use alloy::primitives::{Address, Bytes, Log, U256};
use foundry_config::Chain;
use foundry_evm::backend::Backend;
use foundry_evm::executors::{Executor, ExecutorBuilder};
use foundry_evm::fork::CreateFork;
use foundry_evm::opts::EvmOpts;
use foundry_evm::traces::identifier::{EtherscanIdentifier, SignaturesIdentifier};
use foundry_evm::traces::{
    CallTraceArena, CallTraceDecoder, CallTraceDecoderBuilder, CallTraceNode, TraceWriter,
};
use revm::{interpreter::InstructionResult, DatabaseCommit, DatabaseRef};
use revm_primitives::{Account, Bytecode, Env, EvmStorageSlot};
use std::collections::HashMap;

use crate::errors::{EvmCreateError, EvmError, OverrideError};
use crate::simulation::CallTrace;

#[derive(Debug, Clone)]
pub struct CallRawRequest {
    pub from: Address,
    pub to: Address,
    pub value: Option<U256>,
    pub data: Option<Bytes>,
    pub access_list: Option<AccessList>,
    pub format_trace: bool,
}

#[derive(Debug, Clone)]
pub struct CallRawResult {
    pub gas_used: u64,
    pub block_number: u64,
    pub success: bool,
    pub trace: Option<CallTraceArena>,
    pub logs: Vec<Log>,
    pub exit_reason: InstructionResult,
    pub return_data: Bytes,
    pub formatted_trace: Option<String>,
}

impl From<CallTraceNode> for CallTrace {
    fn from(item: CallTraceNode) -> Self {
        CallTrace {
            call_type: item.trace.kind,
            from: item.trace.caller,
            to: item.trace.address,
            value: item.trace.value,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageOverride {
    pub slots: HashMap<U256, U256>,
    pub diff: bool,
}

pub struct Evm {
    executor: Executor,
    decoder: CallTraceDecoder,
    etherscan_identifier: Option<EtherscanIdentifier>,
}

impl Evm {
    pub async fn new(
        env: Option<Env>,
        fork_url: String,
        fork_block_number: Option<u64>,
        gas_limit: u64,
        etherscan_key: Option<String>,
    ) -> Result<Self, EvmCreateError> {
        let evm_opts = EvmOpts {
            fork_url: Some(fork_url.clone()),
            fork_block_number,
            env: foundry_evm::opts::Env {
                chain_id: None,
                code_size_limit: None,
                gas_price: Some(0),
                gas_limit: u64::MAX,
                ..Default::default()
            },
            memory_limit: foundry_config::Config::default().memory_limit,
            ..Default::default()
        };

        let fork_opts = CreateFork {
            url: fork_url,
            enable_caching: true,
            env: evm_opts.evm_env().await.map_err(|err| {
                log::error!("Error creating EVM environment: {:?}", err);
                EvmCreateError(err)
            })?,
            evm_opts,
        };

        let db = Backend::spawn(Some(fork_opts.clone()));

        let builder = ExecutorBuilder::default().gas_limit(gas_limit.into());

        let executor = builder.build(env.unwrap_or(fork_opts.env.clone()), db);

        let foundry_config = foundry_config::Config {
            etherscan_api_key: etherscan_key,
            ..Default::default()
        };

        let chain: Chain = fork_opts.env.cfg.chain_id.into();
        let etherscan_identifier =
            EtherscanIdentifier::new(&foundry_config, Some(chain)).unwrap_or_default();

        let decoder = CallTraceDecoderBuilder::new().with_verbosity(5);

        let decoder = if let Ok(identifier) =
            SignaturesIdentifier::new(foundry_config::Config::foundry_cache_dir(), false)
        {
            decoder.with_signature_identifier(identifier)
        } else {
            decoder
        };

        Ok(Evm {
            executor,
            decoder: decoder.build(),
            etherscan_identifier,
        })
    }

    pub async fn call_raw(&mut self, call: CallRawRequest) -> Result<CallRawResult, EvmError> {
        self.set_access_list(call.access_list)?;
        let mut res = self
            .executor
            .call_raw(
                call.from,
                call.to,
                call.data.unwrap_or_default(),
                call.value.unwrap_or_default(),
            )
            .map_err(|err| {
                log::error!("Error calling raw: {:?}", err);
                EvmError(err)
            })?;

        let formatted_trace = if call.format_trace {
            let mut trace_writer = TraceWriter::new(Vec::<u8>::new());
            if let Some(trace) = &mut res.traces {
                if let Some(identifier) = &mut self.etherscan_identifier {
                    self.decoder.identify(trace, identifier);
                }
                trace_writer.write_arena(trace).map_err(|err| {
                    log::error!("Error writing trace: {:?}", err);
                    EvmError(err.into())
                })?;
            }
            Some(
                String::from_utf8(trace_writer.into_writer()).map_err(|err| {
                    log::error!("Error converting trace to string: {:?}", err);
                    EvmError(err.into())
                })?,
            )
        } else {
            None
        };

        Ok(CallRawResult {
            gas_used: res.gas_used,
            block_number: res.env.block.number.to(),
            success: !res.reverted,
            trace: None,
            logs: res.logs,
            exit_reason: res.exit_reason,
            return_data: res.result,
            formatted_trace,
        })
    }

    pub fn override_account(
        &mut self,
        address: Address,
        balance: Option<U256>,
        nonce: Option<u64>,
        code: Option<Bytes>,
        storage: Option<StorageOverride>,
    ) -> Result<(), OverrideError> {
        let mut account = Account {
            info: self
                .executor
                .backend()
                .basic_ref(address)
                .map_err(|_| OverrideError)?
                .unwrap_or_default(),
            ..Account::new_not_existing()
        };

        if let Some(balance) = balance {
            account.info.balance = balance;
        }
        if let Some(nonce) = nonce {
            account.info.nonce = nonce;
        }
        if let Some(code) = code {
            let bytecode = Bytecode::new_raw_checked(code.to_vec().into()).map_err(|err| {
                log::error!("Error overriding account code: {:?}", err);
                OverrideError
            })?;
            account.info.code = Some(bytecode);
        }
        if let Some(storage) = storage {
            // If we do a "full storage override", clear all current account storage
            if storage.diff {
                account.storage.clear();
            }
            account.storage.extend(
                storage
                    .slots
                    .into_iter()
                    .map(|(key, value)| (key, EvmStorageSlot::new(value))),
            );
        }

        self.executor
            .backend_mut()
            .commit([(address, account)].into_iter().collect());

        Ok(())
    }

    pub async fn transact_raw(
        &mut self,
        call: CallRawRequest,
        gas_limit: u64,
    ) -> Result<CallRawResult, EvmError> {
        self.executor.set_gas_limit(gas_limit);
        self.set_access_list(call.access_list)?;
        let mut res = self
            .executor
            .transact_raw(
                call.from,
                call.to,
                call.data.unwrap_or_default(),
                call.value.unwrap_or_default(),
            )
            .map_err(|err| {
                log::error!("Error transacting raw: {:?}", err);
                EvmError(err)
            })?;

        let formatted_trace = if call.format_trace {
            let mut trace_writer = TraceWriter::new(Vec::<u8>::new());
            if let Some(trace) = &mut res.traces {
                if let Some(identifier) = &mut self.etherscan_identifier {
                    self.decoder.identify(trace, identifier);
                }
                trace_writer.write_arena(trace).map_err(|err| {
                    log::error!("Error writing trace: {:?}", err);
                    EvmError(err.into())
                })?;
            }
            Some(
                String::from_utf8(trace_writer.into_writer()).map_err(|err| {
                    log::error!("Error converting trace to string: {:?}", err);
                    EvmError(err.into())
                })?,
            )
        } else {
            None
        };

        Ok(CallRawResult {
            gas_used: res.gas_used,
            block_number: res.env.block.number.to(),
            success: !res.reverted,
            trace: None,
            logs: res.logs,
            exit_reason: res.exit_reason,
            return_data: res.result,
            formatted_trace,
        })
    }

    pub async fn set_block(&mut self, number: U256) -> Result<(), EvmError> {
        self.executor.env_mut().block.number = number;
        Ok(())
    }

    pub fn get_block(&self) -> U256 {
        self.executor.env().block.number
    }

    pub async fn set_block_timestamp(&mut self, timestamp: U256) -> Result<(), EvmError> {
        self.executor.env_mut().block.timestamp = timestamp;
        Ok(())
    }

    pub fn get_block_timestamp(&self) -> U256 {
        self.executor.env().block.timestamp
    }

    pub fn get_chain_id(&self) -> u64 {
        self.executor.env().cfg.chain_id
    }

    fn set_access_list(&mut self, access_list: Option<AccessList>) -> Result<(), EvmError> {
        if let Some(access_list) = access_list {
            self.executor.env_mut().tx.access_list = access_list.into();
        }

        Ok(())
    }
}
