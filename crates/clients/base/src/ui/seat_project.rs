//! The project a seat states, as the daemon judged it, carried as display
//! data.
//!
//! The daemon computes the value when a binding is admitted and again when it
//! is rebound, and it rides each binding descriptor. A client draws it and
//! decides nothing from it: every act stays authorized by the daemon from the
//! caller's key, grants and capabilities, and the daemon never reads this
//! value back as authority.
//!
//! The wire shape is
//! `{"state": "stated"|"absent"|"projectless", "project"?: "<identity>", "absence"?: "<code>"}`.
//! A descriptor with no such value is refused. An absence code this client
//! does not know parses to [`UiSeatAbsence::Unknown`] and is drawn as the
//! fixed "not known" line; it is never an error and never guessed at.

use serde::{Deserialize, Serialize};

/// Longest project name part drawn on the chip, in characters.
pub const SEAT_PROJECT_NAME_DISPLAY_MAX: usize = 64;

/// Drawn when the signed wording has no sentence for an absence, or has not
/// been read. The signed wording replaces it whenever it holds a sentence.
pub const SEAT_PROJECT_UNKNOWN_LABEL: &str = "This seat's project is not known.";

/// Prefix of the key under which the signed wording keeps the sentence for
/// one absence code.
const WORDING_KEY_PREFIX: &str = "seat_project_";

const IDENTITY_PREFIX: &str = "pid:v1:";
const OWNER_HEX_LEN: usize = 64;
const NAME_MAX_LEN: usize = 64;
/// Longest absence code kept verbatim when it is not one this client knows.
const UNKNOWN_CODE_MAX_LEN: usize = 64;
/// Kept in place of an unrecognized code that is not a plain identifier.
const UNRECOGNIZED_CODE: &str = "unrecognized";

/// Why a seat bound to a checkout states no project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiSeatAbsence {
    Unregistered,
    OwnerKeyNotActive,
    NoProjectName,
    CheckoutNameChanged,
    /// A code outside the closed four. Carries the code only so the value
    /// serializes back as it arrived; it is never drawn.
    Unknown(String),
}

impl UiSeatAbsence {
    /// The wire code.
    pub fn code(&self) -> &str {
        match self {
            Self::Unregistered => "unregistered",
            Self::OwnerKeyNotActive => "owner_key_not_active",
            Self::NoProjectName => "no_project_name",
            Self::CheckoutNameChanged => "checkout_name_changed",
            Self::Unknown(code) => code,
        }
    }

    /// Read a wire code. Never fails: a code outside the closed four, or one
    /// that is not a plain bounded identifier, is `Unknown`.
    pub fn from_code(code: &str) -> Self {
        match code {
            "unregistered" => Self::Unregistered,
            "owner_key_not_active" => Self::OwnerKeyNotActive,
            "no_project_name" => Self::NoProjectName,
            "checkout_name_changed" => Self::CheckoutNameChanged,
            other => {
                let plain = !other.is_empty()
                    && other.len() <= UNKNOWN_CODE_MAX_LEN
                    && other.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                    });
                Self::Unknown(if plain { other } else { UNRECOGNIZED_CODE }.to_owned())
            }
        }
    }

    /// The key of this absence's sentence in the signed wording, or `None`
    /// for a code this client does not know.
    pub fn wording_key(&self) -> Option<String> {
        match self {
            Self::Unknown(_) => None,
            known => Some(format!("{WORDING_KEY_PREFIX}{}", known.code())),
        }
    }
}

/// A seat's project state, display data only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiSeatProject {
    /// The seat states one project: `pid:v1:<64 lowercase hex>/<name>`.
    Stated { project: String },
    /// The seat is bound to a checkout that states no project now.
    Absent { absence: UiSeatAbsence },
    /// The seat is bound to no project.
    Projectless,
}

/// A project identity is `pid:v1:` and 64 lowercase hex digits, a `/`, then a
/// name of `[a-z0-9][a-z0-9._-]{0,63}`. Shape only: it names a project and
/// proves nothing about who owns it.
pub fn validate_seat_project_identity(value: &str) -> Result<(), &'static str> {
    let rest = value
        .strip_prefix(IDENTITY_PREFIX)
        .ok_or("seat project identity does not start with pid:v1:")?;
    let (owner, name) = rest
        .split_once('/')
        .ok_or("seat project identity has no name part")?;
    if owner.len() != OWNER_HEX_LEN
        || !owner
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("seat project identity owner is not 64 lowercase hex digits");
    }
    let mut bytes = name.bytes();
    let first = bytes.next().ok_or("seat project name is empty")?;
    if name.len() > NAME_MAX_LEN
        || !(first.is_ascii_lowercase() || first.is_ascii_digit())
        || !bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
    {
        return Err("seat project name is not a valid identifier");
    }
    Ok(())
}

