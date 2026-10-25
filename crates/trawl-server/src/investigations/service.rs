// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The investigation capability, transport-free (ADR-0059).
//!
//! # Authority
//!
//! Reading needs `query` and `investigation_read`; writing needs
//! `investigation_write` as well; extending retention and deleting need
//! `server_manage` on top of that. Every function checks its authority
//! first, before it parses the id or looks anything up, so a key without
//! it gets the same 403 for an id that exists and one that does not. A
//! reader then gets 404 for an id the installation does not hold.
//!
//! No statement is scoped by the caller's key: every reader sees every
//! investigation. The key's prefix and name are recorded as attribution
//! ([`StoreActor`]); its id stays server side, for the create replay check
//! and for leaving the caller's own entries out of `activity`.
//!
//! # Bounds
//!
//! Serde admits any length, so every text bound of
//! [`trawl_api::investigations`] is checked here before the store sees the
//! request, and so is the absence of NUL, which `PostgreSQL` text refuses.
//! A refusal is 400 and names the field and the rule, never the stored
//! state. A keep and a finding cite rows of results kept for this
//! investigation, checked by [`resolve_references`]. A citation of the
//! caller's own live temporary result promotes that whole result into the
//! investigation with the append, charged once to the evidence budgets. The
//! promotion's transaction and the settlement of its charge run in a task
//! of their own, so a request that goes away cuts neither short
//! ([`append_promoting`]).
//!
//! # Allowances
//!
//! A read answers [`READ_DEFAULT_LIMIT`] entries unless it asks for
//! another `limit`, at most [`READ_MAX_LIMIT`]. An append that carries a
//! `seen_seq` answers at most [`ACTIVITY_LIMIT`] entries of other actors'
//! activity, with `more` when it cut some.

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use fleet_auth::VerifiedKey;
use trawl_api::investigations::{
    AppendEntry, AppendResponse, CreateInvestigation, EntryPayload, EvidenceRef,
    FINDING_MAX_COUNTEREVIDENCE, FINDING_MAX_REFERENCES, FINDING_STATEMENT_MAX_BYTES,
    FindingStatusUpdate, Investigation, InvestigationId, InvestigationList, InvestigationRead,
    InvestigationStatus, KEEP_MAX_ROWS, KEEP_REASON_MAX_BYTES, NOTE_MAX_BYTES, PatchInvestigation,
    QUESTION_MAX_BYTES, REMAINING_QUESTION_MAX_BYTES, RetentionExtension, SCOPE_MAX_BYTES,
    TITLE_MAX_BYTES,
};
use trawl_api::results::ResultState;

use crate::deadline::Deadline;
use crate::error::ServerError;
use crate::policy::{Permission, TrawlAuthz as _};
use crate::report_window::truncate_to_micros;
use crate::results::ResultsState;
use crate::results::charge::{EvidenceBudget, Refusal, Reservation};
use crate::results::root::{self, ResultsRoot};
use crate::results::service::COMMIT_GRACE;
use crate::state::AppState;
use crate::store::investigations::MAX_LIST;
use crate::store::{
    Appended, CaptureRow, Citing, CreateOutcome, DeleteOutcome, ExpiredFile, InvestigationRow,
    InvestigationStore, NewInvestigation, Overwrite, RetentionRule, StoreActor, StoreError,
};

/// Entries a read answers when it names no `limit`.
pub const READ_DEFAULT_LIMIT: u64 = 100;

/// The most entries one read answers.
pub const READ_MAX_LIMIT: u64 = 200;

/// The most entries of other actors' activity an append answers. Small
/// beside a read: it rides every append, and `more` points at the read
/// that has the rest.
pub const ACTIVITY_LIMIT: u64 = 50;

// -- authority ---------------------------------------------------------------

/// What a route needs of the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Authority {
    /// `query` and `investigation_read`.
    Read,
    /// [`Self::Read`] and `investigation_write`.
    Write,
    /// [`Self::Write`] and `server_manage`.
    Manage,
}

/// Whether `verified` reads investigations and their kept evidence: it
/// holds `query` and `investigation_read`.
#[must_use]
pub fn reads_investigations(verified: &VerifiedKey) -> bool {
    verified.has_permission(Permission::Query)
        && verified.has_permission(Permission::InvestigationRead)
}

/// The caller's authority, checked before anything is looked up: the same
/// 403 for every id.
fn authorize(verified: &VerifiedKey, needed: Authority) -> Result<(), ServerError> {
    let read = reads_investigations(verified);
    let write = read && verified.has_permission(Permission::InvestigationWrite);
    let granted = match needed {
        Authority::Read => read,
        Authority::Write => write,
        Authority::Manage => write && verified.has_permission(Permission::ServerManage),
    };
    if granted {
        Ok(())
    } else {
        Err(ServerError::Forbidden("insufficient permissions".into()))
    }
}

/// 403 unless `verified` may read investigations. The routes call it
/// before they look at the request, so a refusal of the request's shape
/// never comes first.
///
/// # Errors
/// 403 without `query` and `investigation_read`.
pub(crate) fn authorize_read(verified: &VerifiedKey) -> Result<(), ServerError> {
    authorize(verified, Authority::Read)
}

/// [`authorize_read`] for writing: `investigation_write` as well.
///
/// # Errors
/// 403 without write authority.
pub(crate) fn authorize_write(verified: &VerifiedKey) -> Result<(), ServerError> {
    authorize(verified, Authority::Write)
}

