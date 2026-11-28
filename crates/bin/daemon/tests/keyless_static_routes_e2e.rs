//! A keyless static route neither sets a cookie nor reads one.
//!
//! The shipped keyless static routes (`/ui` and `/ui/assets/{asset}`) are
//! fetched with no cookie and with a forged session cookie. Neither answer
//! sets a cookie, and the two answers are the same, so the handler did not
//! consult the cookie it was sent.

use ryeosd::test_support as common;

use common::DaemonHarness;

const FORGED_COOKIE: &str = "ryeos_session=forged-session-value; ryeos_csrf=forged";

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn fetch(h: &DaemonHarness, path: &str, cookie: Option<&str>) -> Answer {
    let mut request = reqwest::Client::new().get(format!("http://{}{path}", h.bind));
    if let Some(cookie) = cookie {
        request = request.header(reqwest::header::COOKIE, cookie);
    }
    let response = request.send().await.expect("GET");
    let status = response.status().as_u16();
    let mut headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter(|(name, _)| *name != reqwest::header::DATE)
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    headers.sort();
    let body = response.bytes().await.expect("body").to_vec();
    Answer {
        status,
        headers,
        body,
    }
}

fn assert_sets_no_cookie(path: &str, answer: &Answer) {
    assert!(
        !answer
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("set-cookie")),
        "{path} set a cookie: {:?}",
        answer.headers
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn keyless_static_routes_set_no_cookie_and_ignore_one_sent() {
    let (h, _fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {})
        .await
        .expect("start daemon");

    let index = fetch(&h, "/ui", None).await;
    assert_eq!(index.status, 200, "GET /ui");
    let html = String::from_utf8_lossy(&index.body).into_owned();
    let asset = html
        .split('"')
        .find(|part| part.starts_with("/ui/assets/"))
        .unwrap_or_else(|| panic!("/ui links no asset: {html}"))
        .to_owned();

    for path in ["/ui", asset.as_str()] {
        let plain = fetch(&h, path, None).await;
        let with_cookie = fetch(&h, path, Some(FORGED_COOKIE)).await;
        assert_eq!(plain.status, 200, "GET {path}");
        assert_sets_no_cookie(path, &plain);
        assert_sets_no_cookie(path, &with_cookie);
        assert_eq!(
            (plain.status, &plain.headers, &plain.body),
            (with_cookie.status, &with_cookie.headers, &with_cookie.body),
            "{path} answered differently when sent a cookie"
        );
    }
}