impl UiSeatProject {
    /// The project name part: the segment after the last `/`. Never the
    /// owner. `None` unless the seat states a project.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Stated { project } => project.rsplit('/').next(),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Tag {
    Stated,
    Absent,
    Projectless,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    state: Tag,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    absence: Option<String>,
}

impl From<&UiSeatProject> for Wire {
    fn from(value: &UiSeatProject) -> Self {
        match value {
            UiSeatProject::Stated { project } => Wire {
                state: Tag::Stated,
                project: Some(project.clone()),
                absence: None,
            },
            UiSeatProject::Absent { absence } => Wire {
                state: Tag::Absent,
                project: None,
                absence: Some(absence.code().to_owned()),
            },
            UiSeatProject::Projectless => Wire {
                state: Tag::Projectless,
                project: None,
                absence: None,
            },
        }
    }
}

impl TryFrom<Wire> for UiSeatProject {
    type Error = String;

    fn try_from(wire: Wire) -> Result<Self, Self::Error> {
        match (wire.state, wire.project, wire.absence) {
            (Tag::Stated, Some(project), None) => {
                validate_seat_project_identity(&project).map_err(str::to_owned)?;
                Ok(Self::Stated { project })
            }
            (Tag::Absent, None, Some(code)) => Ok(Self::Absent {
                absence: UiSeatAbsence::from_code(&code),
            }),
            (Tag::Projectless, None, None) => Ok(Self::Projectless),
            _ => Err(
                "seat project: `stated` carries only `project`, `absent` only \
                      `absence`, `projectless` neither"
                    .to_owned(),
            ),
        }
    }
}

impl Serialize for UiSeatProject {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Wire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for UiSeatProject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Wire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// The words a project chip draws and the signed tone name they are drawn
/// in. Display only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiSeatProjectChip {
    /// The words drawn: the project name, or the signed sentence for an
    /// absence. Inert text.
    pub text: String,
    /// The signed tone name of the sentence; `none` for a project name.
    pub tone: String,
}

/// Bound a project name to what the chip draws, by characters.
fn bounded_name(name: &str) -> String {
    name.chars().take(SEAT_PROJECT_NAME_DISPLAY_MAX).collect()
}

