//! `ryeos node external-execution provision-credential`: offline provisioning
//! of a placement credential into the node vault.
//!
//! Authorization is filesystem access to the node's keys, as for the other
//! offline lifecycle verbs. The secret enters only through standard input or a
//! sealed memfd; it is never an argument, an environment variable or output.

use std::io::{IsTerminal as _, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use ryeos_app::node_config::sections::external_execution::provision::{
    MAX_PLACEMENT_SECRET_BYTES, ProvisionOutcome, ProvisionReport, ProvisionSection,
    provision_installed_credential,
};
use zeroize::Zeroizing;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos node external-execution provision-credential",
    about = "Store the provider secret for an installed external-execution binding in the \
             node vault. Pipe the secret on stdin (or pass a sealed memfd with --secret-fd). \
             Insert-only: the same secret again is a no-op; a different secret for the \
             same credential generation is refused. Works with the daemon stopped or running.",
    no_binary_name = true
)]
struct ExternalProvisionCredentialArgs {
    /// Installed binding id (the file stem under the section directory).
    #[arg(long)]
    binding: String,

    /// Section the binding is installed under: `external_execution` or
    /// `runtime_snapshot_production`.
    #[arg(long, default_value = "external_execution")]
    section: String,

    /// App root (parent of `.ai/`). Defaults to XDG data dir / ryeos.
    #[arg(long)]
    app_root: Option<PathBuf>,

    /// Read the secret from this inherited sealed memfd instead of stdin.
    #[arg(long)]
    secret_fd: Option<u32>,

    /// Emit JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

/// Entry point for the registered verb. `argv` excludes the verb words.
pub fn run_external_provision_credential(
    argv: &[String],
    console: &crate::tty::Console,
) -> Result<()> {
    use clap::error::ErrorKind;
    let args = match ExternalProvisionCredentialArgs::try_parse_from(argv) {
        Ok(args) => args,
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                console.text(&error.render().to_string())?;
                return Ok(());
            }
            _ => bail!("{error}"),
        },
    };
    let section = ProvisionSection::parse(&args.section)?;
    let secret = match args.secret_fd {
        Some(fd) => read_secret_from_sealed_fd(fd)?,
        None => {
            let stdin = std::io::stdin();
            let is_terminal = stdin.is_terminal();
            read_secret_from_stdin(is_terminal, &mut stdin.lock())?
        }
    };
    let app_root = ryeos_node::NodeConfig::load_local(args.app_root)?.app_root;
    let report = provision_installed_credential(&app_root, section, &args.binding, &secret)?;
    if args.json {
        crate::tty::write_json(&serde_json::json!({ "app_root": app_root, "provision": report }))?;
        return Ok(());
    }
    console.status(&provision_banner(&app_root, &report))?;
    Ok(())
}

fn provision_banner(
    app_root: &std::path::Path,
    report: &ProvisionReport,
) -> crate::tty::StatusBanner {
    let (tone, heading) = match report.outcome {
        ProvisionOutcome::Inserted => (
            crate::tty::Tone::Success,
            "PLACEMENT CREDENTIAL PROVISIONED",
        ),
        ProvisionOutcome::AlreadyPresent => (
            crate::tty::Tone::Neutral,
            "PLACEMENT CREDENTIAL ALREADY PRESENT",
        ),
    };
    let mut banner = crate::tty::StatusBanner::new(tone, heading);
    for (key, value) in [
        ("app root", app_root.display().to_string()),
        ("section", report.section.clone()),
        ("binding", report.binding_id.clone()),
        ("owner", report.owner.clone()),
        ("generation", report.generation.clone()),
        ("backend", report.backend.clone()),
        ("account", report.account.clone()),
        ("outcome", report.outcome.as_str().to_owned()),
    ] {
        banner.rows.push(crate::tty::Row::key_value(key, value));
    }
    banner
}

fn read_secret_from_stdin(is_terminal: bool, stdin: &mut impl Read) -> Result<Zeroizing<String>> {
    ensure!(
        !is_terminal,
        "pipe the secret on stdin; refusing interactive echo"
    );
    // Preallocate the whole bound so reading never reallocates a copy of the
    // secret that would escape zeroization; one extra byte detects overflow.
    let limit = MAX_PLACEMENT_SECRET_BYTES + 3;
    let mut bytes = Zeroizing::new(Vec::with_capacity(limit));
    stdin
        .take(limit as u64)
        .read_to_end(&mut bytes)
        .context("read secret from stdin")?;
    normalize_secret(bytes)
}

/// Accept only a sealed memfd; Lillux refuses any other descriptor kind.
fn read_secret_from_sealed_fd(fd: u32) -> Result<Zeroizing<String>> {
    let bytes = lillux::read_sealed_inherited_descriptor(fd, MAX_PLACEMENT_SECRET_BYTES + 3)
        .map_err(|error| {
            anyhow::anyhow!("--secret-fd must name a sealed memfd; refusing it: {error}")
        })?;
    normalize_secret(Zeroizing::new(bytes))
}

