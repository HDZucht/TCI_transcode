//! The SDRconnect side: the published SDRconnect WebSocket API 1.0.3 (SDRplay,
//! six pages), served to clients written for it (msdr, J. van Katwijk's
//! modules, TandemSDR …).
//!
//! Text frames carry one JSON object `{event_type, property, value, device?}`.
//! Binary frames carry a 2-byte little-endian payload type, then int16 or
//! uint8 data: 1 = audio PCM16 stereo 48 kHz, 2 = IQ int16 interleaved,
//! 3 = spectrum, uint8 bins normalised to the visible range. Types 4–6 are
//! the same for a second tuner, which the bridge does not have.
//!
//! Every value goes out as a string, as the spec says. Incoming values are
//! accepted as strings, numbers or booleans: real clients send all three.

use serde_json::{Value, json};

use crate::state::{Lead, RADIO_BOUND, State, sdrc_to_tci};
use crate::tci;

pub const PAYLOAD_AUDIO: u16 = 1;
pub const PAYLOAD_IQ: u16 = 2;
pub const PAYLOAD_SPECTRUM: u16 = 3;

/// Which binary streams one client connection has switched on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Streams {
    pub iq: bool,
    pub audio: bool,
    pub spectrum: bool,
}

/// What handling one client message produced.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Text frames for the connection that sent the message.
    pub reply: Vec<String>,
    /// Text frames for every connection (property changes).
    pub broadcast: Vec<String>,
    /// TCI commands for the server.
    pub tci: Vec<String>,
}

pub fn message(event_type: &str, property: &str, value: &str) -> String {
    json!({ "event_type": event_type, "property": property, "device": "primary", "value": value })
        .to_string()
}

pub fn property_changed(state: &State, prop: &str) -> Option<String> {
    state.value(prop).map(|v| message("property_changed", prop, &v))
}

fn value_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn truthy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1")
}

/// Handle one text frame from a client.
pub fn handle(state: &mut State, streams: &mut Streams, text: &str) -> Outcome {
    let mut out = Outcome::default();
    let Ok(msg) = serde_json::from_str::<Value>(text) else {
        return out;
    };
    // The bridge has one tuner; requests for the secondary get no answer.
    if msg.get("device").and_then(Value::as_str).is_some_and(|d| d != "primary") {
        return out;
    }
    let event = msg.get("event_type").and_then(Value::as_str).unwrap_or("");
    let prop = msg.get("property").and_then(Value::as_str).unwrap_or("");
    let value = value_text(msg.get("value"));

    match event {
        "get_property" => {
            if let Some(v) = state.value(prop) {
                out.reply.push(message("get_property_response", prop, &v));
            }
        }
        "set_property" => set_property(state, prop, value, &mut out),
        "iq_stream_enable" => streams.iq = truthy(&value),
        "audio_stream_enable" => streams.audio = truthy(&value),
        "spectrum_enable" => streams.spectrum = truthy(&value),
        // Device selection, recording, profiles and per-tuner filters have no
        // meaning for a single TCI receiver.
        _ => tracing::debug!(event, prop, "ignored"),
    }
    out
}

fn set_property(state: &mut State, prop: &str, value: String, out: &mut Outcome) {
    if RADIO_BOUND.contains(&prop) {
        // The IQ rate is the bridge's own subscription, not the operator's
        // radio, so a client may always choose it.
        let command = match prop {
            "device_sample_rate" => value.parse::<f64>().ok().map(|r| tci::cmd::iq_samplerate(r as u32)),
            _ if state.lead == Lead::Frontend => None,
            "device_vfo_frequency" => value.parse::<f64>().ok().map(|hz| tci::cmd::vfo(hz as i64)),
            "device_center_frequency" => value.parse::<f64>().ok().map(|hz| tci::cmd::dds(hz as i64)),
            "demodulator" => sdrc_to_tci(&value).map(tci::cmd::modulation),
            _ => None,
        };
        match command {
            // The change is reported when the TCI server echoes it.
            Some(c) => out.tci.push(c),
            // Refused or unparseable: tell the client what holds instead.
            None => out.broadcast.extend(property_changed(state, prop)),
        }
    } else if state.is_local(prop) {
        state.set_local(prop, value);
        out.broadcast.extend(property_changed(state, prop));
    } else {
        tracing::debug!(prop, "set on a read-only or unknown property ignored");
    }
}

/// A binary frame: payload type, then the payload.
pub fn frame(payload_type: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + payload.len());
    out.extend_from_slice(&payload_type.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// IQ frame from interleaved float32 I,Q (full scale ±1).
pub fn iq_frame(iq: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + iq.len() * 2);
    out.extend_from_slice(&PAYLOAD_IQ.to_le_bytes());
    for &s in iq {
        out.extend_from_slice(&to_i16(s).to_le_bytes());
    }
    out
}

