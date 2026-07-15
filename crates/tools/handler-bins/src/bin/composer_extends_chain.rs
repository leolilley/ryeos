use ryeos_handler_bins::{extends_chain, run_handler};
use ryeos_handler_protocol::{HandlerRequest, HandlerResponse, ResolutionStepNameWire};

fn main() {
    std::process::exit(run_handler(|req| match req {
        HandlerRequest::Compose(c) => match extends_chain::compose(&c.composer_config, &c) {
            Ok(success) => HandlerResponse::ComposeOk(success),
            Err((step, reason)) => HandlerResponse::ComposeErr { step, reason },
        },
        HandlerRequest::ValidateComposerConfig(v) => {
            match extends_chain::validate_config(&v.composer_config).and_then(|()| {
                extends_chain::validate_field_requirements(
                    &v.composer_config,
                    &v.field_requirements,
                )
            }) {
                Ok(()) => HandlerResponse::ValidateComposerOk {
                    field_requirements: v.field_requirements,
                },
                Err(msg) => HandlerResponse::ValidateErr { message: msg },
            }
        }
        HandlerRequest::Parse(_)
        | HandlerRequest::ValidateParserConfig(_)
        | HandlerRequest::LaunchPrepare(_)
        | HandlerRequest::ValidateLaunchPreparerConfig(_) => HandlerResponse::ComposeErr {
            step: ResolutionStepNameWire::PipelineInit,
            reason: "this is a composer binary; received parser request".into(),
        },
    }));
}
