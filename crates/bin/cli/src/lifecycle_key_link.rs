//! `ryeos key {link-body,link-sign,link,unlink}`: build, sign, apply and
//! withdraw key links.
//!
//! A link joins one more key to a principal's owner. Two keys sign the same
//! body where their private halves are, so neither key ever travels:
//!
//! 1. `ryeos key link-body` builds the body from the public halves;
//! 2. `ryeos key link-sign` signs it with one private key (the principal's own
//!    key says "this key is me", the added key says "I am that principal");
//! 3. `ryeos key link` applies the body and both signatures on a stopped node
//!    as the signed offline operation `node.key.link`.
//!
//! `ryeos key unlink` withdraws one link by id, or every link one key signed.
//! A link into the node operator's owner can only be applied by the node's own
//! operator; `link` and `unlink` are local-signer-only operations.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use clap::Parser;
use serde_json::json;

use crate::lifecycle_commands::parse_or_render_help;

#[derive(Parser, Debug)]
#[command(
    name = "ryeos key link-body",
    about = "Build the body of a key link from the two public keys",
    long_about = "Prints the JSON body both keys must sign. The principal is the one the linking key acts for: pass --principal, or let the stopped node's registry resolve --linked-by. The lifetime is required, at most 365 days; a link into the node operator's owner should use the default of 90 days.",
    no_binary_name = true
)]
struct BodyArgs {
    /// The principal the key is joined to, pid:v1:<64 hex>; defaults to the
    /// owner the linking key acts for on this node.
    #[arg(long)]
    principal: Option<String>,

    /// The principal's own key whose signature vouches for the link: a key
    /// file, or `fp:<hex>:<base64 public key>` when the file is elsewhere.
    #[arg(long)]
    linked_by: String,

    /// The key being added: a key file, or `fp:<hex>:<base64 public key>`.
    #[arg(long)]
    key: String,

    /// Days the link lasts, from now.
    #[arg(long, default_value_t = 90)]
    days: u64,

    #[arg(long)]
    app_root: Option<PathBuf>,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos key link-sign",
    about = "Sign a key-link body with one private key",
    long_about = "Reads a body from `link-body` and prints the base64 signature of its id. Run it once with the principal's own key and once with the added key.",
    no_binary_name = true
)]
struct SignArgs {
    /// The body file written from `link-body`.
    #[arg(long)]
    body: PathBuf,

    /// The private key file that signs.
    #[arg(long)]
    key_file: PathBuf,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos key link",
    about = "Pin a signed key link on a stopped node",
    long_about = "Applies the body and both signatures as the signed offline operation `node.key.link`. Only the node's own operator, or a key holding a single-use grant that the operator's side issued for exactly that key, may apply it. The node must be stopped; the link takes effect at the next start.",
    no_binary_name = true
)]
struct LinkArgs {
    #[arg(long)]
    body: PathBuf,
    /// Signature by the principal's own key (from `link-sign`).
    #[arg(long)]
    linked_by_signature: String,
    /// Signature by the added key (from `link-sign`).
    #[arg(long)]
    key_signature: String,
    #[arg(long)]
    app_root: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "ryeos key unlink",
    about = "Withdraw a key link, or every link one key signed, on a stopped node",
    long_about = "Writes node-signed tombstones named by the link id. With --signed-by, withdraws every link that key signed: the cleanup after it is compromised. The node must be stopped.",
    no_binary_name = true
)]
struct UnlinkArgs {
    /// The link id.
    #[arg(long, conflicts_with = "signed_by")]
    link_id: Option<String>,
    /// `fp:<hex>` of the linking key.
    #[arg(long)]
    signed_by: Option<String>,
    #[arg(long)]
    reason: String,
    #[arg(long)]
    app_root: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

/// A key named on the command line: its fingerprint and base64 public key.
fn public_half(spec: &str) -> Result<(String, String)> {
    if let Some(rest) = spec.strip_prefix("fp:") {
        let (hex, public) = rest
            .split_once(':')
            .context("expected `fp:<hex>:<base64 public key>` or a key file")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(public)
            .context("the public key is not base64")?;
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("the public key is not 32 bytes"))?;
        let verifying = lillux::crypto::VerifyingKey::from_bytes(&array)?;
        let fingerprint = lillux::signature::compute_fingerprint(&verifying);
        ensure!(
            fingerprint == hex,
            "the public key does not hash to the fingerprint {hex}"
        );
        return Ok((format!("fp:{hex}"), public.to_owned()));
    }
    let key = lillux::crypto::load_signing_key(std::path::Path::new(spec))
        .with_context(|| format!("read the key file {spec}"))?;
    let verifying = key.verifying_key();
    Ok((
        format!("fp:{}", lillux::signature::compute_fingerprint(&verifying)),
        base64::engine::general_purpose::STANDARD.encode(verifying.to_bytes()),
    ))
}

fn read_body(path: &std::path::Path) -> Result<ryeos_engine::trust::KeyLinkBody> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the link body {}", path.display()))?;
    let body: ryeos_engine::trust::KeyLinkBody =
        serde_json::from_str(&text).context("the file is not a key-link body")?;
    body.validate_shape()?;
    Ok(body)
}

