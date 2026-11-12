//! Which seat a session names, and whether an admitted session still names it.
//!
//! The selection functions read the raw session record (`serde_json::Value`)
//! so a malformed record is refused with a specific reason instead of failing
//! typed decoding opaquely. They are independent of how the host learned the
//! record. The post-claim checks run on the typed [`BrowserSession`] the core
//! is built from.
//!
//! Admission is structural: two session records are the same when their
//! serialized values are equal, so object key order never matters.

use ryeos_client_base::ui::{BrowserSession, UiBindingAttachmentCoordinate};
use serde_json::{Map, Value};

use crate::api::safe_bound;
use crate::error::RuntimeError;
use crate::history::SeatCoordinate;

fn fatal(message: &str) -> RuntimeError {
    RuntimeError::Fatal(message.to_owned())
}

/// A root attachment and the append bound the daemon signed for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatBinding {
    pub coordinate: UiBindingAttachmentCoordinate,
    pub max_seat_append_bytes: u64,
}

/// A retained presentation selected by the document address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSelection {
    pub root_attachment: UiBindingAttachmentCoordinate,
    pub presentation: SeatCoordinate,
}

fn record(session: &Value) -> Result<&Map<String, Value>, RuntimeError> {
    session
        .as_object()
        .ok_or_else(|| fatal("authenticated browser session is not an object"))
}

fn required_text(record: &Map<String, Value>, field: &str) -> Result<String, RuntimeError> {
    match record.get(field).and_then(Value::as_str) {
        Some(text) if !text.is_empty() => Ok(text.to_owned()),
        _ => Err(RuntimeError::Fatal(format!(
            "authenticated browser session {field} is invalid"
        ))),
    }
}

/// A subject string: non-empty, untrimmed-free and without control characters.
fn valid_text(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|text| {
        !text.is_empty()
            && text.trim() == text
            && !text
                .chars()
                .any(|c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}'))
    })
}

/// Mechanical shape check of an attachment's `subject` against its
/// `project_path`. Subject metadata never grants access.
pub fn require_attachment_subject(attachment: &Map<String, Value>) -> Result<(), RuntimeError> {
    let subject = attachment
        .get("subject")
        .and_then(Value::as_object)
        .ok_or_else(|| fatal("binding attachment subject is required"))?;
    let project = attachment.get("project_path");
    let has_project = valid_text(project);
    let kind = subject.get("kind").and_then(Value::as_str);
    let ok = match kind {
        Some("projectless") => subject.len() == 1 && project == Some(&Value::Null),
        Some("unregistered_project") => subject.len() == 1 && has_project,
        Some("registered_project") => {
            subject.len() == 2 && valid_text(subject.get("local_id")) && has_project
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(fatal(
            "binding attachment subject contradicts its project metadata or closed schema",
        ))
    }
}

fn session_attachment<'a>(
    session: &'a Map<String, Value>,
    attachment_id: &str,
) -> Result<&'a Map<String, Value>, RuntimeError> {
    let attachments = session
        .get("binding_attachments")
        .and_then(Value::as_array)
        .ok_or_else(|| fatal("authenticated browser session binding_attachments is invalid"))?;
    let mut matches = attachments
        .iter()
        .filter_map(Value::as_object)
        .filter(|entry| {
            entry.get("binding_attachment_id").and_then(Value::as_str) == Some(attachment_id)
        });
    let (Some(attachment), None) = (matches.next(), matches.next()) else {
        return Err(fatal(
            "authenticated browser session does not contain exactly one root attachment",
        ));
    };
    require_attachment_subject(attachment)?;
    Ok(attachment)
}

fn attachment_coordinate(
    attachment: &Map<String, Value>,
) -> Result<UiBindingAttachmentCoordinate, RuntimeError> {
    let generation = attachment
        .get("binding_generation")
        .and_then(Value::as_u64)
        .filter(|generation| *generation >= 1)
        .ok_or_else(|| {
            fatal("authenticated browser session root attachment generation is invalid")
        })?;
    Ok(UiBindingAttachmentCoordinate {
        binding_attachment_id: required_text(attachment, "binding_attachment_id")?,
        binding_generation: generation,
        binding_digest: required_text(attachment, "binding_digest")?,
    })
}

