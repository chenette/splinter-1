// Copyright 2019 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, fmt, path::Path, time::SystemTime};

use protobuf::Message;
use sawtooth_sabre::handler::SabreTransactionHandler;
use sawtooth_sabre::{ADMINISTRATORS_SETTING_ADDRESS, ADMINISTRATORS_SETTING_KEY};
use transact::context::manager::sync::ContextManager;
use transact::database::{
    lmdb::{LmdbContext, LmdbDatabase},
    Database,
};
use transact::sawtooth::SawtoothToTransactHandlerAdapter;
use transact::scheduler::{
    serial::SerialScheduler, BatchExecutionResult, InvalidTransactionResult, Scheduler,
    TransactionExecutionResult,
};
use transact::state::{
    merkle::{MerkleRadixTree, MerkleState, INDEXES},
    StateChange, Write,
};
use transact::{
    execution::{adapter::static_adapter::StaticExecutionAdapter, executor::Executor},
    protocol::{batch::BatchPair, receipt::TransactionReceipt},
};

#[cfg(feature = "events")]
use crate::events::{ParseBytes, ParseError};
use crate::hex;
use crate::protos::scabbard::{Setting, Setting_Entry};
use crate::rest_api::{EventDealer, LocalEventHistory, Request, Response, ResponseError};

use super::error::ScabbardStateError;

const EXECUTION_TIMEOUT: u64 = 300; // five minutes

const CURRENT_STATE_ROOT_INDEX: &str = "current_state_root";

pub struct ScabbardState {
    db: Box<dyn Database>,
    context_manager: ContextManager,
    executor: Executor,
    current_state_root: String,
    pending_changes: Option<(String, Vec<StateChange>)>,
    event_dealer: EventDealer<Vec<StateChangeEvent>, LocalEventHistory<Vec<StateChangeEvent>>>,
    batch_history: BatchHistory,
}

impl ScabbardState {
    pub fn new(
        db_path: &Path,
        db_size: usize,
        admin_keys: Vec<String>,
    ) -> Result<Self, ScabbardStateError> {
        // Initialize the database
        let mut indexes = INDEXES.to_vec();
        indexes.push(CURRENT_STATE_ROOT_INDEX);
        let db = Box::new(LmdbDatabase::new(
            LmdbContext::new(db_path, indexes.len(), Some(db_size))?,
            &indexes,
        )?);

        let current_state_root = if let Some(current_state_root) =
            Self::read_current_state_root(&*db)?
        {
            debug!("Restoring scabbard state on root {}", current_state_root);
            current_state_root
        } else {
            // Set initial state (admin keys)
            let mut admin_keys_entry = Setting_Entry::new();
            admin_keys_entry.set_key(ADMINISTRATORS_SETTING_KEY.into());
            admin_keys_entry.set_value(admin_keys.join(","));
            let mut admin_keys_setting = Setting::new();
            admin_keys_setting.set_entries(vec![admin_keys_entry].into());
            let admin_keys_setting_bytes = admin_keys_setting.write_to_bytes().map_err(|err| {
                ScabbardStateError(format!(
                    "failed to write admin keys setting to bytes: {}",
                    err
                ))
            })?;
            let admin_keys_state_change = StateChange::Set {
                key: ADMINISTRATORS_SETTING_ADDRESS.into(),
                value: admin_keys_setting_bytes,
            };

            let initial_state_root = MerkleRadixTree::new(db.clone_box(), None)?.get_merkle_root();
            MerkleState::new(db.clone()).commit(
                &initial_state_root,
                vec![admin_keys_state_change].as_slice(),
            )?
        };

        // Initialize transact
        let context_manager = ContextManager::new(Box::new(MerkleState::new(db.clone())));
        let mut executor = Executor::new(vec![Box::new(StaticExecutionAdapter::new_adapter(
            vec![Box::new(SawtoothToTransactHandlerAdapter::new(
                SabreTransactionHandler::new(),
            ))],
            context_manager.clone(),
        )?)]);
        executor
            .start()
            .map_err(|err| ScabbardStateError(format!("failed to start executor: {}", err)))?;

        let event_dealer = EventDealer::new();

        Ok(ScabbardState {
            db,
            context_manager,
            executor,
            current_state_root,
            pending_changes: None,
            event_dealer,
            batch_history: BatchHistory::new(),
        })
    }

    fn read_current_state_root(db: &dyn Database) -> Result<Option<String>, ScabbardStateError> {
        db.get_reader()
            .and_then(|reader| reader.index_get(CURRENT_STATE_ROOT_INDEX, b"HEAD"))
            .map(|head| head.map(|bytes| hex::to_hex(&bytes)))
            .map_err(|e| ScabbardStateError(format!("Unable to read HEAD entry: {}", e)))
    }

