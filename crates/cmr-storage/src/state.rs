use std::{collections::HashSet, path::Path, sync::Mutex, time::Duration};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{ProviderOwnerId, Result, StorageError};

/// Durable lifecycle state of one Responses request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    /// The upstream response may still emit output items.
    InProgress,
    /// The response finished normally.
    #[default]
    Completed,
    /// The response stopped early and may carry `incomplete_details`.
    Incomplete,
}

impl ResponseStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "incomplete" => Ok(Self::Incomplete),
            _ => Err(StorageError::InvalidConfig(format!(
                "database contains unsupported response status {value}"
            ))),
        }
    }
}

/// One response plus canonical input and output items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseRecord {
    /// Router or upstream response id.
    pub id: String,
    /// Stable logical session id.
    pub session_id: String,
    /// Parent response id.
    pub previous_response_id: Option<String>,
    /// Provider that generated this response.
    pub provider_id: String,
    /// Immutable provider/config/credential-generation owner of private items.
    ///
    /// `None` is accepted only for migration of historical rows. Such rows are
    /// never considered an owner match when private material is replayed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_owner_id: Option<ProviderOwnerId>,
    /// Picker model id.
    pub model_id: String,
    /// Canonical Responses input items for this turn.
    pub input: Vec<Value>,
    /// Canonical Responses output items for this turn.
    pub output: Vec<Value>,
    /// Lifecycle status.
    #[serde(default)]
    pub status: ResponseStatus,
    /// Canonical `incomplete_details`, when status is `incomplete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<Value>,
    /// UTC creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// One output item durably journaled before `response.output_item.done` delivery.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournaledOutputItem {
    /// Owning response id.
    pub response_id: String,
    /// Canonical Responses output index.
    pub output_index: u32,
    /// Complete canonical output item, including `function_call` items.
    pub item: Value,
    /// UTC time at which the completed item became durable.
    pub journaled_at: DateTime<Utc>,
}

/// A recorded provider/model transition inside one logical session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SwitchRecord {
    /// Logical session id.
    pub session_id: String,
    /// Previous provider/model key.
    pub from_model: String,
    /// New provider/model key.
    pub to_model: String,
    /// Response that first used the new model.
    pub response_id: String,
    /// UTC transition timestamp.
    pub created_at: DateTime<Utc>,
}

/// Mapping between a provider-neutral summary and a genuine compaction item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionRecord {
    /// Stable fingerprint of the opaque compaction item.
    ///
    /// The database column keeps its original `response_id` name for schema
    /// compatibility, but records use [`compaction_key`].
    pub response_id: String,
    /// Provider that produced the opaque item.
    pub source_provider: String,
    /// Immutable provider/config/credential-generation owner.
    ///
    /// A legacy `None` owner is safe for portable-summary replay only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_owner_id: Option<ProviderOwnerId>,
    /// Provider-neutral summary used when crossing owner boundaries.
    pub portable_summary: String,
    /// Genuine opaque `type=compaction` item returned by Responses.
    pub encrypted_item: Value,
    /// UTC creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Thread-safe `SQLite` conversation store.
#[derive(Debug)]
pub struct StateStore {
    connection: Mutex<Connection>,
}

impl StateStore {
    /// Opens or creates the database and applies idempotent migrations.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory cannot be created, the
    /// `SQLite` database cannot be opened, or schema and durability setup fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// Creates an isolated in-memory database.
    ///
    /// # Errors
    ///
    /// Returns an error when the in-memory database cannot be opened or its
    /// schema and durability settings cannot be initialized.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut connection: Connection) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys=ON;")?;

        // Schema discovery and every DDL statement share one exclusive
        // transaction. A second process waits for this transaction, then
        // rechecks columns after acquiring the lock instead of racing two
        // `ALTER TABLE` statements from stale observations.
        let migration = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
        migration.execute_batch(
            "CREATE TABLE IF NOT EXISTS responses (
               id TEXT PRIMARY KEY,
               session_id TEXT NOT NULL,
               previous_response_id TEXT,
               provider_id TEXT NOT NULL,
               provider_owner_id TEXT,
               model_id TEXT NOT NULL,
               input_json TEXT NOT NULL,
               output_json TEXT NOT NULL,
               status TEXT NOT NULL DEFAULT 'completed',
               incomplete_details_json TEXT,
               created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS responses_session_created
               ON responses(session_id, created_at);
             CREATE TABLE IF NOT EXISTS model_switches (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id TEXT NOT NULL,
               from_model TEXT NOT NULL,
               to_model TEXT NOT NULL,
               response_id TEXT NOT NULL REFERENCES responses(id),
               created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS model_switches_response
               ON model_switches(response_id);
             CREATE TABLE IF NOT EXISTS compactions (
               response_id TEXT PRIMARY KEY,
               source_provider TEXT NOT NULL,
               source_owner_id TEXT,
               portable_summary TEXT NOT NULL,
               encrypted_item_json TEXT NOT NULL,
               created_at TEXT NOT NULL
             );
              CREATE TABLE IF NOT EXISTS chatgpt_account_binding (
               singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
               account_id_sha256 BLOB NOT NULL CHECK(length(account_id_sha256) = 32)
             );
             CREATE TABLE IF NOT EXISTS router_metadata (
               key TEXT PRIMARY KEY,
               value TEXT NOT NULL
             );",
        )?;

        // Old databases are migrated without replacing or weakening existing rows.
        ensure_column(&migration, "responses", "provider_owner_id", "TEXT")?;
        ensure_column(
            &migration,
            "responses",
            "status",
            "TEXT NOT NULL DEFAULT 'completed'",
        )?;
        ensure_column(&migration, "responses", "incomplete_details_json", "TEXT")?;
        ensure_column(&migration, "compactions", "source_owner_id", "TEXT")?;
        migration.execute_batch(
            "CREATE TABLE IF NOT EXISTS response_output_journal (
               response_id TEXT NOT NULL REFERENCES responses(id) ON DELETE CASCADE,
               output_index INTEGER NOT NULL CHECK(output_index >= 0),
               item_json TEXT NOT NULL,
               journaled_at TEXT NOT NULL,
               PRIMARY KEY(response_id, output_index)
             );
             CREATE INDEX IF NOT EXISTS response_output_journal_created
               ON response_output_journal(journaled_at);",
        )?;
        migration.commit()?;
        configure_durability_pragmas(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Reports whether a SHA-256 digest matches the persisted `ChatGPT` account binding.
    ///
    /// `None` means that no account has completed trust-on-first-use binding yet.
    /// Only the fixed-size digest is persisted, never the account header value.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the account
    /// binding cannot be read from `SQLite`.
    pub fn chatgpt_account_matches(&self, digest: &[u8; 32]) -> Result<Option<bool>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let stored = connection
            .query_row(
                "SELECT account_id_sha256 FROM chatgpt_account_binding WHERE singleton=1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(stored.map(|stored| digest_matches(&stored, digest)))
    }

    /// Atomically creates or verifies the persistent `ChatGPT` account binding.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or the binding
    /// transaction cannot be read, written, or committed.
    pub fn bind_or_verify_chatgpt_account(&self, digest: &[u8; 32]) -> Result<bool> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO chatgpt_account_binding (singleton,account_id_sha256)
             VALUES (1,?1)",
            [digest.as_slice()],
        )?;
        let stored: Vec<u8> = transaction.query_row(
            "SELECT account_id_sha256 FROM chatgpt_account_binding WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let matches = digest_matches(&stored, digest);
        transaction.commit()?;
        Ok(matches)
    }

