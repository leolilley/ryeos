use ryeos_handler_bins::{directive_launch, run_handler};
use ryeos_handler_protocol::HandlerRequest;

fn main() {
    std::process::exit(run_handler(|request| match request {
        HandlerRequest::LaunchPrepare(request) => directive_launch::prepare(request),
        HandlerRequest::ValidateLaunchPreparerConfig(request) => {
            directive_launch::validate(request)
        }
        HandlerRequest::Parse(_)
        | HandlerRequest::ValidateParserConfig(_)
        | HandlerRequest::Compose(_)
        | HandlerRequest::ValidateComposerConfig(_) => directive_launch::wrong_request(),
    }));
}
