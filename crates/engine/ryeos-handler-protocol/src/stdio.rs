use crate::{HandlerRequest, HandlerRequestEnvelope, HandlerResponse, HandlerResponseEnvelope};
use std::io::{Read, Write};

/// Run one handler exchange over stdio: read a single request envelope from
/// stdin, dispatch it, and write the response envelope to stdout. Returns the
/// process exit code; nonzero only when no response could be produced.
pub fn run_handler<F>(dispatch: F) -> i32
where
    F: FnOnce(HandlerRequest) -> HandlerResponse,
{
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("handler: failed to read stdin: {e}");
        return 2;
    }
    let envelope: HandlerRequestEnvelope = match crate::from_json_str_strict(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("handler: malformed stdin JSON: {e}");
            return 2;
        }
    };
    let request: HandlerRequest = match envelope.into_request() {
        Ok(request) => request,
        Err(e) => {
            eprintln!("handler: invalid request envelope: {e}");
            return 2;
        }
    };
    let response = dispatch(request);
    let bytes = match serde_json::to_vec(&HandlerResponseEnvelope::new(response)) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("handler: failed to encode the response: {e}");
            return 2;
        }
    };
    // The flush is explicit: the one at process exit drops its error, which
    // would leave a clean exit with no response.
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("handler: failed to write stdout: {e}");
            2
        }
    }
}
