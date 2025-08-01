use alloy::primitives::{Address, U256};
use dashmap::mapref::one::RefMut;
use foundry_evm::traces::CallKind;
use revm::interpreter::InstructionResult;
use revm_primitives::{AccessList, Bytes, Log};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;
use warp::reject::Rejection;
use warp::reply::Json;

use crate::errors::{
    FailedSettingBlockNumberError, FailedSettingBlockTimestampError, IncorrectChainIdError,
    InvalidBlockNumbersError, MultipleChainIdsError, NoBlockNumberError, StateNotFound,
};
use crate::evm::StorageOverride;
use crate::SharedSimulationState;

use super::config::Config;
use super::evm::{CallRawRequest, Evm};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulationRequest {
    pub chain_id: u64,
    pub from: Address,
    pub to: Address,
    pub data: Option<Bytes>,
    pub gas_limit: u64,
    pub value: Option<U256>,
    pub access_list: Option<AccessList>,
    pub block_number: Option<u64>,
    pub block_timestamp: Option<U256>,
    pub state_overrides: Option<HashMap<Address, StateOverride>>,
    pub format_trace: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SimulationResponse {
    pub simulation_id: u64,
    pub gas_used: u64,
    pub block_number: u64,
    pub success: bool,
    pub trace: Vec<CallTrace>,
    pub formatted_trace: Option<String>,
    pub logs: Vec<Log>,
    pub exit_reason: InstructionResult,
    pub return_data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSimulationRequest {
    pub chain_id: u64,
    pub gas_limit: u64,
    pub block_number: Option<u64>,
    pub block_timestamp: Option<U256>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatefulSimulationResponse {
    pub stateful_simulation_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatefulSimulationEndResponse {
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateOverride {
    pub balance: Option<U256>,
    pub nonce: Option<u64>,
    pub code: Option<Bytes>,
    #[serde(flatten)]
    pub state: Option<State>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum State {
    Full {
        state: HashMap<U256, U256>,
    },
    #[serde(rename_all = "camelCase")]
    Diff {
        state_diff: HashMap<U256, U256>,
    },
}

impl From<State> for StorageOverride {
    fn from(value: State) -> Self {
        let (slots, diff) = match value {
            State::Full { state } => (state, false),
            State::Diff { state_diff } => (state_diff, true),
        };

        StorageOverride {
            slots: slots
                .into_iter()
                .map(|(key, value)| (key, value.into()))
                .collect(),
            diff,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CallTrace {
    pub call_type: CallKind,
    pub from: Address,
    pub to: Address,
    pub value: U256,
}

async fn run(
    evm: &mut Evm,
    transaction: SimulationRequest,
    commit: bool,
) -> Result<SimulationResponse, Rejection> {
    for (address, state_override) in transaction.state_overrides.into_iter().flatten() {
        evm.override_account(
            address,
            state_override.balance.map(U256::from),
            state_override.nonce,
            state_override.code,
            state_override.state.map(StorageOverride::from),
        )?;
    }

    let call = CallRawRequest {
        from: transaction.from,
        to: transaction.to,
        value: transaction.value,
        data: transaction.data,
        access_list: transaction.access_list,
        format_trace: transaction.format_trace.unwrap_or(false),
    };
    let result = if commit {
        evm.transact_raw(call, transaction.gas_limit).await?
    } else {
        evm.call_raw(call).await?
    };

    Ok(SimulationResponse {
        simulation_id: 1,
        gas_used: result.gas_used,
        block_number: result.block_number,
        success: result.success,
        trace: result
            .trace
            .unwrap_or_default()
            .into_nodes()
            .into_iter()
            .map(CallTrace::from)
            .collect(),
        logs: result.logs,
        exit_reason: result.exit_reason,
        formatted_trace: result.formatted_trace,
        return_data: result.return_data,
    })
}

pub async fn simulate(transaction: SimulationRequest, config: Config) -> Result<Json, Rejection> {
    let fork_url = config.fork_url;
    let mut evm = Evm::new(
        None,
        fork_url,
        transaction.block_number,
        transaction.gas_limit,
        config.etherscan_key,
    )
    .await?;

    if evm.get_chain_id() != transaction.chain_id {
        return Err(warp::reject::custom(IncorrectChainIdError()));
    }

    if let Some(timestamp) = transaction.block_timestamp {
        evm.set_block_timestamp(U256::from(timestamp))
            .await
            .map_err(|_| warp::reject::custom(FailedSettingBlockTimestampError()))?;
    }

    let response = run(&mut evm, transaction, false).await?;

    Ok(warp::reply::json(&response))
}

pub async fn simulate_bundle(
    transactions: Vec<SimulationRequest>,
    config: Config,
) -> Result<Json, Rejection> {
    let first_chain_id = transactions[0].chain_id;
    let first_block_number = transactions[0].block_number;
    let first_block_timestamp = transactions[0].block_timestamp;

    let fork_url = config.fork_url;

    let mut evm = Evm::new(
        None,
        fork_url,
        first_block_number,
        transactions[0].gas_limit,
        config.etherscan_key,
    )
    .await?;

    if evm.get_chain_id() != first_chain_id {
        return Err(warp::reject::custom(IncorrectChainIdError()));
    }

    if let Some(timestamp) = first_block_timestamp {
        evm.set_block_timestamp(timestamp)
            .await
            .map_err(|_| warp::reject::custom(FailedSettingBlockTimestampError()))?;
    }

    let mut response = Vec::with_capacity(transactions.len());
    for transaction in transactions {
        if transaction.chain_id != first_chain_id {
            return Err(warp::reject::custom(MultipleChainIdsError()));
        }

        if transaction.block_number != first_block_number {
            let tx_block = U256::from(
                transaction
                    .block_number
                    .ok_or_else(|| NoBlockNumberError())?,
            );
            if transaction.block_number < first_block_number || tx_block < evm.get_block() {
                return Err(warp::reject::custom(InvalidBlockNumbersError()));
            }

            evm.set_block(tx_block)
                .await
                .map_err(|_| warp::reject::custom(FailedSettingBlockNumberError()))?;

            evm.set_block_timestamp(evm.get_block_timestamp() + U256::from(12)) // TOOD: make block time configurable
                .await
                .map_err(|_| warp::reject::custom(FailedSettingBlockTimestampError()))?;
        }

        response.push(run(&mut evm, transaction, true).await?);
    }

    Ok(warp::reply::json(&response))
}

pub async fn simulate_stateful_new(
    stateful_simulation_request: StatefulSimulationRequest,
    config: Config,
    state: Arc<SharedSimulationState>,
) -> Result<Json, Rejection> {
    let fork_url = config.fork_url;
    let mut evm = Evm::new(
        None,
        fork_url,
        stateful_simulation_request.block_number,
        stateful_simulation_request.gas_limit,
        config.etherscan_key,
    )
    .await?;

    if let Some(timestamp) = stateful_simulation_request.block_timestamp {
        evm.set_block_timestamp(U256::from(timestamp))
            .await
            .map_err(|_| warp::reject::custom(FailedSettingBlockTimestampError()))?;
    }

    let new_id = Uuid::new_v4();
    state.evms.insert(new_id, Arc::new(Mutex::new(evm)));

    let response = StatefulSimulationResponse {
        stateful_simulation_id: new_id,
    };

    Ok(warp::reply::json(&response))
}

pub async fn simulate_stateful_end(
    param: Uuid,
    state: Arc<SharedSimulationState>,
) -> Result<Json, Rejection> {
    if state.evms.contains_key(&param) {
        state.evms.remove(&param);
        let response = StatefulSimulationEndResponse { success: true };
        Ok(warp::reply::json(&response))
    } else {
        Err(warp::reject::custom(StateNotFound()))
    }
}

pub async fn simulate_stateful(
    param: Uuid,
    transactions: Vec<SimulationRequest>,
    state: Arc<SharedSimulationState>,
) -> Result<Json, Rejection> {
    let first_chain_id = transactions[0].chain_id;
    let first_block_number = transactions[0].block_number;

    let mut response = Vec::with_capacity(transactions.len());

    // Get a mutable reference to the EVM here.
    let evm_ref_mut: RefMut<'_, Uuid, Arc<Mutex<Evm>>> = state
        .evms
        .get_mut(&param)
        .ok_or_else(warp::reject::not_found)?;

    // Dereference to obtain the EVM.
    let evm = evm_ref_mut.value();
    let mut evm = evm.lock().await;

    if evm.get_chain_id() != first_chain_id {
        return Err(warp::reject::custom(IncorrectChainIdError()));
    }

    for transaction in transactions {
        if transaction.chain_id != first_chain_id {
            return Err(warp::reject::custom(MultipleChainIdsError()));
        }

        if transaction.block_number != first_block_number
            || U256::from(transaction.block_number.unwrap_or_default()) != evm.get_block()
        {
            let tx_block = U256::from(
                transaction
                    .block_number
                    .ok_or_else(|| NoBlockNumberError())?,
            );
            if transaction.block_number < first_block_number || tx_block < evm.get_block() {
                return Err(warp::reject::custom(InvalidBlockNumbersError()));
            }

            evm.set_block(tx_block)
                .await
                .map_err(|_| warp::reject::custom(FailedSettingBlockNumberError()))?;

            let block_timestamp = evm.get_block_timestamp();
            evm.set_block_timestamp(block_timestamp + U256::from(12)) // TOOD: make block time configurable
                .await
                .map_err(|_| warp::reject::custom(FailedSettingBlockTimestampError()))?;
        }

        response.push(run(&mut evm, transaction, true).await?);
    }

    Ok(warp::reply::json(&response))
}
