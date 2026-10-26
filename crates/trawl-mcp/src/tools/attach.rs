// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A corpus call attached to an investigation (ADR-0059).
//!
//! `search` with `mode: execute`, `context`, `sightings` and `compare`
//! attach the same way, through the pieces here, each worded for its
//! [`Caller`]:
//!
//! - [`attachment`] checks the call's flat `investigation` and `purpose`
//!   before any request: the id's shape, the purpose's bound and that it
//!   holds no NUL, which trawld refuses, and no purpose without an
//!   investigation.
//! - [`hold`] takes the session's lock for an attached call, before it
//!   sends anything, and reads the `seen_seq` it sends; [`Held::record`]
//!   moves `seen` after the render under [`crate::session`]'s rules. An
//!   unattached call takes no lock.
//! - [`unsound`] refuses an answer nothing here can stand behind before
//!   anything prints its result id.
//! - [`acknowledgement`] is the line every error opens with once trawld
//!   accepted the attached call, and [`acknowledged`] puts it in front of
//!   a render that failed.
//! - [`view`] is the attached view the reply prints: the kept result and
//!   the activity since `seen_seq`, or where the session's following
//!   starts.
//! - [`refused`] names the way back from trawld's refusal.
//! - [`may_attach`] says whether this key may make an attached call, for
//!   a reply that proposes one it did not just make.
//!
//! Nothing here retries.

use rmcp::model::CallToolResult;
use trawl_api::investigations::PURPOSE_MAX_BYTES;
use trawl_client::{Activity, Attach, CapturedResult, ClientError, InvestigationId, is_result_id};

use crate::backend::Backend;
use crate::render::activity::{activity_view, printed_through};
use crate::render::refusal;
use crate::render::rows::json_string;
use crate::reply::{AttachedView, InvestigationAction};
use crate::session::{Seen, SeenGuard};

/// The permissions an attached call needs, in words.
const ATTACHED_NEEDS: &str = "query, investigation_read and investigation_write";

/// The permissions an attached call needs, as `whoami` names them.
const ATTACHED_PERMISSIONS: [&str; 3] = ["query", "investigation_read", "investigation_write"];

/// Whether this key holds every permission an attached call needs, by its
/// `whoami`. A key that reads an investigation can read its kept evidence
/// without being able to write to it, and an attached call it was proposed
/// would be refused 403. A failed `whoami` answers no: the proposal then
/// attaches to nothing, which the key can still run.
pub(crate) async fn may_attach<B: Backend>(backend: &B) -> bool {
    backend.whoami().await.is_ok_and(|whoami| {
        ATTACHED_PERMISSIONS
            .iter()
            .all(|needed| whoami.permissions.iter().any(|held| held == needed))
    })
}

/// How a tool that attaches is named in what it says.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Caller {
    /// The tool's name, as its errors open: `search`.
    pub tool: &'static str,
    /// What an input refusal names: `search mode=execute`.
    pub input: &'static str,
    /// What the step is: `search`, `context read`.
    pub noun: &'static str,
    /// What trawld's answer is: `capture answer`.
    pub answer: &'static str,
}

/// The investigation a call attaches to, and why.
#[derive(Debug, Clone)]
pub(crate) struct Attachment {
    /// The investigation.
    pub id: InvestigationId,
    /// The sentence the step records.
    pub purpose: Option<String>,
}

