//! Cookie data types for the cookie provider cohort.
//!
//! Some providers expose usage ONLY through their website, authenticated by a
//! session cookie -- there is no headless token API. The cookie reaches this
//! module as a deposit in the credential vault (`cookie:<domain>[:<account>]`),
//! captured by Cerebellum in a throwaway browser. Insula never reads a browser
//! store itself; this module only holds the parsed form of a deposited
//! `Cookie:` request header so providers can ask it questions.

/// One cookie from a deposited request header.
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// The domain the cookie belongs to, when known. Empty for a cookie parsed
    /// from a request header, which does not carry one.
    pub host_key: String,
}

/// The cookies for one provider domain, plus a `Cookie:` header built from them.
pub struct CookieJar {
    pub cookies: Vec<Cookie>,
}

impl CookieJar {
    /// Build a jar from a `Cookie:` REQUEST header, the form a deposit holds.
    ///
    /// The exact inverse of [`Self::header`], and unambiguous in a way that
    /// parsing `Set-Cookie` would not be: a request header is bare `name=value`
    /// pairs with no attributes, no expiry, no domain and no flags. There is
    /// nothing to interpret, which is why this is not the RFC 6265 parser the
    /// vault deliberately does not own -- the vault stores the bytes verbatim
    /// and never looks inside; this splits them at the point of USE.
    ///
    /// Exists so providers can ask whether the deposit holds a recognised
    /// session cookie and report a specific diagnosis when it does not -- "the
    /// captured header has only trackers in it" versus "your session expired"
    /// are different instructions to a human, and an opaque string could give
    /// neither.
    ///
    /// `host_key` is empty because a request header does not carry one. Absent
    /// rather than wrong: inventing the provider's own domain there would assert
    /// a provenance nobody established.
    pub fn from_header(header: &str) -> Self {
        let cookies = header
            .split(';')
            .filter_map(|pair| {
                let pair = pair.trim();
                // split_once, not splitn: a cookie VALUE may contain '=' (base64
                // padding routinely does), so only the first separator is a
                // delimiter and the rest is payload.
                let (name, value) = pair.split_once('=')?;
                let name = name.trim();
                if name.is_empty() {
                    return None;
                }
                Some(Cookie {
                    name: name.to_string(),
                    value: value.trim().to_string(),
                    host_key: String::new(),
                })
            })
            .collect();
        Self { cookies }
    }

