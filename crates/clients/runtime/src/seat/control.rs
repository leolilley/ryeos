//! Reading and changing who owns a seat: observe, touch, relinquish.
//!
//! Control requests (`seat/open`, `seat/close`) are frozen as one encoded
//! body and resent identically, so a lost reply can never turn into a second
//! operation. Observation is read-only and is the only way a client learns a
//! control predecessor.

use std::rc::Rc;

use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{
    UiBindingAttachmentCoordinate, UiObservedPresentationProducer,
    UiPresentationProducerObservation, UiSeatControlPredecessor, UiSeatControlRequest,
};
use serde_json::{Value, json};

use crate::api::SESSION_CURRENT_PATH;
use crate::api::{SEAT_CLOSE_PATH, SEAT_OPEN_PATH, SEAT_TOUCH_PATH};
use crate::codec::{decode_strict, encode_exact};
use crate::error::RuntimeError;
use crate::history::SeatCoordinate;
use crate::platform::{Clock, Platform, Transport};

use super::ack::unwrap_result;
use super::{
    RetryPolicy, SeatShared, TouchOutcome, classify_touch_error, classify_touch_response,
    random_uuid,
};

/// POST an already-encoded control body until the daemon gives a readable
/// answer. Delivery failures, unreadable answers and 5xx are retried with the
/// identical bytes under [`RetryPolicy::WEB`]; any other status is definite.
pub(crate) async fn control_post<P: Platform>(
    platform: &P,
    path: &str,
    encoded: &str,
) -> Result<Value, RuntimeError> {
    let policy = RetryPolicy::WEB;
    let mut attempt = 1;
    loop {
        let outcome = match platform
            .transport()
            .post_encoded(path, encoded.to_owned())
            .await
        {
            Ok(text) => decode_strict(&text).map_err(|_| RuntimeError::UnreadableResponse {
                path: path.to_owned(),
            }),
            Err(error) => Err(error),
        };
        let error = match outcome {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let retryable =
            error.is_unknown_delivery() || error.status().is_some_and(|status| status >= 500);
        if !retryable || !policy.allows_another(attempt) {
            return Err(error);
        }
        platform.clock().sleep(policy.delay(attempt)).await;
        attempt += 1;
    }
}

/// Encode a control body once, the way the daemon will receive every retry.
pub(crate) fn encode_control(body: &Value) -> Result<String, RuntimeError> {
    encode_exact(body)
}

/// Read the retained writer of a seat without claiming anything. The answer
/// must describe exactly this seat and root binding, and its predecessor must
/// agree with its producer state.
pub async fn observe_producer<P: Platform>(
    platform: &P,
    coordinate: &SeatCoordinate,
    root: &UiBindingAttachmentCoordinate,
) -> Result<UiPresentationProducerObservation, RuntimeError> {
    let body = encode_control(&json!({
        "action": "observe",
        "presentation_id": coordinate.presentation_id,
        "thread_id": coordinate.thread_id,
        "chain_root_id": coordinate.chain_root_id,
    }))?;
    let response = control_post(platform, SEAT_OPEN_PATH, &body).await?;
    let record = unwrap_result(&response).map_err(|error| RuntimeError::Fatal(error.0))?;
    let observation: UiPresentationProducerObservation = serde_json::from_value(record.clone())
        .map_err(|error| {
            RuntimeError::Fatal(format!("invalid exact producer observation: {error}"))
        })?;
    observation
        .control_predecessor
        .validate()
        .map_err(|message| RuntimeError::Fatal(message.to_owned()))?;
    let unclaimed = matches!(
        observation.producer,
        UiObservedPresentationProducer::Unclaimed
    );
    let initialization = matches!(
        observation.control_predecessor,
        UiSeatControlPredecessor::Initialization { .. }
    );
    if observation.presentation_id != coordinate.presentation_id
        || observation.thread_id != coordinate.thread_id
        || observation.chain_root_id != coordinate.chain_root_id
        || unclaimed != initialization
    {
        return Err(RuntimeError::Fatal(
            "producer observation differs from its exact seat or control state".to_owned(),
        ));
    }
    if observation.root_binding != *root {
        return Err(RuntimeError::Fatal(
            "prepared seat root binding differs from admitted attachment".to_owned(),
        ));
    }
    Ok(observation)
}

/// Renew the seat's presence lease. Best effort and independent of durable
/// seat events.
pub async fn touch_seat<P: Platform>(
    shared: &Rc<SeatShared<P>>,
    producer: &UiPresentationProducer,
) -> TouchOutcome {
    if !shared.is_current(producer) {
        return TouchOutcome::Refused;
    }
    let Ok(body) = serde_json::to_value(producer)
        .map_err(RuntimeError::from)
        .and_then(|value| encode_exact(&value))
    else {
        return TouchOutcome::Retry;
    };
    match shared
        .platform
        .transport()
        .post_encoded(SEAT_TOUCH_PATH, body)
        .await
    {
        Ok(text) => match decode_strict(&text) {
            Ok(value) => {
                classify_touch_response(&value, &producer.presentation_id, &producer.thread_id)
            }
            Err(_) => TouchOutcome::Retry,
        },
        // A refused credential gets one look at the session: when that is
        // refused too the session is over, otherwise the next interval retries.
        Err(error) if error.status() == Some(401) => {
            match shared.platform.transport().get(SESSION_CURRENT_PATH).await {
                Err(again) if again.status() == Some(401) => TouchOutcome::SessionEnded,
                _ => TouchOutcome::Retry,
            }
        }
        Err(error) => classify_touch_error(&error),
    }
}

/// Relinquish the writer named by an observation, retrying the identical
/// request. A relinquished predecessor is eligible for an explicit resume;
/// this is deliberately not `close`, which is terminal.
async fn relinquish_observed<P: Platform>(
    platform: &P,
    producer: &UiPresentationProducer,
    coordinate: &SeatCoordinate,
    observation: UiPresentationProducerObservation,
) -> Result<(), RuntimeError> {
    let mut request = UiSeatControlRequest::Relinquish {
        schema: 2,
        operation_id: String::new(),
        presentation_id: producer.presentation_id.clone(),
        thread_id: producer.thread_id.clone(),
        chain_root_id: coordinate.chain_root_id.clone(),
        predecessor: observation.control_predecessor,
        intent_nonce: random_uuid(platform),
        producer_incarnation: producer.producer_incarnation.clone(),
    };
    request
        .assign_identity()
        .map_err(|message| RuntimeError::Fatal(message.to_owned()))?;
    let body = serde_json::to_value(&request)?;
    let response = control_post(platform, SEAT_CLOSE_PATH, &encode_control(&body)?).await?;
    let acknowledgement = unwrap_result(&response).map_err(|error| RuntimeError::Fatal(error.0))?;
    let echoed = [
        "presentation_id",
        "thread_id",
        "producer_incarnation",
        "operation_id",
    ]
    .iter()
    .all(|key| acknowledgement.get(*key) == body.get(*key));
    if echoed && acknowledgement.get("state").and_then(Value::as_str) == Some("relinquished") {
        Ok(())
    } else {
        Err(RuntimeError::UnreadableResponse {
            path: SEAT_CLOSE_PATH.to_owned(),
        })
    }
}

/// Relinquish a producer this client claimed and holds no local state for:
/// observe first, and do nothing unless that incarnation is still the active
/// writer. Used to give a claimed seat back when admission fails afterwards.
pub async fn relinquish_claimed_seat<P: Platform>(
    platform: &P,
    producer: &UiPresentationProducer,
    coordinate: &SeatCoordinate,
    root: &UiBindingAttachmentCoordinate,
) -> Result<(), RuntimeError> {
    let observation = observe_producer(platform, coordinate, root).await?;
    let ours = matches!(
        &observation.producer,
        UiObservedPresentationProducer::Active { producer_incarnation }
            if *producer_incarnation == producer.producer_incarnation
    );
    if !ours {
        return Ok(());
    }
    relinquish_observed(platform, producer, coordinate, observation).await
}

/// Learn this producer's own control predecessor, for a relinquish sent as
/// the page goes away. Best effort: nothing is recorded unless the observed
/// active writer is exactly this producer, still current when the answer
/// comes back.
pub async fn learn_relinquish_basis<P: Platform>(shared: &Rc<SeatShared<P>>) {
    let Some(producer) = shared.current_producer() else {
        return;
    };
    let (coordinate, root) = {
        let delivery = shared.delivery.borrow();
        let (Some(coordinate), Some(root)) = (delivery.coordinate.clone(), delivery.root.clone())
        else {
            return;
        };
        (coordinate, root)
    };
    let Ok(observation) = observe_producer(&*shared.platform, &coordinate, &root).await else {
        return;
    };
    let ours = matches!(
        &observation.producer,
        UiObservedPresentationProducer::Active { producer_incarnation }
            if *producer_incarnation == producer.producer_incarnation
    );
    if ours && shared.is_current(&producer) {
        *shared.relinquish_basis.borrow_mut() = Some((producer, observation.control_predecessor));
    }
}

/// The relinquish to send as the page goes away, encoded, when this producer
/// can give the seat back without awaiting anything: it is current, nothing
/// it wrote is unsettled, and it learned its own control predecessor. The
/// answer is never seen; a reload that finds the seat relinquished resumes
/// it, and one that finds it still held offers the takeover.
pub fn unload_relinquish<P: Platform>(shared: &SeatShared<P>) -> Result<String, &'static str> {
    let producer = shared.current_producer().ok_or("no current producer")?;
    let coordinate = {
        let delivery = shared.delivery.borrow();
        if delivery.pending.is_some() {
            return Err("an append is unsettled");
        }
        if delivery.blocked {
            return Err("delivery is blocked");
        }
        delivery.coordinate.clone().ok_or("no seat coordinate")?
    };
    let predecessor = {
        let basis = shared.relinquish_basis.borrow();
        let (owner, predecessor) = basis.as_ref().ok_or("its own control was not learned")?;
        if *owner != producer {
            return Err("the learned control names another producer");
        }
        predecessor.clone()
    };
    let mut request = UiSeatControlRequest::Relinquish {
        schema: 2,
        operation_id: String::new(),
        presentation_id: producer.presentation_id.clone(),
        thread_id: producer.thread_id.clone(),
        chain_root_id: coordinate.chain_root_id,
        predecessor,
        intent_nonce: random_uuid(&*shared.platform),
        producer_incarnation: producer.producer_incarnation,
    };
    request
        .assign_identity()
        .map_err(|_| "the relinquish identity was refused")?;
    let body = serde_json::to_value(&request).map_err(|_| "the relinquish did not encode")?;
    encode_control(&body).map_err(|_| "the relinquish did not encode")
}

