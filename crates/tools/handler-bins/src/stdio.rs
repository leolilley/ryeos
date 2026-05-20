use ryeos_handler_protocol::{HandlerRequest, HandlerResponse};
use std::io::Read;

pub fn run_handler<F>(dispatch: F) -> i32
where
    F: FnOnce(HandlerRequest) -> HandlerResponse,
{
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("handler: failed to read stdin: {e}");
        return 2;
    }
    let request: HandlerRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("handler: malformed stdin JSON: {e}");
            return 2;
        }
    };
    let response = dispatch(request);
    match serde_json::to_writer(std::io::stdout(), &response) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("handler: failed to write stdout: {e}");
            2
        }
    }
}
