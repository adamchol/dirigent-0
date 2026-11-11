//! The web's sign-ins: a user who signed in with the OpenID provider (Google) trades its ID token
//! once for a sign-in of control's ([`Store::open_web_session`](crate::leases::Store)), whose
//! secret the browser keeps in an `HttpOnly` cookie, out of the page's reach. From it, control
//! mints its own short-lived tokens, which its API and chlive take, for as long as the sign-in
//! is used; signing out ends it. The cookie has no `Path`: it's the directory of the request
//! that set it, `/api` through the web (`/api/session`), which its other requests are under.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use uuid::Uuid;

pub const COOKIE: &str = "dirigent_session";

/// A new sign-in's secret.
pub fn secret() -> [u8; 32] {
    let mut s = [0u8; 32];
    rand::rng().fill_bytes(&mut s);
    s
}

/// The `Set-Cookie` holding the sign-in `id`'s `secret`, for `max_age` seconds.
pub fn set_cookie(id: Uuid, secret: &[u8], max_age: u64) -> String {
    format!(
        "{COOKIE}={id}.{}; HttpOnly; Secure; SameSite=Strict; Max-Age={max_age}",
        URL_SAFE_NO_PAD.encode(secret)
    )
}

/// Again with a new `max_age`, from a `Cookie` header's value of it.
pub fn renew_cookie(value: &str, max_age: u64) -> String {
    format!("{COOKIE}={value}; HttpOnly; Secure; SameSite=Strict; Max-Age={max_age}")
}

/// The `Set-Cookie` that drops it.
pub fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; Secure; SameSite=Strict; Max-Age=0")
}

/// The sign-in a `Cookie` header holds: its id and secret, and the cookie's value as it came.
pub fn from_cookies(header: &str) -> Option<(Uuid, Vec<u8>, &str)> {
    let value = header
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)?
        .1;
    let (id, secret) = value.split_once('.')?;
    let id = Uuid::parse_str(id).ok()?;
    let secret = URL_SAFE_NO_PAD.decode(secret).ok()?;
    (secret.len() == 32).then_some((id, secret, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sign_in_comes_back_from_its_cookie() {
        let (id, s) = (Uuid::new_v4(), secret());
        let set = set_cookie(id, &s, 60);
        assert!(set.contains("HttpOnly; Secure; SameSite=Strict; Max-Age=60"));
        let value = set.split(';').next().unwrap().split_once('=').unwrap().1;
        let header = format!("theme=dark; {COOKIE}={value}; other=1");
        let (got_id, got_secret, got_value) = from_cookies(&header).unwrap();
        assert_eq!(
            (got_id, got_secret.as_slice(), got_value),
            (id, &s[..], value)
        );
        assert!(renew_cookie(value, 9).starts_with(&format!("{COOKIE}={value};")));
    }

    #[test]
    fn other_cookies_are_no_sign_in() {
        assert!(from_cookies("theme=dark").is_none());
        assert!(from_cookies(&format!("{COOKIE}=nope")).is_none());
        assert!(from_cookies(&format!("{COOKIE}={}.short", Uuid::new_v4())).is_none());
        assert!(from_cookies(&format!("x{COOKIE}={}.AAAA", Uuid::new_v4())).is_none());
    }
}
