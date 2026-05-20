use ryeos_handler_bins::{identity, run_handler};
use ryeos_handler_protocol::{HandlerRequest, HandlerResponse, ResolutionStepNameWire};

fn main() {
    std::process::exit(run_handler(|req| match req {
        HandlerRequest::Compose(c) => match identity::compose(&c.composer_config, &c) {
            Ok(success) => HandlerResponse::ComposeOk(success),
            Err((step, reason)) => HandlerResponse::ComposeErr {
                step,
                reason,
            },
        },
        HandlerRequest::ValidateComposerConfig(v) => {
            match identity::validate_config(&v.composer_config) {
                Ok(()) => HandlerResponse::ValidateOk,
                Err(msg) => HandlerResponse::ValidateErr { message: msg },
            }
        }
        HandlerRequest::Parse(_) | HandlerRequest::ValidateParserConfig(_) => {
            HandlerResponse::ComposeErr {
                step: ResolutionStepNameWire::PipelineInit,
                reason: "this is a composer binary; received parser request".into(),
            }
        }
    }));
}
