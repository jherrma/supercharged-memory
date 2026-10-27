//! The fake Ollama's embedding: deterministic, from the text alone.
//!
//! A bag of words: every run of ASCII letters and digits (lowercased) adds a
//! pseudo-random vector of -1/0/1 components drawn from that word, so texts
//! sharing words lie close together and a text always has the same vector.
//! A text without a word gets the first unit vector. The components are
//! small integers, exact in f32 and in the reference's `%.7g` literals, so
//! both implementations store the same bytes and compute the same distances.
//! `bench/bench.py` implements the same function; a test compares the two.

pub const DIM: usize = 1024;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(FNV_PRIME)
    })
}

/// splitmix64: the next state and its output.
fn splitmix(state: u64) -> (u64, u64) {
    let state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (state, z ^ (z >> 31))
}

/// The -1/0/1 vector of one word: two bits per component, 32 per draw.
fn word(token: &[u8]) -> Vec<i32> {
    let mut state = fnv1a(token);
    let mut out = Vec::with_capacity(DIM);
    for _ in 0..DIM / 32 {
        let (next, z) = splitmix(state);
        state = next;
        for j in 0..32 {
            out.push([-1, 0, 1, 0][((z >> (2 * j)) & 3) as usize]);
        }
    }
    out
}

/// The vector of `text`.
pub fn vector(text: &str) -> Vec<i32> {
    let lower = text.as_bytes().to_ascii_lowercase();
    let mut v = vec![0; DIM];
    let mut any = false;
    for token in lower
        .split(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit()))
        .filter(|t| !t.is_empty())
    {
        any = true;
        for (acc, x) in v.iter_mut().zip(word(token)) {
            *acc += x;
        }
    }
    if !any {
        v[0] = 1;
    }
    v
}

/// The same vector as the little-endian f32 bytes the engine stores.
pub fn blob(text: &str) -> Vec<u8> {
    vector(text)
        .iter()
        .flat_map(|&x| (x as f32).to_le_bytes())
        .collect()
}

/// The vector as Ollama's JSON array of floats.
pub fn json(text: &str) -> String {
    let parts: Vec<String> = vector(text).iter().map(|x| format!("{x}.0")).collect();
    format!("[{}]", parts.join(","))
}
