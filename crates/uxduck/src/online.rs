//! Clients for UXDuck's API: the account, sync and feedback (S16, S27,
//! S29, S31). None of them follows a redirect. The API only answers JSON,
//! so a redirect means something is in the way: a network that wants you to
//! sign in to it first, or Cloudflare Access in front of a staging server.
//! Followed, it turns a request into a fetch of a sign-in page that answers
//! 200, and the request seems to work when nothing was sent. Downloads of
//! releases and simulator builds keep following redirects: GitHub's release
//! links are redirects.

use std::time::Duration;

use reqwest::Url;
use reqwest::header::LOCATION;

/// A client for the API, with this build in its user agent.
#[must_use]
pub fn client(timeout: Duration) -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("uxduck/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()
}

/// Why the API answered with a redirect, in words for the person; `None`
/// when it did not.
#[must_use]
pub fn redirected(answer: &reqwest::Response) -> Option<String> {
    explain(
        answer.status().as_u16(),
        answer.url(),
        answer
            .headers()
            .get(LOCATION)
            .and_then(|location| location.to_str().ok()),
    )
}

fn explain(status: u16, from: &Url, location: Option<&str>) -> Option<String> {
    if !(300..400).contains(&status) {
        return None;
    }
    let to = location.and_then(|location| from.join(location).ok());
    let host = to.as_ref().and_then(Url::host_str).unwrap_or_default();
    Some(
        if host == "cloudflareaccess.com" || host.ends_with(".cloudflareaccess.com") {
            "UXDuck's server is behind Cloudflare Access, which UXDuck cannot sign in to.".into()
        } else if !host.is_empty() && Some(host) != from.host_str() {
            "The network wants you to sign in to it first. Open any web page to do that, then try again."
                .into()
        } else {
            format!("UXDuck's server answered with a redirect ({status}), which it never should.")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redirect_is_explained_by_where_it_goes() {
        let api = Url::parse("https://api-staging.uxduck.com/v1/me").unwrap();
        let access = explain(
            302,
            &api,
            Some("https://orth.cloudflareaccess.com/cdn-cgi/access/login/api-staging.uxduck.com"),
        );
        assert!(access.unwrap().contains("Cloudflare Access"));
        let portal = explain(302, &api, Some("http://hotel-wifi.example/login"));
        assert!(portal.unwrap().contains("sign in to it first"));
        let same = explain(301, &api, Some("/v1/me/"));
        assert!(same.unwrap().contains("redirect (301)"));
        assert_eq!(explain(200, &api, None), None);
        assert_eq!(explain(404, &api, None), None);
    }
}
