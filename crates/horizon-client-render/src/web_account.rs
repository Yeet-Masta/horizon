//! Microsoft account sign-in for the web build (docs/microsoft-account.md):
//! the launcher's flow (`launcher/src/account.rs`), from the page. The
//! device-code flow, then Xbox Live, XSTS and the Minecraft services, the
//! ownership check and the profile, each request through the relay with TLS
//! in this module (horizon-platform's `web::https`), so the relay never sees
//! a token.
//!
//! The page (`web/horizon.js`) calls the two functions below and keeps the
//! account's record (its tokens) encrypted in IndexedDB, apart from the game
//! folder; on Play, [`horizon_account_session`] renews the tokens as needed
//! and hands the game its session (`auth::set_current`), which online-mode
//! servers need. Tokens never reach the console or an error.
use crate::auth::{AuthSession, parse_uuid, set_current};
use horizon_platform::{
    time::{Duration, SystemTime, UNIX_EPOCH},
    web::https,
};
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;

const SCOPE: &str = "XboxLive.signin offline_access";
const MICROSOFT: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";
const XBOX_USER: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MINECRAFT: &str = "https://api.minecraftservices.com";
/// The Minecraft token is renewed when it has less than this left.
const RENEW_BEFORE: u64 = 3600;
const TIMEOUT: Duration = Duration::from_secs(20);
const NOT_CONFIGURED: &str = "This build has no Microsoft sign-in (HORIZON_MSA_CLIENT_ID).";
const DOES_NOT_OWN: &str = "This account doesn't own Minecraft: Java Edition.";
const NO_PROFILE: &str =
    "This account has no Minecraft profile yet. Create one at minecraft.net, then sign in again.";
const OTHER_PROFILE: &str =
    "The Microsoft account now has another Minecraft profile. Sign in again.";
const SIGN_IN_AGAIN: &str = "Your Microsoft sign-in expired. Sign in again.";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Horizon's Azure app: the one the page names (`?env=HORIZON_MSA_CLIENT_ID=`), else the build's.
fn app_id(given: &str) -> Result<String, String> {
    Some(given.trim().to_owned())
        .filter(|id| !id.is_empty())
        .or_else(|| option_env!("HORIZON_MSA_CLIENT_ID").map(|id| id.trim().to_owned()))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| NOT_CONFIGURED.to_owned())
}

