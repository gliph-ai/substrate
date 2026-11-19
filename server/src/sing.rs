//! Sing a chat message with CHIMERA (github.com/jamesondh/chimera-vocal-synth, MIT): a
//! physically modelled adult male voice sings the words up a major-scale ladder. Rendered
//! here, not in the browser, so the chat needs no AudioWorklet or WebGL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chimera::anatomy::human::{BodyMix, BODY_MALE};
use chimera::phonetics::english::EnglishG2P;
use chimera::phonetics::sing::{midi_to_hz, plan_song, SongOptions, SungNote};
use chimera::phonetics::syllables::lyrics_to_syllables;
use chimera::voice::human_voice::{HumanVoice, SourcePatch};
use serde::Deserialize;

use crate::app::{App, Fail};

const FS: f64 = 48000.0;
const MAX_SYLLABLES: usize = 32;
/// The ladder: a major scale from C3 up an octave and back, one step a syllable.
const LADDER: [f64; 14] = [0.0, 2.0, 4.0, 5.0, 7.0, 9.0, 11.0, 12.0, 11.0, 9.0, 7.0, 5.0, 4.0, 2.0];

fn g2p() -> &'static EnglishG2P {
    static G: OnceLock<EnglishG2P> = OnceLock::new();
    G.get_or_init(|| {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&include_bytes!("../assets/chimera/cmudict.txt.gz")[..]), &mut text).expect("cmudict");
        let mut g = EnglishG2P::new();
        g.load_dict(&text);
        g
    })
}

/// The sung samples, peak-normalised; None when nothing in the text can be sung.
pub fn render(text: &str) -> Option<Vec<f32>> {
    let mut syl = lyrics_to_syllables(text, g2p());
    syl.truncate(MAX_SYLLABLES);
    if syl.is_empty() {
        return None;
    }
    let last = syl.len() - 1;
    let notes: Vec<SungNote> = syl.into_iter().enumerate()
        .map(|(i, s)| SungNote { midi: 48.0 + LADDER[i % LADDER.len()], beats: if i == last { 2.0 } else { 0.5 }, syl: s })
        .collect();
    let mut v = HumanVoice::new(FS, BODY_MALE, 2);
    v.set_source(&SourcePatch { f0: Some(midi_to_hz(notes[0].midi)), voiced: Some(true), pressure: Some(9.0), tension: Some(0.55), ..Default::default() });
    let plan = plan_song(&notes, &SongOptions { body: BodyMix::new(0.0, 0.0), bpm: 132.0, t0: 0.3, vibrato: 35.0 });
    v.schedule(&plan.gestures);
    let mut out = vec![0f32; (FS * (plan.end + 0.4)) as usize];
    for block in out.chunks_mut(128) {
        v.render(block);
    }
    let peak = out.iter().fold(0f32, |m, x| m.max(x.abs()));
    if peak > 0.0 {
        out.iter_mut().for_each(|x| *x *= 0.9 / peak);
    }
    Some(out)
}

/// 16-bit mono PCM WAV.
pub fn wav(x: &[f32]) -> Vec<u8> {
    let data = (x.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    for v in [16u32, 1 | (1 << 16), FS as u32, FS as u32 * 2, 2 | (16 << 16)] {
        w.extend_from_slice(&v.to_le_bytes());
    }
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data.to_le_bytes());
    for s in x {
        w.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    w
}

#[derive(Deserialize)]
pub struct SingReq {
    text: String,
}

/// POST /sing {text}: the chat's sing button. A signed-in session, or an access link in
/// `x-link`. Rendering costs CPU, so it is rate limited and the last 64 songs are cached.
pub async fn sing(State(app): State<App>, headers: HeaderMap, Json(r): Json<SingReq>) -> Result<Response, Fail> {
    let who = match crate::auth::session(&app, &headers).await {
        Some(a) => a.id,
        None => {
            let secret = headers.get("x-link").and_then(|v| v.to_str().ok()).unwrap_or("");
            crate::links::resolve(&app, secret).await.ok_or_else(|| Fail::new(401, "sign in or use an access link"))?.0
        }
    };
    let text: String = r.text.chars().take(240).collect();
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let hit = cache.lock().unwrap().get(&text).cloned();
    let bytes = match hit {
        Some(b) => b,
        None => {
            if !app.limiter.allow(&format!("sing:{who}"), 0.2, 6.0) {
                return Err(Fail::new(429, "Too many songs. Try again in a minute."));
            }
            let t = text.clone();
            let samples = tokio::task::spawn_blocking(move || render(&t)).await.map_err(|_| Fail::new(500, "the voice failed"))?
                .ok_or_else(|| Fail::new(422, "Nothing in that message can be sung."))?;
            let b = Arc::new(wav(&samples));
            let mut c = cache.lock().unwrap();
            if c.len() >= 64 {
                c.clear(); // ponytail: crude bound, an LRU if repeats matter
            }
            c.insert(text, b.clone());
            b
        }
    };
    Ok(([(header::CONTENT_TYPE, "audio/wav"), (header::CACHE_CONTROL, "private, max-age=3600")], bytes.to_vec()).into_response())
}

#[cfg(test)]
mod tests {
    #[test]
    fn sings_a_message_up_the_ladder() {
        let x = super::render("what is this").expect("sung");
        assert!(x.len() > 48000, "at least a second of audio");
        assert!(x.iter().any(|s| s.abs() > 0.5), "not silent");
        let w = super::wav(&x);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(w.len(), 44 + x.len() * 2);
        assert!(super::render("…").is_none());
    }
}