/// [`authorize_read`] for extending and deleting: `server_manage` as well.
///
/// # Errors
/// 403 without `server_manage` and write authority.
pub(crate) fn authorize_manage(verified: &VerifiedKey) -> Result<(), ServerError> {
    authorize(verified, Authority::Manage)
}

/// The path's investigation id. The text is never quoted back: it is the
/// caller's and may be anything.
fn parse_id(text: &str) -> Result<InvestigationId, ServerError> {
    InvestigationId::parse(text).map_err(|_| {
        ServerError::BadRequest(
            "the investigation id in the path is not 32 lowercase hex digits".into(),
        )
    })
}

/// The caller as the store records it.
fn actor(verified: &VerifiedKey) -> StoreActor {
    StoreActor {
        key_id: verified.id,
        prefix: verified.prefix.clone(),
        name: verified.name.clone(),
    }
}

/// `secs` as a duration, saturating.
fn seconds(secs: u64) -> TimeDelta {
    i64::try_from(secs)
        .ok()
        .and_then(TimeDelta::try_seconds)
        .unwrap_or(TimeDelta::MAX)
}

/// `at + secs`, saturating at the latest instant.
fn after(at: DateTime<Utc>, secs: u64) -> DateTime<Utc> {
    at.checked_add_signed(seconds(secs))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// How far each change slides the evidence clock: `[investigation]
/// evidence_idle_secs`.
fn retention(state: &AppState) -> RetentionRule {
    RetentionRule {
        idle: seconds(state.results.investigation.evidence_idle_secs),
    }
}

// -- bounds ------------------------------------------------------------------

/// `text`, the field `what`, holds at most `max` bytes and no NUL.
fn check_text(what: &str, text: &str, max: usize) -> Result<(), ServerError> {
    if text.len() > max {
        return Err(ServerError::BadRequest(format!(
            "{what} is {} bytes, above the limit of {max} bytes",
            text.len()
        )));
    }
    if text.contains('\0') {
        return Err(ServerError::BadRequest(format!(
            "{what} contains a NUL character, which cannot be stored"
        )));
    }
    Ok(())
}

/// [`check_text`], and `text` is not blank.
fn check_required(what: &str, text: &str, max: usize) -> Result<(), ServerError> {
    if text.trim().is_empty() {
        return Err(ServerError::BadRequest(format!("{what} is empty")));
    }
    check_text(what, text, max)
}

/// At most `max` items in the list `what`.
fn check_count(what: &str, count: usize, max: usize) -> Result<(), ServerError> {
    if count > max {
        return Err(ServerError::BadRequest(format!(
            "{what} has {count} items, above the limit of {max}"
        )));
    }
    Ok(())
}

/// `id`, cited by `what`, has the result id shape.
fn check_result_id(what: &str, id: &str) -> Result<(), ServerError> {
    if root::is_result_id(id) {
        Ok(())
    } else {
        Err(ServerError::BadRequest(format!(
            "{what} names a result id that is not 32 lowercase hex digits"
        )))
    }
}

/// The create request's bounds.
fn check_creation(req: &CreateInvestigation) -> Result<(), ServerError> {
    check_required("title", &req.title, TITLE_MAX_BYTES)?;
    check_required("question", &req.question, QUESTION_MAX_BYTES)?;
    check_text("scope", &req.scope, SCOPE_MAX_BYTES)
}

/// A row an append cites: where it was cited, for the refusal, and the row.
#[derive(Debug)]
struct Citation<'a> {
    what: String,
    result_id: &'a str,
    row: u64,
}

/// The append's bounds, and the rows it cites. A keep cites each of its
/// rows; a finding its references and its counterevidence.
fn check_append(entry: &AppendEntry) -> Result<Vec<Citation<'_>>, ServerError> {
    match entry {
        AppendEntry::Keep {
            result_id,
            rows,
            reason,
            ..
        } => {
            check_result_id("the keep", result_id)?;
            if rows.is_empty() {
                return Err(ServerError::BadRequest(
                    "the keep names no rows; keep at least one".into(),
                ));
            }
            check_count("rows", rows.len(), KEEP_MAX_ROWS)?;
            check_required("reason", reason, KEEP_REASON_MAX_BYTES)?;
            Ok(rows
                .iter()
                .enumerate()
                .map(|(i, &row)| Citation {
                    what: format!("rows[{i}]"),
                    result_id,
                    row,
                })
                .collect())
        }
        AppendEntry::Finding {
            statement,
            references,
            counterevidence,
            remaining_question,
            ..
        } => {
            check_required("statement", statement, FINDING_STATEMENT_MAX_BYTES)?;
            check_count("references", references.len(), FINDING_MAX_REFERENCES)?;
            check_count(
                "counterevidence",
                counterevidence.len(),
                FINDING_MAX_COUNTEREVIDENCE,
            )?;
            if let Some(remaining) = remaining_question {
                check_text(
                    "remaining_question",
                    remaining,
                    REMAINING_QUESTION_MAX_BYTES,
                )?;
            }
            let mut citations = cite("references", references)?;
            citations.extend(cite("counterevidence", counterevidence)?);
            Ok(citations)
        }
        AppendEntry::Note { text, .. } => {
            check_required("text", text, NOTE_MAX_BYTES)?;
            Ok(Vec::new())
        }
    }
}