enum Body {
    None,
    Form(Vec<(&'static str, String)>),
    Json(Value),
}

/// `application/x-www-form-urlencoded`.
fn form(fields: &[(&str, String)]) -> String {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(b).to_string()
                }
                b => format!("%{b:02X}"),
            })
            .collect()
    };
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// One request's status and JSON body (null when there is none). Errors name
/// the service, never the request.
async fn send(
    relay: &str,
    method: &str,
    url: &str,
    bearer: Option<&str>,
    body: Body,
    service: &str,
) -> Result<(u16, Value), String> {
    let agent = format!("Horizon/{} (web)", horizon_core::release::VERSION);
    let authorization = bearer.map(|t| format!("Bearer {t}"));
    let mut headers = vec![
        ("User-Agent", agent.as_str()),
        ("Accept", "application/json"),
    ];
    if let Some(a) = &authorization {
        headers.push(("Authorization", a));
    }
    let bytes = match body {
        Body::None => Vec::new(),
        Body::Form(fields) => {
            headers.push(("Content-Type", "application/x-www-form-urlencoded"));
            form(&fields).into_bytes()
        }
        Body::Json(value) => {
            headers.push(("Content-Type", "application/json"));
            value.to_string().into_bytes()
        }
    };
    let response = https::request(relay, method, url, &headers, &bytes, TIMEOUT)
        .await
        .map_err(|e| format!("Could not reach {service}: {e}"))?;
    let value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    Ok((response.status, value))
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// The OAuth error codes as messages for the player.
fn oauth_error(v: &Value) -> String {
    match v.get("error").and_then(Value::as_str) {
        Some("authorization_declined") => "Sign-in was declined in the browser.".into(),
        Some("expired_token") => "The sign-in code expired. Start again.".into(),
        Some("bad_verification_code") => "The sign-in code was not recognised. Start again.".into(),
        Some("invalid_grant") => SIGN_IN_AGAIN.into(),
        Some("invalid_client" | "unauthorized_client") => {
            "Microsoft refused this app registration (client ID).".into()
        }
        Some(other) => format!("Microsoft sign-in failed ({other})."),
        None => "Microsoft sign-in failed.".into(),
    }
}

/// The Microsoft tokens: the access token is for the Xbox step only.
struct MsTokens {
    access: String,
    refresh: String,
}

fn ms_tokens(status: u16, v: &Value) -> Result<MsTokens, String> {
    match (
        status,
        str_field(v, "access_token"),
        str_field(v, "refresh_token"),
    ) {
        (200, Some(access), Some(refresh)) => Ok(MsTokens { access, refresh }),
        _ => Err(oauth_error(v)),
    }
}

async fn refresh_ms(relay: &str, client_id: &str, refresh: &str) -> Result<MsTokens, String> {
    let (status, v) = send(
        relay,
        "POST",
        &format!("{MICROSOFT}/token"),
        None,
        Body::Form(vec![
            ("grant_type", "refresh_token".into()),
            ("client_id", client_id.into()),
            ("refresh_token", refresh.into()),
            ("scope", SCOPE.into()),
        ]),
        "Microsoft",
    )
    .await?;
    ms_tokens(status, &v)
}

/// The XSTS `XErr` codes, as the official launcher explains them.
fn xsts_error(code: u64) -> String {
    match code {
        2148916233 => "This Microsoft account has no Xbox profile yet. Sign in once at xbox.com to create one, then try again.".into(),
        2148916235 => "Xbox Live is not available in your country or region.".into(),
        2148916236 | 2148916237 => "This account needs adult verification on xbox.com before it can play.".into(),
        2148916238 => "This is a child account: an adult has to add it to a Microsoft family first.".into(),
        other => format!("Xbox Live refused the sign-in (error {other})."),
    }
}

/// The Minecraft access token, its expiry and the Xbox user id.
struct Minecraft {
    token: String,
    expires_at: u64,
    xuid: Option<String>,
}

/// From the Microsoft access token, through Xbox Live and XSTS.
async fn minecraft_login(relay: &str, ms_access: &str) -> Result<Minecraft, String> {
    let (status, xbl) = send(
        relay,
        "POST",
        XBOX_USER,
        None,
        Body::Json(json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={ms_access}"),
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        })),
        "Xbox Live",
    )
    .await?;
    let xbl_token = str_field(&xbl, "Token")
        .filter(|_| status == 200)
        .ok_or("Xbox Live refused the sign-in.")?;
    let (status, xsts) = send(
        relay,
        "POST",
        XSTS,
        None,
        Body::Json(json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT",
        })),
        "Xbox Live",
    )
    .await?;
    if let Some(code) = xsts.get("XErr").and_then(Value::as_u64) {
        return Err(xsts_error(code));
    }
    let claims = xsts.pointer("/DisplayClaims/xui/0");
    let (Some(xsts_token), Some(uhs)) = (
        str_field(&xsts, "Token").filter(|_| status == 200),
        claims.and_then(|c| str_field(c, "uhs")),
    ) else {
        return Err("Xbox Live refused the sign-in.".into());
    };
    let xuid = claims.and_then(|c| str_field(c, "xid"));
    let (token, expires_in) =
        minecraft_token(relay, &format!("XBL3.0 x={uhs};{xsts_token}")).await?;
    Ok(Minecraft {
        token,
        expires_at: now() + expires_in,
        xuid,
    })
}

/// `launcher/login` first (a new account's `login_with_xbox` token names no
/// profile), `login_with_xbox` as the fallback, as the launcher does.
async fn minecraft_token(relay: &str, xtoken: &str) -> Result<(String, u64), String> {
    let lifetime = |v: &Value| v.get("expires_in").and_then(Value::as_u64).unwrap_or(86400);
    let launcher = send(
        relay,
        "POST",
        &format!("{MINECRAFT}/launcher/login"),
        None,
        Body::Json(json!({ "xtoken": xtoken, "platform": "PC_LAUNCHER" })),
        "the Minecraft services",
    )
    .await;
    if let Ok((200, v)) = &launcher
        && let Some(token) = str_field(v, "access_token")
    {
        return Ok((token, lifetime(v)));
    }
    let (status, mc) = send(
        relay,
        "POST",
        &format!("{MINECRAFT}/authentication/login_with_xbox"),
        None,
        Body::Json(json!({ "identityToken": xtoken })),
        "the Minecraft services",
    )
    .await?;
    match (status, str_field(&mc, "access_token")) {
        (200, Some(token)) => Ok((token, lifetime(&mc))),
        (403, _) => {
            Err("The Minecraft services refused this app: it is not approved by Mojang yet.".into())
        }
        (429, _) => Err("Too many sign-ins in a short time. Wait a minute and try again.".into()),
        _ => Err("The Minecraft services refused the sign-in.".into()),
    }
}

