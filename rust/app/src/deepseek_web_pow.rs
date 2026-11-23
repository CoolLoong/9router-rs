use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

const ALGORITHM: &str = "DeepSeekHashV1";
const MAX_ATTEMPTS: u32 = 250_000;
const RATE: usize = 136;
static WORKERS: Semaphore = Semaphore::const_new(2);

pub(super) async fn answer(challenge: &Value, endpoint: &str) -> Result<String, &'static str> {
    const INVALID: &str = "DeepSeek Web returned an invalid proof-of-work challenge";
    if challenge["algorithm"] != ALGORITHM
        || challenge
            .get("target_path")
            .is_some_and(|path| path != endpoint)
    {
        return Err(INVALID);
    }
    let digest = field(challenge, "challenge", 64)
        .filter(|digest| digest.len() == 64)
        .ok_or(INVALID)?;
    let mut target = [0; 32];
    for (index, byte) in target.iter_mut().enumerate() {
        let high = char::from(digest.as_bytes()[index * 2])
            .to_digit(16)
            .ok_or(INVALID)?;
        let low = char::from(digest.as_bytes()[index * 2 + 1])
            .to_digit(16)
            .ok_or(INVALID)?;
        *byte = ((high << 4) | low) as u8;
    }
    let salt = field(challenge, "salt", 1024).ok_or(INVALID)?;
    let signature = field(challenge, "signature", 8192).ok_or(INVALID)?;
    let attempts = challenge["difficulty"]
        .as_u64()
        .and_then(|count| u32::try_from(count).ok())
        .filter(|count| (1..=MAX_ATTEMPTS).contains(count))
        .ok_or(INVALID)?;
    let expiry = challenge["expire_at"].as_u64().ok_or(INVALID)?;
    let expiry_time = UNIX_EPOCH
        .checked_add(if expiry >= 100_000_000_000 {
            Duration::from_millis(expiry)
        } else {
            Duration::from_secs(expiry)
        })
        .ok_or(INVALID)?;
    let budget = expiry_time
        .duration_since(SystemTime::now())
        .map_err(|_| "DeepSeek Web proof-of-work challenge expired")?
        .min(Duration::from_secs(10));
    let deadline = Instant::now() + budget;
    let permit = WORKERS
        .try_acquire()
        .map_err(|_| "DeepSeek Web proof-of-work is busy")?;
    let prefix = format!("{salt}_{expiry}_");
    let cancelled = Cancelled(Arc::new(AtomicBool::new(false)));
    let cancel = cancelled.0.clone();
    let solution = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        solve(prefix.as_bytes(), target, attempts, deadline, &cancel)
    })
    .await
    .map_err(|_| "DeepSeek Web proof-of-work failed")?
    .ok_or("DeepSeek Web could not solve its proof-of-work challenge")?;
    if SystemTime::now() >= expiry_time {
        return Err("DeepSeek Web proof-of-work challenge expired");
    }
    Ok(STANDARD.encode(
        json!({
            "algorithm":ALGORITHM,"challenge":digest,"salt":salt,"answer":solution,
            "signature":signature,"target_path":endpoint
        })
        .to_string(),
    ))
}

struct Cancelled(Arc<AtomicBool>);

impl Drop for Cancelled {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn field<'a>(body: &'a Value, key: &str, maximum: usize) -> Option<&'a str> {
    body[key]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= maximum)
}

fn solve(
    prefix: &[u8],
    target: [u8; 32],
    attempts: u32,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Option<u32> {
    let mut answer = None;
    keccak::Keccak::new().with_p1600::<23>(|permute| {
        let mut prefix_state = [0; 25];
        let mut prefix_pos = 0;
        absorb(&mut prefix_state, &mut prefix_pos, prefix, permute);
        for nonce in 0..attempts {
            if nonce % 256 == 0 && (Instant::now() >= deadline || cancelled.load(Ordering::Relaxed))
            {
                break;
            }
            let mut state = prefix_state;
            let mut position = prefix_pos;
            absorb(
                &mut state,
                &mut position,
                nonce.to_string().as_bytes(),
                permute,
            );
            state[position / 8] ^= 0x06 << ((position % 8) * 8);
            state[RATE / 8 - 1] ^= 0x80 << 56;
            permute(&mut state);
            if state[..4]
                .iter()
                .flat_map(|word| word.to_le_bytes())
                .eq(target)
            {
                answer = Some(nonce);
                break;
            }
        }
    });
    answer
}

fn absorb(state: &mut [u64; 25], position: &mut usize, bytes: &[u8], permute: fn(&mut [u64; 25])) {
    for byte in bytes {
        state[*position / 8] ^= u64::from(*byte) << ((*position % 8) * 8);
        *position += 1;
        if *position == RATE {
            permute(state);
            *position = 0;
        }
    }
}