/// The citations of the finding list `list`, each checked for the result
/// id shape.
fn cite<'a>(list: &str, refs: &'a [EvidenceRef]) -> Result<Vec<Citation<'a>>, ServerError> {
    refs.iter()
        .enumerate()
        .map(|(i, reference)| {
            let what = format!("{list}[{i}]");
            check_result_id(&what, &reference.result_id)?;
            Ok(Citation {
                what,
                result_id: &reference.result_id,
                row: reference.row,
            })
        })
        .collect()
}

/// The payload an append records: the entry without its `seen_seq`.
fn payload(entry: AppendEntry) -> EntryPayload {
    match entry {
        AppendEntry::Keep {
            result_id,
            rows,
            reason,
            ..
        } => EntryPayload::Keep {
            result_id,
            rows,
            reason,
        },
        AppendEntry::Finding {
            statement,
            status,
            references,
            counterevidence,
            remaining_question,
            ..
        } => EntryPayload::Finding {
            statement,
            status,
            references,
            counterevidence,
            remaining_question,
        },
        AppendEntry::Note { text, .. } => EntryPayload::Note { text },
    }
}

/// A cited result the append promotes: the caller's own live temporary
/// result, its file's size, and the first citation that named it.
struct Promotion {
    result_id: String,
    file_bytes: u64,
    what: String,
}

/// What a cited result is to investigation `id` and the caller.
enum Cited {
    /// Kept evidence of this investigation.
    Evidence,
    /// The caller's own live temporary result with its file: the append
    /// promotes it.
    Promotable,
    /// Kept evidence of another investigation.
    KeptElsewhere,
}

/// Classify `row` for a citation by `key_id` into investigation `id` at
/// `now`; `None` for anything a citation cannot name. A temporary result
/// whose file a read or reconciliation found missing is one: as evidence
/// it would charge the budget and fail every read.
fn classify(
    row: &CaptureRow,
    id: &InvestigationId,
    key_id: i64,
    now: DateTime<Utc>,
) -> Option<Cited> {
    match (row.state, row.investigation_id.as_ref()) {
        (ResultState::Kept, Some(of)) if of == id => Some(Cited::Evidence),
        (ResultState::Kept, Some(_)) => Some(Cited::KeptElsewhere),
        (ResultState::Temporary, None)
            if row.key_id == key_id
                && row.expires_at.is_some_and(|at| at > now)
                && !row.file_missing =>
        {
            Some(Cited::Promotable)
        }
        _ => None,
    }
}

