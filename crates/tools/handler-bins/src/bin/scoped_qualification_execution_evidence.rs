use ryeos_handler_bins::{run_handler, scoped_qualification_execution_evidence};
use ryeos_handler_protocol::HandlerRequest;

fn main() {
    std::process::exit(run_handler(|request| match request {
        HandlerRequest::ExecutionEvidenceDescribe(request) => {
            scoped_qualification_execution_evidence::describe(request)
        }
        HandlerRequest::ExecutionEvidenceProject(request) => {
            scoped_qualification_execution_evidence::project(request)
        }
        _ => scoped_qualification_execution_evidence::wrong_request(),
    }));
}