    /// Reads one persisted router metadata value, such as the query string a
    /// real client last used for a successful official catalog fetch.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or `SQLite` fails.
    pub fn metadata_get(&self, key: &str) -> Result<Option<String>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        connection
            .query_row(
                "SELECT value FROM router_metadata WHERE key=?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    /// Persists one router metadata value. The write is small and
    /// idempotent, so it uses an immediate transaction without retry loops.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned or `SQLite` fails.
    pub fn metadata_set(&self, key: &str, value: &str) -> Result<()> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO router_metadata (key,value) VALUES (?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            (key, value),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Returns the harness tool definitions from the most recent recorded
    /// request that carried an `additional_tools` input item. The client
    /// sends them once in a warmup frame and later turns rely on Responses
    /// server-side state, so cross-provider replays must reinject them.
    ///
    /// # Errors
    ///
    /// Returns an error when the connection lock is poisoned or `SQLite` fails.
    pub fn latest_harness_tools(&self) -> Result<Option<Vec<Value>>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT input_json FROM responses WHERE input_json LIKE '%additional_tools%'
             ORDER BY created_at DESC LIMIT 5",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            let Ok(payload) = row else {
                continue;
            };
            let Ok(items) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            let Some(items) = items.as_array() else {
                continue;
            };
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
                    continue;
                }
                if let Some(tools) = item.get("tools").and_then(Value::as_array) {
                    // Desktop warmups may carry only `namespace` declarations,
                    // which are not callable definitions; keep scanning for a
                    // set that actually contains custom/function tools.
                    let usable = tools.iter().any(|tool| {
                        matches!(
                            tool.get("type").and_then(Value::as_str),
                            Some("custom" | "function")
                        ) && tool
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| !name.is_empty())
                    });
                    if usable {
                        return Ok(Some(tools.clone()));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Starts a streaming response before any completed output item is delivered.
    ///
    /// The record must use `in_progress`, have an empty output list, and have no
    /// incomplete details. Repeating the exact same start is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid start record, conflicting existing
    /// response, poisoned connection lock, or failed `SQLite` transaction.
    pub fn begin_response(&self, record: &ResponseRecord) -> Result<()> {
        validate_response(record)?;
        if record.status != ResponseStatus::InProgress {
            return Err(StorageError::InvalidConfig(
                "begin_response requires status=in_progress".into(),
            ));
        }
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match query_response(&transaction, &record.id)? {
            None => insert_response(&transaction, record)?,
            Some(existing) if existing == *record => {}
            Some(_) => {
                return Err(StorageError::Conflict(format!(
                    "response {} already exists with different content",
                    record.id
                )));
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Journals a complete output item before its `output_item.done` event is sent.
    ///
    /// Repeating the same `(response_id, output_index, item)` is idempotent;
    /// attempting to reuse an index for different content is a conflict.
    ///
    /// # Errors
    ///
    /// Returns an error when the item cannot be serialized, the response is
    /// missing or terminal, the index conflicts, the connection lock is
    /// poisoned, or the `SQLite` transaction fails.
    pub fn journal_output_item(
        &self,
        response_id: &str,
        output_index: u32,
        item: &Value,
    ) -> Result<()> {
        let item_json = serde_json::to_string(item)?;
        let now = Utc::now();
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status: Option<String> = transaction
            .query_row(
                "SELECT status FROM responses WHERE id=?1",
                [response_id],
                |row| row.get(0),
            )
            .optional()?;
        match status.as_deref() {
            Some("in_progress") => {}
            Some(_) => {
                return Err(StorageError::Conflict(format!(
                    "response {response_id} is already terminal"
                )));
            }
            None => {
                return Err(StorageError::InvalidConfig(format!(
                    "unknown in-progress response: {response_id}"
                )));
            }
        }
        let existing: Option<String> = transaction
            .query_row(
                "SELECT item_json FROM response_output_journal
                 WHERE response_id=?1 AND output_index=?2",
                params![response_id, i64::from(output_index)],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            Some(existing) if serde_json::from_str::<Value>(&existing)? == *item => {}
            Some(_) => {
                return Err(StorageError::Conflict(format!(
                    "response {response_id} output index {output_index} was already journaled"
                )));
            }
            None => {
                transaction.execute(
                    "INSERT INTO response_output_journal
                     (response_id,output_index,item_json,journaled_at) VALUES (?1,?2,?3,?4)",
                    params![
                        response_id,
                        i64::from(output_index),
                        item_json,
                        now.to_rfc3339()
                    ],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Journals a complete `function_call` output item.
    ///
    /// # Errors
    ///
    /// Returns an error when `item` is not a `function_call`, or for any error
    /// described by [`Self::journal_output_item`].
    pub fn journal_function_call(
        &self,
        response_id: &str,
        output_index: u32,
        item: &Value,
    ) -> Result<()> {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return Err(StorageError::InvalidConfig(
                "journal_function_call requires a type=function_call item".into(),
            ));
        }
        self.journal_output_item(response_id, output_index, item)
    }

    /// Loads journaled output items in output-index order.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned, the query fails, or
    /// persisted JSON or timestamps are malformed.
    pub fn journaled_output_items(&self, response_id: &str) -> Result<Vec<JournaledOutputItem>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        query_journal(&connection, response_id)
    }

    /// Lists responses that were durable but not finalized before shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned, the query fails, or
    /// a persisted response is malformed.
    pub fn recoverable_in_progress(&self) -> Result<Vec<ResponseRecord>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        query_in_progress(&connection)
    }

    /// Atomically terminalizes responses interrupted by a prior process exit.
    ///
    /// Each response output is reconstructed from its durable journal in
    /// `output_index` order, marked as a standard `incomplete` response with a
    /// `router_restart` reason, and its model-switch row is committed in the same
    /// transaction. Journaled `function_call` items therefore remain available
    /// for a subsequent request containing their `function_call_output`.
    ///
    /// # Errors
    ///
    /// Returns an error if a persisted response or journal item is invalid, a
    /// concurrent update conflicts, the connection lock is poisoned, or the
    /// `SQLite` transaction fails.
    pub fn recover_interrupted_responses(&self) -> Result<Vec<ResponseRecord>> {
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut recovered = query_in_progress(&transaction)?;

        for record in &mut recovered {
            record.output = query_journal(&transaction, &record.id)?
                .into_iter()
                .map(|journal| journal.item)
                .collect();
            record.status = ResponseStatus::Incomplete;
            record.incomplete_details = Some(serde_json::json!({"reason":"router_restart"}));
            validate_response(record)?;
            let changed = transaction.execute(
                "UPDATE responses SET output_json=?2,status='incomplete',incomplete_details_json=?3
                 WHERE id=?1 AND status='in_progress'",
                params![
                    record.id,
                    serde_json::to_string(&record.output)?,
                    serde_json::to_string(&record.incomplete_details)?
                ],
            )?;
            if changed != 1 {
                return Err(StorageError::Conflict(format!(
                    "response {} changed while interrupted state was being recovered",
                    record.id
                )));
            }
        }
        for record in &recovered {
            persist_model_switch(&transaction, record)?;
        }
        transaction.commit()?;
        Ok(recovered)
    }

    /// Loads one response regardless of lifecycle state.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned, the query fails, or
    /// the persisted response is malformed.
    pub fn response(&self, response_id: &str) -> Result<Option<ResponseRecord>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        query_response(&connection, response_id)
    }

    /// Persists a terminal response and its model switch atomically.
    ///
    /// A response that contains a compaction output must instead use
    /// [`Self::record_response_with_compactions`] so the mapping cannot tear.
    ///
    /// # Errors
    ///
    /// Returns an error when the response is invalid or conflicts with persisted
    /// state, the connection lock is poisoned, or the transaction fails.
    pub fn record_response(&self, record: &ResponseRecord) -> Result<()> {
        self.record_response_with_compactions(record, &[])
    }

    /// Atomically finalizes/inserts a response, compaction mappings and model switch.
    ///
    /// Exact retries succeed. Any existing row with different content produces a
    /// conflict, and the entire transaction is rolled back.
    ///
    /// # Errors
    ///
    /// Returns an error when the response or mappings are invalid or conflicting,
    /// the connection lock is poisoned, or the atomic `SQLite` transaction fails.
    pub fn record_response_with_compactions(
        &self,
        record: &ResponseRecord,
        compactions: &[CompactionRecord],
    ) -> Result<()> {
        validate_response(record)?;
        if record.status == ResponseStatus::InProgress {
            return Err(StorageError::InvalidConfig(
                "terminal persistence requires completed or incomplete status".into(),
            ));
        }
        validate_response_compactions(record, compactions)?;
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        persist_terminal_response(&transaction, record)?;
        for compaction in compactions {
            persist_compaction(&transaction, compaction)?;
        }
        persist_model_switch(&transaction, record)?;
        transaction.commit()?;
        Ok(())
    }

    /// Persists a standalone compaction mapping with conflict validation.
    ///
    /// New response paths should prefer [`Self::record_response_with_compactions`].
    ///
    /// # Errors
    ///
    /// Returns an error when the mapping is invalid or conflicts with an existing
    /// row, the connection lock is poisoned, or the transaction fails.
    pub fn record_compaction(&self, record: &CompactionRecord) -> Result<()> {
        validate_compaction(record)?;
        let mut connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        persist_compaction(&transaction, record)?;
        transaction.commit()?;
        Ok(())
    }

    /// Loads a compaction mapping.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned, the query fails, or
    /// the persisted mapping is malformed.
    pub fn compaction(&self, response_id: &str) -> Result<Option<CompactionRecord>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        query_compaction(&connection, response_id)
    }

    /// Resolves a genuine opaque item to its provider-neutral mapping.
    ///
    /// # Errors
    ///
    /// Returns an error if `item` is not a standard compaction item, its key
    /// cannot be computed, or the mapping lookup fails.
    pub fn compaction_for_item(&self, item: &Value) -> Result<Option<CompactionRecord>> {
        self.compaction(&compaction_key(item)?)
    }

    /// Lists recorded model transitions oldest-first.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection lock is poisoned, the query fails, or
    /// a persisted transition timestamp is malformed.
    pub fn switches(&self, session_id: &str) -> Result<Vec<SwitchRecord>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT session_id,from_model,to_model,response_id,created_at
             FROM model_switches WHERE session_id=?1 ORDER BY id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (session_id, from_model, to_model, response_id, created) = row?;
            Ok(SwitchRecord {
                session_id,
                from_model,
                to_model,
                response_id,
                created_at: parse_timestamp(&created)?,
            })
        })
        .collect()
    }

