//! Notifications for the phone's Home Screen app over Web Push: messages
//! signed for the hub (VAPID, RFC 8292) and encrypted for one phone
//! (aes128gcm, RFC 8291), sent through that phone's push service (Apple's for
//! an iPhone). The push service can't read them.
//!
//! The signing key and the phones' subscriptions are kept next to the
//! Dispatch TLS files, so they survive restarts.

use std::cell::RefCell;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom as _, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use ring::{aead, agreement, hkdf};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::relay::NoticeKind;

const KEY_FILE: &str = "vapid.pk8";
const SUBSCRIPTIONS_FILE: &str = "push-subscriptions.json";
const MAX_SUBSCRIPTIONS: usize = 20;
/// One record holds the whole message; payloads stay far below this.
const RECORD_SIZE: u32 = 4096;
const MAX_PAYLOAD_BYTES: usize = 3000;

/// Which notices a phone wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kinds {
    pub approval: bool,
    pub done: bool,
    pub stopped: bool,
}

impl Default for Kinds {
    fn default() -> Self {
        Self {
            approval: true,
            done: true,
            stopped: true,
        }
    }
}

impl Kinds {
    pub fn wants(&self, kind: NoticeKind) -> bool {
        match kind {
            NoticeKind::Approval => self.approval,
            NoticeKind::Done => self.done,
            NoticeKind::Stopped => self.stopped,
        }
    }
}

/// One phone's push subscription, as `PushSubscription.toJSON()` gives it,
/// plus what it wants to hear about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    /// The phone's P-256 public key, base64url.
    pub p256dh: String,
    /// The phone's 16-byte auth secret, base64url.
    pub auth: String,
    #[serde(default)]
    pub kinds: Kinds,
}

impl Subscription {
    fn check(&self) -> Result<()> {
        ensure!(
            self.endpoint.starts_with("https://") && self.endpoint.len() <= 2048,
            "push endpoints must be HTTPS URLs"
        );
        let key = URL_SAFE_NO_PAD
            .decode(self.p256dh.trim_end_matches('='))
            .context("the phone's push key isn't base64url")?;
        ensure!(
            key.len() == 65 && key[0] == 4,
            "the phone's push key isn't a P-256 point"
        );
        let auth = URL_SAFE_NO_PAD
            .decode(self.auth.trim_end_matches('='))
            .context("the phone's push secret isn't base64url")?;
        ensure!(auth.len() == 16, "the phone's push secret must be 16 bytes");
        Ok(())
    }
}

/// An encrypted message ready to POST to a push service.
pub struct PushMessage {
    pub endpoint: String,
    pub authorization: String,
    pub body: Vec<u8>,
}

pub struct Push {
    key: EcdsaKeyPair,
    public_key: String,
    /// Who to contact about these messages: the hub's Dispatch origin.
    subject: String,
    subscriptions_path: PathBuf,
    subscriptions: RefCell<Vec<Subscription>>,
    rng: SystemRandom,
}

