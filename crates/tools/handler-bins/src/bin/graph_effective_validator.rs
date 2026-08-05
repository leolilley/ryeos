use ryeos_handler_bins::{graph_effective_validator, run_handler};
use ryeos_handler_protocol::HandlerRequest;

fn main() {
    std::process::exit(run_handler(|request| match request {
        HandlerRequest::EffectiveValidate(request) => graph_effective_validator::validate(request),
        HandlerRequest::Parse(_)
        | HandlerRequest::ValidateParserConfig(_)
        | HandlerRequest::Compose(_)
        | HandlerRequest::ValidateComposerConfig(_)
        | HandlerRequest::LaunchPrepare(_)
        | HandlerRequest::ValidateLaunchPreparerConfig(_) => {
            graph_effective_validator::wrong_request()
        }
    }));
}