    /// Loads an ancestry chain oldest-first, detecting cycles and in-progress rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the chain is missing, cyclic, contains an in-progress
    /// response, has malformed persisted data, or cannot be queried.
    pub fn ancestry(&self, response_id: &str) -> Result<Vec<ResponseRecord>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let (records, _) = query_ancestry(&connection, response_id)?;
        Ok(records)
    }

    /// Builds a portable-only replay for legacy callers without an owner id.
    ///
    /// Private reasoning and native compaction are never returned through this
    /// compatibility method, even when provider ids happen to match.
    ///
    /// # Errors
    ///
    /// Returns an error when ancestry or compaction mappings cannot be read, the
    /// chain is unsafe to replay, or persisted data is malformed.
    pub fn replay_items(&self, response_id: &str, target_provider: &str) -> Result<Vec<Value>> {
        self.replay_items_internal(response_id, target_provider, None, false)
    }

    /// Builds replay items for an exact provider/config/credential owner.
    ///
    /// # Errors
    ///
    /// Returns an error when ancestry or compaction mappings cannot be read, the
    /// chain is unsafe to replay, or persisted data is malformed.
    pub fn replay_items_for_owner(
        &self,
        response_id: &str,
        target_provider: &str,
        target_owner: &ProviderOwnerId,
    ) -> Result<Vec<Value>> {
        self.replay_items_internal(response_id, target_provider, Some(target_owner), false)
    }

    /// Builds the full replayable history for an external target, ignoring
    /// compaction boundaries: pre-compaction items are included and the
    /// compaction items themselves are skipped. The caller falls back to the
    /// boundary-preserving replay when the result does not fit the target
    /// model's context window or the chain predates the router.
    ///
    /// # Errors
    ///
    /// Returns an error when ancestry cannot be read, the chain starts at an
    /// opaque pre-router response, or persisted data is malformed.
    pub fn replay_items_full_for_owner(
        &self,
        response_id: &str,
        target_provider: &str,
        target_owner: Option<&ProviderOwnerId>,
    ) -> Result<Vec<Value>> {
        self.replay_items_internal(response_id, target_provider, target_owner, true)
    }

    fn replay_items_internal(
        &self,
        response_id: &str,
        target_provider: &str,
        target_owner: Option<&ProviderOwnerId>,
        full: bool,
    ) -> Result<Vec<Value>> {
        let connection = self.connection.lock().map_err(|_| StorageError::Poisoned)?;
        let (chain, truncated_at_opaque_root) = query_ancestry(&connection, response_id)?;
        let same_native_official_suffix = target_provider == "official"
            && target_owner.is_some_and(|owner| {
                chain
                    .last()
                    .and_then(|response| response.provider_owner_id.as_ref())
                    == Some(owner)
            });
        let mut items = Vec::new();
        let mut crossed_compaction = false;
        // A compaction boundary is a deliberate forgetting operation. Full
        // (lossless) replay must never resurrect pre-compaction content across
        // one; callers fall back to the compaction-boundary replay instead.
        let mut full_saw_compaction = false;
        for response in chain {
            for item in response.input {
                let is_compaction = item.get("type").and_then(Value::as_str) == Some("compaction");
                if full && is_compaction {
                    full_saw_compaction = true;
                    continue;
                }
                if let Some(item) = sanitize_replay_item(
                    &connection,
                    item,
                    response.provider_owner_id.as_ref(),
                    target_owner,
                    StoredItemLocation::Input,
                )? {
                    if is_compaction {
                        items.clear();
                        crossed_compaction = true;
                    }
                    items.push(item);
                }
            }
            for item in response.output {
                let is_compaction = item.get("type").and_then(Value::as_str) == Some("compaction");
                if full && is_compaction {
                    full_saw_compaction = true;
                    continue;
                }
                if let Some(item) = sanitize_replay_item(
                    &connection,
                    item,
                    response.provider_owner_id.as_ref(),
                    target_owner,
                    StoredItemLocation::Output,
                )? {
                    if is_compaction {
                        items.clear();
                        crossed_compaction = true;
                    }
                    items.push(item);
                }
            }
        }
        if full && full_saw_compaction {
            return Err(StorageError::InvalidConfig(
                "conversation history was compacted; replaying from the compaction boundary \
                 instead of the full pre-compaction chain"
                    .into(),
            ));
        }
        if truncated_at_opaque_root && !crossed_compaction && !same_native_official_suffix {
            return Err(StorageError::InvalidConfig(
                "conversation history begins at an opaque pre-router response; compact it with an official model before switching owners"
                    .into(),
            ));
        }
        Ok(items)
    }
}