/// The immutable authored launch attachment. Array order is not authority.
pub fn session_root_attachment(
    session: &Value,
) -> Result<UiBindingAttachmentCoordinate, RuntimeError> {
    let session = record(session)?;
    let root_id = required_text(session, "root_attachment_id")?;
    attachment_coordinate(session_attachment(session, &root_id)?)
}

/// The address that resumes `presentation_id` from `current`: the UI root on
/// the same origin, selecting only that presentation and keeping the page's
/// hash route. Every other selector is dropped, so a reload reopens exactly
/// the installed presentation.
pub fn retained_presentation_url(
    current: &str,
    presentation_id: &str,
) -> Result<String, RuntimeError> {
    let path = crate::documents::document_path(presentation_id)
        .map_err(|_| fatal("installed presentation identity is invalid"))?;
    let (scheme, rest) = current
        .split_once("://")
        .ok_or_else(|| fatal("the page address is not absolute"))?;
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|authority| !authority.is_empty())
        .ok_or_else(|| fatal("the page address has no origin"))?;
    let hash = current.find('#').map_or("", |at| &current[at..]);
    Ok(format!("{scheme}://{authority}{path}{hash}"))
}

/// The session's id and the coordinate of one of its attachments, read
/// without trusting array order.
pub fn session_attachment_coordinate(
    session: &Value,
    attachment_id: &str,
) -> Result<(String, UiBindingAttachmentCoordinate), RuntimeError> {
    let session = record(session)?;
    let session_id = required_text(session, "session_id")?;
    let coordinate = attachment_coordinate(session_attachment(session, attachment_id)?)?;
    Ok((session_id, coordinate))
}

/// The signed seat-append byte bound of one attachment, an integer in
/// `1..=2^53-1` so every client reads the same limit.
pub fn session_seat_append_bound(
    session: &Value,
    attachment_id: &str,
) -> Result<u64, RuntimeError> {
    let session = record(session)?;
    let attachment = session_attachment(session, attachment_id)?;
    let bounds = attachment
        .get("binding_request_bounds")
        .and_then(Value::as_object)
        .ok_or_else(|| fatal("authenticated browser session is not an object"))?;
    bounds
        .get("max_seat_append_bytes")
        .and_then(Value::as_u64)
        .and_then(|bound| safe_bound(bound, "max_seat_append_bytes").ok())
        .ok_or_else(|| {
            fatal("authenticated browser session signed seat append byte bound is invalid")
        })
}

/// Select the retained presentation a document address names. `selections`
/// are the values of every `presentation_id` query parameter. Prepared links
/// select membership under the existing session; they contain no grant and
/// never ask for resume or takeover.
pub fn prepared_presentation_selection(
    session: &Value,
    selections: &[String],
) -> Result<Option<PreparedSelection>, RuntimeError> {
    let Some(selected) = selections.first() else {
        return Ok(None);
    };
    if selections.len() != 1 || selected.is_empty() {
        return Err(fatal(
            "prepared presentation selection is ambiguous or empty",
        ));
    }
    let session = record(session)?;
    let memberships = session
        .get("presentations")
        .and_then(Value::as_array)
        .ok_or_else(|| fatal("authenticated presentation memberships are invalid"))?;
    let mut matches = memberships.iter().filter(|membership| {
        membership.get("presentation_id").and_then(Value::as_str) == Some(selected.as_str())
    });
    let (Some(membership), None) = (matches.next(), matches.next()) else {
        return Err(fatal(
            "prepared presentation is not an exact authenticated membership",
        ));
    };
    let membership = record(membership)?;
    let coordinate = membership
        .get("root_binding")
        .ok_or_else(|| fatal("authenticated browser session is not an object"))
        .and_then(record)?;
    let root_id = required_text(coordinate, "binding_attachment_id")?;
    let root = attachment_coordinate(session_attachment(session, &root_id)?)?;
    if coordinate.get("binding_generation").and_then(Value::as_u64) != Some(root.binding_generation)
        || coordinate.get("binding_digest").and_then(Value::as_str)
            != Some(root.binding_digest.as_str())
    {
        return Err(fatal(
            "prepared presentation root binding is stale or invalid",
        ));
    }
    Ok(Some(PreparedSelection {
        root_attachment: root,
        presentation: SeatCoordinate {
            presentation_id: required_text(membership, "presentation_id")?,
            thread_id: required_text(membership, "thread_id")?,
            chain_root_id: required_text(membership, "chain_root_id")?,
        },
    }))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'+' {
            out.push(b' ');
        } else if byte == b'%'
            && let Some(pair) = bytes.get(index + 1..index + 3)
            && let Some(decoded) = std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        {
            out.push(decoded);
            index += 2;
        } else {
            out.push(byte);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every value of query parameter `name` in `url`, in order.
pub fn query_values(url: &str, name: &str) -> Vec<String> {
    let query = url.split_once('?').map_or("", |(_, rest)| rest);
    let query = query.split_once('#').map_or(query, |(head, _)| head);
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key) == name).then(|| percent_decode(value))
        })
        .collect()
}