/// The attachment of a call naming `investigation` and `purpose`, checked
/// before any request; `None` for a call attached to nothing.
pub(crate) fn attachment(
    caller: Caller,
    investigation: Option<&String>,
    purpose: Option<&String>,
) -> Result<Option<Attachment>, CallToolResult> {
    let input = caller.input;
    let Some(id) = investigation else {
        if purpose.is_some() {
            return Err(refusal::error_result(format!(
                "{input} takes purpose only with investigation: the purpose is recorded on the \
                 investigation's step."
            )));
        }
        return Ok(None);
    };
    let Ok(id) = InvestigationId::parse(id) else {
        return Err(refusal::error_result(format!(
            "{input}: investigation is 32 lowercase hex digits, and {} is not; investigation \
             action=list lists them.",
            json_string(id)
        )));
    };
    if let Some(purpose) = purpose
        && purpose.len() > PURPOSE_MAX_BYTES
    {
        return Err(refusal::error_result(format!(
            "{input}: purpose is {} bytes; it may hold at most {PURPOSE_MAX_BYTES}.",
            purpose.len()
        )));
    }
    if purpose.is_some_and(|purpose| purpose.contains('\0')) {
        return Err(refusal::error_result(format!(
            "{input}: purpose holds a NUL character, which a step cannot record."
        )));
    }
    Ok(Some(Attachment {
        id,
        purpose: purpose.cloned(),
    }))
}

/// An attached call's hold on the session: the lock, taken before the call
/// sends anything, and the `seen_seq` it sends.
pub(crate) struct Held<'a> {
    attachment: Attachment,
    guard: SeenGuard<'a>,
    seen_seq: Option<u64>,
}

/// The session's lock for an attached call, `None` for a call attached to
/// nothing. An attached call holds it across the request and the render,
/// so the session's next call sends the seq this one printed through. An
/// unattached call reads and writes nothing of `seen`, so it takes no
/// lock and never waits on, or holds up, another call.
pub(crate) async fn hold(seen: &Seen, attachment: Option<Attachment>) -> Option<Held<'_>> {
    let attachment = attachment?;
    let guard = seen.lock().await;
    let seen_seq = guard.get(&attachment.id);
    Some(Held {
        attachment,
        guard,
        seen_seq,
    })
}

impl Held<'_> {
    /// The investigation.
    pub(crate) fn id(&self) -> &InvestigationId {
        &self.attachment.id
    }

    /// The session's `seen_seq` for it, sent with the call.
    pub(crate) fn seen_seq(&self) -> Option<u64> {
        self.seen_seq
    }

    /// The attachment the request carries.
    pub(crate) fn attach(&self) -> Attach {
        Attach {
            id: self.attachment.id.clone(),
            seen_seq: self.seen_seq,
            purpose: self.attachment.purpose.clone(),
        }
    }

    /// Move `seen` after the reply `result` rendered: through the last
    /// activity entry its `part` printed; or, for a session that had seen
    /// nothing of the investigation, to the step the call appended
    /// (`step_seq`), once the reply saying so fitted: the entries after it
    /// are what the next attached call asks for.
    pub(crate) fn record(&mut self, result: &CallToolResult, part: &[&str], step_seq: Option<u64>) {
        let id = &self.attachment.id;
        match (self.seen_seq, step_seq) {
            (Some(since), _) => self.guard.printed(id, since, printed_through(result, part)),
            (None, Some(step_seq)) if result.is_error != Some(true) => {
                self.guard.advance(id, step_seq);
            }
            _ => {}
        }
    }
}

