use ryeos_handler_bins::{graph_execution_evidence, run_handler};
use ryeos_handler_protocol::HandlerRequest;

fn main() {
    std::process::exit(run_handler(|request| match request {
        HandlerRequest::ExecutionEvidenceDescribe(request) => {
            graph_execution_evidence::describe(request)
        }
        HandlerRequest::ExecutionEvidenceProject(request) => {
            graph_execution_evidence::project(request)
        }
        _ => graph_execution_evidence::wrong_request(),
    }));
}