fn configure_durability_pragmas(connection: &Connection) -> Result<()> {
    const ATTEMPTS: usize = 100;
    for attempt in 0..ATTEMPTS {
        match connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;",
        ) {
            Ok(()) => return Ok(()),
            Err(error) if sqlite_is_busy(&error) && attempt + 1 < ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("bounded durability pragma retry loop always returns")
}

fn sqlite_is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if !names.iter().any(|name| name == column) {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
        ))?;
    }
    Ok(())
}

fn validate_response(record: &ResponseRecord) -> Result<()> {
    for (label, value) in [
        ("response id", record.id.as_str()),
        ("session id", record.session_id.as_str()),
        ("provider id", record.provider_id.as_str()),
        ("model id", record.model_id.as_str()),
    ] {
        if value.is_empty() {
            return Err(StorageError::InvalidConfig(format!(
                "{label} cannot be empty"
            )));
        }
    }
    if record.previous_response_id.as_deref() == Some(record.id.as_str()) {
        return Err(StorageError::InvalidConfig(
            "a response cannot be its own parent".into(),
        ));
    }
    match record.status {
        ResponseStatus::InProgress => {
            if !record.output.is_empty() || record.incomplete_details.is_some() {
                return Err(StorageError::InvalidConfig(
                    "in-progress response must start with empty output and no incomplete_details"
                        .into(),
                ));
            }
        }
        ResponseStatus::Completed if record.incomplete_details.is_some() => {
            return Err(StorageError::InvalidConfig(
                "completed response cannot contain incomplete_details".into(),
            ));
        }
        ResponseStatus::Completed | ResponseStatus::Incomplete => {}
    }
    let compaction_count = record
        .output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        .count();
    if compaction_count > 1 {
        return Err(StorageError::InvalidConfig(
            "a response may contain exactly one compaction output item at most".into(),
        ));
    }
    Ok(())
}

fn validate_compaction(record: &CompactionRecord) -> Result<()> {
    if record.source_provider.is_empty() {
        return Err(StorageError::InvalidConfig(
            "compaction source_provider cannot be empty".into(),
        ));
    }
    if record.portable_summary.trim().is_empty() {
        return Err(StorageError::InvalidConfig(
            "compaction portable_summary cannot be empty".into(),
        ));
    }
    if record.encrypted_item.get("type").and_then(Value::as_str) != Some("compaction") {
        return Err(StorageError::InvalidConfig(
            "encrypted_item must be exactly one type=compaction output item".into(),
        ));
    }
    let actual_key = compaction_key(&record.encrypted_item)?;
    if record.response_id != actual_key {
        return Err(StorageError::InvalidConfig(
            "compaction response_id must equal compaction_key(encrypted_item)".into(),
        ));
    }
    Ok(())
}

fn validate_response_compactions(
    response: &ResponseRecord,
    compactions: &[CompactionRecord],
) -> Result<()> {
    let output_keys = response
        .output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        .map(compaction_key)
        .collect::<Result<Vec<_>>>()?;
    let mut mapping_keys = HashSet::new();
    for compaction in compactions {
        validate_compaction(compaction)?;
        if !mapping_keys.insert(compaction.response_id.as_str()) {
            return Err(StorageError::InvalidConfig(
                "duplicate compaction mapping in response transaction".into(),
            ));
        }
    }
    if output_keys.len() != compactions.len()
        || output_keys
            .iter()
            .any(|key| !mapping_keys.contains(key.as_str()))
    {
        return Err(StorageError::InvalidConfig(
            "response compaction outputs and transactional mappings must match exactly".into(),
        ));
    }
    Ok(())
}

fn insert_response(transaction: &Transaction<'_>, record: &ResponseRecord) -> Result<()> {
    let input = serde_json::to_string(&record.input)?;
    let output = serde_json::to_string(&record.output)?;
    let incomplete_details = record
        .incomplete_details
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    transaction.execute(
        "INSERT INTO responses
         (id,session_id,previous_response_id,provider_id,provider_owner_id,model_id,input_json,
          output_json,status,incomplete_details_json,created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            record.id,
            record.session_id,
            record.previous_response_id,
            record.provider_id,
            record
                .provider_owner_id
                .as_ref()
                .map(ProviderOwnerId::as_str),
            record.model_id,
            input,
            output,
            record.status.as_str(),
            incomplete_details,
            record.created_at.to_rfc3339()
        ],
    )?;
    Ok(())
}

fn persist_terminal_response(transaction: &Transaction<'_>, record: &ResponseRecord) -> Result<()> {
    let existing = query_response(transaction, &record.id)?;
    match existing {
        None => insert_response(transaction, record)?,
        Some(existing) if existing == *record => {}
        Some(existing) if existing.status == ResponseStatus::InProgress => {
            if !same_response_identity(&existing, record) {
                return Err(StorageError::Conflict(format!(
                    "response {} finalization changed immutable fields",
                    record.id
                )));
            }
            verify_journal_matches(transaction, record)?;
            let output = serde_json::to_string(&record.output)?;
            let incomplete_details = record
                .incomplete_details
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let changed = transaction.execute(
                "UPDATE responses SET output_json=?2,status=?3,incomplete_details_json=?4
                 WHERE id=?1 AND status='in_progress'",
                params![
                    record.id,
                    output,
                    record.status.as_str(),
                    incomplete_details
                ],
            )?;
            if changed != 1 {
                return Err(StorageError::Conflict(format!(
                    "response {} changed while it was being finalized",
                    record.id
                )));
            }
        }
        Some(_) => {
            return Err(StorageError::Conflict(format!(
                "response {} already exists with different terminal content",
                record.id
            )));
        }
    }
    Ok(())
}

fn same_response_identity(existing: &ResponseRecord, final_record: &ResponseRecord) -> bool {
    existing.id == final_record.id
        && existing.session_id == final_record.session_id
        && existing.previous_response_id == final_record.previous_response_id
        && existing.provider_id == final_record.provider_id
        && existing.provider_owner_id == final_record.provider_owner_id
        && existing.model_id == final_record.model_id
        && existing.input == final_record.input
        && existing.created_at == final_record.created_at
}

fn verify_journal_matches(transaction: &Transaction<'_>, record: &ResponseRecord) -> Result<()> {
    let journals = query_journal(transaction, &record.id)?;
    if journals.len() != record.output.len() {
        return Err(StorageError::Conflict(format!(
            "response {} terminal output was not completely journaled before delivery",
            record.id
        )));
    }
    for (expected_index, journal) in journals.into_iter().enumerate() {
        if journal.output_index as usize != expected_index {
            return Err(StorageError::Conflict(format!(
                "response {} journal has a gap before output index {}",
                record.id, journal.output_index
            )));
        }
        let Some(output) = record.output.get(journal.output_index as usize) else {
            return Err(StorageError::Conflict(format!(
                "response {} omitted journaled output index {}",
                record.id, journal.output_index
            )));
        };
        if output != &journal.item {
            return Err(StorageError::Conflict(format!(
                "response {} changed journaled output index {}",
                record.id, journal.output_index
            )));
        }
    }
    Ok(())
}

