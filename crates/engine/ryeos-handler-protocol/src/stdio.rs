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
    let written =
        lillux::stdout_exclusive_file().and_then(|mut stdout| write_response(&mut stdout, &bytes));
    match written {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("failed to write the handler response to stdout: {e}");
            2
        }
    }
}

/// Write the whole response and flush it, surfacing either error. The flush
/// is explicit: the one at process exit drops its error, which would leave a
/// clean exit with no response.
fn write_response(out: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    out.write_all(bytes)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::write_response;
    use std::io::{Error, ErrorKind, Write};

    struct Failing(ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(Error::from(self.0))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(Error::from(self.0))
        }
    }

    #[test]
    fn write_failure_is_reported_not_swallowed() {
        let error = write_response(&mut Failing(ErrorKind::BrokenPipe), b"{}").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenPipe);
    }

    #[test]
    fn flush_failure_is_reported() {
        struct FlushFails;
        impl Write for FlushFails {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(Error::from(ErrorKind::BrokenPipe))
            }
        }
        assert!(write_response(&mut FlushFails, b"{}").is_err());
    }

    #[test]
    fn successful_write_delivers_all_bytes() {
        let mut out = Vec::new();
        write_response(&mut out, b"{\"a\":1}").unwrap();
        assert_eq!(out, b"{\"a\":1}");
    }
}