impl Push {
    /// Loads the hub's signing key, making one the first time, and the
    /// phones subscribed so far.
    pub fn load(directory: &Path, subject: String) -> Result<Self> {
        let rng = SystemRandom::new();
        let key_path = directory.join(KEY_FILE);
        let pkcs8 = match std::fs::read(&key_path) {
            Ok(pkcs8) => pkcs8,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                    .map_err(|_| anyhow!("couldn't make a push signing key"))?;
                write_private(&key_path, pkcs8.as_ref())?;
                pkcs8.as_ref().to_vec()
            }
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", key_path.display()));
            }
        };
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng)
            .map_err(|_| anyhow!("{} isn't a P-256 key", key_path.display()))?;
        let public_key = URL_SAFE_NO_PAD.encode(key.public_key().as_ref());
        let subscriptions_path = directory.join(SUBSCRIPTIONS_FILE);
        let subscriptions = match std::fs::read(&subscriptions_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", subscriptions_path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading {}", subscriptions_path.display()));
            }
        };
        Ok(Self {
            key,
            public_key,
            subject,
            subscriptions_path,
            subscriptions: RefCell::new(subscriptions),
            rng,
        })
    }

    /// The key phones subscribe with (`applicationServerKey`), base64url.
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// Adds a phone, or updates what an already subscribed one wants.
    pub fn subscribe(&self, subscription: Subscription) -> Result<()> {
        subscription.check()?;
        let mut subscriptions = self.subscriptions.borrow_mut();
        subscriptions.retain(|existing| existing.endpoint != subscription.endpoint);
        if subscriptions.len() >= MAX_SUBSCRIPTIONS {
            subscriptions.remove(0);
        }
        subscriptions.push(subscription);
        self.save(&subscriptions)
    }

    pub fn unsubscribe(&self, endpoint: &str) -> Result<bool> {
        let mut subscriptions = self.subscriptions.borrow_mut();
        let before = subscriptions.len();
        subscriptions.retain(|existing| existing.endpoint != endpoint);
        if subscriptions.len() == before {
            return Ok(false);
        }
        self.save(&subscriptions)?;
        Ok(true)
    }

    pub fn subscription(&self, endpoint: &str) -> Option<Subscription> {
        self.subscriptions
            .borrow()
            .iter()
            .find(|subscription| subscription.endpoint == endpoint)
            .cloned()
    }

    /// The phones that want to hear about `kind`.
    pub fn subscribers(&self, kind: NoticeKind) -> Vec<Subscription> {
        self.subscriptions
            .borrow()
            .iter()
            .filter(|subscription| subscription.kinds.wants(kind))
            .cloned()
            .collect()
    }

    /// Encrypts `payload` for one phone and signs the request.
    pub fn message(&self, subscription: &Subscription, payload: &[u8]) -> Result<PushMessage> {
        ensure!(
            payload.len() <= MAX_PAYLOAD_BYTES,
            "the notification is too long"
        );
        let peer = URL_SAFE_NO_PAD.decode(subscription.p256dh.trim_end_matches('='))?;
        let auth = URL_SAFE_NO_PAD.decode(subscription.auth.trim_end_matches('='))?;
        let body = encrypt(payload, &peer, &auth, &self.rng)?;
        Ok(PushMessage {
            endpoint: subscription.endpoint.clone(),
            authorization: format!(
                "vapid t={}, k={}",
                self.token(&subscription.endpoint)?,
                self.public_key
            ),
            body,
        })
    }

    /// A VAPID token for the push service at `endpoint`, valid for 12 hours.
    fn token(&self, endpoint: &str) -> Result<String> {
        let audience = origin_of(endpoint)?;
        let expires = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + 12 * 3600;
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = URL_SAFE_NO_PAD
            .encode(json!({ "aud": audience, "exp": expires, "sub": self.subject }).to_string());
        let signed = format!("{header}.{claims}");
        let signature = self
            .key
            .sign(&self.rng, signed.as_bytes())
            .map_err(|_| anyhow!("couldn't sign the push request"))?;
        Ok(format!(
            "{signed}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }

    fn save(&self, subscriptions: &[Subscription]) -> Result<()> {
        let temporary = self.subscriptions_path.with_extension("json.tmp");
        write_private(&temporary, &serde_json::to_vec_pretty(subscriptions)?)?;
        std::fs::rename(&temporary, &self.subscriptions_path)
            .with_context(|| format!("saving {}", self.subscriptions_path.display()))
    }
}

/// Writes a file only the hub's user can read.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))
}

/// `https://host[:port]` of a push endpoint.
fn origin_of(endpoint: &str) -> Result<String> {
    let rest = endpoint
        .strip_prefix("https://")
        .context("push endpoints must be HTTPS URLs")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.contains('@') {
        bail!("the push endpoint has no host");
    }
    Ok(format!("https://{host}"))
}

struct Length(usize);

impl hkdf::KeyType for Length {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[&[u8]], length: usize) -> Result<Vec<u8>> {
    let mut out = vec![0; length];
    prk.expand(info, Length(length))
        .and_then(|okm| okm.fill(&mut out))
        .map_err(|_| anyhow!("couldn't derive the push keys"))?;
    Ok(out)
}

/// Encrypts one push message for a phone's key and secret (RFC 8291) as a
/// single aes128gcm record (RFC 8188).
fn encrypt(payload: &[u8], peer: &[u8], auth: &[u8], rng: &SystemRandom) -> Result<Vec<u8>> {
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, rng)
        .map_err(|_| anyhow!("couldn't make a push key"))?;
    let public = private
        .compute_public_key()
        .map_err(|_| anyhow!("couldn't make a push key"))?;
    let shared = agreement::agree_ephemeral(
        private,
        &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, peer),
        |secret| secret.to_vec(),
    )
    .map_err(|_| anyhow!("the phone's push key is invalid"))?;

    // The secret for this phone mixes in its auth secret and both keys.
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(&shared);
    let input = expand(&prk, &[b"WebPush: info\0", peer, public.as_ref()], 32)?;

    let mut salt = [0u8; 16];
    rng.fill(&mut salt)
        .map_err(|_| anyhow!("couldn't make a push salt"))?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(&input);
    let key = expand(&prk, &[b"Content-Encoding: aes128gcm\0"], 16)?;
    let nonce = expand(&prk, &[b"Content-Encoding: nonce\0"], 12)?;

    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &key)
            .map_err(|_| anyhow!("couldn't make the push cipher"))?,
    );
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce)
        .map_err(|_| anyhow!("couldn't make the push nonce"))?;
    let mut record = payload.to_vec();
    // The delimiter that marks the last (only) record.
    record.push(2);
    key.seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut record)
        .map_err(|_| anyhow!("couldn't encrypt the notification"))?;

    let public = public.as_ref();
    let mut body = Vec::with_capacity(16 + 4 + 1 + public.len() + record.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(public.len() as u8);
    body.extend_from_slice(public);
    body.extend_from_slice(&record);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decrypts a message the way the phone does, to check `encrypt`.
    fn decrypt(
        body: &[u8],
        phone: agreement::EphemeralPrivateKey,
        phone_public: &[u8],
        auth: &[u8],
    ) -> Vec<u8> {
        let salt = &body[..16];
        let id_length = body[20] as usize;
        let hub_public = &body[21..21 + id_length];
        let shared = agreement::agree_ephemeral(
            phone,
            &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, hub_public),
            |secret| secret.to_vec(),
        )
        .unwrap();
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(&shared);
        let input = expand(&prk, &[b"WebPush: info\0", phone_public, hub_public], 32).unwrap();
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&input);
        let key = expand(&prk, &[b"Content-Encoding: aes128gcm\0"], 16).unwrap();
        let nonce = expand(&prk, &[b"Content-Encoding: nonce\0"], 12).unwrap();
        let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &key).unwrap());
        let mut record = body[21 + id_length..].to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(),
                aead::Aad::empty(),
                &mut record,
            )
            .unwrap();
        assert_eq!(plain.last(), Some(&2), "the last record ends with 2");
        plain[..plain.len() - 1].to_vec()
    }

    #[test]
    fn only_the_phone_can_read_a_message() {
        let rng = SystemRandom::new();
        let phone = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let phone_public = phone.compute_public_key().unwrap().as_ref().to_vec();
        let auth = [7u8; 16];
        let payload = br#"{"title":"Run tests is done"}"#;
        let body = encrypt(payload, &phone_public, &auth, &rng).unwrap();
        assert_eq!(&body[16..20], &RECORD_SIZE.to_be_bytes());
        assert_eq!(body[20], 65);
        assert_eq!(decrypt(&body, phone, &phone_public, &auth), payload);
    }

    #[test]
    fn subscriptions_are_kept_signed_and_checked() {
        let directory = tempfile::tempdir().unwrap();
        let push = Push::load(directory.path(), "https://hub.example.test:7767".into()).unwrap();
        let again = Push::load(directory.path(), "https://hub.example.test:7767".into()).unwrap();
        assert_eq!(
            push.public_key(),
            again.public_key(),
            "the key is made once"
        );

        let rng = SystemRandom::new();
        let phone = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let subscription = Subscription {
            endpoint: "https://web.push.apple.com/abc".into(),
            p256dh: URL_SAFE_NO_PAD.encode(phone.compute_public_key().unwrap().as_ref()),
            auth: URL_SAFE_NO_PAD.encode([1u8; 16]),
            kinds: Kinds {
                done: false,
                ..Kinds::default()
            },
        };
        assert!(
            push.subscribe(Subscription {
                endpoint: "http://insecure.example.test/".into(),
                ..subscription.clone()
            })
            .is_err()
        );
        push.subscribe(subscription.clone()).unwrap();
        push.subscribe(subscription.clone()).unwrap();
        let reloaded =
            Push::load(directory.path(), "https://hub.example.test:7767".into()).unwrap();
        assert_eq!(
            reloaded.subscribers(NoticeKind::Approval),
            vec![subscription.clone()]
        );
        assert!(reloaded.subscribers(NoticeKind::Done).is_empty());

        let message = push.message(&subscription, b"{}").unwrap();
        let token = message
            .authorization
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split(", k=").next())
            .unwrap();
        let [header, claims, signature]: [&str; 3] =
            token.split('.').collect::<Vec<_>>().try_into().unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["sub"], "https://hub.example.test:7767");
        let public_key = URL_SAFE_NO_PAD.decode(push.public_key()).unwrap();
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            public_key,
        )
        .verify(
            format!("{header}.{}", token.split('.').nth(1).unwrap()).as_bytes(),
            &URL_SAFE_NO_PAD.decode(signature).unwrap(),
        )
        .expect("the push service can check the hub's signature");

        assert!(push.unsubscribe(&subscription.endpoint).unwrap());
        assert!(!push.unsubscribe(&subscription.endpoint).unwrap());
    }
}
