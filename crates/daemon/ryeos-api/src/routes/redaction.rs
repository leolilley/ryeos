//! Request paths that carry a secret in a path segment.
//!
//! A route whose template captures `{token}` puts a one-time secret in the
//! request path. Anything that logs the path of such a request goes through
//! [`redact_secret_segments`], which replaces that segment.

/// The path capture names whose value is a secret.
const SECRET_CAPTURES: &[&str] = &["token"];

/// What replaces a secret segment.
pub const REDACTED: &str = "<redacted>";

/// `path` with every segment that the route `template` captures under a
/// secret name replaced. A path with fewer segments than the template is
/// returned with the segments it has redacted.
pub fn redact_secret_segments(template: &str, path: &str) -> String {
    let secret_positions: Vec<usize> = template
        .split('/')
        .enumerate()
        .filter(|(_, segment)| {
            segment
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
                .is_some_and(|name| SECRET_CAPTURES.contains(&name))
        })
        .map(|(index, _)| index)
        .collect();
    path.split('/')
        .enumerate()
        .map(|(index, segment)| {
            if secret_positions.contains(&index) {
                REDACTED
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launch_token_is_removed_from_a_logged_path() {
        let logged = redact_secret_segments("/ui/launch/{token}", "/ui/launch/abc123secret");
        assert_eq!(logged, "/ui/launch/<redacted>");
        assert!(!logged.contains("abc123secret"));
    }

    #[test]
    fn a_path_without_a_secret_capture_is_unchanged() {
        assert_eq!(
            redact_secret_segments("/threads/{id}", "/threads/T-1"),
            "/threads/T-1"
        );
    }
}