/// Strip exactly one trailing LF or CRLF, then enforce the secret bounds.
fn normalize_secret(bytes: Zeroizing<Vec<u8>>) -> Result<Zeroizing<String>> {
    let mut end = bytes.len();
    if bytes.ends_with(b"\r\n") {
        end -= 2;
    } else if bytes.ends_with(b"\n") {
        end -= 1;
    }
    let secret = &bytes[..end];
    ensure!(!secret.is_empty(), "refusing an empty secret");
    ensure!(
        secret.len() <= MAX_PLACEMENT_SECRET_BYTES,
        "refusing a secret larger than {MAX_PLACEMENT_SECRET_BYTES} bytes"
    );
    let text = std::str::from_utf8(secret).context("secret is not UTF-8")?;
    Ok(Zeroizing::new(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdin(bytes: &[u8]) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(bytes.to_vec())
    }

    #[test]
    fn section_defaults_to_external_execution_and_is_validated() {
        let args = ExternalProvisionCredentialArgs::try_parse_from(["--binding", "b"]).unwrap();
        assert_eq!(args.section, "external_execution");
        assert_eq!(
            ProvisionSection::parse(&args.section).unwrap(),
            ProvisionSection::ExternalExecution
        );
        let args = ExternalProvisionCredentialArgs::try_parse_from([
            "--binding",
            "b",
            "--section",
            "runtime_snapshot_production",
        ])
        .unwrap();
        assert_eq!(
            ProvisionSection::parse(&args.section).unwrap(),
            ProvisionSection::RuntimeSnapshotProduction
        );
        assert!(ProvisionSection::parse("nonsense").is_err());
    }

    #[test]
    fn a_terminal_stdin_is_refused_without_reading() {
        let error = read_secret_from_stdin(true, &mut stdin(b"secret\n")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "pipe the secret on stdin; refusing interactive echo"
        );
    }

    #[test]
    fn exactly_one_trailing_newline_is_stripped() {
        for (input, expected) in [
            (&b"abc"[..], "abc"),
            (b"abc\n", "abc"),
            (b"abc\r\n", "abc"),
            (b"abc\n\n", "abc\n"),
            (b"a\nb\n", "a\nb"),
        ] {
            let secret = read_secret_from_stdin(false, &mut stdin(input)).unwrap();
            assert_eq!(secret.as_str(), expected);
        }
    }

    #[test]
    fn empty_and_oversize_secrets_are_refused() {
        for input in [&b""[..], b"\n", b"\r\n"] {
            assert!(read_secret_from_stdin(false, &mut stdin(input)).is_err());
        }
        let exact = vec![b'x'; MAX_PLACEMENT_SECRET_BYTES];
        assert!(read_secret_from_stdin(false, &mut stdin(&exact)).is_ok());
        let mut with_newline = exact.clone();
        with_newline.push(b'\n');
        assert!(read_secret_from_stdin(false, &mut stdin(&with_newline)).is_ok());
        let mut over = exact;
        over.push(b'x');
        assert!(read_secret_from_stdin(false, &mut stdin(&over)).is_err());
        assert!(read_secret_from_stdin(false, &mut stdin(&vec![b'x'; 1 << 20])).is_err());
        assert!(read_secret_from_stdin(false, &mut stdin(&[0xff, 0xfe])).is_err());
    }

    #[test]
    fn secret_fd_accepts_only_a_sealed_memfd() {
        let sealed =
            lillux::sealed_memfd(c"provision-credential-test", b"sealed-secret\n").unwrap();
        let fd = sealed.inherited_descriptor().unwrap();
        assert_eq!(
            read_secret_from_sealed_fd(fd).unwrap().as_str(),
            "sealed-secret"
        );

        // A regular file descriptor is not sealed against mutation.
        let file = tempfile::tempfile().unwrap();
        let coordinate = lillux::inherited_descriptor_coordinate(&file).unwrap();
        let error = read_secret_from_sealed_fd(coordinate).unwrap_err();
        assert!(error.to_string().contains("sealed memfd"), "{error:#}");
        // Standard streams and closed descriptors are refused too.
        assert!(read_secret_from_sealed_fd(0).is_err());
        assert!(read_secret_from_sealed_fd(1_000_000).is_err());
    }

    #[test]
    fn the_report_banner_carries_coordinates_and_no_secret() {
        let report = ProvisionReport {
            section: "external_execution".into(),
            binding_id: "b".into(),
            owner: "a".repeat(64),
            generation: "c".repeat(64),
            backend: "render-sandbox-early-access".into(),
            account: "acct".into(),
            outcome: ProvisionOutcome::Inserted,
        };
        let banner = provision_banner(std::path::Path::new("/srv/node"), &report);
        assert_eq!(banner.heading, "PLACEMENT CREDENTIAL PROVISIONED");
        let value = |key: &str| {
            banner
                .rows
                .iter()
                .find(|row| row.key.as_deref() == Some(key))
                .map(|row| row.value.clone())
                .unwrap_or_else(|| panic!("no {key} row"))
        };
        assert_eq!(value("outcome"), "inserted");
        assert_eq!(value("section"), "external_execution");
        assert_eq!(value("account"), "acct");
    }
}