fn persist_compaction(transaction: &Transaction<'_>, record: &CompactionRecord) -> Result<()> {
    validate_compaction(record)?;
    match query_compaction(transaction, &record.response_id)? {
        None => {
            transaction.execute(
                "INSERT INTO compactions
                 (response_id,source_provider,source_owner_id,portable_summary,
                  encrypted_item_json,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    record.response_id,
                    record.source_provider,
                    record.source_owner_id.as_ref().map(ProviderOwnerId::as_str),
                    record.portable_summary,
                    serde_json::to_string(&record.encrypted_item)?,
                    record.created_at.to_rfc3339()
                ],
            )?;
        }
        Some(existing) if existing == *record => {}
        Some(_) => {
            return Err(StorageError::Conflict(format!(
                "compaction {} already has a different mapping",
                record.response_id
            )));
        }
    }
    Ok(())
}

fn persist_model_switch(transaction: &Transaction<'_>, record: &ResponseRecord) -> Result<()> {
    let expected = if let Some(parent_id) = &record.previous_response_id {
        query_response(transaction, parent_id)?.map(|parent| {
            if parent.session_id != record.session_id {
                return Err(StorageError::InvalidConfig(
                    "previous_response_id belongs to a different session".into(),
                ));
            }
            if parent.status == ResponseStatus::InProgress {
                return Err(StorageError::Conflict(
                    "cannot continue from an in-progress response".into(),
                ));
            }
            let from_model = format!("{}/{}", parent.provider_id, parent.model_id);
            let to_model = format!("{}/{}", record.provider_id, record.model_id);
            Ok((from_model != to_model).then_some(SwitchRecord {
                session_id: record.session_id.clone(),
                from_model,
                to_model,
                response_id: record.id.clone(),
                created_at: record.created_at,
            }))
        })
    } else {
        None
    }
    .transpose()?
    .flatten();

    let existing = query_switches_for_response(transaction, &record.id)?;
    match (expected, existing.as_slice()) {
        (None, []) => Ok(()),
        (Some(expected), []) => {
            transaction.execute(
                "INSERT INTO model_switches
                 (session_id,from_model,to_model,response_id,created_at)
                 VALUES (?1,?2,?3,?4,?5)",
                params![
                    expected.session_id,
                    expected.from_model,
                    expected.to_model,
                    expected.response_id,
                    expected.created_at.to_rfc3339()
                ],
            )?;
            Ok(())
        }
        (Some(expected), [existing]) if *existing == expected => Ok(()),
        _ => Err(StorageError::Conflict(format!(
            "model switch rows conflict for response {}",
            record.id
        ))),
    }
}

fn query_switches_for_response(
    connection: &Connection,
    response_id: &str,
) -> Result<Vec<SwitchRecord>> {
    let mut statement = connection.prepare(
        "SELECT session_id,from_model,to_model,response_id,created_at
         FROM model_switches WHERE response_id=?1 ORDER BY id",
    )?;
    let rows = statement.query_map([response_id], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (session_id, from_model, to_model, response_id, created) = row?;
        Ok(SwitchRecord {
            session_id,
            from_model,
            to_model,
            response_id,
            created_at: parse_timestamp(&created)?,
        })
    })
    .collect()
}

