use ryeos_handler_bins::graph_launch;
use ryeos_handler_protocol::{HandlerRequest, run_handler};

fn main() {
    std::process::exit(run_handler(|request| match request {
        HandlerRequest::LaunchPrepare(request) => graph_launch::prepare(request),
        HandlerRequest::ValidateLaunchPreparerConfig(request) => graph_launch::validate(request),
        HandlerRequest::Parse(_)
        | HandlerRequest::EditSource(_)
        | HandlerRequest::ValidateParserConfig(_)
        | HandlerRequest::Compose(_)
        | HandlerRequest::ValidateComposerConfig(_)
        | HandlerRequest::EffectiveValidate(_)
        | HandlerRequest::ExecutionEvidenceDescribe(_)
        | HandlerRequest::ExecutionEvidenceProject(_) => graph_launch::wrong_request(),
    }));
}
