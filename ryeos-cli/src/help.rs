use std::io::Write;

use crate::verbs::VerbTable;

pub fn print_table_help(table: &VerbTable, mut out: impl Write) -> std::io::Result<()> {
    writeln!(out, "rye — CLI for Rye OS")?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "  rye [-p PROJECT] [--debug] <verb...> [args...]")?;
    writeln!(out)?;
    writeln!(out, "COMMANDS:")?;
    let mut sorted: Vec<_> = table.all().iter().collect();
    sorted.sort_by(|a, b| a.verb_tokens.cmp(&b.verb_tokens));
    for e in sorted {
        writeln!(
            out,
            "  {:<30} {}",
            e.verb_tokens.join(" "),
            e.description
        )?;
    }
    writeln!(out)?;
    writeln!(out, "Run `rye help <verb>` for verb-specific help.")?;
    Ok(())
}

pub fn print_verb_help(
    table: &VerbTable,
    verb_tokens: &[String],
    mut out: impl Write,
) -> Result<(), crate::error::CliError> {
    let entry = table.all().iter().find(|e| e.verb_tokens == verb_tokens);
    match entry {
        Some(e) => {
            writeln!(out, "rye {} — {}", e.verb_tokens.join(" "), e.description)
                .map_err(|e| crate::error::CliError::Internal { detail: e.to_string() })?;
            writeln!(out)
                .map_err(|e| crate::error::CliError::Internal { detail: e.to_string() })?;
            writeln!(out, "  Execute: {}", e.execute)
                .map_err(|e| crate::error::CliError::Internal { detail: e.to_string() })?;
            writeln!(out, "  Cap:     {}", e.required_cap)
                .map_err(|e| crate::error::CliError::Internal { detail: e.to_string() })?;
            writeln!(out, "  Source:  {}", e.source_file.display())
                .map_err(|e| crate::error::CliError::Internal { detail: e.to_string() })?;
            writeln!(out, "  Signer:  {}", e.signer_fingerprint)
                .map_err(|e| crate::error::CliError::Internal { detail: e.to_string() })?;
            Ok(())
        }
        None => Err(crate::error::CliError::UnknownVerb {
            argv: verb_tokens.to_vec(),
        }),
    }
}