/// The error for an answer nothing here can stand behind, before anything
/// prints its result id; `None` for a sound one. An unsound answer names a
/// result id that is not 32 lowercase hex digits, which trawld never mints
/// and no read can page, or, for a call attached to investigation
/// `attached`, a result trawld does not say it keeps for that
/// investigation.
///
/// The error names no result. An attached call's step may stand whatever
/// the answer said, so its error says so and names the board read from
/// `seen_seq`, the session's sequence, whose step entry for this call
/// names the result kept.
pub(crate) fn unsound(
    caller: Caller,
    captured: &CapturedResult,
    attached: Option<&InvestigationId>,
    seen_seq: Option<u64>,
) -> Option<CallToolResult> {
    const MALFORMED: &str = "names a result id that is not 32 lowercase hex digits";
    let Caller {
        tool, noun, answer, ..
    } = caller;
    let malformed = !is_result_id(&captured.id);
    let Some(id) = attached else {
        return malformed.then(|| {
            refusal::error_result(format!(
                "{tool} failed: trawld's {answer} {MALFORMED}, so no read can page the result. \
                 Run the {noun} again."
            ))
        });
    };
    let reason = if malformed {
        MALFORMED
    } else {
        match &captured.investigation_id {
            Some(kept_for) if kept_for == id => return None,
            Some(_) => "names another investigation as the one the result is kept for",
            None => "names no investigation the result is kept for",
        }
    };
    let since = seen_seq.unwrap_or(0);
    Some(refusal::error_result(format!(
        "{tool} with investigation cannot tell what trawld kept: its {answer} {reason}. \
         Investigation {} may hold this {noun} as a step; investigation action=read id={} \
         since={since} prints the entries changed after #{since}, and a step for this {noun} \
         names the result it keeps.",
        json_string(id.as_str()),
        id.as_str(),
    )))
}

/// The line an error opens with after trawld recorded the call as a step
/// of investigation `id` and kept result `result_id` for it, as an
/// investigation write's error opens with its acknowledgement.
pub(crate) fn acknowledgement(caller: Caller, id: &InvestigationId, result_id: &str) -> String {
    format!(
        "Investigation {} recorded this {} as a step and keeps result {} for it.",
        json_string(id.as_str()),
        caller.noun,
        json_string(result_id)
    )
}

/// `result`, or, when it is an error and the call was attached, the same
/// error opening with `acknowledgement`. The renderer's error names the
/// budget and the result in a few hundred bytes, which its cap never
/// cuts, so this is the one cap that can.
pub(crate) fn acknowledged(acknowledgement: &str, result: CallToolResult) -> CallToolResult {
    if result.is_error != Some(true) || acknowledgement.is_empty() {
        return result;
    }
    let text = result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map_or("", |text| text.text.as_str());
    refusal::error_after(acknowledgement, text)
}

/// The attached view of a result kept for investigation `id`: the activity
/// since `seen_seq`, or why there is none. A first call, with no
/// `seen_seq`, says where the session's following starts: at the step the
/// call appended (`step_seq`), or, when trawld named none, at the
/// session's next write.
pub(crate) fn view(
    caller: Caller,
    id: &InvestigationId,
    captured: &CapturedResult,
    seen_seq: Option<u64>,
    step_seq: Option<u64>,
    activity: Option<&Activity>,
) -> AttachedView {
    let (activity, note) = match (seen_seq, activity) {
        (Some(since), Some(activity)) => (Some(activity_view(id.as_str(), since, activity)), None),
        (Some(_), None) => (
            None,
            Some(format!("trawld sent no activity with this {}", caller.noun)),
        ),
        (None, _) if step_seq.is_some() => (
            None,
            Some(format!(
                "this session follows this investigation from this step on; what other keys \
                 wrote before it is on the board, which investigation action=read id={} \
                 since=0 prints",
                id.as_str()
            )),
        ),
        (None, _) => (
            None,
            Some(format!(
                "this session starts following this investigation at its next write here; what \
                 other keys wrote is on the board, which investigation action=read id={} \
                 since=0 prints",
                id.as_str()
            )),
        ),
    };
    AttachedView {
        id: id.as_str().to_owned(),
        result_id: Some(captured.id.clone()),
        activity,
        note,
    }
}

/// The refusal of an attached call trawld refused, with the way back.
/// `sent` is the query text an excerpt is cut from, if any.
pub(crate) fn refused(
    caller: Caller,
    id: &InvestigationId,
    error: &ClientError,
    sent: Option<&str>,
) -> CallToolResult {
    refusal::refusal_with(
        caller.tool,
        error,
        sent,
        &super::investigation::advice(
            &format!("{} with investigation", caller.tool),
            ATTACHED_NEEDS,
            InvestigationAction::Note,
            Some(id),
            error,
        ),
    )
}