/// Re-read check used after an authentication response during an append: the
/// session id, exactly one matching root attachment and exactly one matching
/// presentation membership must all be unchanged.
pub fn require_current_session(
    session: &Value,
    expected_session_id: &str,
    expected_attachment: &UiBindingAttachmentCoordinate,
    expected: &SeatCoordinate,
) -> Result<(), RuntimeError> {
    let session = record(session)?;
    if required_text(session, "session_id")? != expected_session_id {
        return Err(fatal(
            "browser session changed while reconciling a seat append",
        ));
    }
    let attachments = session
        .get("binding_attachments")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            fatal("browser session attachments changed while reconciling a seat append")
        })?;
    let mut matches = attachments
        .iter()
        .filter_map(Value::as_object)
        .filter(|entry| {
            entry.get("binding_attachment_id").and_then(Value::as_str)
                == Some(expected_attachment.binding_attachment_id.as_str())
                && entry.get("binding_generation").and_then(Value::as_u64)
                    == Some(expected_attachment.binding_generation)
                && entry.get("binding_digest").and_then(Value::as_str)
                    == Some(expected_attachment.binding_digest.as_str())
        });
    let (Some(attachment), None) = (matches.next(), matches.next()) else {
        return Err(fatal(
            "browser root attachment changed while reconciling a seat append",
        ));
    };
    require_attachment_subject(attachment)?;
    let memberships = session.get("presentations").and_then(Value::as_array);
    let mut matching = memberships
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter(|entry| {
            entry.get("presentation_id").and_then(Value::as_str)
                == Some(expected.presentation_id.as_str())
                && entry.get("thread_id").and_then(Value::as_str)
                    == Some(expected.thread_id.as_str())
                && entry.get("chain_root_id").and_then(Value::as_str)
                    == Some(expected.chain_root_id.as_str())
        });
    let (Some(membership), None) = (matching.next(), matching.next()) else {
        return Err(fatal(
            "browser presentation membership changed while reconciling a seat append",
        ));
    };
    let binding = membership.get("root_binding").and_then(Value::as_object);
    let same_root = binding.is_some_and(|binding| {
        binding.get("binding_attachment_id").and_then(Value::as_str)
            == Some(expected_attachment.binding_attachment_id.as_str())
            && binding.get("binding_generation").and_then(Value::as_u64)
                == Some(expected_attachment.binding_generation)
            && binding.get("binding_digest").and_then(Value::as_str)
                == Some(expected_attachment.binding_digest.as_str())
    });
    if same_root {
        Ok(())
    } else {
        Err(fatal(
            "prepared seat root binding differs from admitted attachment",
        ))
    }
}

// ---------------------------------------------------------------------------
// Typed session checks
// ---------------------------------------------------------------------------

fn seat_binding_of(
    session: &BrowserSession,
    coordinate: &UiBindingAttachmentCoordinate,
    what: &str,
) -> Result<SeatBinding, RuntimeError> {
    let mut matches = session.binding_attachments.iter().filter(|attachment| {
        attachment.binding_attachment_id == coordinate.binding_attachment_id
            && attachment.binding_generation == coordinate.binding_generation
            && attachment.binding_digest == coordinate.binding_digest
    });
    let (Some(attachment), None) = (matches.next(), matches.next()) else {
        return Err(RuntimeError::Fatal(format!(
            "{what} is not exactly one current attachment"
        )));
    };
    Ok(SeatBinding {
        coordinate: coordinate.clone(),
        max_seat_append_bytes: attachment.binding_request_bounds.max_seat_append_bytes,
    })
}

