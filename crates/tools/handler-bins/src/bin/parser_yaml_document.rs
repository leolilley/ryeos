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
        HandlerRequest::ValidateParserConfig(v) => {
            match yaml_document::validate_config(&v.parser_config) {
                Ok(()) => HandlerResponse::ValidateOk,
                Err(msg) => HandlerResponse::ValidateErr { message: msg },
            }
        }
        HandlerRequest::Compose(_) | HandlerRequest::ValidateComposerConfig(_) => {
            HandlerResponse::ParseErr {
                kind: ryeos_handler_protocol::ParseErrKind::Internal,
                message: "this is a parser binary; received composer request".into(),
            }
        }
    }));
}