fn build_body(args: BodyArgs) -> Result<ryeos_engine::trust::KeyLinkBody> {
    ensure!(
        args.days > 0 && args.days <= ryeos_engine::trust::MAX_LINK_LIFETIME_SECS / 86_400,
        "--days must be between 1 and 365"
    );
    let (linked_by, linked_by_public) = public_half(&args.linked_by)?;
    let (key, key_public) = public_half(&args.key)?;
    let principal = match args.principal {
        Some(principal) => principal,
        None => {
            let app_root = ryeos_node::NodeConfig::load_local(args.app_root)?.app_root;
            let trust = ryeos_engine::trust::TrustStore::load_node_registry(&app_root)
                .map_err(|error| anyhow::anyhow!("load the node signer registry: {error}"))?;
            trust
                .owner_for(&linked_by)
                .map_err(|refusal| anyhow::anyhow!("{refusal}"))?
                .principal
                .to_string()
        }
    };
    let now = lillux::time::unix_secs_now();
    let nonce: [u8; 16] = rand::random();
    let body = ryeos_engine::trust::KeyLinkBody {
        schema: ryeos_engine::trust::KEY_LINK_SCHEMA.to_owned(),
        nonce: lillux::sha256_hex(&nonce)[..32].to_owned(),
        principal,
        key,
        key_public,
        linked_by,
        linked_by_public,
        created_at: lillux::time::iso8601_from_unix_secs(now),
        not_after: lillux::time::iso8601_from_unix_secs(now + args.days * 86_400),
        request_digest: lillux::sha256_hex(b"ryeos/key-link-cli/v1"),
    };
    body.validate_shape()?;
    Ok(body)
}

pub fn run(verb: &str, argv: &[String], console: &crate::tty::Console) -> Result<()> {
    match verb {
        "link-body" => {
            let Some(args) = parse_or_render_help::<BodyArgs>(argv, console)? else {
                return Ok(());
            };
            let body = build_body(args)?;
            console.text(&format!("{}\n", serde_json::to_string_pretty(&body)?))?;
            console.text(&format!(
                "# link id {}\n",
                body.id().map_err(anyhow::Error::new)?
            ))?;
            Ok(())
        }
        "link-sign" => {
            let Some(args) = parse_or_render_help::<SignArgs>(argv, console)? else {
                return Ok(());
            };
            let body = read_body(&args.body)?;
            let key = lillux::crypto::load_signing_key(&args.key_file)
                .with_context(|| format!("read the key file {}", args.key_file.display()))?;
            let fingerprint = format!(
                "fp:{}",
                lillux::signature::compute_fingerprint(&key.verifying_key())
            );
            ensure!(
                fingerprint == body.key || fingerprint == body.linked_by,
                "{fingerprint} is neither the linking key nor the added key this body names"
            );
            let signed =
                ryeos_engine::trust::sign_link_side(&body, &key).map_err(anyhow::Error::new)?;
            console.text(&format!(
                "{}\n",
                json!({ "link_id": body.id().map_err(anyhow::Error::new)?, "signer": fingerprint, "signature": signed })
            ))?;
            Ok(())
        }
        "link" => {
            let Some(args) = parse_or_render_help::<LinkArgs>(argv, console)? else {
                return Ok(());
            };
            let body = read_body(&args.body)?;
            let link = ryeos_engine::trust::SignedKeyLink {
                body,
                linked_by_signature: args.linked_by_signature,
                key_signature: args.key_signature,
            };
            link.verify().map_err(anyhow::Error::new)?;
            apply(
                "node.key.link",
                serde_json::to_value(ryeos_node::offline_operation::handlers::KeyLinkArguments {
                    link,
                })?,
                args.app_root,
                args.json,
                console,
            )
        }
        "unlink" => {
            let Some(args) = parse_or_render_help::<UnlinkArgs>(argv, console)? else {
                return Ok(());
            };
            let arguments = ryeos_node::offline_operation::handlers::KeyUnlinkArguments {
                link_id: args.link_id,
                signed_by: args.signed_by,
                reason: args.reason,
            };
            arguments.validate()?;
            apply(
                "node.key.unlink",
                serde_json::to_value(arguments)?,
                args.app_root,
                args.json,
                console,
            )
        }
        other => bail!("unknown key verb `{other}`"),
    }
}

fn apply(
    operation: &str,
    arguments: serde_json::Value,
    app_root: Option<PathBuf>,
    json: bool,
    console: &crate::tty::Console,
) -> Result<()> {
    let app_root = ryeos_node::NodeConfig::load_local(app_root)?.app_root;
    ryeos_node::trust_registry::refuse_while_running(&app_root).map_err(anyhow::Error::new)?;
    let applied = crate::offline_apply::CliHost::new().apply(
        &app_root,
        operation,
        arguments,
        BTreeMap::new(),
        BTreeMap::new(),
    )?;
    if json {
        return crate::tty::write_json(
            &json!({ "status": "applied", "operation": operation, "result": applied.result }),
        )
        .map_err(Into::into);
    }
    console.text(&format!(
        "{operation} applied: {}\n",
        serde_json::to_string_pretty(&applied.result)?
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlink_names_exactly_one_selector() {
        assert!(
            UnlinkArgs::try_parse_from([
                "--link-id",
                "ab",
                "--signed-by",
                "fp:cd",
                "--reason",
                "x"
            ])
            .is_err()
        );
        let args = UnlinkArgs::try_parse_from(["--signed-by", "fp:cd", "--reason", "x"]).unwrap();
        assert!(args.link_id.is_none());
    }

    #[test]
    fn a_public_half_must_hash_to_its_fingerprint() {
        let key = lillux::crypto::SigningKey::from_bytes(&[3; 32]);
        let verifying = key.verifying_key();
        let hex = lillux::signature::compute_fingerprint(&verifying);
        let public = base64::engine::general_purpose::STANDARD.encode(verifying.to_bytes());
        let (fingerprint, encoded) = public_half(&format!("fp:{hex}:{public}")).unwrap();
        assert_eq!(fingerprint, format!("fp:{hex}"));
        assert_eq!(encoded, public);
        assert!(public_half(&format!("fp:{}:{public}", "0".repeat(64))).is_err());
    }
}
