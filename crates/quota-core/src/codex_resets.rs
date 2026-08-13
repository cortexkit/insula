//! Codex banked-reset credits, trigger policy, and crash-safe redemption journal.
//!
//! Mutation is fenced by a pending journal record written before every POST. The
//! file lives at `$XDG_STATE_HOME/cortexkit/ck-quota/redemptions.json` (default
//! `~/.local/state/cortexkit/ck-quota/redemptions.json`). Tests and supervised
//! deployments may override the containing directory with `CK_QUOTA_STATE_DIR`.
//! The journal is host-local, so only one host should arm a given account.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::credential_source::{CredentialSource, VaultCapability};
use crate::http::{Header, JsonRequest};
use crate::model::{CreditExpiry, SavedResets, Usage};
use crate::provider::FetchError;

pub const CREDITS_PATH: &str = "/wham/rate-limit-reset-credits";
pub const CONSUME_PATH: &str = "/wham/rate-limit-reset-credits/consume";
pub const CREDITS_TIMEOUT: Duration = Duration::from_secs(8);
pub const CONSUME_TIMEOUT: Duration = Duration::from_secs(8);
pub const PRE_POST_CUTOFF: Duration = Duration::from_secs(20);
pub const PENDING_RETRY_INTERVAL: Duration = Duration::from_secs(60);
pub const CREDIT_SAFETY_MARGIN_SECS: i64 = 60;
pub const PENDING_OLD_AFTER_SECS: i64 = 24 * 60 * 60;
pub const SPEND_BOUND_SECS: i64 = 30 * 60;
pub const RESOLVED_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// One verifiably available reset credit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetCredit {
    pub id: String,
    pub expires_at: DateTime<Utc>,
}

/// Normalized credit inventory from the dedicated credits endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditsSnapshot {
    pub available: Vec<ResetCredit>,
    pub reported_available_count: u64,
    /// Expiries from every available response item, including entries without an
    /// id that are unsafe to redeem but still useful for display.
    pub available_expiries: Vec<DateTime<Utc>>,
}

impl CreditsSnapshot {
    /// The soonest expiry among all available credits, including ones too close
    /// to expiry to redeem safely.
    ///
    /// For diagnostics only -- it answers "what does the account hold", not
    /// "what could be spent". Use [`Self::earliest_usable_expiry`] for any
    /// decision about redeeming, or a credit inside the safety margin will be
    /// treated as spendable.
    pub fn earliest_available_expiry(&self) -> Option<DateTime<Utc>> {
        self.available.iter().map(|credit| credit.expires_at).min()
    }

    /// The soonest expiry among credits that can still be safely redeemed.
    ///
    /// Credits within [`CREDIT_SAFETY_MARGIN_SECS`] of expiring are excluded: a
    /// redemption is not instantaneous, and one raced against its own expiry can
    /// be consumed without resetting anything -- spending an irreplaceable credit
    /// for nothing.
    ///
    /// This is the one to reach for when the answer feeds an action. Its
    /// unfiltered twin above looks equivalent and reads more simply, which is
    /// exactly how a credit inside the margin gets spent for nothing.
    pub fn earliest_usable_expiry(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let safety_cutoff = now + chrono::Duration::seconds(CREDIT_SAFETY_MARGIN_SECS);
        self.available
            .iter()
            .filter(|credit| credit.expires_at > safety_cutoff)
            .map(|credit| credit.expires_at)
            .min()
    }