/// Audio frame, PCM16 stereo L,R, from float32 audio with `channels`
/// interleaved channels (1 is duplicated to both ears).
pub fn audio_frame(audio: &[f32], channels: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + audio.len() * 4);
    out.extend_from_slice(&PAYLOAD_AUDIO.to_le_bytes());
    if channels == 1 {
        for &s in audio {
            let b = to_i16(s).to_le_bytes();
            out.extend_from_slice(&b);
            out.extend_from_slice(&b);
        }
    } else {
        for &s in audio {
            out.extend_from_slice(&to_i16(s).to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn get_answers_with_a_string_value() {
        let mut st = State::new(Lead::Frontend);
        st.iq_rate = 250_000;
        let mut sm = Streams::default();
        let o = handle(
            &mut st,
            &mut sm,
            r#"{"event_type":"get_property","property":"device_sample_rate","value":""}"#,
        );
        let r = parse(&o.reply[0]);
        assert_eq!(r["event_type"], "get_property_response");
        assert_eq!(r["value"], "250000");
    }

    #[test]
    fn unknown_get_gets_no_answer() {
        let mut st = State::new(Lead::Frontend);
        let o = handle(
            &mut st,
            &mut Streams::default(),
            r#"{"event_type":"get_property","property":"no_such_thing","value":""}"#,
        );
        assert!(o.reply.is_empty());
    }

    #[test]
    fn frontend_lead_refuses_tuning_with_the_current_value() {
        let mut st = State::new(Lead::Frontend);
        st.vfo_hz = 433_920_000;
        let o = handle(
            &mut st,
            &mut Streams::default(),
            r#"{"event_type":"set_property","property":"device_vfo_frequency","value":"144800000"}"#,
        );
        assert!(o.tci.is_empty());
        assert_eq!(parse(&o.broadcast[0])["value"], "433920000");
    }

    #[test]
    fn client_lead_forwards_tuning_to_tci() {
        let mut st = State::new(Lead::Client);
        let mut sm = Streams::default();
        let o = handle(
            &mut st,
            &mut sm,
            r#"{"event_type":"set_property","property":"device_vfo_frequency","value":144800000}"#,
        );
        assert_eq!(o.tci, vec!["vfo:0,0,144800000;"]);
        let o = handle(
            &mut st,
            &mut sm,
            r#"{"event_type":"set_property","property":"demodulator","value":"NFM"}"#,
        );
        assert_eq!(o.tci, vec!["modulation:0,nfm;"]);
    }

    #[test]
    fn sample_rate_is_always_the_clients_to_choose() {
        let mut st = State::new(Lead::Frontend);
        let o = handle(
            &mut st,
            &mut Streams::default(),
            r#"{"event_type":"set_property","property":"device_sample_rate","value":"2400000"}"#,
        );
        assert_eq!(o.tci, vec!["iq_samplerate:2400000;"]);
    }

    #[test]
    fn local_properties_are_kept_and_echoed() {
        let mut st = State::new(Lead::Frontend);
        let o = handle(
            &mut st,
            &mut Streams::default(),
            r#"{"event_type":"set_property","property":"squelch_enable","value":true}"#,
        );
        assert_eq!(parse(&o.broadcast[0])["value"], "true");
        assert_eq!(st.value("squelch_enable").unwrap(), "true");
    }

    #[test]
    fn stream_switches_are_per_connection() {
        let mut st = State::new(Lead::Frontend);
        let mut sm = Streams::default();
        handle(&mut st, &mut sm, r#"{"event_type":"spectrum_enable","property":"","value":"true"}"#);
        handle(&mut st, &mut sm, r#"{"event_type":"iq_stream_enable","property":"","value":"true"}"#);
        assert_eq!(sm, Streams { iq: true, audio: false, spectrum: true });
    }

    #[test]
    fn secondary_device_is_not_served() {
        let mut st = State::new(Lead::Client);
        let o = handle(
            &mut st,
            &mut Streams::default(),
            r#"{"event_type":"get_property","property":"api_version","device":"secondary","value":""}"#,
        );
        assert_eq!(o, Outcome::default());
    }

    #[test]
    fn binary_frames_carry_type_and_int16() {
        let f = iq_frame(&[1.0, -1.0, 0.0, 2.0]);
        assert_eq!(&f[..2], &[2, 0]);
        let v: Vec<i16> = f[2..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(v, vec![32767, -32767, 0, 32767]);
        let a = audio_frame(&[0.5], 1);
        assert_eq!(&a[..2], &[1, 0]);
        assert_eq!(a.len(), 2 + 4);
    }
}