/// The chip a seat's project draws. `stated` draws the name part only;
/// `absent` draws the signed sentence for its code, or the fixed "not known"
/// line when the wording has none; `projectless` draws nothing. The wording
/// entry for an absence is read from `reasons` under
/// `seat_project_<code>`.
pub fn project_chip(
    seat_project: &UiSeatProject,
    wording: Option<&super::dto::RyeOsCoverageWordingDto>,
) -> Option<UiSeatProjectChip> {
    match seat_project {
        UiSeatProject::Projectless => None,
        UiSeatProject::Stated { .. } => seat_project.name().map(|name| UiSeatProjectChip {
            text: bounded_name(name),
            tone: "none".to_owned(),
        }),
        UiSeatProject::Absent { absence } => {
            let entry = absence
                .wording_key()
                .and_then(|key| wording.and_then(|wording| wording.reasons.get(&key)));
            Some(match entry {
                Some(entry) => UiSeatProjectChip {
                    text: entry.label.clone(),
                    tone: entry.tone.clone(),
                },
                None => UiSeatProjectChip {
                    text: SEAT_PROJECT_UNKNOWN_LABEL.to_owned(),
                    tone: "none".to_owned(),
                },
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity(name: &str) -> String {
        format!("pid:v1:{}/{name}", "ab".repeat(32))
    }

    fn wording() -> super::super::dto::RyeOsCoverageWordingDto {
        super::super::dto::RyeOsCoverageWordingDto::from_response(&json!({
            "schema_version": "ryeos.ui.coverage_wording.v1",
            "fallback": {"label": "Not known.", "tone": "none"},
            "states": {},
            "reasons": {
                "seat_project_unregistered": {"label": "Register this folder.", "tone": "warn"},
                "seat_project_owner_key_not_active": {"label": "Activate the key.", "tone": "warn"},
                "seat_project_no_project_name": {"label": "Name the project.", "tone": "warn"},
                "seat_project_checkout_name_changed": {"label": "The name changed.", "tone": "warn"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn the_three_states_and_four_codes_round_trip() {
        let stated = json!({"state": "stated", "project": identity("snap-track")});
        let parsed: UiSeatProject = serde_json::from_value(stated.clone()).unwrap();
        assert_eq!(parsed.name(), Some("snap-track"));
        assert_eq!(serde_json::to_value(&parsed).unwrap(), stated);
        let projectless = json!({"state": "projectless"});
        let parsed: UiSeatProject = serde_json::from_value(projectless.clone()).unwrap();
        assert_eq!(parsed, UiSeatProject::Projectless);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), projectless);
        for code in [
            "unregistered",
            "owner_key_not_active",
            "no_project_name",
            "checkout_name_changed",
        ] {
            let wire = json!({"state": "absent", "absence": code});
            let parsed: UiSeatProject = serde_json::from_value(wire.clone()).unwrap();
            assert!(matches!(
                &parsed,
                UiSeatProject::Absent { absence } if !matches!(absence, UiSeatAbsence::Unknown(_))
            ));
            assert_eq!(serde_json::to_value(&parsed).unwrap(), wire);
        }
    }

    #[test]
    fn a_state_with_the_wrong_fields_or_a_bad_identity_is_refused() {
        for wire in [
            json!({"state": "stated"}),
            json!({"state": "stated", "project": identity("a"), "absence": "unregistered"}),
            json!({"state": "absent"}),
            json!({"state": "absent", "absence": "unregistered", "project": identity("a")}),
            json!({"state": "projectless", "project": identity("a")}),
            json!({"state": "projectless", "absence": "unregistered"}),
            json!({"state": "elsewhere"}),
            json!({"state": "projectless", "extra": true}),
            json!({"state": "stated", "project": "pid:v1:short/name"}),
            json!({"state": "stated", "project": identity("Upper")}),
            json!({"state": "stated", "project": identity("has space")}),
            json!({"state": "stated", "project": identity("")}),
            json!({"state": "stated", "project": identity(&"a".repeat(65))}),
            json!({"state": "stated", "project": format!("pid:v1:{}", "ab".repeat(32))}),
            json!({"state": "stated", "project": format!("fp:{}/name", "ab".repeat(32))}),
        ] {
            assert!(
                serde_json::from_value::<UiSeatProject>(wire.clone()).is_err(),
                "{wire}"
            );
        }
    }

    #[test]
    fn an_unknown_absence_code_parses_and_draws_the_not_known_line() {
        let wire = json!({"state": "absent", "absence": "key_was_eaten"});
        let parsed: UiSeatProject = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            parsed,
            UiSeatProject::Absent {
                absence: UiSeatAbsence::Unknown("key_was_eaten".into())
            }
        );
        assert_eq!(serde_json::to_value(&parsed).unwrap(), wire);
        let words = wording();
        let chip = project_chip(&parsed, Some(&words)).unwrap();
        assert_eq!(chip.text, SEAT_PROJECT_UNKNOWN_LABEL);
        // A code that is not a plain identifier is also just unknown.
        let odd: UiSeatProject =
            serde_json::from_value(json!({"state": "absent", "absence": "<b>Loud!</b>"})).unwrap();
        assert_eq!(
            project_chip(&odd, Some(&words)).unwrap().text,
            SEAT_PROJECT_UNKNOWN_LABEL
        );
    }

    #[test]
    fn each_state_draws_its_signed_sentence_or_nothing() {
        let words = wording();
        let stated = UiSeatProject::Stated {
            project: identity("snap-track"),
        };
        let chip = project_chip(&stated, Some(&words)).unwrap();
        assert_eq!(chip.text, "snap-track");
        assert!(!chip.text.contains("pid:") && !chip.text.contains("abab"));
        assert_eq!(
            project_chip(&UiSeatProject::Projectless, Some(&words)),
            None
        );
        for (code, sentence) in [
            ("unregistered", "Register this folder."),
            ("owner_key_not_active", "Activate the key."),
            ("no_project_name", "Name the project."),
            ("checkout_name_changed", "The name changed."),
        ] {
            let absent = UiSeatProject::Absent {
                absence: UiSeatAbsence::from_code(code),
            };
            let chip = project_chip(&absent, Some(&words)).unwrap();
            assert_eq!(chip.text, sentence);
            assert_eq!(chip.tone, "warn");
        }
    }

    #[test]
    fn missing_wording_draws_the_not_known_line() {
        let absent = UiSeatProject::Absent {
            absence: UiSeatAbsence::Unregistered,
        };
        assert_eq!(
            project_chip(&absent, None).unwrap().text,
            SEAT_PROJECT_UNKNOWN_LABEL
        );
        let without = super::super::dto::RyeOsCoverageWordingDto::from_response(&json!({
            "schema_version": "ryeos.ui.coverage_wording.v1",
            "fallback": {"label": "Not known.", "tone": "none"},
            "states": {},
            "reasons": {}
        }))
        .unwrap();
        assert_eq!(
            project_chip(&absent, Some(&without)).unwrap().text,
            SEAT_PROJECT_UNKNOWN_LABEL
        );
    }

    #[test]
    fn the_chip_name_is_bounded() {
        // A name is at most 64 characters, so the bound is a ceiling the
        // validator already holds; the chip keeps it independently.
        let long = "a".repeat(200);
        assert_eq!(
            bounded_name(&long).chars().count(),
            SEAT_PROJECT_NAME_DISPLAY_MAX
        );
    }
}