/// Whether the account owns Minecraft: Java Edition (Game Pass included).
async fn owns_game(relay: &str, token: &str) -> Result<bool, String> {
    let (status, v) = send(
        relay,
        "GET",
        &format!("{MINECRAFT}/entitlements/mcstore"),
        Some(token),
        Body::None,
        "the Minecraft services",
    )
    .await?;
    if status != 200 {
        return Err("Could not check which games this account owns.".into());
    }
    Ok(v.get("items")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|i| {
                matches!(
                    i.get("name").and_then(Value::as_str),
                    Some("game_minecraft" | "product_minecraft")
                )
            })
        }))
}

/// The profile's UUID (undashed), name and active skin; none when the
/// account has no profile.
async fn profile(relay: &str, token: &str) -> Result<Option<Value>, String> {
    let (status, v) = send(
        relay,
        "GET",
        &format!("{MINECRAFT}/minecraft/profile"),
        Some(token),
        Body::None,
        "the Minecraft services",
    )
    .await?;
    if status == 404 {
        return Ok(None);
    }
    let (Some(uuid), Some(name)) = (str_field(&v, "id"), str_field(&v, "name")) else {
        return Err("Could not read the Minecraft profile.".into());
    };
    let skin = v
        .get("skins")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .find(|s| s.get("state").and_then(Value::as_str) == Some("ACTIVE"))
        })
        .and_then(|s| {
            Some(json!({
                "url": str_field(s, "url")?,
                "variant": str_field(s, "variant")
                    .unwrap_or_else(|| "CLASSIC".into())
                    .to_lowercase(),
            }))
        });
    Ok(Some(json!({
        "uuid": uuid.replace('-', "").to_lowercase(),
        "name": name,
        "skin": skin,
    })))
}

/// The page's record of an account: its profile and its tokens.
fn record(profile: &Value, refresh: &str, mc: &Minecraft) -> Value {
    let mut record = profile.clone();
    record["refresh_token"] = refresh.into();
    record["minecraft_token"] = mc.token.as_str().into();
    record["minecraft_expires_at"] = mc.expires_at.into();
    record["xuid"] = mc.xuid.clone().into();
    record
}

/// Whether the page's `AbortSignal` (or nothing) was aborted.
fn aborted(signal: &JsValue) -> bool {
    !signal.is_undefined()
        && !signal.is_null()
        && js_sys::Reflect::get(signal, &"aborted".into())
            .ok()
            .and_then(|a| a.as_bool())
            .unwrap_or(false)
}

/// The device-code flow: `on_code(user_code, verification_uri)` says what to
/// type where, then this waits for the player (or `signal`), and finishes
/// through Xbox Live and the Minecraft services. Resolves to the account's
/// record (JSON, with its tokens: the page keeps it encrypted), rejects with
/// a message for the player.
#[wasm_bindgen]
pub async fn horizon_account_sign_in(
    relay: String,
    client_id: String,
    on_code: js_sys::Function,
    signal: JsValue,
) -> Result<String, JsValue> {
    sign_in(&relay, &client_id, &on_code, &signal)
        .await
        .map_err(|e| JsValue::from_str(&e))
}