    fn write_current_state_root(&self) -> Result<(), ScabbardStateError> {
        let current_root_bytes = hex::parse_hex(&self.current_state_root).map_err(|e| {
            ScabbardStateError(format!(
                "The in-memory current state root is invalid: {}",
                e
            ))
        })?;

        let mut writer = self.db.get_writer().map_err(|e| {
            ScabbardStateError(format!(
                "Unable to start write transaction for HEAD entry: {}",
                e
            ))
        })?;

        writer
            .index_put(CURRENT_STATE_ROOT_INDEX, b"HEAD", &current_root_bytes)
            .map_err(|e| ScabbardStateError(format!("Unable to write HEAD entry: {}", e)))?;

        writer
            .commit()
            .map_err(|e| ScabbardStateError(format!("Unable to commit HEAD entry: {}", e)))?;

        Ok(())
    }

    pub fn prepare_change(&mut self, batch: BatchPair) -> Result<String, ScabbardStateError> {
        // Setup the transact scheduler
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let mut scheduler = SerialScheduler::new(
            Box::new(self.context_manager.clone()),
            self.current_state_root.clone(),
        )?;
        scheduler.set_result_callback(Box::new(move |batch_result| {
            if result_tx.send(batch_result).is_err() {
                error!("Unable to send batch result; receiver must have dropped");
            }
        }))?;

        // Add the batch to, finalize, and execute the scheduler
        scheduler.add_batch(batch.clone())?;
        scheduler.finalize()?;
        self.executor
            .execute(scheduler.take_task_iterator()?, scheduler.new_notifier()?)?;

        // Get the results and shutdown the scheduler
        let batch_result = result_rx
            .recv_timeout(std::time::Duration::from_secs(EXECUTION_TIMEOUT))
            .map_err(|_| ScabbardStateError("failed to receive result in reasonable time".into()))?
            .ok_or_else(|| ScabbardStateError("no result returned from executor".into()))?;

        let batch_status = batch_result.clone().into();
        let signature = batch.batch().header_signature();
        self.batch_history
            .update_batch_status(&signature, batch_status);

        let txn_results = batch_result
            .results
            .into_iter()
            .map(|txn_result| match txn_result {
                TransactionExecutionResult::Valid(receipt) => Ok(receipt),
                TransactionExecutionResult::Invalid(invalid_result) => Err(ScabbardStateError(
                    format!("transaction failed: {:?}", invalid_result),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;

        let state_changes = txn_results
            .into_iter()
            .flat_map(|txn_result| {
                txn_result
                    .state_changes
                    .into_iter()
                    .map(into_writable_state_change)
            })
            .collect::<Vec<_>>();
        scheduler.shutdown();

        // Save the results and compute the resulting state root
        self.pending_changes = Some((signature.to_string(), state_changes.clone()));
        Ok(MerkleState::new(self.db.clone()).compute_state_id(
            &self.current_state_root,
            self.pending_changes.as_ref().unwrap().1.as_slice(),
        )?)
    }

    pub fn commit(&mut self) -> Result<(), ScabbardStateError> {
        match self.pending_changes.take() {
            Some((signature, state_changes)) => {
                self.current_state_root = MerkleState::new(self.db.clone())
                    .commit(&self.current_state_root, state_changes.as_slice())?;

                self.write_current_state_root()?;

                info!(
                    "committed {} change(s) for new state root {}",
                    state_changes.len(),
                    self.current_state_root,
                );

                let events = state_changes
                    .into_iter()
                    .map(StateChangeEvent::from_state_change)
                    .collect();

                if let Err(err) = self.event_dealer.dispatch(events) {
                    error!("An error occured while dispatching events {}", err);
                }

                self.batch_history.commit(&signature);

                Ok(())
            }
            None => Err(ScabbardStateError("no pending changes to commit".into())),
        }
    }

    pub fn rollback(&mut self) -> Result<(), ScabbardStateError> {
        match self.pending_changes.take() {
            Some((_, state_changes)) => info!("discarded {} change(s)", state_changes.len()),
            None => debug!("no changes to rollback"),
        }

        Ok(())
    }

    pub fn batch_history(&mut self) -> &mut BatchHistory {
        &mut self.batch_history
    }

    pub fn subscribe_to_state(&mut self, request: Request) -> Result<Response, ResponseError> {
        self.event_dealer.subscribe(request)
    }

    pub fn shutdown_event_dealer(&self) {
        self.event_dealer.stop();
    }
}

fn into_writable_state_change(
    change: transact::protocol::receipt::StateChange,
) -> transact::state::StateChange {
    match change {
        transact::protocol::receipt::StateChange::Set { key, value } => {
            transact::state::StateChange::Set { key, value }
        }
        transact::protocol::receipt::StateChange::Delete { key } => {
            transact::state::StateChange::Delete { key }
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "eventType", content = "message")]
pub enum StateChangeEvent {
    Set { key: String, value: Vec<u8> },
    Delete { key: String },
}

impl StateChangeEvent {
    fn from_state_change(state_change: StateChange) -> Self {
        match state_change {
            StateChange::Set { key, value } => StateChangeEvent::Set { key, value },
            StateChange::Delete { key } => StateChangeEvent::Delete { key },
        }
    }
}

impl fmt::Display for StateChangeEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            StateChangeEvent::Set { key, value } => {
                write!(f, "Set(key: {}, payload_size: {})", key, value.len())
            }
            StateChangeEvent::Delete { key } => write!(f, "Delete(key: {})", key),
        }
    }
}

impl fmt::Debug for StateChangeEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self)
    }
}