/// Relinquish the current producer after the seat has settled.
///
/// Local mutation is fenced before the observation is awaited, so nothing is
/// sent while the predecessor is being read; if the observation fails the
/// fence is lifted again and the error returned.
pub async fn relinquish_current<P: Platform>(
    shared: &Rc<SeatShared<P>>,
) -> Result<(), RuntimeError> {
    let Some(producer) = shared.current_producer() else {
        if shared.relinquishing.borrow().is_some() {
            return Err(RuntimeError::Fatal(
                "an earlier relinquishment is unresolved; the seat was not given back".to_owned(),
            ));
        }
        return Ok(());
    };
    let (coordinate, root) = {
        let mut delivery = shared.delivery.borrow_mut();
        if delivery.pending.is_some() || delivery.blocked {
            return Err(RuntimeError::Fatal(
                "seat mutations must settle before producer relinquishment".to_owned(),
            ));
        }
        let (Some(coordinate), Some(root)) = (delivery.coordinate.clone(), delivery.root.clone())
        else {
            return Err(RuntimeError::Fatal(
                "no active presentation producer to relinquish".to_owned(),
            ));
        };
        delivery.blocked = true;
        (coordinate, root)
    };
    *shared.producer.borrow_mut() = None;
    *shared.relinquishing.borrow_mut() = Some(producer.clone());
    let platform = &*shared.platform;
    let observation = match observe_producer(platform, &coordinate, &root).await {
        Ok(observation) => observation,
        Err(error) => {
            // No control request exists yet: a failed read must not strand an
            // otherwise acknowledged writer, unless it was fenced meanwhile.
            if shared.relinquishing.borrow_mut().take().is_some() {
                shared.delivery.borrow_mut().blocked = false;
                *shared.producer.borrow_mut() = Some(producer);
            }
            return Err(error);
        }
    };
    let ours = matches!(
        &observation.producer,
        UiObservedPresentationProducer::Active { producer_incarnation }
            if *producer_incarnation == producer.producer_incarnation
    );
    if !ours {
        // Another producer holds the seat: there is nothing of ours to give back.
        *shared.relinquishing.borrow_mut() = None;
        return Err(RuntimeError::Fatal(
            "relinquishment no longer names the observed active producer".to_owned(),
        ));
    }
    let result = relinquish_observed(platform, &producer, &coordinate, observation).await;
    if result.is_ok() {
        *shared.relinquishing.borrow_mut() = None;
    }
    result
}