    /// `name=value; name=value` header from all cookies.
    pub fn header(&self) -> String {
        self.cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// True if any cookie name matches `predicate` — used to confirm a real
    /// SESSION cookie is present (not just incidental analytics cookies) before
    /// treating the jar as a usable login.
    pub fn has_cookie_named(&self, predicate: impl Fn(&str) -> bool) -> bool {
        self.cookies.iter().any(|c| predicate(&c.name))
    }

    /// Why no session was found, for the message that says so.
    ///
    /// Providers recognise a session by an allow-list of cookie names, so "no
    /// session cookie" covers two states a reader needs to tell apart. An empty
    /// jar means nobody signed in, which is ordinary and permanent. A jar with
    /// cookies none of which we recognise means either the same thing, or that
    /// the upstream renamed its session cookie and OUR LIST IS STALE -- in which
    /// case a signed-in account is reporting as never configured, the class that
    /// authorises a consumer to forget it.
    ///
    /// Nothing distinguishes those two from outside, which is exactly why the
    /// count belongs in the message: someone who is signed in and sees this
    /// provider missing needs to know cookies were there and went unrecognised.
    /// Names are deliberately not included -- a cookie name is not a secret, but
    /// this string is published on the wire and the count is what answers the
    /// question.
    pub fn session_absence_detail(&self) -> String {
        match self.cookies.len() {
            0 => "no cookies for this domain".to_string(),
            1 => "1 cookie present, not recognised as a session".to_string(),
            n => format!("{n} cookies present, none recognised as a session"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header round-trips to the same jar it was built from.
    ///
    /// The inverse pair is asserted rather than each direction alone: a parser
    /// checked only against hand-written input drifts from the serializer it is
    /// supposed to invert, and the two live in this file precisely so they
    /// cannot.
    #[test]
    fn a_header_round_trips_through_the_jar() {
        let jar = CookieJar {
            cookies: vec![
                Cookie {
                    name: "session".into(),
                    value: "abc123".into(),
                    host_key: ".x.com".into(),
                },
                Cookie {
                    name: "other".into(),
                    value: "v".into(),
                    host_key: ".x.com".into(),
                },
            ],
        };
        let parsed = CookieJar::from_header(&jar.header());
        assert_eq!(parsed.cookies.len(), 2);
        assert_eq!(parsed.cookies[0].name, "session");
        assert_eq!(parsed.cookies[0].value, "abc123");
    }

    /// A value containing '=' survives, because base64 padding routinely has one.
    ///
    /// The tempting split is on every '=', which silently truncates such a value
    /// to its first segment -- producing a cookie that is well-formed, shorter
    /// than what was deposited, and rejected upstream as a bad session with no
    /// indication the truncation happened here.
    #[test]
    fn a_value_containing_equals_is_not_truncated() {
        let jar = CookieJar::from_header("t=YWJjZA==; s=1");
        assert_eq!(jar.cookies[0].value, "YWJjZA==");
        assert_eq!(jar.cookies.len(), 2);
    }

    /// Whitespace around pairs is tolerated, since a human may paste this.
    #[test]
    fn a_pasted_header_tolerates_spacing() {
        let jar = CookieJar::from_header("  a=1 ;  b=2  ");
        assert_eq!(jar.cookies.len(), 2);
        assert_eq!(jar.cookies[1].name, "b");
        assert_eq!(jar.cookies[1].value, "2");
    }

    /// A fragment with no '=' is dropped rather than becoming a nameless cookie.
    #[test]
    fn a_fragment_without_a_separator_is_dropped() {
        let jar = CookieJar::from_header("a=1; garbage; =novalue; b=2");
        let names: Vec<&str> = jar.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"], "only well-formed pairs survive");
    }

    /// The absence detail separates an empty jar from an unrecognised one.
    ///
    /// Both reach the same NoSession, and they are different facts. An empty jar
    /// means nobody signed in. A jar with cookies we do not recognise means that
    /// OR that the upstream renamed its session cookie and our allow-list is
    /// stale -- in which case a signed-in account reports as never configured,
    /// and the operator's only clue is this string.
    ///
    /// Measured shapes: cursor's jar has held three anonymous ids
    /// (_rdt_uuid, cursor_anonymous_id, statsig_stable_id) and qoder's one
    /// tracking cookie, so both the singular and plural forms occur in practice.
    #[test]
    fn the_session_absence_detail_separates_empty_from_unrecognised() {
        let jar = |names: &[&str]| CookieJar {
            cookies: names
                .iter()
                .map(|name| Cookie {
                    name: (*name).to_string(),
                    value: "x".to_string(),
                    host_key: "example.test".to_string(),
                })
                .collect(),
        };

        assert_eq!(
            jar(&[]).session_absence_detail(),
            "no cookies for this domain"
        );
        assert_eq!(
            jar(&["tfstk"]).session_absence_detail(),
            "1 cookie present, not recognised as a session"
        );
        assert_eq!(
            jar(&["_rdt_uuid", "cursor_anonymous_id", "statsig_stable_id"])
                .session_absence_detail(),
            "3 cookies present, none recognised as a session"
        );

        // Not vacuous: the three answers differ, so a helper collapsing them into
        // one string fails here rather than passing with a plausible message.
        let all = [
            jar(&[]).session_absence_detail(),
            jar(&["a"]).session_absence_detail(),
            jar(&["a", "b"]).session_absence_detail(),
        ];
        assert_eq!(
            all.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "the three states must stay distinguishable: {all:?}"
        );
    }

    #[test]
    fn jar_header_and_session_detection() {
        let jar = CookieJar {
            cookies: vec![
                Cookie {
                    name: "aid".into(),
                    value: "1".into(),
                    host_key: "ollama.com".into(),
                },
                Cookie {
                    name: "__Secure-session".into(),
                    value: "tok".into(),
                    host_key: "ollama.com".into(),
                },
            ],
        };
        assert_eq!(jar.header(), "aid=1; __Secure-session=tok");
        assert!(jar.has_cookie_named(|n| n.contains("session")));
        assert!(!jar.has_cookie_named(|n| n == "missing"));
    }
}