    /// The read-only credit inventory as it goes on the wire.
    ///
    /// Timestamps use the canonical formatter rather than `to_rfc3339`, which
    /// picks its precision from the value and so gives one instant several
    /// spellings. An expiry landing on a whole second would print with no
    /// fractional part at all beside siblings carrying nine digits.
    pub fn saved_resets(&self) -> SavedResets {
        SavedResets {
            available_count: self.reported_available_count.min(u32::MAX as u64) as u32,
            soonest_expires_at: self
                .available_expiries
                .iter()
                .min()
                .copied()
                .map(crate::rfc3339_canonical),
            credits: self
                .available_expiries
                .iter()
                .map(|expires_at| CreditExpiry {
                    expires_at: crate::rfc3339_canonical(*expires_at),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawCreditsResponse {
    #[serde(default)]
    credits: Vec<RawCredit>,
    #[serde(default)]
    available_count: u64,
}

#[derive(Debug, Deserialize)]
struct RawCredit {
    id: Option<String>,
    status: Option<String>,
    expires_at: Option<String>,
}

/// Normalize the live credits response. Available entries without a trustworthy
/// id are excluded from mutation inventory, while valid expiries remain displayable.
/// Entries without an RFC3339 expiry are discarded because neither path can use them.
pub fn normalize_credits(body: &[u8]) -> Result<CreditsSnapshot, FetchError> {
    let response: RawCreditsResponse = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("credits response not decodable: {error}")))?;
    let mut available = Vec::new();
    let mut available_expiries = Vec::new();
    for credit in response
        .credits
        .into_iter()
        .filter(|credit| credit.status.as_deref() == Some("available"))
    {
        let Some(expires_at) = credit
            .expires_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
        else {
            continue;
        };
        available_expiries.push(expires_at);
        if let Some(id) = credit.id.filter(|id| !id.is_empty()) {
            available.push(ResetCredit { id, expires_at });
        }
    }
    Ok(CreditsSnapshot {
        available,
        reported_available_count: response.available_count,
        available_expiries,
    })
}

/// Prefer the upstream HTTP `Date` header to hedge gross local clock skew.
pub fn response_now(date_header: Option<&str>, local_now: DateTime<Utc>) -> DateTime<Utc> {
    date_header
        .and_then(|value| {
            DateTime::parse_from_rfc2822(value)
                .map(|parsed| parsed.with_timezone(&Utc))
                .ok()
                .or_else(|| {
                    NaiveDateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT")
                        .map(|parsed| parsed.and_utc())
                        .ok()
                })
        })
        .unwrap_or(local_now)
}

/// Window facts used by both trigger and honest-reporting gates.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageFacts {
    pub raw_percents: Vec<f64>,
    pub any_used_floor: bool,
    pub at_wall: bool,
    pub wall_clear: bool,
}

impl UsageFacts {
    pub fn from_usage(usage: &Usage, limit_reached: Option<bool>) -> Self {
        // Enumerated through the shared helper so a slot added to the wire type
        // cannot be missed here. These percents decide whether the account is at
        // its wall, and a missed slot reads as *lower* usage than the account
        // really has -- reporting a walled account as having room.
        let raw_percents: Vec<f64> = crate::model::windows(usage)
            .map(|window| window.used_percent)
            .collect();
        Self {
            any_used_floor: raw_percents.iter().any(|percent| *percent >= 1.0),
            at_wall: limit_reached == Some(true)
                || raw_percents.iter().any(|percent| *percent >= 99.0),
            wall_clear: limit_reached == Some(false),
            raw_percents,
        }
    }

    pub fn below_wall(&self) -> bool {
        self.wall_clear && !self.at_wall
    }
}

/// Inputs to the pure reset-trigger truth table.
#[derive(Debug, Clone)]
pub struct TriggerInput {
    pub armed: bool,
    pub now: DateTime<Utc>,
    pub earliest_expiry: Option<DateTime<Utc>>,
    pub auto_use_resets_secs: u64,
    pub any_used_floor: bool,
    pub at_wall: bool,
    pub pending: bool,
    pub spend_bound_allows: bool,
    pub before_post_cutoff: bool,
}

/// Individual trigger reasons and the fully fenced fire decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerDecision {
    pub expiry_trigger: bool,
    pub exhaustion_trigger: bool,
    pub fire: bool,
}

pub fn evaluate_trigger(input: &TriggerInput) -> TriggerDecision {
    let expiry_trigger = input.any_used_floor
        && input.earliest_expiry.is_some_and(|expiry| {
            expiry.signed_duration_since(input.now).num_seconds()
                <= input.auto_use_resets_secs.min(i64::MAX as u64) as i64
        });
    let exhaustion_trigger = input.at_wall;
    let fire = input.armed
        && (expiry_trigger || exhaustion_trigger)
        && !input.pending
        && input.spend_bound_allows
        && input.before_post_cutoff;
    TriggerDecision {
        expiry_trigger,
        exhaustion_trigger,
        fire,
    }
}

/// Only a fresh, below-wall, mutation-free, journal-clean tick may relax output.
pub fn reporting_eligible(
    armed: bool,
    facts: &UsageFacts,
    consume_attempted: bool,
    pending: bool,
    journal_ok: bool,
) -> bool {
    armed && facts.below_wall() && !consume_attempted && !pending && journal_ok
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumeOutcome {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

impl ConsumeOutcome {
    pub fn as_code(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::NothingToReset => "nothing_to_reset",
            Self::NoCredit => "no_credit",
            Self::AlreadyRedeemed => "already_redeemed",
        }
    }
}

#[derive(Debug, Deserialize)]
struct ConsumeResponse {
    code: String,
}

pub fn normalize_consume_response(body: &[u8]) -> Result<ConsumeOutcome, FetchError> {
    let response: ConsumeResponse = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("consume response not decodable: {error}")))?;
    match response.code.as_str() {
        "reset" => Ok(ConsumeOutcome::Reset),
        "nothing_to_reset" => Ok(ConsumeOutcome::NothingToReset),
        "no_credit" => Ok(ConsumeOutcome::NoCredit),
        "already_redeemed" => Ok(ConsumeOutcome::AlreadyRedeemed),
        code => Err(FetchError::Decode(format!(
            "consume response has unknown code {code:?}"
        ))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStatus {
    Pending,
    Resolved,
}

/// Durable record for one logical redemption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedemptionRecord {
    pub account_id: String,
    pub redeem_request_id: String,
    pub created_at: String,
    #[serde(default)]
    pub last_attempt_at: Option<String>,
    #[serde(default)]
    pub attempt_count: u32,
    pub status: JournalStatus,
    pub outcome: Option<ConsumeOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalError(String);

impl JournalError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for JournalError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountJournalState {
    pub pending_id: Option<String>,
    pub spend_bound_allows: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reservation {
    New(String),
    ExistingPending(String),
    SpendBound,
    NoAction,
}

/// Filesystem-local redemption journal. Callers serialize load/modify/save
/// operations; each save itself is atomic temp-file + rename.
#[derive(Debug, Clone)]
pub struct RedemptionJournal {
    path: PathBuf,
}

impl RedemptionJournal {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn from_env() -> Result<Self, JournalError> {
        Ok(Self::new(redemption_journal_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn records(&self) -> Result<Vec<RedemptionRecord>, JournalError> {
        self.load()
    }

    /// Prove at startup that this journal can be written, and say where it is.
    ///
    /// The path is announced because a relocated journal is otherwise
    /// indistinguishable from a fresh one: a missing file reads as an empty
    /// history, so the module starts clean and looks healthy while every
    /// pending record at the previous location is unfenced. `CK_QUOTA_STATE_DIR`
    /// moves this directory ahead of both XDG_STATE_HOME and the default, so
    /// the difference between "no redemptions yet" and "the history is
    /// somewhere else" is invisible in every other surface -- the wire, health,
    /// and the journal's own contents all look identical.
    ///
    /// One line at startup rather than a check, because there is nothing to
    /// check against: a first run on a new host is legitimately empty, and no
    /// stored marker can distinguish it from a moved directory without becoming
    /// the state it is trying to verify. Printing the resolved path lets an
    /// operator compare it against the one they expect, which is the only
    /// comparison that can be made from outside.
    fn probe_atomic_write(&self) -> Result<(), JournalError> {
        let records = self.load()?;
        eprintln!(
            "[ck-quota] codex reset journal: {} ({} record(s))",
            self.path.display(),
            records.len()
        );
        self.save(&records)
    }

    /// Prune old resolved records and report pending state. Pending ids are never
    /// abandoned automatically: even an old record remains the only id for its
    /// logical redemption until the server supplies a resolvable outcome.
    pub fn inspect_account(
        &self,
        account_id: &str,
        now: DateTime<Utc>,
    ) -> Result<AccountJournalState, JournalError> {
        let mut records = self.load()?;
        if prune_records(&mut records, now)? > 0 {
            self.save(&records)?;
        }
        let pending = records.iter().find(|record| {
            record.account_id == account_id && record.status == JournalStatus::Pending
        });
        if let Some(record) = pending {
            let latest = parse_record_latest_time(record)?;
            if now.signed_duration_since(latest).num_seconds() > PENDING_OLD_AFTER_SECS {
                eprintln!(
                    "[ck-quota] codex reset journal pending-old account_id={} redeem_request_id={} attempt_count={}",
                    account_id, record.redeem_request_id, record.attempt_count
                );
            }
        }
        let pending_id = pending.map(|record| record.redeem_request_id.clone());
        let spend_bound_allows = !records.iter().any(|record| {
            record.account_id == account_id
                && record.status == JournalStatus::Resolved
                && parse_record_latest_time(record).is_ok_and(|latest| {
                    now.signed_duration_since(latest).num_seconds() < SPEND_BOUND_SECS
                })
        });
        Ok(AccountJournalState {
            pending_id,
            spend_bound_allows,
        })
    }

    /// Reserve a request id durably before network mutation. Existing pending ids
    /// are always reused; a fresh id is never minted alongside one.
    pub fn reserve(
        &self,
        account_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Reservation, JournalError> {
        let state = self.inspect_account(account_id, now)?;
        if let Some(id) = state.pending_id {
            return Ok(Reservation::ExistingPending(id));
        }
        if !state.spend_bound_allows {
            return Ok(Reservation::SpendBound);
        }

        let id = Uuid::new_v4().to_string();
        let mut records = self.load()?;
        records.push(RedemptionRecord {
            account_id: account_id.to_string(),
            redeem_request_id: id.clone(),
            created_at: now.to_rfc3339(),
            last_attempt_at: None,
            attempt_count: 0,
            status: JournalStatus::Pending,
            outcome: None,
        });
        self.save(&records)?;
        eprintln!(
            "[ck-quota] codex reset journal reserve account_id={account_id} redeem_request_id={id}"
        );
        Ok(Reservation::New(id))
    }

    /// Persist attempt metadata immediately before sending a consume POST.
    pub fn record_attempt(
        &self,
        account_id: &str,
        redeem_request_id: &str,
        attempted_at: DateTime<Utc>,
    ) -> Result<(), JournalError> {
        let mut records = self.load()?;
        let record = records
            .iter_mut()
            .find(|record| {
                record.account_id == account_id
                    && record.redeem_request_id == redeem_request_id
                    && record.status == JournalStatus::Pending
            })
            .ok_or_else(|| {
                JournalError::new(format!(
                    "pending redemption not found for account {account_id} id {redeem_request_id}"
                ))
            })?;
        record.last_attempt_at = Some(attempted_at.to_rfc3339());
        record.attempt_count = record.attempt_count.saturating_add(1);
        let attempt_count = record.attempt_count;
        self.save(&records)?;
        eprintln!(
            "[ck-quota] codex reset journal attempt account_id={account_id} redeem_request_id={redeem_request_id} attempt_count={attempt_count}"
        );
        Ok(())
    }

    pub fn resolve(
        &self,
        account_id: &str,
        redeem_request_id: &str,
        outcome: ConsumeOutcome,
    ) -> Result<(), JournalError> {
        let mut records = self.load()?;
        let record = records
            .iter_mut()
            .find(|record| {
                record.account_id == account_id
                    && record.redeem_request_id == redeem_request_id
                    && record.status == JournalStatus::Pending
            })
            .ok_or_else(|| {
                JournalError::new(format!(
                    "pending redemption not found for account {account_id} id {redeem_request_id}"
                ))
            })?;
        record.status = JournalStatus::Resolved;
        record.outcome = Some(outcome);
        self.save(&records)?;
        eprintln!(
            "[ck-quota] codex reset journal resolve account_id={account_id} redeem_request_id={redeem_request_id} outcome={}",
            outcome.as_code()
        );
        Ok(())
    }

    fn load(&self) -> Result<Vec<RedemptionRecord>, JournalError> {
        let body = match fs::read(&self.path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(JournalError::new(format!(
                    "reading {}: {error}",
                    self.path.display()
                )))
            }
        };
        let records: Vec<RedemptionRecord> = serde_json::from_slice(&body).map_err(|error| {
            JournalError::new(format!("decoding {}: {error}", self.path.display()))
        })?;
        for record in &records {
            parse_record_latest_time(record)?;
        }
        Ok(records)
    }

    fn save(&self, records: &[RedemptionRecord]) -> Result<(), JournalError> {
        let parent = self.path.parent().ok_or_else(|| {
            JournalError::new(format!("{} has no parent directory", self.path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            JournalError::new(format!("creating {}: {error}", parent.display()))
        })?;
        let body = serde_json::to_vec_pretty(records)
            .map_err(|error| JournalError::new(format!("encoding journal: {error}")))?;
        let temp_path = parent.join(format!(
            ".redemptions-{}.tmp",
            Uuid::new_v4().as_hyphenated()
        ));
        let write_result = (|| {
            let mut file = File::create(&temp_path).map_err(|error| {
                JournalError::new(format!("creating {}: {error}", temp_path.display()))
            })?;
            file.write_all(&body).map_err(|error| {
                JournalError::new(format!("writing {}: {error}", temp_path.display()))
            })?;
            file.sync_all().map_err(|error| {
                JournalError::new(format!("syncing {}: {error}", temp_path.display()))
            })?;
            fs::rename(&temp_path, &self.path).map_err(|error| {
                JournalError::new(format!(
                    "renaming {} to {}: {error}",
                    temp_path.display(),
                    self.path.display()
                ))
            })?;
            sync_parent_directory(parent)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result
    }
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), JournalError> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| JournalError::new(format!("syncing {}: {error}", parent.display())))
}

#[cfg(windows)]
fn sync_parent_directory(_parent: &Path) -> Result<(), JournalError> {
    // Nothing to do here, but NOT because the guarantee is unnecessary -- this
    // is a gap, and it is recorded rather than papered over.
    //
    // Windows has no documented equivalent of fsync on a directory. A directory
    // handle can be opened with FILE_FLAG_BACKUP_SEMANTICS, but FlushFileBuffers
    // is specified in terms of a file's data and does not promise directory-entry
    // durability, so calling it here would look like a fix while guaranteeing
    // nothing.
    //
    // The durable sequence on Windows puts the guarantee in the RENAME instead:
    // MoveFileExW with MOVEFILE_WRITE_THROUGH does not return until the move is
    // on disk. Rust's fs::rename does not pass that flag, so the rename above is
    // not crash-durable on Windows as written -- a crash in the window after it
    // returns can lose the rename while both the old and new contents survive.
    //
    // What that costs: this journal fences a banked reset credit against being
    // spent twice. Losing a rename can lose the record of a redemption that
    // already happened. The upstream consume endpoint is idempotent on the
    // request id, which is the outer fence, so the failure is bounded -- but the
    // local half of a two-part fence is weaker here than on Unix, and that is
    // worth knowing before anyone relies on it.
    //
    // Not yet fixed because the feature is off unless `auto_use_resets` is
    // configured, and no Windows host has configured it. Fixing it means calling
    // MoveFileExW directly rather than fs::rename.
    Ok(())
}

fn parse_record_timestamp(
    record: &RedemptionRecord,
    field: &str,
    value: &str,
) -> Result<DateTime<Utc>, JournalError> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|error| {
            JournalError::new(format!(
                "invalid {field} for redemption {}: {error}",
                record.redeem_request_id
            ))
        })
}

fn parse_record_latest_time(record: &RedemptionRecord) -> Result<DateTime<Utc>, JournalError> {
    let created_at = parse_record_timestamp(record, "created_at", &record.created_at)?;
    let last_attempt_at = record
        .last_attempt_at
        .as_deref()
        .map(|value| parse_record_timestamp(record, "last_attempt_at", value))
        .transpose()?;
    Ok(last_attempt_at.map_or(created_at, |attempted_at| attempted_at.max(created_at)))
}

fn prune_records(
    records: &mut Vec<RedemptionRecord>,
    now: DateTime<Utc>,
) -> Result<usize, JournalError> {
    let before = records.len();
    let mut keep = Vec::with_capacity(before);
    for record in records.drain(..) {
        let latest = parse_record_latest_time(&record)?;
        let age = now.signed_duration_since(latest).num_seconds();
        if record.status == JournalStatus::Resolved && age > RESOLVED_RETENTION_SECS {
            continue;
        }
        keep.push(record);
    }
    *records = keep;
    Ok(before - records.len())
}

/// Where the redemption journal lives.
///
/// THE `ck-quota` SEGMENT IS DELIBERATE AND MUST NOT BE TIDIED. It is a literal,
/// not a value derived from the binary or the module id, and both of those have
/// since been renamed — so beside a binary called `ck-insula` this path reads
/// like a leftover from an old name. It is not. It is where every redemption
/// this host has ever reserved is recorded.
///
/// Renaming the segment does not fail. The old file is simply not found, an
/// empty journal is created in its place, and an empty journal is
/// indistinguishable from a correctly migrated one — while every pending
/// redemption it used to fence is now unfenced, free to be spent a second time.
/// The cost is money and there is no symptom.
///
/// If this ever has to move, it is a migration with its own window: move the
/// file first, then verify by its CONTENTS rather than its existence.
pub fn redemption_journal_path() -> Result<PathBuf, JournalError> {
    if let Some(path) = std::env::var_os("CK_QUOTA_STATE_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path).join("redemptions.json"));
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path)
            .join("cortexkit/ck-quota")
            .join("redemptions.json"));
    }
    crate::env::home_dir()
        .map(|home| {
            home.join(".local/state/cortexkit/ck-quota")
                .join("redemptions.json")
        })
        .ok_or_else(|| {
            JournalError::new("cannot resolve CK_QUOTA_STATE_DIR, XDG_STATE_HOME, or HOME")
        })
}

/// CAS identity used to report a provider rejection of a vault-served token.
#[derive(Clone)]
pub struct AuthFailureContext {
    pub source: Arc<dyn CredentialSource>,
    pub capability: VaultCapability,
    pub record_version: u64,
}

impl std::fmt::Debug for AuthFailureContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthFailureContext")
            .field("source", &"<credential-source>")
            .field("capability", &"<redacted>")
            .field("record_version", &self.record_version)
            .finish()
    }
}

/// Authentication and endpoint information shared by credits and consume calls.
#[derive(Clone)]
pub struct ResetRequest {
    pub base_url: String,
    pub bearer: String,
    pub account_id: String,
    pub auth_failure: Option<AuthFailureContext>,
}

impl ResetRequest {
    pub fn report_auth_failure(&self, error: &FetchError) {
        let FetchError::ProviderStatus(status @ (401 | 403)) = error else {
            return;
        };
        let Some(context) = self.auth_failure.clone() else {
            return;
        };
        let status = *status;
        tokio::spawn(async move {
            context
                .source
                .report_auth_failure(&context.capability, status, context.record_version)
                .await;
        });
    }
}

impl std::fmt::Debug for ResetRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResetRequest")
            .field("base_url", &self.base_url)
            .field("bearer", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("auth_failure", &self.auth_failure)
            .finish()
    }
}

/// Credits response plus the server clock hedge carried in its `Date` header.
#[derive(Debug, Clone)]
pub struct CreditsHttpResponse {
    pub body: Vec<u8>,
    pub date_header: Option<String>,
}

#[async_trait]
pub trait ResetTransport: Send + Sync {
    async fn fetch_credits(
        &self,
        request: &ResetRequest,
    ) -> Result<CreditsHttpResponse, FetchError>;

    async fn consume(
        &self,
        request: &ResetRequest,
        redeem_request_id: &str,
    ) -> Result<Vec<u8>, FetchError>;
}

/// Production reset endpoint transport.
pub struct ReqwestResetTransport {
    http: reqwest::Client,
}

impl ReqwestResetTransport {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

fn endpoint(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

fn reset_headers(request: &ResetRequest) -> [Header; 4] {
    [
        Header::new("User-Agent", "ai-provider-quota"),
        Header::new("ChatGPT-Account-Id", request.account_id.clone()),
        Header::new("OpenAI-Beta", "codex-1"),
        Header::new("originator", "Codex Desktop"),
    ]
}

#[async_trait]
impl ResetTransport for ReqwestResetTransport {
    async fn fetch_credits(
        &self,
        request: &ResetRequest,
    ) -> Result<CreditsHttpResponse, FetchError> {
        let mut http_request = JsonRequest::get(endpoint(&request.base_url, CREDITS_PATH))
            .timeout(CREDITS_TIMEOUT)
            .bearer(&request.bearer);
        for header in reset_headers(request) {
            http_request = http_request.header(header);
        }
        let response = if request.auth_failure.is_some() {
            http_request
                .send_provider_status_first(&self.http, "codex")
                .await?
        } else {
            http_request.send_full(&self.http).await?
        };
        Ok(CreditsHttpResponse {
            date_header: response.header("Date").map(ToString::to_string),
            body: response.body,
        })
    }

    async fn consume(
        &self,
        request: &ResetRequest,
        redeem_request_id: &str,
    ) -> Result<Vec<u8>, FetchError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "redeem_request_id": redeem_request_id
        }))
        .map_err(|error| FetchError::Decode(format!("encoding consume request: {error}")))?;
        let mut http_request =
            JsonRequest::post_json(endpoint(&request.base_url, CONSUME_PATH), body)
                .timeout(CONSUME_TIMEOUT)
                .bearer(&request.bearer);
        for header in reset_headers(request) {
            http_request = http_request.header(header);
        }
        if request.auth_failure.is_some() {
            http_request
                .send_provider_status_first(&self.http, "codex")
                .await
                .map(|response| response.body)
        } else {
            http_request.send(&self.http).await
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResetTickInput {
    pub armed: bool,
    pub now: DateTime<Utc>,
    pub earliest_expiry: Option<DateTime<Utc>>,
    pub auto_use_resets_secs: u64,
    pub facts: UsageFacts,
    pub elapsed_since_attempt_start: Duration,
}

#[derive(Debug, Clone)]
pub struct ResetTickResult {
    pub armed: bool,
    pub relax_eligible: bool,
    pub consume_attempted: bool,
    pub pending: bool,
    pub journal_ok: bool,
    pub outcome: Option<ConsumeOutcome>,
    pub trigger: TriggerDecision,
}

impl ResetTickResult {
    fn disarmed(input: &ResetTickInput) -> Self {
        Self {
            armed: false,
            relax_eligible: false,
            consume_attempted: false,
            pending: false,
            journal_ok: false,
            outcome: None,
            trigger: evaluate_trigger(&TriggerInput {
                armed: false,
                now: input.now,
                earliest_expiry: input.earliest_expiry,
                auto_use_resets_secs: input.auto_use_resets_secs,
                any_used_floor: input.facts.any_used_floor,
                at_wall: input.facts.at_wall,
                pending: false,
                spend_bound_allows: false,
                before_post_cutoff: false,
            }),
        }
    }
}

#[derive(Default)]
struct AccountMutationState {
    in_flight: bool,
    last_post_at: Option<Instant>,
}

#[derive(Default)]
struct AccountMutation {
    state: Mutex<AccountMutationState>,
}

struct InFlightReset {
    account: Arc<AccountMutation>,
}

impl Drop for InFlightReset {
    fn drop(&mut self) {
        self.account
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .in_flight = false;
    }
}

/// In-process mutation fence layered over the durable journal.
pub struct ResetCoordinator {
    journal: RedemptionJournal,
    journal_io: Mutex<()>,
    accounts: Mutex<HashMap<String, Arc<AccountMutation>>>,
}

impl ResetCoordinator {
    pub fn new(journal: RedemptionJournal) -> Result<Self, JournalError> {
        journal.probe_atomic_write()?;
        Ok(Self {
            journal,
            journal_io: Mutex::new(()),
            accounts: Mutex::new(HashMap::new()),
        })
    }

    pub fn from_env() -> Result<Self, JournalError> {
        Self::new(RedemptionJournal::from_env()?)
    }

    pub fn journal(&self) -> &RedemptionJournal {
        &self.journal
    }

    fn account_mutation(&self, account_id: &str) -> Arc<AccountMutation> {
        self.accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(account_id.to_string())
            .or_default()
            .clone()
    }

    /// Evaluate, reserve, mutate, and resolve without ever holding a standard
    /// mutex guard across an await point.
    pub async fn process_tick(
        &self,
        account_id: &str,
        input: ResetTickInput,
        transport: &dyn ResetTransport,
        request: &ResetRequest,
    ) -> ResetTickResult {
        self.process_tick_with_timeout(account_id, input, transport, request, CONSUME_TIMEOUT)
            .await
    }

    pub async fn process_tick_with_timeout(
        &self,
        account_id: &str,
        input: ResetTickInput,
        transport: &dyn ResetTransport,
        request: &ResetRequest,
        consume_timeout: Duration,
    ) -> ResetTickResult {
        if !input.armed {
            return ResetTickResult::disarmed(&input);
        }

        let coordinator_started = Instant::now();
        let account = self.account_mutation(account_id);
        let (request_id, trigger, no_post_result) = {
            let mut account_state = account
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if account_state.in_flight {
                let trigger = evaluate_trigger(&TriggerInput {
                    armed: true,
                    now: input.now,
                    earliest_expiry: input.earliest_expiry,
                    auto_use_resets_secs: input.auto_use_resets_secs,
                    any_used_floor: input.facts.any_used_floor,
                    at_wall: input.facts.at_wall,
                    pending: true,
                    spend_bound_allows: false,
                    before_post_cutoff: input.elapsed_since_attempt_start < PRE_POST_CUTOFF,
                });
                return ResetTickResult {
                    armed: true,
                    relax_eligible: false,
                    consume_attempted: false,
                    pending: true,
                    journal_ok: true,
                    outcome: None,
                    trigger,
                };
            }

            let journal_state = {
                let _journal_guard = self
                    .journal_io
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                self.journal.inspect_account(account_id, Utc::now())
            };
            let journal_state = match journal_state {
                Ok(state) => state,
                Err(error) => {
                    eprintln!(
                    "[ck-quota] warning: codex reset journal unavailable for account_id={account_id}: {error}; tick disarmed"
                );
                    return ResetTickResult::disarmed(&input);
                }
            };

            let before_post_cutoff = input
                .elapsed_since_attempt_start
                .saturating_add(coordinator_started.elapsed())
                < PRE_POST_CUTOFF;
            let trigger = evaluate_trigger(&TriggerInput {
                armed: true,
                now: input.now,
                earliest_expiry: input.earliest_expiry,
                auto_use_resets_secs: input.auto_use_resets_secs,
                any_used_floor: input.facts.any_used_floor,
                at_wall: input.facts.at_wall,
                pending: journal_state.pending_id.is_some(),
                spend_bound_allows: journal_state.spend_bound_allows,
                before_post_cutoff,
            });
            // A pending id represents an already-triggered logical redemption. Retry
            // that same id even if the current usage no longer triggers, so a crash
            // after a landed POST can promptly resolve as `already_redeemed` instead
            // of leaving an unresolved record for a day.
            let pending_retry_interval_elapsed = account_state
                .last_post_at
                .is_none_or(|last_post_at| last_post_at.elapsed() >= PENDING_RETRY_INTERVAL);
            let may_retry_pending = journal_state.pending_id.is_some()
                && before_post_cutoff
                && pending_retry_interval_elapsed;
            let should_reserve = trigger.fire;

            let reservation = if should_reserve || may_retry_pending {
                let _journal_guard = self
                    .journal_io
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                self.journal.reserve(account_id, Utc::now())
            } else {
                Ok(if journal_state.pending_id.is_some() {
                    Reservation::ExistingPending(journal_state.pending_id.clone().unwrap())
                } else if !journal_state.spend_bound_allows {
                    Reservation::SpendBound
                } else {
                    Reservation::NoAction
                })
            };
            let reservation = match reservation {
                Ok(reservation) => reservation,
                Err(error) => {
                    eprintln!(
                    "[ck-quota] warning: codex reset reserve failed for account_id={account_id}: {error}; tick disarmed"
                );
                    return ResetTickResult::disarmed(&input);
                }
            };

            let reservation_is_pending = matches!(
                &reservation,
                Reservation::New(_) | Reservation::ExistingPending(_)
            );
            let still_before_post_cutoff = input
                .elapsed_since_attempt_start
                .saturating_add(coordinator_started.elapsed())
                < PRE_POST_CUTOFF;
            let request_id = match reservation {
                Reservation::New(id) if should_reserve && still_before_post_cutoff => Some(id),
                Reservation::ExistingPending(id)
                    if may_retry_pending && still_before_post_cutoff =>
                {
                    Some(id)
                }
                _ => None,
            };
            let no_post_result = request_id.is_none().then(|| {
                let pending = reservation_is_pending || journal_state.pending_id.is_some();
                ResetTickResult {
                    armed: true,
                    relax_eligible: reporting_eligible(true, &input.facts, false, pending, true),
                    consume_attempted: false,
                    pending,
                    journal_ok: true,
                    outcome: None,
                    trigger,
                }
            });
            if let Some(request_id) = request_id.as_deref() {
                let attempt_recorded = {
                    let _journal_guard = self
                        .journal_io
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    self.journal
                        .record_attempt(account_id, request_id, Utc::now())
                };
                if let Err(error) = attempt_recorded {
                    eprintln!(
                        "[ck-quota] warning: codex reset attempt record failed for account_id={account_id}: {error}; tick disarmed"
                    );
                    return ResetTickResult::disarmed(&input);
                }
                account_state.in_flight = true;
                account_state.last_post_at = Some(Instant::now());
            }
            (request_id, trigger, no_post_result)
        };
        let Some(request_id) = request_id else {
            drop(account);
            self.prune_account_mutexes();
            return no_post_result.expect("a missing request id has a no-POST result");
        };
        let in_flight = InFlightReset {
            account: Arc::clone(&account),
        };

        let response =
            tokio::time::timeout(consume_timeout, transport.consume(request, &request_id)).await;
        let outcome = match response {
            Ok(Ok(body)) => match normalize_consume_response(&body) {
                Ok(outcome) => Some(outcome),
                Err(error) => {
                    eprintln!(
                        "[ck-quota] warning: codex reset response invalid account_id={account_id} redeem_request_id={request_id}: {error}"
                    );
                    None
                }
            },
            Ok(Err(error)) => {
                request.report_auth_failure(&error);
                eprintln!(
                    "[ck-quota] warning: codex reset POST failed account_id={account_id} redeem_request_id={request_id}: {error}"
                );
                None
            }
            Err(_) => {
                eprintln!(
                    "[ck-quota] warning: codex reset POST timed out account_id={account_id} redeem_request_id={request_id}"
                );
                None
            }
        };

        let mut journal_ok = true;
        let mut pending = true;
        if let Some(outcome) = outcome {
            {
                let _account_guard = account
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let _journal_guard = self
                    .journal_io
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Err(error) = self.journal.resolve(account_id, &request_id, outcome) {
                    journal_ok = false;
                    eprintln!(
                        "[ck-quota] warning: codex reset resolve failed account_id={account_id} redeem_request_id={request_id}: {error}; tick disarmed"
                    );
                } else {
                    pending = false;
                }
            }
        }

        drop(in_flight);
        drop(account);
        self.prune_account_mutexes();
        ResetTickResult {
            armed: journal_ok,
            relax_eligible: false,
            consume_attempted: true,
            pending,
            journal_ok,
            outcome,
            trigger,
        }
    }

    fn prune_account_mutexes(&self) {
        let retained_accounts: HashSet<String> = {
            let _journal_guard = self
                .journal_io
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match self.journal.records() {
                Ok(records) => records
                    .into_iter()
                    .map(|record| record.account_id)
                    .collect(),
                Err(_) => return,
            }
        };
        self.accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|account_id, account| {
                retained_accounts.contains(account_id) || Arc::strong_count(account) > 1
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RateWindow, Usage};

    /// Facts describing an account with room left and no wall reported.
    fn relaxable_facts() -> UsageFacts {
        UsageFacts::from_usage(&usage_at(70.0), Some(false))
    }

    /// Every condition guarding relaxed output must be load-bearing.
    ///
    /// When this returns true the module publishes `usedPercent: 0` for an
    /// account that is really part-way through its quota, on the promise that a
    /// banked credit will reset the window before it runs out. Consumers pace on
    /// that zero, so each condition is what keeps the promise honest: relaxing
    /// without one claims capacity that may not exist, and the consumer has no
    /// way to tell the difference.
    ///
    /// Asserted here rather than through the tick that calls it, because a test
    /// driving the whole tick fixes every input at once and cannot show which
    /// A window at its limit blocks relaxation even when the upstream says the
    /// account is not limited.
    ///
    /// `limit_reached` describes whichever window the upstream is currently
    /// enforcing, which is typically the shortest one. A longer window can sit
    /// at its limit while that flag reads false -- so the two disagree, and the
    /// percent check is the only thing that notices.
    ///
    /// Relaxing here would publish `usedPercent: 0` for an account whose weekly
    /// allowance is spent, on the promise that a banked credit will restore it.
    /// A consumer cannot tell that zero from an idle account and keeps routing
    /// work to it.
    ///
    /// Separate from the gate test below because that one sets both signals at
    /// once: an account reported as limited *and* at 100%. Either term alone
    /// refuses that fixture, so it cannot show that both are required.
    #[test]
    fn a_window_at_its_limit_blocks_relaxation_even_when_the_upstream_reports_clear() {
        let mut usage = usage_at(4.0);
        usage.secondary = Some(RateWindow {
            used_percent: 99.5,
            raw_used_percent: None,
            window_minutes: Some(10080),
            resets_at: None,
            used_count: None,
            total_count: None,
        });

        // The upstream is explicit that it is not currently refusing requests.
        let facts = UsageFacts::from_usage(&usage, Some(false));
        assert!(
            facts.wall_clear,
            "the fixture must model an upstream reporting itself unlimited"
        );
        assert!(
            facts.at_wall,
            "the fixture must model a window that has reached its limit"
        );

        assert!(
            !reporting_eligible(true, &facts, false, false, true),
            "a spent window must block relaxation even when the upstream reports clear"
        );

        // The control: with that window well below its limit, the same inputs do
        // relax -- so the refusal above comes from the percent, not from some
        // other condition of the fixture.
        let healthy = UsageFacts::from_usage(&usage_at(4.0), Some(false));
        assert!(
            reporting_eligible(true, &healthy, false, false, true),
            "an account below its wall must still relax"
        );
    }

    /// condition did the work.
    #[test]
    fn every_condition_guarding_relaxed_output_is_required() {
        let facts = relaxable_facts();

        // The control: with every condition met, relaxation is permitted. Without
        // this the assertions below would pass against a gate that never relaxes
        // at all.
        assert!(
            reporting_eligible(true, &facts, false, false, true),
            "a fresh, below-wall, mutation-free, journal-clean tick must relax"
        );

        // Not armed: the feature is off, or this account has no credits to spend.
        // Relaxing here reports capacity that nothing will ever restore.
        assert!(
            !reporting_eligible(false, &facts, false, false, true),
            "relaxed output must require the feature to be armed"
        );

        // At the wall: the upstream is already refusing. A zero here tells a
        // consumer to keep routing work at an account that cannot serve it.
        let walled = UsageFacts::from_usage(&usage_at(100.0), Some(true));
        assert!(
            !reporting_eligible(true, &walled, false, false, true),
            "relaxed output must require the account to be below its wall"
        );

        // A consume was attempted this tick, so the credit balance is mid-flight:
        // the spend may have failed, and the percents in hand predate its result.
        assert!(
            !reporting_eligible(true, &facts, true, false, true),
            "a tick that attempted a consume must report the true numbers"
        );

        // A journal record is pending, so a previous spend never resolved. Until
        // it does, whether a credit was actually redeemed is unknown.
        assert!(
            !reporting_eligible(true, &facts, false, true, true),
            "an unresolved redemption must report the true numbers"
        );

        // The journal could not be read or written, so double-spend protection is
        // unavailable -- and with it any basis for promising a reset.
        assert!(
            !reporting_eligible(true, &facts, false, false, false),
            "an unusable journal must report the true numbers"
        );
    }

    fn usage_at(percent: f64) -> Usage {
        Usage {
            primary: Some(RateWindow {
                used_percent: percent,
                raw_used_percent: None,
                window_minutes: Some(10080),
                resets_at: None,
                used_count: None,
                total_count: None,
            }),
            ..Usage::default()
        }
    }

    /// The at-wall threshold decides whether an account is treated as having hit
    /// its limit, which is one of the two conditions that spend a banked credit.
    ///
    /// Driven from percentages rather than by handing `at_wall` in directly. The
    /// trigger tests take it as a parameter, so they exercise what the gate does
    /// with the answer and never how the answer is reached -- and the derivation
    /// is where a mis-set threshold would live.
    ///
    /// Both sides are asserted because the two directions fail differently and
    /// each survives a test of the other. Too high and a walled account is never
    /// relieved, so the credits it holds expire unspent while it sits blocked.
    /// Too low and a credit is spent on an account that still had room, which
    /// cannot be undone.
    #[test]
    fn the_at_wall_threshold_holds_on_both_sides() {
        let below = UsageFacts::from_usage(&usage_at(98.9), None);
        assert!(!below.at_wall, "98.9% must not read as walled");

        let at = UsageFacts::from_usage(&usage_at(99.0), None);
        assert!(at.at_wall, "99.0% is the wall");

        let above = UsageFacts::from_usage(&usage_at(99.5), None);
        assert!(above.at_wall, "99.5% must read as walled");

        // The upstream saying so outranks the percentages: a provider that
        // reports a limit reached is believed even when its figures look low,
        // because it knows its own enforcement and the percentages are inference.
        let stated = UsageFacts::from_usage(&usage_at(3.0), Some(true));
        assert!(
            stated.at_wall,
            "a stated limit is the wall regardless of percent"
        );
    }

    /// The used floor stops a credit being spent on an account that has consumed
    /// nothing.
    ///
    /// Without it an expiring credit would be redeemed against an untouched
    /// window, spending something irreplaceable to reset a limit nobody had
    /// approached.
    #[test]
    fn the_used_floor_holds_on_both_sides() {
        assert!(
            !UsageFacts::from_usage(&usage_at(0.9), None).any_used_floor,
            "0.9% is below the floor"
        );
        assert!(
            UsageFacts::from_usage(&usage_at(1.0), None).any_used_floor,
            "1.0% is the floor"
        );
    }

    /// The facts read every window, not only the headline slot.
    ///
    /// An account is walled when *any* of its windows is exhausted, and the
    /// exhausted one is routinely not the first: the headline slot is the
    /// shortest window, while the weekly limit is the one that blocks work.
    #[test]
    fn a_wall_in_a_later_window_is_still_a_wall() {
        let usage = Usage {
            primary: Some(RateWindow {
                used_percent: 4.0,
                raw_used_percent: None,
                window_minutes: Some(300),
                resets_at: None,
                used_count: None,
                total_count: None,
            }),
            secondary: Some(RateWindow {
                used_percent: 99.4,
                raw_used_percent: None,
                window_minutes: Some(10080),
                resets_at: None,
                used_count: None,
                total_count: None,
            }),
            ..Usage::default()
        };

        let facts = UsageFacts::from_usage(&usage, None);
        assert!(facts.at_wall, "an exhausted weekly window is a wall");
        // Not vacuous: the headline window alone would not have tripped it.
        assert!(!UsageFacts::from_usage(&usage_at(4.0), None).at_wall);
    }
}