/// The session's authored root attachment: the binding a fresh seat claims.
pub fn root_seat_binding(session: &BrowserSession) -> Result<SeatBinding, RuntimeError> {
    let mut matches = session
        .binding_attachments
        .iter()
        .filter(|attachment| attachment.binding_attachment_id == session.root_attachment_id);
    let (Some(attachment), None) = (matches.next(), matches.next()) else {
        return Err(fatal(
            "UI session omitted or duplicated its authored root attachment",
        ));
    };
    Ok(SeatBinding {
        coordinate: UiBindingAttachmentCoordinate {
            binding_attachment_id: attachment.binding_attachment_id.clone(),
            binding_generation: attachment.binding_generation,
            binding_digest: attachment.binding_digest.clone(),
        },
        max_seat_append_bytes: attachment.binding_request_bounds.max_seat_append_bytes,
    })
}

/// The binding a retained presentation was created under, which must still be
/// exactly one current attachment of the session.
pub fn retained_seat_binding(
    session: &BrowserSession,
    presentation_id: &str,
) -> Result<SeatBinding, RuntimeError> {
    let membership = exact_membership(session, presentation_id)?;
    seat_binding_of(
        session,
        &membership.root_binding,
        "retained presentation root binding",
    )
}

/// The one membership of `presentation_id`, or an error if it is absent or
/// duplicated.
pub fn exact_membership<'a>(
    session: &'a BrowserSession,
    presentation_id: &str,
) -> Result<&'a ryeos_client_base::ui::UiPresentationMembership, RuntimeError> {
    let mut memberships = session
        .presentations
        .iter()
        .filter(|membership| membership.presentation_id == presentation_id);
    match (memberships.next(), memberships.next()) {
        (Some(membership), None) => Ok(membership),
        (None, _) => Err(fatal("UI session has no retained presentation membership")),
        (Some(_), Some(_)) => Err(fatal(
            "UI session duplicated the retained presentation membership",
        )),
    }
}

/// What an admitted session must still say.
#[derive(Debug, Clone)]
pub struct AdmissionTarget<'a> {
    pub session_id: &'a str,
    pub seat: &'a SeatCoordinate,
    pub root: &'a UiBindingAttachmentCoordinate,
}

/// Post-claim admission: the re-read session is the same session and holds
/// exactly one membership naming this seat under this root binding.
pub fn check_admitted_session(
    session: &BrowserSession,
    target: &AdmissionTarget<'_>,
) -> Result<(), RuntimeError> {
    if session.session_id != target.session_id {
        return Err(fatal("session changed during terminal admission"));
    }
    let membership = exact_membership(session, &target.seat.presentation_id)
        .map_err(|_| fatal("presentation membership changed before core construction"))?;
    if membership.thread_id.as_deref() != Some(target.seat.thread_id.as_str())
        || membership.chain_root_id.as_deref() != Some(target.seat.chain_root_id.as_str())
        || membership.root_binding != *target.root
    {
        return Err(fatal(
            "presentation membership changed before core construction",
        ));
    }
    Ok(())
}

/// Structural equality of two session records.
pub fn same_session_record(left: &BrowserSession, right: &BrowserSession) -> bool {
    matches!(
        (serde_json::to_value(left), serde_json::to_value(right)),
        (Ok(left), Ok(right)) if left == right
    )
}

/// Decode a `session/current` body into both views of it: the raw record the
/// selection functions read and the typed session the core is built from.
pub fn decode_session(text: &str) -> Result<(Value, BrowserSession), RuntimeError> {
    let value = crate::codec::decode_strict(text)?;
    let unwrapped = value
        .get("result")
        .cloned()
        .unwrap_or_else(|| value.clone());
    let session: BrowserSession = serde_json::from_value(unwrapped.clone())?;
    Ok((unwrapped, session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_decode_every_occurrence() {
        let url =
            "http://h/ui?presentation_id=a%20b&x=1&presentation_id=c+d#frag?presentation_id=no";
        assert_eq!(query_values(url, "presentation_id"), ["a b", "c d"]);
        assert_eq!(
            query_values("http://h/ui", "presentation_id"),
            Vec::<String>::new()
        );
        assert_eq!(
            query_values("http://h/ui?presentation_id=", "presentation_id"),
            [""]
        );
    }
}