fn query_journal(connection: &Connection, response_id: &str) -> Result<Vec<JournaledOutputItem>> {
    let mut statement = connection.prepare(
        "SELECT response_id,output_index,item_json,journaled_at
         FROM response_output_journal WHERE response_id=?1 ORDER BY output_index",
    )?;
    let rows = statement.query_map([response_id], |row| {
        Ok((
            row.get(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    rows.map(|row| {
        let (response_id, output_index, item, journaled_at) = row?;
        Ok(JournaledOutputItem {
            response_id,
            output_index,
            item: serde_json::from_str(&item)?,
            journaled_at: parse_timestamp(&journaled_at)?,
        })
    })
    .collect()
}

fn query_in_progress(connection: &Connection) -> Result<Vec<ResponseRecord>> {
    let mut statement = connection.prepare(
        "SELECT id,session_id,previous_response_id,provider_id,provider_owner_id,
                model_id,input_json,output_json,status,incomplete_details_json,created_at
         FROM responses WHERE status='in_progress' ORDER BY created_at,id",
    )?;
    let rows = statement.query_map([], raw_response_from_row)?;
    rows.map(|row| response_from_raw(row?)).collect()
}

fn query_ancestry(
    connection: &Connection,
    response_id: &str,
) -> Result<(Vec<ResponseRecord>, bool)> {
    let mut current = Some(response_id.to_owned());
    let mut seen = HashSet::new();
    let mut records = Vec::new();
    let mut truncated_at_opaque_root = false;
    while let Some(id) = current.take() {
        if records.len() >= 10_000 || !seen.insert(id.clone()) {
            return Err(StorageError::InvalidConfig(
                "response ancestry contains a cycle or is too deep".into(),
            ));
        }
        let Some(record) = query_response(connection, &id)? else {
            if records.is_empty() {
                return Err(StorageError::InvalidConfig(format!(
                    "unknown previous_response_id: {id}"
                )));
            }
            truncated_at_opaque_root = true;
            break;
        };
        if record.status == ResponseStatus::InProgress {
            return Err(StorageError::Conflict(format!(
                "response {id} is still in progress"
            )));
        }
        current.clone_from(&record.previous_response_id);
        records.push(record);
    }
    records.reverse();
    Ok((records, truncated_at_opaque_root))
}

#[derive(Debug)]
struct RawResponse {
    id: String,
    session_id: String,
    previous_response_id: Option<String>,
    provider_id: String,
    provider_owner_id: Option<String>,
    model_id: String,
    input_json: String,
    output_json: String,
    status: String,
    incomplete_details_json: Option<String>,
    created_at: String,
}

fn raw_response_from_row(row: &Row<'_>) -> rusqlite::Result<RawResponse> {
    Ok(RawResponse {
        id: row.get(0)?,
        session_id: row.get(1)?,
        previous_response_id: row.get(2)?,
        provider_id: row.get(3)?,
        provider_owner_id: row.get(4)?,
        model_id: row.get(5)?,
        input_json: row.get(6)?,
        output_json: row.get(7)?,
        status: row.get(8)?,
        incomplete_details_json: row.get(9)?,
        created_at: row.get(10)?,
    })
}

fn response_from_raw(raw: RawResponse) -> Result<ResponseRecord> {
    Ok(ResponseRecord {
        id: raw.id,
        session_id: raw.session_id,
        previous_response_id: raw.previous_response_id,
        provider_id: raw.provider_id,
        provider_owner_id: raw
            .provider_owner_id
            .as_deref()
            .map(ProviderOwnerId::parse)
            .transpose()?,
        model_id: raw.model_id,
        input: serde_json::from_str(&raw.input_json)?,
        output: serde_json::from_str(&raw.output_json)?,
        status: ResponseStatus::parse(&raw.status)?,
        incomplete_details: raw
            .incomplete_details_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?,
        created_at: parse_timestamp(&raw.created_at)?,
    })
}

fn query_response(connection: &Connection, id: &str) -> Result<Option<ResponseRecord>> {
    connection
        .query_row(
            "SELECT id,session_id,previous_response_id,provider_id,provider_owner_id,
                    model_id,input_json,output_json,status,incomplete_details_json,created_at
             FROM responses WHERE id=?1",
            [id],
            raw_response_from_row,
        )
        .optional()?
        .map(response_from_raw)
        .transpose()
}

#[derive(Debug)]
struct RawCompaction {
    response_id: String,
    source_provider: String,
    source_owner_id: Option<String>,
    portable_summary: String,
    encrypted_item_json: String,
    created_at: String,
}

fn query_compaction(connection: &Connection, id: &str) -> Result<Option<CompactionRecord>> {
    let raw = connection
        .query_row(
            "SELECT response_id,source_provider,source_owner_id,portable_summary,
                    encrypted_item_json,created_at
             FROM compactions WHERE response_id=?1",
            [id],
            |row| {
                Ok(RawCompaction {
                    response_id: row.get(0)?,
                    source_provider: row.get(1)?,
                    source_owner_id: row.get(2)?,
                    portable_summary: row.get(3)?,
                    encrypted_item_json: row.get(4)?,
                    created_at: row.get(5)?,
                })
            },
        )
        .optional()?;
    raw.map(|raw| {
        Ok(CompactionRecord {
            response_id: raw.response_id,
            source_provider: raw.source_provider,
            source_owner_id: raw
                .source_owner_id
                .as_deref()
                .map(ProviderOwnerId::parse)
                .transpose()?,
            portable_summary: raw.portable_summary,
            encrypted_item: serde_json::from_str(&raw.encrypted_item_json)?,
            created_at: parse_timestamp(&raw.created_at)?,
        })
    })
    .transpose()
}

fn digest_matches(stored: &[u8], candidate: &[u8; 32]) -> bool {
    stored.len() == candidate.len()
        && stored
            .iter()
            .zip(candidate)
            .fold(0_u8, |difference, (left, right)| {
                difference | (*left ^ *right)
            })
            == 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredItemLocation {
    Input,
    Output,
}

fn sanitize_replay_item(
    connection: &Connection,
    mut item: Value,
    response_owner: Option<&ProviderOwnerId>,
    target_owner: Option<&ProviderOwnerId>,
    location: StoredItemLocation,
) -> Result<Option<Value>> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let metadata_owner = provider_metadata_owner(&item);
    let exact_metadata_owner =
        target_owner.is_some_and(|target| metadata_owner == Some(target.as_str()));
    let exact_output_owner = location == StoredItemLocation::Output
        && target_owner.is_some_and(|target| response_owner == Some(target));

    if kind == "reasoning" {
        return Ok((exact_metadata_owner || exact_output_owner).then_some(item));
    }

    if kind == "compaction" {
        let key = compaction_key(&item)?;
        if let Some(mapping) = query_compaction(connection, &key)? {
            return if target_owner
                .is_some_and(|target| mapping.source_owner_id.as_ref() == Some(target))
            {
                Ok(Some(item))
            } else {
                Ok(Some(portable_summary_item(&mapping.portable_summary)))
            };
        }
        if exact_metadata_owner || exact_output_owner {
            return Ok(Some(item));
        }
        return Err(StorageError::InvalidConfig(format!(
            "portable mapping is unavailable for compaction {key}"
        )));
    }

    if item.get("provider_metadata").is_some() && !exact_metadata_owner {
        if let Some(object) = item.as_object_mut() {
            object.remove("provider_metadata");
        }
    }
    Ok(Some(item))
}

fn provider_metadata_owner(item: &Value) -> Option<&str> {
    item.pointer("/provider_metadata/cmr_provider_owner_id")
        .and_then(Value::as_str)
}

/// Returns a stable identifier without exposing opaque encrypted content.
///
/// # Errors
///
/// Returns [`StorageError::InvalidConfig`] if `item` is not a standard
/// `type=compaction` item, or a serialization error when an item without
/// `encrypted_content` cannot be encoded deterministically.
pub fn compaction_key(item: &Value) -> Result<String> {
    if item.get("type").and_then(Value::as_str) != Some("compaction") {
        return Err(StorageError::InvalidConfig(
            "compaction key requires a type=compaction item".into(),
        ));
    }
    let mut digest = Sha256::new();
    if let Some(encrypted) = item.get("encrypted_content").and_then(Value::as_str) {
        digest.update(encrypted.as_bytes());
    } else {
        digest.update(serde_json::to_vec(item)?);
    }
    Ok(format!("cmp_sha256_{:x}", digest.finalize()))
}

fn portable_summary_item(summary: &str) -> Value {
    serde_json::json!({
        "type": "message",
        "role": "developer",
        "content": [{"type": "input_text", "text": summary}],
        "metadata": {"cmr_portable_compaction": true}
    })
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .map_err(|error| StorageError::InvalidConfig(error.to_string()))?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigInstanceId;
    use std::sync::{Arc, Barrier};

    fn owner(provider: &str, generation: &str) -> ProviderOwnerId {
        let instance = ConfigInstanceId::parse(&"1".repeat(64)).expect("instance");
        ProviderOwnerId::for_credential_generation(
            &instance,
            provider,
            "https://example.test/v1",
            generation,
        )
        .expect("owner")
    }

    fn response(
        id: &str,
        parent: Option<&str>,
        provider: &str,
        model: &str,
        provider_owner: Option<ProviderOwnerId>,
    ) -> ResponseRecord {
        ResponseRecord {
            id: id.into(),
            session_id: "session-1".into(),
            previous_response_id: parent.map(str::to_owned),
            provider_id: provider.into(),
            provider_owner_id: provider_owner,
            model_id: model.into(),
            input: vec![serde_json::json!({
                "type":"message","role":"user","content":"hello"
            })],
            output: vec![
                serde_json::json!({"type":"reasoning","encrypted_content":"opaque"}),
                serde_json::json!({
                    "type":"message","role":"assistant","content":"world"
                }),
            ],
            status: ResponseStatus::Completed,
            incomplete_details: None,
            created_at: Utc::now(),
        }
    }

    fn compaction(item: Value, source_owner: ProviderOwnerId) -> CompactionRecord {
        CompactionRecord {
            response_id: compaction_key(&item).expect("key"),
            source_provider: "official".into(),
            source_owner_id: Some(source_owner),
            portable_summary: "portable state".into(),
            encrypted_item: item,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn account_binding_handles_first_same_and_different_digest() {
        let store = StateStore::in_memory().expect("database");
        let first: [u8; 32] = Sha256::digest(b"workspace-one").into();
        let different: [u8; 32] = Sha256::digest(b"workspace-two").into();
        assert_eq!(
            store.chatgpt_account_matches(&first).expect("unbound"),
            None
        );
        assert!(store.bind_or_verify_chatgpt_account(&first).expect("bind"));
        assert_eq!(
            store.chatgpt_account_matches(&first).expect("same"),
            Some(true)
        );
        assert!(
            !store
                .bind_or_verify_chatgpt_account(&different)
                .expect("different")
        );
    }

    #[test]
    fn response_status_and_incomplete_details_round_trip() {
        let store = StateStore::in_memory().expect("database");
        let mut incomplete = response("r1", None, "zhipu", "glm-5.2", Some(owner("zhipu", "one")));
        incomplete.status = ResponseStatus::Incomplete;
        incomplete.incomplete_details = Some(serde_json::json!({"reason":"max_output_tokens"}));
        store.record_response(&incomplete).expect("record");
        assert_eq!(store.response("r1").expect("load"), Some(incomplete));
    }

    #[test]
    fn durable_journal_is_idempotent_recoverable_and_checked_at_finalize() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("state.db");
        let call = serde_json::json!({
            "type":"function_call","call_id":"call-1","name":"lookup","arguments":"{}"
        });
        let mut started = response("r1", None, "zhipu", "glm-5.2", Some(owner("zhipu", "one")));
        started.status = ResponseStatus::InProgress;
        started.output.clear();
        {
            let store = StateStore::open(&path).expect("database");
            store.begin_response(&started).expect("begin");
            store
                .journal_function_call("r1", 0, &call)
                .expect("journal");
            store
                .journal_function_call("r1", 0, &call)
                .expect("idempotent journal");
        }
        let store = StateStore::open(&path).expect("reopen");
        assert_eq!(
            store.recoverable_in_progress().expect("recover"),
            vec![started.clone()]
        );
        assert_eq!(
            store.journaled_output_items("r1").expect("items")[0].item,
            call
        );

        let mut final_record = started;
        final_record.status = ResponseStatus::Completed;
        final_record.output = vec![call.clone()];
        store.record_response(&final_record).expect("finalize");
        assert_eq!(
            store.response("r1").expect("load").expect("record").status,
            ResponseStatus::Completed
        );
        assert!(store.journal_output_item("r1", 1, &call).is_err());
    }

    #[test]
    fn startup_recovery_terminalizes_journal_and_allows_function_output_continuation() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("state.db");
        let provider_owner = owner("zhipu", "generation-one");
        let call = serde_json::json!({
            "type":"function_call","call_id":"call-1","name":"lookup","arguments":"{}"
        });
        let mut started = response(
            "interrupted",
            None,
            "zhipu",
            "glm-5.2",
            Some(provider_owner.clone()),
        );
        started.status = ResponseStatus::InProgress;
        started.output.clear();
        {
            let store = StateStore::open(&path).expect("database");
            store.begin_response(&started).expect("begin");
            store
                .journal_function_call("interrupted", 0, &call)
                .expect("journal function call");
        }

        let store = StateStore::open(&path).expect("reopen database");
        let recovered = store
            .recover_interrupted_responses()
            .expect("recover interrupted response");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].status, ResponseStatus::Incomplete);
        assert_eq!(recovered[0].output, vec![call.clone()]);
        assert_eq!(
            recovered[0].incomplete_details,
            Some(serde_json::json!({"reason":"router_restart"}))
        );
        assert!(
            store
                .recover_interrupted_responses()
                .expect("idempotent second recovery")
                .is_empty()
        );

        let mut continuation = response(
            "continued",
            Some("interrupted"),
            "zhipu",
            "glm-5.2",
            Some(provider_owner.clone()),
        );
        continuation.input = vec![serde_json::json!({
            "type":"function_call_output","call_id":"call-1","output":"result"
        })];
        continuation.output = vec![serde_json::json!({
            "type":"message","role":"assistant","content":"done"
        })];
        store
            .record_response(&continuation)
            .expect("continue from recovered function call");
        let replay = store
            .replay_items_for_owner("continued", "zhipu", &provider_owner)
            .expect("replay continuation");
        assert!(replay.iter().any(|item| item == &call));
        assert!(replay.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
        }));
    }

    #[test]
    fn journal_conflicts_cannot_change_delivered_function_call() {
        let store = StateStore::in_memory().expect("database");
        let mut started = response("r1", None, "zhipu", "glm-5.2", Some(owner("zhipu", "one")));
        started.status = ResponseStatus::InProgress;
        started.output.clear();
        store.begin_response(&started).expect("begin");
        let first = serde_json::json!({"type":"function_call","name":"one"});
        let changed = serde_json::json!({"type":"function_call","name":"two"});
        store.journal_output_item("r1", 0, &first).expect("first");
        assert!(store.journal_output_item("r1", 0, &changed).is_err());
        let mut final_record = started;
        final_record.status = ResponseStatus::Completed;
        final_record.output = vec![changed];
        assert!(store.record_response(&final_record).is_err());
        assert_eq!(
            store.response("r1").expect("load").expect("record").status,
            ResponseStatus::InProgress
        );
    }

    #[test]
    fn finalization_rejects_output_that_was_not_journaled_before_delivery() {
        let store = StateStore::in_memory().expect("database");
        let mut started = response("r1", None, "zhipu", "glm-5.2", Some(owner("zhipu", "one")));
        started.status = ResponseStatus::InProgress;
        started.output.clear();
        store.begin_response(&started).expect("begin");
        let first = serde_json::json!({"type":"message","content":"journaled"});
        let unjournaled = serde_json::json!({"type":"message","content":"not durable"});
        store.journal_output_item("r1", 0, &first).expect("journal");

        let mut final_record = started;
        final_record.status = ResponseStatus::Completed;
        final_record.output = vec![first, unjournaled];
        assert!(matches!(
            store.record_response(&final_record),
            Err(StorageError::Conflict(_))
        ));
    }

    #[test]
    fn response_compaction_and_model_switch_commit_together() {
        let store = StateStore::in_memory().expect("database");
        let official_owner = owner("official", "account-one");
        let external_owner = owner("zhipu", "generation-one");
        store
            .record_response(&response(
                "parent",
                None,
                "official",
                "gpt",
                Some(official_owner),
            ))
            .expect("parent");
        let item = serde_json::json!({
            "type":"compaction","encrypted_content":"external-opaque"
        });
        let mapping = compaction(item.clone(), external_owner.clone());
        let mut child = response(
            "child",
            Some("parent"),
            "zhipu",
            "glm-5.2",
            Some(external_owner),
        );
        child.output = vec![item];
        store
            .record_response_with_compactions(&child, &[mapping.clone()])
            .expect("atomic record");
        assert_eq!(
            store.compaction(&mapping.response_id).expect("mapping"),
            Some(mapping)
        );
        assert_eq!(store.switches("session-1").expect("switches").len(), 1);
    }

    #[test]
    fn compaction_conflict_rolls_back_response_and_switch() {
        let store = StateStore::in_memory().expect("database");
        let official_owner = owner("official", "account-one");
        let external_owner = owner("zhipu", "generation-one");
        store
            .record_response(&response(
                "parent",
                None,
                "official",
                "gpt",
                Some(official_owner),
            ))
            .expect("parent");
        let item = serde_json::json!({"type":"compaction","encrypted_content":"opaque"});
        let stored = compaction(item.clone(), external_owner.clone());
        store.record_compaction(&stored).expect("seed mapping");
        let mut conflicting = stored.clone();
        conflicting.portable_summary = "different summary".into();
        let mut child = response(
            "child",
            Some("parent"),
            "zhipu",
            "glm-5.2",
            Some(external_owner),
        );
        child.output = vec![item];
        assert!(
            store
                .record_response_with_compactions(&child, &[conflicting])
                .is_err()
        );
        assert_eq!(store.response("child").expect("lookup"), None);
        assert!(store.switches("session-1").expect("switches").is_empty());
    }

    #[test]
    fn owner_generation_controls_reasoning_replay_and_legacy_is_safe() {
        let store = StateStore::in_memory().expect("database");
        let first_owner = owner("zhipu", "generation-one");
        let rotated_owner = owner("zhipu", "generation-two");
        store
            .record_response(&response(
                "owned",
                None,
                "zhipu",
                "glm-5.2",
                Some(first_owner.clone()),
            ))
            .expect("owned");
        let same = store
            .replay_items_for_owner("owned", "zhipu", &first_owner)
            .expect("same owner");
        assert!(
            same.iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        );
        let rotated = store
            .replay_items_for_owner("owned", "zhipu", &rotated_owner)
            .expect("rotated owner");
        assert!(
            rotated
                .iter()
                .all(|item| item.get("type").and_then(Value::as_str) != Some("reasoning"))
        );

        store
            .record_response(&response("legacy", None, "zhipu", "glm-5.2", None))
            .expect("legacy");
        let legacy = store
            .replay_items_for_owner("legacy", "zhipu", &first_owner)
            .expect("legacy replay");
        assert!(
            legacy
                .iter()
                .all(|item| item.get("type").and_then(Value::as_str) != Some("reasoning"))
        );
    }

    #[test]
    fn legacy_source_provider_metadata_is_never_treated_as_an_owner() {
        let store = StateStore::in_memory().expect("database");
        let target_owner = owner("zhipu", "generation-one");
        let mut record = response(
            "legacy-metadata",
            None,
            "zhipu",
            "glm-5.2",
            Some(target_owner.clone()),
        );
        record.input = vec![serde_json::json!({
            "type":"reasoning",
            "encrypted_content":"legacy-private",
            "provider_metadata":{"source_provider_id":target_owner.as_str()}
        })];
        record.output = vec![serde_json::json!({
            "type":"message","role":"assistant","content":"portable"
        })];
        store.record_response(&record).expect("record");

        let replay = store
            .replay_items_for_owner("legacy-metadata", "zhipu", &target_owner)
            .expect("replay");
        assert!(
            replay
                .iter()
                .all(|item| { item.get("type").and_then(Value::as_str) != Some("reasoning") })
        );
    }

    #[test]
    fn compaction_replays_native_only_to_exact_owner() {
        let store = StateStore::in_memory().expect("database");
        let first_owner = owner("official", "account-one");
        let rotated_owner = owner("official", "account-two");
        let item = serde_json::json!({"type":"compaction","encrypted_content":"opaque"});
        let mapping = compaction(item.clone(), first_owner.clone());
        let mut record = response(
            "compact",
            None,
            "official",
            "gpt",
            Some(first_owner.clone()),
        );
        record.output = vec![item.clone()];
        store
            .record_response_with_compactions(&record, &[mapping])
            .expect("record");
        assert_eq!(
            store
                .replay_items_for_owner("compact", "official", &first_owner)
                .expect("native"),
            vec![item]
        );
        let portable = store
            .replay_items_for_owner("compact", "official", &rotated_owner)
            .expect("portable");
        assert_eq!(portable.len(), 1);
        assert_eq!(
            portable[0].pointer("/metadata/cmr_portable_compaction"),
            Some(&Value::Bool(true))
        );
        let legacy_api = store
            .replay_items("compact", "official")
            .expect("legacy API");
        assert_eq!(
            legacy_api[0].pointer("/metadata/cmr_portable_compaction"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn conflicting_retry_never_replaces_terminal_record() {
        let store = StateStore::in_memory().expect("database");
        let original = response("r1", None, "zhipu", "glm-5.2", Some(owner("zhipu", "one")));
        store.record_response(&original).expect("first");
        store.record_response(&original).expect("idempotent");
        let mut changed = original.clone();
        changed.output = vec![serde_json::json!({"type":"message","content":"changed"})];
        assert!(store.record_response(&changed).is_err());
        assert_eq!(store.response("r1").expect("load"), Some(original));
    }

    #[test]
    fn migrates_legacy_rows_as_completed_without_owner() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("legacy.db");
        {
            let connection = Connection::open(&path).expect("legacy database");
            connection
                .execute_batch(
                    "CREATE TABLE responses (
                       id TEXT PRIMARY KEY,session_id TEXT NOT NULL,previous_response_id TEXT,
                       provider_id TEXT NOT NULL,model_id TEXT NOT NULL,input_json TEXT NOT NULL,
                       output_json TEXT NOT NULL,created_at TEXT NOT NULL
                     );
                     CREATE TABLE compactions (
                       response_id TEXT PRIMARY KEY,source_provider TEXT NOT NULL,
                       portable_summary TEXT NOT NULL,encrypted_item_json TEXT NOT NULL,
                       created_at TEXT NOT NULL
                     );",
                )
                .expect("schema");
            connection
                .execute(
                    "INSERT INTO responses
                     (id,session_id,provider_id,model_id,input_json,output_json,created_at)
                     VALUES ('legacy','s','zhipu','glm','[]','[]',?1)",
                    [Utc::now().to_rfc3339()],
                )
                .expect("row");
        }
        let store = StateStore::open(path).expect("migrate");
        let legacy = store.response("legacy").expect("load").expect("row");
        assert_eq!(legacy.status, ResponseStatus::Completed);
        assert_eq!(legacy.provider_owner_id, None);
    }

    #[test]
    fn concurrent_openers_serialize_and_recheck_the_entire_schema_migration() {
        const OPENERS: usize = 4;
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("concurrent-legacy.db");
        {
            let connection = Connection::open(&path).expect("legacy database");
            connection
                .execute_batch(
                    "CREATE TABLE responses (
                       id TEXT PRIMARY KEY,session_id TEXT NOT NULL,previous_response_id TEXT,
                       provider_id TEXT NOT NULL,model_id TEXT NOT NULL,input_json TEXT NOT NULL,
                       output_json TEXT NOT NULL,created_at TEXT NOT NULL
                     );
                     CREATE TABLE compactions (
                       response_id TEXT PRIMARY KEY,source_provider TEXT NOT NULL,
                       portable_summary TEXT NOT NULL,encrypted_item_json TEXT NOT NULL,
                       created_at TEXT NOT NULL
                     );",
                )
                .expect("legacy schema");
        }

        let barrier = Arc::new(Barrier::new(OPENERS + 1));
        let handles = (0..OPENERS)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    StateStore::open(path).expect("concurrent migration")
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for handle in handles {
            drop(handle.join().expect("migration thread"));
        }

        let connection = Connection::open(path).expect("inspect database");
        let mut statement = connection
            .prepare("PRAGMA table_info(responses)")
            .expect("response columns");
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query columns")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect columns");
        for expected in ["provider_owner_id", "status", "incomplete_details_json"] {
            assert_eq!(
                columns.iter().filter(|column| column == &expected).count(),
                1,
                "column {expected} must be migrated exactly once"
            );
        }
        let journal_exists: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='response_output_journal'",
                [],
                |row| row.get(0),
            )
            .expect("journal table");
        assert_eq!(journal_exists, 1);
    }

    #[test]
    fn compaction_requires_standard_item_and_matching_key() {
        let store = StateStore::in_memory().expect("database");
        let invalid = CompactionRecord {
            response_id: "not-a-key".into(),
            source_provider: "official".into(),
            source_owner_id: Some(owner("official", "one")),
            portable_summary: "summary".into(),
            encrypted_item: serde_json::json!({"type":"message"}),
            created_at: Utc::now(),
        };
        assert!(store.record_compaction(&invalid).is_err());
        let item = serde_json::json!({"type":"compaction","encrypted_content":"opaque"});
        let mut wrong_key = compaction(item, owner("official", "one"));
        wrong_key.response_id = "wrong".into();
        assert!(store.record_compaction(&wrong_key).is_err());
    }
}
