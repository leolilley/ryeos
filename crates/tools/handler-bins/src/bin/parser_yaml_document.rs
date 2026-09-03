use ryeos_handler_bins::{run_handler, yaml_document};
use ryeos_handler_protocol::{HandlerRequest, HandlerResponse};

fn main() {
    std::process::exit(run_handler(|req| match req {
        HandlerRequest::Parse(p) => match yaml_document::parse(&p.parser_config, &p.content) {
            Ok(v) => HandlerResponse::ParseOk { value: v },
            Err(e) => HandlerResponse::ParseErr {
                kind: e.kind,
                message: e.message,
            },
        },
        HandlerRequest::EditSource(p) => {
            match yaml_document::edit_source(&p.parser_config, &p.content, &p.edits) {
                Ok((content, value)) => HandlerResponse::EditSourceOk { content, value },
                Err(error) => HandlerResponse::EditSourceErr {
                    kind: error.kind,
                    message: error.message,
                },
            }
        }
        HandlerRequest::ValidateParserConfig(v) => {
            match yaml_document::validate_config(&v.parser_config) {
                Ok(()) => HandlerResponse::ValidateOk,
                Err(msg) => HandlerResponse::ValidateErr { message: msg },
            }
        }
        HandlerRequest::Compose(_)
        | HandlerRequest::ValidateComposerConfig(_)
        | HandlerRequest::LaunchPrepare(_)
        | HandlerRequest::ValidateLaunchPreparerConfig(_)
        | HandlerRequest::EffectiveValidate(_) => HandlerResponse::ParseErr {
            kind: ryeos_handler_protocol::ParseErrKind::Internal,
            message: "this is a parser binary; received composer request".into(),
        },
    }));
}
