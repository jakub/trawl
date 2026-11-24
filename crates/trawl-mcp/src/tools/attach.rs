// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A corpus call attached to an investigation (ADR-0059).

use rmcp::model::CallToolResult;
use trawl_api::investigations::PURPOSE_MAX_BYTES;
use trawl_client::{Activity, Attach, CapturedResult, ClientError, InvestigationId, is_result_id};

use crate::backend::Backend;
use crate::render::activity::{activity_view, printed_through};
use crate::render::refusal;
use crate::render::rows::json_string;
use crate::reply::{AttachedView, InvestigationAction};
use crate::session::{Seen, SeenGuard};

const ATTACHED_NEEDS: &str = "query, investigation_read and investigation_write";

const ATTACHED_PERMISSIONS: [&str; 3] = ["query", "investigation_read", "investigation_write"];

/// A key that reads an investigation can read its kept evidence without
/// being able to write to it, and an attached call it was proposed would be
/// refused 403. A failed `whoami` answers no: the proposal then attaches to
/// nothing, which the key can still run.
pub(crate) async fn may_attach<B: Backend>(backend: &B) -> bool {
    backend.whoami().await.is_ok_and(|whoami| {
        ATTACHED_PERMISSIONS
            .iter()
            .all(|needed| whoami.permissions.iter().any(|held| held == needed))
    })
}

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

#[derive(Debug, Clone)]
pub(crate) struct Attachment {
    pub id: InvestigationId,
    /// The sentence the step records.
    pub purpose: Option<String>,
}

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
    pub(crate) fn id(&self) -> &InvestigationId {
        &self.attachment.id
    }

    pub(crate) fn seen_seq(&self) -> Option<u64> {
        self.seen_seq
    }

    pub(crate) fn attach(&self) -> Attach {
        Attach {
            id: self.attachment.id.clone(),
            seen_seq: self.seen_seq,
            purpose: self.attachment.purpose.clone(),
        }
    }

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
/// prints its result id; `None` for a sound one.
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