#[cfg(feature = "events")]
impl ParseBytes<StateChangeEvent> for StateChangeEvent {
    fn from_bytes(bytes: &[u8]) -> Result<StateChangeEvent, ParseError> {
        serde_json::from_slice(bytes)
            .map_err(Box::new)
            .map_err(|err| ParseError::MalformedMessage(err))
    }
}

#[cfg(feature = "events")]
impl ParseBytes<Vec<StateChangeEvent>> for Vec<StateChangeEvent> {
    fn from_bytes(bytes: &[u8]) -> Result<Vec<StateChangeEvent>, ParseError> {
        serde_json::from_slice(bytes)
            .map_err(Box::new)
            .map_err(|err| ParseError::MalformedMessage(err))
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "statusType", content = "message")]
pub enum BatchStatus {
    Unknown,
    Pending,
    Invalid(Vec<InvalidTransaction>),
    Valid(Vec<ValidTransaction>),
    Committed(Vec<ValidTransaction>),
}

impl From<BatchExecutionResult> for BatchStatus {
    fn from(batch_result: BatchExecutionResult) -> Self {
        let mut valid = Vec::new();
        let mut invalid = Vec::new();

        for result in batch_result.results.into_iter() {
            match result {
                TransactionExecutionResult::Valid(r) => {
                    valid.push(ValidTransaction::from(r));
                }
                TransactionExecutionResult::Invalid(r) => {
                    invalid.push(InvalidTransaction::from(r));
                }
            }
        }

        if !invalid.is_empty() {
            BatchStatus::Invalid(invalid)
        } else {
            BatchStatus::Valid(valid)
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidTransaction {
    transaction_id: String,
}

impl From<TransactionReceipt> for ValidTransaction {
    fn from(receipt: TransactionReceipt) -> Self {
        Self {
            transaction_id: receipt.transaction_id,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct InvalidTransaction {
    transaction_id: String,
    error_message: String,
    error_data: Vec<u8>,
}

impl From<InvalidTransactionResult> for InvalidTransaction {
    fn from(result: InvalidTransactionResult) -> Self {
        Self {
            transaction_id: result.transaction_id,
            error_message: result.error_message,
            error_data: result.error_data,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct BatchInfo {
    pub id: String,
    pub status: BatchStatus,
    #[serde(skip, default = "SystemTime::now")]
    pub timestamp: SystemTime,
}

impl BatchInfo {
    fn set_status(&mut self, status: BatchStatus) {
        self.status = status;
    }
}

/// BatchHistory keeps track of batches submitted to scabbard
pub struct BatchHistory {
    history: HashMap<String, BatchInfo>,
    limit: usize,
}

impl BatchHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_batch(&mut self, signature: &str) -> Result<(), ScabbardStateError> {
        self.history.insert(
            signature.to_string(),
            BatchInfo {
                id: signature.to_string(),
                status: BatchStatus::Pending,
                timestamp: SystemTime::now(),
            },
        );

        if self.history.len() > self.limit {
            self.history
                .clone()
                .into_iter()
                .min_by_key(|(_, v)| v.timestamp)
                .and_then(|(k, _)| self.history.remove(&k));
        }

        Ok(())
    }

    fn update_batch_status(&mut self, signature: &str, status: BatchStatus) {
        match self.history.get_mut(signature) {
            Some(ref mut batch) if batch.status == BatchStatus::Pending => {
                batch.set_status(status);
            }
            _ => (),
        };
    }

    fn commit(&mut self, signature: &str) {
        let info = if let Some(info) = self.history.get_mut(signature) {
            info
        } else {
            return;
        };

        if let BatchStatus::Valid(t) = info.status.clone() {
            info.set_status(BatchStatus::Committed(t));
        }
    }

    pub fn get_batch_info(&self, signature: &str) -> Result<BatchInfo, ScabbardStateError> {
        if let Some(info) = self.history.get(signature) {
            Ok(info.clone())
        } else {
            Ok(BatchInfo {
                id: signature.to_string(),
                status: BatchStatus::Unknown,
                timestamp: SystemTime::now(),
            })
        }
    }
}

impl Default for BatchHistory {
    fn default() -> Self {
        Self {
            history: HashMap::new(),
            limit: 100,
        }
    }
}
