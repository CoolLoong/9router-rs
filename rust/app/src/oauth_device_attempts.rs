use std::collections::{HashMap, hash_map::Entry};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const MAX_PENDING: usize = 512;
type Key = [u8; 32];

#[derive(Clone, Default)]
pub(crate) struct Attempts(Arc<Mutex<HashMap<Key, Attempt>>>);

struct Attempt {
    owner: Key,
    proof: Key,
    code: Arc<str>,
    expires: Instant,
    next_poll: Instant,
    interval: Duration,
    polling: bool,
}

pub(crate) struct Lease {
    attempts: Attempts,
    key: Key,
    waiting: Option<Waiting>,
}

pub(crate) enum Waiting {
    Pending,
    SlowDown,
}

impl Attempts {
    pub(crate) fn issue(
        &self,
        owner: Key,
        code: &str,
        lifetime: Duration,
        interval: Duration,
    ) -> Option<(String, String)> {
        let now = Instant::now();
        let expires = now.checked_add(lifetime)?;
        let next_poll = now.checked_add(interval)?;
        let handle = random()?;
        let proof = random()?;
        let mut pending = self.entries();
        pending.retain(|_, attempt| attempt.polling || attempt.expires > now);
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let Entry::Vacant(slot) = pending.entry(hash(&handle)) else {
            return None;
        };
        slot.insert(Attempt {
            owner,
            proof: hash(&proof),
            code: Arc::from(code),
            expires,
            next_poll,
            interval,
            polling: false,
        });
        Some((handle, proof))
    }

    pub(crate) fn claim(
        &self,
        owner: Key,
        handle: &str,
        proof: &str,
    ) -> Result<(Arc<str>, Lease), &'static str> {
        let key = hash(handle);
        let proof = hash(proof);
        let mut pending = self.entries();
        let now = Instant::now();
        let Some(attempt) = pending.get_mut(&key).filter(|attempt| {
            attempt.owner == owner
                && attempt.expires > now
                && bool::from(attempt.proof.ct_eq(&proof))
        }) else {
            return Err("expired_token");
        };
        if attempt.polling {
            return Err("authorization_pending");
        }
        if attempt.next_poll > now {
            return Err("slow_down");
        }
        attempt.polling = true;
        attempt.next_poll = now.checked_add(attempt.interval).unwrap_or(attempt.expires);
        Ok((
            attempt.code.clone(),
            Lease {
                attempts: self.clone(),
                key,
                waiting: None,
            },
        ))
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<Key, Attempt>> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl Lease {
    pub(crate) fn wait(&mut self, waiting: Waiting) {
        self.waiting = Some(waiting);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut pending = self.attempts.entries();
        let now = Instant::now();
        if let Some(attempt) = pending
            .get_mut(&self.key)
            .filter(|attempt| attempt.expires > now)
            && let Some(waiting) = &self.waiting
        {
            attempt.polling = false;
            if matches!(waiting, Waiting::SlowDown) {
                attempt.interval = attempt.interval.saturating_add(Duration::from_secs(5));
                attempt.next_poll = now.checked_add(attempt.interval).unwrap_or(attempt.expires);
            }
        } else {
            pending.remove(&self.key);
        }
    }
}

fn random() -> Option<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(URL_SAFE_NO_PAD.encode(bytes))
}

fn hash(input: &str) -> Key {
    Sha256::digest(input.as_bytes()).into()
}