async fn sign_in(
    relay: &str,
    client_id: &str,
    on_code: &js_sys::Function,
    signal: &JsValue,
) -> Result<String, String> {
    if relay.trim().is_empty() {
        return Err("Signing in needs a WebSocket relay (Options): a page can't reach Microsoft's sign-in by itself.".into());
    }
    let client_id = app_id(client_id)?;
    let (status, v) = send(
        relay,
        "POST",
        &format!("{MICROSOFT}/devicecode"),
        None,
        Body::Form(vec![
            ("client_id", client_id.clone()),
            ("scope", SCOPE.into()),
        ]),
        "Microsoft",
    )
    .await?;
    let (Some(user_code), Some(device_code), 200) = (
        str_field(&v, "user_code"),
        str_field(&v, "device_code"),
        status,
    ) else {
        return Err(oauth_error(&v));
    };
    let uri = str_field(&v, "verification_uri")
        .unwrap_or_else(|| "https://www.microsoft.com/link".into());
    let mut interval = v
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .max(1);
    let expires_at = now() + v.get("expires_in").and_then(Value::as_u64).unwrap_or(900);
    let _ = on_code.call2(
        &JsValue::NULL,
        &JsValue::from_str(&user_code),
        &JsValue::from_str(&uri),
    );
    let ms = loop {
        for _ in 0..interval * 4 {
            horizon_platform::thread::sleep_task(Duration::from_millis(250)).await;
            if aborted(signal) {
                return Err("Sign-in cancelled.".into());
            }
        }
        if now() > expires_at {
            return Err("The sign-in code expired. Start again.".into());
        }
        let (status, v) = send(
            relay,
            "POST",
            &format!("{MICROSOFT}/token"),
            None,
            Body::Form(vec![
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code".into(),
                ),
                ("client_id", client_id.clone()),
                ("device_code", device_code.clone()),
            ]),
            "Microsoft",
        )
        .await?;
        match v.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            _ => break ms_tokens(status, &v)?,
        }
    };
    let mc = minecraft_login(relay, &ms.access).await?;
    if !owns_game(relay, &mc.token).await? {
        return Err(DOES_NOT_OWN.into());
    }
    let profile = profile(relay, &mc.token).await?.ok_or(NO_PROFILE)?;
    log::info!(
        "Horizon: signed in as {}",
        profile["name"].as_str().unwrap_or("?")
    );
    Ok(record(&profile, &ms.refresh, &mc).to_string())
}

/// On Play: the Minecraft token renewed when it has less than an hour left,
/// the profile read again (a name may change), and the game handed the
/// session. Resolves to the record to keep (the tokens may have changed);
/// rejects with a message when the account can't play online, and the game
/// then starts offline.
#[wasm_bindgen]
pub async fn horizon_account_session(
    relay: String,
    client_id: String,
    record: String,
) -> Result<String, JsValue> {
    session(&relay, &client_id, &record)
        .await
        .map_err(|e| JsValue::from_str(&e))
}

async fn session(relay: &str, client_id: &str, record: &str) -> Result<String, String> {
    let mut record: Value =
        serde_json::from_str(record).map_err(|_| "The saved account can't be read.")?;
    let expires_at = record["minecraft_expires_at"].as_u64().unwrap_or(0);
    if expires_at <= now() + RENEW_BEFORE {
        let renewed = async {
            let id = app_id(client_id)?;
            let refresh = record["refresh_token"].as_str().ok_or(SIGN_IN_AGAIN)?;
            let ms = refresh_ms(relay, &id, refresh).await?;
            Ok::<_, String>((minecraft_login(relay, &ms.access).await?, ms.refresh))
        }
        .await;
        match renewed {
            Ok((mc, refresh)) => {
                let profile = record.clone();
                record = self::record(&profile, &refresh, &mc);
            }
            // Offline for now: a token that hasn't expired still works.
            Err(e) if expires_at > now() + 60 => {
                log::warn!("Horizon: the Microsoft sign-in was not renewed: {e}")
            }
            Err(e) => return Err(e),
        }
    }
    let token = record["minecraft_token"]
        .as_str()
        .ok_or(SIGN_IN_AGAIN)?
        .to_owned();
    match profile(relay, &token).await {
        Ok(Some(p)) if p["uuid"] != record["uuid"] => return Err(OTHER_PROFILE.into()),
        Ok(Some(p)) => {
            record["name"] = p["name"].clone();
            record["skin"] = p["skin"].clone();
        }
        Ok(None) => return Err(NO_PROFILE.into()),
        // No answer: the name kept stands.
        Err(e) => log::warn!("Horizon: the Minecraft profile was not read: {e}"),
    }
    let name = record["name"].as_str().unwrap_or_default().to_owned();
    let uuid = record["uuid"]
        .as_str()
        .and_then(parse_uuid)
        .ok_or("The saved account can't be read.")?;
    let mut session = AuthSession::new(&name, uuid, &token);
    session.xuid = record["xuid"].as_str().map(str::to_owned);
    set_current(session);
    log::info!("Horizon: playing as {name} (Microsoft account)");
    Ok(record.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn forms_are_encoded() {
        assert_eq!(
            super::form(&[("a", "x y&z".into()), ("scope", super::SCOPE.into())]),
            "a=x%20y%26z&scope=XboxLive.signin%20offline_access"
        );
    }
}