/// Check that every cited row is a row of a result the append may cite in
/// investigation `id`: kept evidence of it, or the caller's own live
/// temporary result, which the append promotes. Answers the promotions.
///
/// A result kept by another investigation is 409: evidence belongs to one
/// investigation. Anything else is 400 naming the citation, with the same
/// words whether the result is absent, another key's temporary result,
/// past its deadline, pruned, or without its file, so a refusal tells the
/// caller nothing a read would not. The store checks each promotion again under the
/// investigation's lock ([`crate::store::InvestigationStore::append_citing`]).
///
/// A cited result can be pruned between this check and the append's
/// commit. That is the order the two would have had anyway: a finding
/// keeps citing a row pruned after it was written.
async fn resolve_references(
    state: &AppState,
    verified: &VerifiedKey,
    id: &InvestigationId,
    citations: &[Citation<'_>],
) -> Result<Vec<Promotion>, ServerError> {
    let now = state.results.clock.now();
    let mut resolved: Vec<(&str, Option<(CaptureRow, Cited)>)> = Vec::new();
    let mut promotions = Vec::new();
    for citation in citations {
        let known = resolved
            .iter()
            .position(|(result_id, _)| *result_id == citation.result_id);
        let index = if let Some(index) = known {
            index
        } else {
            let row = state
                .results
                .store
                .get_readable(citation.result_id, verified.id)
                .await?
                .and_then(|row| classify(&row, id, verified.id, now).map(|cited| (row, cited)));
            if let Some((row, Cited::Promotable)) = &row {
                promotions.push(Promotion {
                    result_id: row.id.clone(),
                    file_bytes: row.file_bytes,
                    what: citation.what.clone(),
                });
            }
            resolved.push((citation.result_id, row));
            resolved.len() - 1
        };
        let row = match &resolved[index].1 {
            Some((_, Cited::KeptElsewhere)) => {
                return Err(kept_elsewhere(&citation.what, citation.result_id));
            }
            Some((row, _)) => row,
            None => return Err(not_citable(&citation.what, citation.result_id)),
        };
        if citation.row >= row.captured_rows {
            return Err(ServerError::BadRequest(format!(
                "{} cites row {} of result {}, which holds {} rows",
                citation.what, citation.row, citation.result_id, row.captured_rows
            )));
        }
    }
    Ok(promotions)
}

/// The 400 for a citation of a result the append cannot cite.
fn not_citable(what: &str, result_id: &str) -> ServerError {
    ServerError::BadRequest(format!(
        "{what} cites result {result_id}, which is not kept evidence of this investigation"
    ))
}

/// The 409 for a citation of another investigation's evidence.
fn kept_elsewhere(what: &str, result_id: &str) -> ServerError {
    ServerError::Conflict(format!(
        "{what} cites result {result_id}, which is already kept by another investigation"
    ))
}

/// Append `payload` and promote `promotions` with it: charge their file
/// bytes to the evidence budgets under the gate's pass lock, by a pass
/// that waits no later than `deadline`, then write the promotions and the
/// entry in one transaction, then settle the charge. An overflow is 409
/// `evidence_budget_exceeded` with nothing written, which [`append`] may
/// still turn into an append at no charge; a promotion the store refuses
/// under the lock answers as [`resolve_references`] would have.
///
/// The transaction and the charge's settlement run in a task of their own
/// ([`Promoting::run`]), which the request awaits. A request that goes
/// away, as when its client hangs up, cuts neither short: the charge
/// stays on the book until the transaction has ended and the table counts
/// what it committed. Released with the request, it would let the next
/// pass admit a promotion past the budget while this one's commit lands.
async fn append_promoting(
    state: &AppState,
    id: &InvestigationId,
    actor: &StoreActor,
    payload: &EntryPayload,
    promotions: &[Promotion],
    now: DateTime<Utc>,
    deadline: Deadline,
) -> Result<Appended, ServerError> {
    let bytes = promotions.iter().fold(0_u64, |bytes, promotion| {
        bytes.saturating_add(promotion.file_bytes)
    });
    let budget = EvidenceBudget::of(&state.results.investigation);
    let reservation = state
        .results
        .gate
        .reserve_evidence(id, &promoted_ids(promotions), bytes, budget, deadline)
        .await
        .map_err(|refusal| match refusal {
            Refusal::EvidenceBudgetExceeded { used, limit } => {
                ServerError::EvidenceBudgetExceeded { used, limit }
            }
            Refusal::Busy { retry_after_secs } | Refusal::QuotaExceeded { retry_after_secs } => {
                ServerError::CaptureBusy { retry_after_secs }
            }
            Refusal::Store(e) => ServerError::Store(e),
        })?;
    let promoting = Promoting {
        results: Arc::clone(&state.results),
        store: state.storage.investigations.clone(),
        id: id.clone(),
        actor: actor.clone(),
        payload: payload.clone(),
        result_ids: promotions.iter().map(|p| p.result_id.clone()).collect(),
        now,
        retention: retention(state),
    };
    let cited = tokio::spawn(promoting.run(reservation))
        .await
        .map_err(|e| ServerError::from_join("promotion", e))??;
    let named = |result_id: &str| {
        promotions
            .iter()
            .find(|p| p.result_id == result_id)
            .map_or("the entry", |p| p.what.as_str())
            .to_owned()
    };
    match cited {
        Citing::Appended(appended) => Ok(*appended),
        Citing::KeptElsewhere(result_id) => Err(kept_elsewhere(&named(&result_id), &result_id)),
        Citing::NotCitable(result_id) => Err(not_citable(&named(&result_id), &result_id)),
    }
}

/// The ids of the results `promotions` promotes.
fn promoted_ids(promotions: &[Promotion]) -> Vec<&str> {
    promotions.iter().map(|p| p.result_id.as_str()).collect()
}

/// A promotion's transaction, owned by the task that runs it and settles
/// its charge ([`append_promoting`]).
struct Promoting {
    results: Arc<ResultsState>,
    store: InvestigationStore,
    id: InvestigationId,
    actor: StoreActor,
    payload: EntryPayload,
    result_ids: Vec<String>,
    now: DateTime<Utc>,
    retention: RetentionRule,
}

impl Promoting {
    /// Run the transaction, then settle `reservation` as its outcome says.
    ///
    /// Appended, the table counts the promoted results: settle. A refusal
    /// under the lock rolled back: the charge goes with the task. Any
    /// other store error was decided under the lock and rolled back too,
    /// except [`StoreError::Unavailable`]: a connection lost after the
    /// commit was sent may still see it land, so the charge is held for
    /// [`COMMIT_GRACE`] in a task of its own ([`Self::in_doubt`]), and the
    /// request answers the error at once.
    async fn run(self, reservation: Reservation) -> Result<Citing, StoreError> {
        let ids: Vec<&str> = self.result_ids.iter().map(String::as_str).collect();
        let cited = self
            .store
            .append_citing(
                &self.id,
                &self.actor,
                &self.payload,
                &ids,
                self.now,
                self.retention,
            )
            .await;
        match cited {
            Ok(Citing::Appended(appended)) => {
                reservation.settle();
                Ok(Citing::Appended(appended))
            }
            Err(StoreError::Unavailable(e)) => {
                self.in_doubt(reservation);
                Err(StoreError::Unavailable(e))
            }
            other => other,
        }
    }

    /// Hold `reservation` for [`COMMIT_GRACE`] in a task of its own, then
    /// read the promoted results: each kept by this investigation, the
    /// commit landed and the table counts them; otherwise it did not. The
    /// charge is done either way once the grace has passed, as a capture's
    /// is ([`crate::results::service`]'s `in_doubt`): released early, it
    /// would admit a promotion past the budget if the commit lands. A read
    /// that fails is logged, and the charge lapses with the grace.
    fn in_doubt(self, reservation: Reservation) {
        tracing::warn!(
            event_type = "investigation_promotion_in_doubt",
            investigation_id = %self.id,
            grace_secs = COMMIT_GRACE.as_secs(),
            "a promotion's transaction answered a store failure; its commit may still land, \
             so its evidence charge is held for the grace"
        );
        #[cfg(any(test, feature = "test-support"))]
        let held = self.results.store.faults().take_grace();
        tokio::spawn(async move {
            #[cfg(any(test, feature = "test-support"))]
            let settled = if let Some(held) = held {
                Some(held.wait().await)
            } else {
                tokio::time::sleep(COMMIT_GRACE).await;
                None
            };
            #[cfg(not(any(test, feature = "test-support")))]
            tokio::time::sleep(COMMIT_GRACE).await;

            let outcome = match self.promoted().await {
                Ok(true) => {
                    reservation.settle();
                    "committed"
                }
                Ok(false) => "absent",
                Err(_) => "unknown",
            };
            drop(reservation);
            tracing::info!(
                event_type = "investigation_promotion_settled",
                investigation_id = %self.id,
                outcome,
                "a promotion whose commit was in doubt is settled after the grace"
            );
            #[cfg(any(test, feature = "test-support"))]
            if let Some(settled) = settled {
                let _ = settled.send(());
            }
        });
    }

    /// Whether every result this promotion names is kept by its
    /// investigation.
    async fn promoted(&self) -> Result<bool, StoreError> {
        for result_id in &self.result_ids {
            let kept_here = self
                .results
                .store
                .get(result_id, self.actor.key_id)
                .await?
                .is_some_and(|row| {
                    row.state == ResultState::Kept
                        && row.investigation_id.as_ref() == Some(&self.id)
                });
            if !kept_here {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

// -- the routes --------------------------------------------------------------

/// Every investigation, open ones first, then each group most recently
/// changed first, at most [`MAX_LIST`].
///
/// # Errors
/// 403 without read authority; a store failure.
pub async fn list(
    state: &AppState,
    verified: &VerifiedKey,
) -> Result<InvestigationList, ServerError> {
    authorize(verified, Authority::Read)?;
    let rows = state.storage.investigations.list(MAX_LIST).await?;
    Ok(InvestigationList {
        investigations: rows.iter().map(InvestigationRow::to_summary).collect(),
    })
}

/// Create investigation `id` unless it exists. Answers whether this call
/// created it, and the investigation. The same creator key sending the same
/// request again gets the existing one back with nothing advanced.
///
/// Its evidence clock starts at `[investigation] evidence_idle_secs` from
/// now, under a ceiling `evidence_ceiling_secs` from now.
///
/// # Errors
/// 403 without write authority; 400 for a malformed id or a field out of
/// bounds; 409 `investigation_exists` when the id is taken by another
/// creator or another request; a store failure.
pub async fn create(
    state: &AppState,
    verified: &VerifiedKey,
    id: &str,
    req: CreateInvestigation,
) -> Result<(bool, Investigation), ServerError> {
    authorize(verified, Authority::Write)?;
    let id = parse_id(id)?;
    check_creation(&req)?;
    let now = state.results.clock.now();
    let config = &state.results.investigation;
    let retain_ceiling = truncate_to_micros(after(now, config.evidence_ceiling_secs));
    let retain_until =
        truncate_to_micros(after(now, config.evidence_idle_secs)).min(retain_ceiling);
    let new = NewInvestigation {
        id,
        actor: actor(verified),
        creation: req,
        now,
        retain_until,
        retain_ceiling,
    };
    match state.storage.investigations.create_if_absent(&new).await? {
        CreateOutcome::Created(row) => Ok((true, row.to_wire())),
        CreateOutcome::Existing(row) => Ok((false, row.to_wire())),
        CreateOutcome::Mismatch => Err(ServerError::InvestigationExists),
    }
}

/// Investigation `id` and at most `limit` entries changed after `since`
/// (default 0, everything), in `updated_seq` order. `limit` defaults to
/// [`READ_DEFAULT_LIMIT`] and may not pass [`READ_MAX_LIMIT`].
///
/// # Errors
/// 403 without read authority; 400 for a malformed id or a limit out of
/// range; 404 for an unknown id; a store failure.
pub async fn read(
    state: &AppState,
    verified: &VerifiedKey,
    id: &str,
    since: Option<u64>,
    limit: Option<u64>,
) -> Result<InvestigationRead, ServerError> {
    authorize(verified, Authority::Read)?;
    let id = parse_id(id)?;
    let limit = limit.unwrap_or(READ_DEFAULT_LIMIT);
    if !(1..=READ_MAX_LIMIT).contains(&limit) {
        return Err(ServerError::BadRequest(format!(
            "limit {limit} is outside 1 to {READ_MAX_LIMIT} entries"
        )));
    }
    let page = state
        .storage
        .investigations
        .read_since(&id, since.unwrap_or(0), limit)
        .await?
        .ok_or(ServerError::Store(StoreError::InvestigationNotFound))?;
    Ok(InvestigationRead {
        investigation: page.investigation.to_wire(),
        entries: page.entries,
        through_seq: page.through_seq,
        more: page.more,
    })
}

/// Append a keep, a finding or a note to investigation `id`, which must be
/// open. A citation of the caller's own live temporary result promotes it
/// into the investigation with the entry. Evidence the due-write rule
/// pruned is unlinked once the append commits. With a `seen_seq`, the
/// answer carries the entries other keys changed after it, at most
/// [`ACTIVITY_LIMIT`].
///
/// The evidence budget counts a result once. So when it refuses the
/// promotions, the append waits for every other promotion of those
/// results into `id` that still holds a charge, bounded by the query
/// timeout ([`crate::results::charge::CaptureGate::promotions_done`]), and
/// resolves its citations again: two keeps of one result, both resolved
/// before either committed, would otherwise refuse the second although
/// the first made the result this investigation's evidence. Results now
/// kept here leave the promotions; with none left the entry is appended
/// without a charge, and any left are refused as before.
///
/// # Errors
/// 403 without write authority; 400 for a malformed id, a field out of
/// bounds or a citation [`resolve_references`] refuses; 404 for an unknown
/// id; 409 `investigation_closed`, for a result kept by another
/// investigation, or `evidence_budget_exceeded`; a store failure.
pub async fn append(
    state: &AppState,
    verified: &VerifiedKey,
    id: &str,
    entry: AppendEntry,
) -> Result<AppendResponse, ServerError> {
    authorize(verified, Authority::Write)?;
    let id = parse_id(id)?;
    let citations = check_append(&entry)?;
    let store = &state.storage.investigations;
    // The store decides open or closed again under its lock; this answers
    // an unknown or closed investigation before the citations are read.
    let head = store
        .get(&id)
        .await?
        .ok_or(ServerError::Store(StoreError::InvestigationNotFound))?;
    if head.status == InvestigationStatus::Closed {
        return Err(ServerError::InvestigationClosed);
    }
    let promotions = resolve_references(state, verified, &id, &citations).await?;

    let seen_seq = entry.seen_seq();
    let actor = actor(verified);
    let now = state.results.clock.now();
    let payload = payload(entry.clone());
    let Appended {
        entry: written,
        pruned,
    } = if promotions.is_empty() {
        store
            .append(&id, &actor, &payload, now, retention(state))
            .await?
    } else {
        let deadline = Deadline::after(std::time::Duration::from_secs(state.query.timeout_secs));
        match append_promoting(state, &id, &actor, &payload, &promotions, now, deadline).await {
            Err(refused @ ServerError::EvidenceBudgetExceeded { .. }) => {
                state
                    .results
                    .gate
                    .promotions_done(&id, &promoted_ids(&promotions), deadline)
                    .await;
                if !resolve_references(state, verified, &id, &citations)
                    .await?
                    .is_empty()
                {
                    return Err(refused);
                }
                store
                    .append(&id, &actor, &payload, now, retention(state))
                    .await?
            }
            appended => appended?,
        }
    };
    unlink_evidence(&state.results.root, pruned).await;
    let activity = match seen_seq {
        None => None,
        // An investigation deleted since the append has no activity left
        // to report.
        Some(seen) => Some(
            store
                .activity(&id, seen, actor.key_id, ACTIVITY_LIMIT)
                .await?
                .unwrap_or(trawl_api::investigations::Activity {
                    entries: Vec::new(),
                    through_seq: written.seq.max(seen),
                    more: false,
                }),
        ),
    };
    Ok(AppendResponse {
        entry: written,
        activity,
    })
}

/// Overwrite the title, the question or the status of investigation `id`
/// if its sequence is still `expected_seq`. A closed investigation takes
/// only a reopen.
///
/// One field per request: the store applies one overwrite per sequence
/// check, and two applied under one `expected_seq` would conflict with
/// each other.
///
/// # Errors
/// 403 without write authority; 400 for a malformed id, a patch that
/// changes nothing or more than one field, or a field out of bounds; 404
/// for an unknown id; 409 `sequence_conflict` or `investigation_closed`; a
/// store failure.
pub async fn patch(
    state: &AppState,
    verified: &VerifiedKey,
    id: &str,
    req: PatchInvestigation,
) -> Result<Investigation, ServerError> {
    authorize(verified, Authority::Write)?;
    let id = parse_id(id)?;
    let mut changes = Vec::new();
    if let Some(title) = req.title {
        check_required("title", &title, TITLE_MAX_BYTES)?;
        changes.push(Overwrite::Title(title));
    }
    if let Some(question) = req.question {
        check_required("question", &question, QUESTION_MAX_BYTES)?;
        changes.push(Overwrite::Question(question));
    }
    if let Some(status) = req.status {
        changes.push(Overwrite::Status(status));
    }
    let change = match changes.len() {
        0 => {
            return Err(ServerError::BadRequest(
                "the patch changes nothing; send a title, a question or a status".into(),
            ));
        }
        1 => changes.remove(0),
        _ => {
            return Err(ServerError::BadRequest(
                "the patch changes more than one field; send the title, the question and the \
                 status one per request"
                    .into(),
            ));
        }
    };
    let store = &state.storage.investigations;
    let now = state.results.clock.now();
    store
        .overwrite(
            &id,
            req.expected_seq,
            &actor(verified),
            &change,
            now,
            retention(state),
        )
        .await?;
    // A delete between the overwrite and this read leaves nothing to show.
    store
        .get(&id)
        .await?
        .map(|row| row.to_wire())
        .ok_or(ServerError::Store(StoreError::InvestigationNotFound))
}

/// Overwrite the status of the finding whose entry is `finding_seq` in
/// investigation `id`, if its sequence is still `expected_seq`. The
/// investigation must be open.
///
/// # Errors
/// 403 without write authority; 400 for a malformed id; 404 for an
/// unknown id or a `finding_seq` that names no finding of it; 409
/// `sequence_conflict` or `investigation_closed`; a store failure.
pub async fn set_finding_status(
    state: &AppState,
    verified: &VerifiedKey,
    id: &str,
    finding_seq: u64,
    req: FindingStatusUpdate,
) -> Result<AppendResponse, ServerError> {
    authorize(verified, Authority::Write)?;
    let id = parse_id(id)?;
    let now = state.results.clock.now();
    let entry = state
        .storage
        .investigations
        .set_finding_status(
            &id,
            finding_seq,
            req.expected_seq,
            req.status,
            &actor(verified),
            now,
            retention(state),
        )
        .await?;
    Ok(AppendResponse {
        entry,
        activity: None,
    })
}

/// Keep the evidence of investigation `id` until `retain_until`, raising
/// its ceiling to it if need be, if its sequence is still `expected_seq`.
/// `retain_until` is an RFC 3339 UTC instant no earlier than now and no
/// earlier than the current deadline: an extension never shortens.
///
/// # Errors
/// 403 without `server_manage` and write authority; 400 for a malformed id
/// or a `retain_until` that does not read or would shorten; 404 for an
/// unknown id; 409 `sequence_conflict`; a store failure.
pub async fn extend(
    state: &AppState,
    verified: &VerifiedKey,
    id: &str,
    req: RetentionExtension,
) -> Result<Investigation, ServerError> {
    authorize(verified, Authority::Manage)?;
    let id = parse_id(id)?;
    let retain_until = parse_utc(&req.retain_until)?;
    let store = &state.storage.investigations;
    let head = store
        .get(&id)
        .await?
        .ok_or(ServerError::Store(StoreError::InvestigationNotFound))?;
    // The deadline moves only with the sequence, so a head read at the
    // expected sequence holds the deadline the store will extend, and the
    // store's own sequence check under its lock closes the gap.
    if head.seq != req.expected_seq {
        return Err(ServerError::SequenceConflict {
            expected: req.expected_seq,
            current: head.seq,
        });
    }
    let now = state.results.clock.now();
    if retain_until < now {
        return Err(ServerError::BadRequest(format!(
            "retain_until {} is in the past; an extension keeps evidence until a later instant",
            req.retain_until
        )));
    }
    if retain_until < head.retain_until {
        return Err(ServerError::BadRequest(format!(
            "retain_until {} is before the current deadline; an extension never shortens \
             retention",
            req.retain_until
        )));
    }
    let row = store
        .extend(&id, req.expected_seq, retain_until, &actor(verified), now)
        .await?;
    Ok(row.to_wire())
}

/// `text` as an RFC 3339 instant in UTC, to the microsecond.
fn parse_utc(text: &str) -> Result<DateTime<Utc>, ServerError> {
    let refused = || {
        ServerError::BadRequest(
            "retain_until is not an RFC 3339 UTC instant, such as 2026-11-30T00:00:00Z".into(),
        )
    };
    let at = DateTime::parse_from_rfc3339(text).map_err(|_| refused())?;
    if at.offset().local_minus_utc() != 0 {
        return Err(refused());
    }
    Ok(truncate_to_micros(at.with_timezone(&Utc)))
}

/// Delete investigation `id` and its log, unless admitted work is still
/// running, and unlink its kept evidence files. An unlink that fails is
/// logged: the row is already pruned, so no read reaches the file, and the
/// boot reconciliation removes a file no live row names.
///
/// # Errors
/// 403 without `server_manage` and write authority; 400 for a malformed
/// id; 404 for an unknown id; 409 while a step's work runs; a store
/// failure.
pub async fn delete(state: &AppState, verified: &VerifiedKey, id: &str) -> Result<(), ServerError> {
    authorize(verified, Authority::Manage)?;
    let id = parse_id(id)?;
    let now = state.results.clock.now();
    match state.storage.investigations.delete(&id, now).await? {
        DeleteOutcome::Deleted(files) => {
            unlink_evidence(&state.results.root, files).await;
            Ok(())
        }
        DeleteOutcome::NotFound => Err(ServerError::Store(StoreError::InvestigationNotFound)),
        DeleteOutcome::StepsRunning => Err(ServerError::Conflict(
            "work admitted to the investigation is still running; delete it once that work has \
             recorded its outcome"
                .into(),
        )),
    }
}

/// Unlink the files of evidence a delete or a write's due-write rule
/// pruned, best effort: the rows are pruned, so no read reaches a file
/// left behind, and the sweep or boot removes a file no live row names.
pub(crate) async fn unlink_evidence(root: &Arc<ResultsRoot>, files: Vec<ExpiredFile>) {
    let ids: Vec<String> = files
        .into_iter()
        .filter(|file| file.file_bytes > 0)
        .map(|file| file.id)
        .collect();
    if ids.is_empty() {
        return;
    }
    let root = Arc::clone(root);
    let unlinked = tokio::task::spawn_blocking(move || {
        for id in &ids {
            if let Err(error) = root.unlink(id) {
                tracing::warn!(
                    event_type = "results_unlink_failed",
                    error = %error,
                    "could not unlink a pruned evidence file; the sweep removes it"
                );
            }
        }
    })
    .await;
    if let Err(error) = unlinked {
        tracing::warn!(
            event_type = "results_unlink_failed",
            error = %error,
            "could not unlink pruned evidence files; the sweep removes them"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_api::investigations::FindingStatus;

    const RESULT: &str = "0123456789abcdef0123456789abcdef";

    fn bad_request(result: Result<impl std::fmt::Debug, ServerError>) -> String {
        match result {
            Err(ServerError::BadRequest(message)) => message,
            other => panic!("expected a 400, got {other:?}"),
        }
    }

    fn note(text: &str) -> AppendEntry {
        AppendEntry::Note {
            text: text.into(),
            seen_seq: None,
        }
    }

    fn finding(references: usize, counterevidence: usize) -> AppendEntry {
        let refs = |n: usize| {
            (0..n as u64)
                .map(|row| EvidenceRef {
                    result_id: RESULT.into(),
                    row,
                })
                .collect()
        };
        AppendEntry::Finding {
            statement: "the 502s start with the deploy".into(),
            status: FindingStatus::Hypothesis,
            references: refs(references),
            counterevidence: refs(counterevidence),
            remaining_question: None,
            seen_seq: None,
        }
    }

    #[test]
    fn a_text_bound_admits_its_limit_and_refuses_one_byte_more() {
        assert!(check_append(&note(&"n".repeat(NOTE_MAX_BYTES))).is_ok());
        let message = bad_request(check_append(&note(&"n".repeat(NOTE_MAX_BYTES + 1))));
        assert_eq!(message, "text is 4097 bytes, above the limit of 4096 bytes");
        // Bytes, not characters.
        let wide = "ą".repeat(NOTE_MAX_BYTES / 2 + 1);
        assert!(bad_request(check_append(&note(&wide))).starts_with("text is 4098 bytes"));
    }

    #[test]
    fn nul_and_blank_text_are_refused() {
        assert_eq!(
            bad_request(check_append(&note("a\0b"))),
            "text contains a NUL character, which cannot be stored"
        );
        assert_eq!(bad_request(check_append(&note(" \n"))), "text is empty");
        let creation = CreateInvestigation {
            title: "t".into(),
            question: "q".into(),
            scope: "s\0".into(),
        };
        assert!(bad_request(check_creation(&creation)).starts_with("scope contains a NUL"));
    }

    #[test]
    fn a_finding_cites_at_most_its_limits_and_names_each_citation() {
        let full = finding(FINDING_MAX_REFERENCES, 2);
        let citations = check_append(&full).unwrap();
        assert_eq!(citations.len(), FINDING_MAX_REFERENCES + 2);
        assert_eq!(citations[0].what, "references[0]");
        assert_eq!(
            citations[FINDING_MAX_REFERENCES + 1].what,
            "counterevidence[1]"
        );
        assert_eq!(
            bad_request(check_append(&finding(FINDING_MAX_REFERENCES + 1, 0))),
            "references has 33 items, above the limit of 32"
        );
        assert_eq!(
            bad_request(check_append(&finding(0, FINDING_MAX_COUNTEREVIDENCE + 1))),
            "counterevidence has 33 items, above the limit of 32"
        );
    }

    #[test]
    fn a_citation_of_a_malformed_result_id_is_refused_by_name() {
        let entry = AppendEntry::Finding {
            statement: "s".into(),
            status: FindingStatus::Supported,
            references: vec![EvidenceRef {
                result_id: "../../etc/passwd".into(),
                row: 0,
            }],
            counterevidence: Vec::new(),
            remaining_question: None,
            seen_seq: None,
        };
        assert_eq!(
            bad_request(check_append(&entry)),
            "references[0] names a result id that is not 32 lowercase hex digits"
        );
    }

    #[test]
    fn a_keep_holds_one_to_its_limit_of_rows_and_a_reason() {
        let keep = |rows: usize, reason: &str| AppendEntry::Keep {
            result_id: RESULT.into(),
            rows: (0..rows as u64).collect(),
            reason: reason.into(),
            seen_seq: Some(3),
        };
        assert_eq!(
            check_append(&keep(KEEP_MAX_ROWS, "why")).unwrap().len(),
            200
        );
        assert_eq!(
            bad_request(check_append(&keep(KEEP_MAX_ROWS + 1, "why"))),
            "rows has 201 items, above the limit of 200"
        );
        assert!(bad_request(check_append(&keep(0, "why"))).contains("names no rows"));
        assert_eq!(bad_request(check_append(&keep(1, ""))), "reason is empty");
        assert!(check_append(&keep(1, &"r".repeat(KEEP_REASON_MAX_BYTES))).is_ok());
        assert!(check_append(&keep(1, &"r".repeat(KEEP_REASON_MAX_BYTES + 1))).is_err());
    }

    #[test]
    fn retain_until_reads_only_as_a_utc_instant() {
        let at = parse_utc("2026-11-30T00:00:00.1234567Z").unwrap();
        assert_eq!(at.timestamp_subsec_nanos(), 123_456_000);
        assert!(parse_utc("2026-11-30T00:00:00+00:00").is_ok());
        for bad in ["2026-11-30", "2026-11-30T01:00:00+01:00", "tomorrow", ""] {
            assert!(bad_request(parse_utc(bad)).starts_with("retain_until is not"));
        }
    }

    #[test]
    fn an_append_records_its_entry_without_the_seen_sequence() {
        assert_eq!(
            payload(AppendEntry::Note {
                text: "x".into(),
                seen_seq: Some(9)
            }),
            EntryPayload::Note { text: "x".into() }
        );
    }
}
