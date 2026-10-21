//! Subscribes to the in-process event bridge and turns the stream into
//! `events.jsonl`, per-call WAV files and a `Summary`.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use trunk_oxide::clock::Clock;
use trunk_oxide_grpc::proto::{self, plugin_event::Event, stream_message::Message};

/// Minimal streaming 16-bit mono PCM WAV writer (header patched on finish).
struct Wav {
    out: BufWriter<File>,
    data_bytes: u32,
}

impl Wav {
    fn create(path: &Path, sample_rate: u32) -> std::io::Result<Self> {
        let mut out = BufWriter::new(File::create(path)?);
        out.write_all(b"RIFF\0\0\0\0WAVEfmt ")?;
        out.write_all(&16_u32.to_le_bytes())?;
        out.write_all(&1_u16.to_le_bytes())?; // PCM
        out.write_all(&1_u16.to_le_bytes())?; // mono
        out.write_all(&sample_rate.to_le_bytes())?;
        out.write_all(&(sample_rate * 2).to_le_bytes())?;
        out.write_all(&2_u16.to_le_bytes())?;
        out.write_all(&16_u16.to_le_bytes())?;
        out.write_all(b"data\0\0\0\0")?;
        Ok(Self { out, data_bytes: 0 })
    }

    fn write(&mut self, pcm: &[u8]) -> std::io::Result<()> {
        self.out.write_all(pcm)?;
        self.data_bytes = self
            .data_bytes
            .saturating_add(u32::try_from(pcm.len()).unwrap_or(u32::MAX));
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<()> {
        self.out.flush()?;
        let f = self.out.get_mut();
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(36 + self.data_bytes).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&self.data_bytes.to_le_bytes())
    }
}

#[derive(Default, Serialize)]
struct SystemStats {
    messages: BTreeMap<String, u64>,
    calls_started: u64,
    calls_ended: u64,
    encrypted_calls: u64,
    audio_seconds: f64,
    decode_errors: i64,
    decode_spikes: i64,
    talkgroups: BTreeMap<i64, TalkgroupStats>,
}

#[derive(Default, Serialize)]
struct TalkgroupStats {
    calls: u64,
    audio_seconds: f64,
}

/// What a run produced; `summary.json`.
#[derive(Serialize)]
pub struct Summary {
    pub wall_seconds: f64,
    /// `1x` (real time) or `max`.
    pub speed: String,
    pub overrides: Vec<String>,
    pub trunk_messages_total: u64,
    pub calls_started_total: u64,
    pub calls_ended_total: u64,
    pub audio_seconds_total: f64,
    pub audio_samples_without_call: u64,
    pub mean_abs_freq_error_hz: Option<f64>,
    pub mean_signal_db: Option<f64>,
    pub mean_noise_db: Option<f64>,
    pub channelizer: Vec<Value>,
    systems: BTreeMap<String, SystemStats>,
    /// P25 voice calls by system and phase, and the control channel
    /// parser's counts.
    pub decoders: Value,
}

/// The P25 voice calls' tallies (`trunk_radio::p25_calls`) and the
/// control channel parser's counts, as JSON.
pub fn decoder_tallies() -> Value {
    let calls: BTreeMap<String, Value> = trunk_radio::p25_calls::call_tally()
        .iter()
        .map(|((s, p), t)| {
            (
                format!("{s}/phase{}", p.label()),
                json!({
                    "calls": t.calls, "audio_seconds": round(t.audio_seconds, 3),
                    "transmissions": t.transmissions, "bit_errors": t.errors, "spikes": t.spikes,
                    "encrypted": t.encrypted,
                }),
            )
        })
        .collect();
    let parser: Vec<Value> = trunk_radio::p25_parse::parse_stats()
        .into_iter()
        .map(|r| {
            let s = r.stats;
            let kinds: BTreeMap<&str, u64> = s.kinds.into_iter().collect();
            json!({
                "system": r.system, "kinds": kinds,
                "channels": { "own": s.own_plans, "shared": s.shared_plans, "unknown": s.unknown_channels },
                "invalid": s.invalid, "plan_conflicts": s.conflicts,
                "held_grants": { "held": s.deferred, "expired": s.expired },
            })
        })
        .collect();
    json!({
        "voice": { "calls": calls },
        "parser": parser,
    })
}

struct OpenCall {
    path: PathBuf,
    /// Created on the first audio chunk, once the sample rate is known.
    wav: Option<Wav>,
    rate: u32,
    samples: u64,
}

pub struct Recorder {
    /// When the run started, on the wall clock (`wall_seconds`).
    t0: Instant,
    /// What events are stamped with.
    clock: Clock,
    events: BufWriter<File>,
    audio_dir: PathBuf,
    systems: BTreeMap<String, SystemStats>,
    open: HashMap<(String, i64), OpenCall>,
    orphan_samples: u64,
    freq_err_sum: f64,
    freq_err_n: u64,
    signal_sum: f64,
    signal_n: u64,
    noise_sum: f64,
    noise_n: u64,
    messages_total: u64,
}

fn round(x: f64, places: i32) -> f64 {
    let m = 10_f64.powi(places);
    (x * m).round() / m
}

fn message_type_name(t: i32) -> String {
    proto::MessageType::try_from(t).map_or_else(
        |_| format!("UNKNOWN_{t}"),
        |m| {
            m.as_str_name()
                .trim_start_matches("MESSAGE_TYPE_")
                .to_string()
        },
    )
}

fn call_info_json(c: &proto::CallInfo) -> Value {
    json!({
        "system": c.short_name, "talkgroup": c.talkgroup, "call_num": c.call_num,
        "freq": c.freq, "signal": c.signal, "noise": c.noise,
        "encrypted": c.encrypted, "emergency": c.emergency, "kind": c.kind,
        "conventional": c.is_conventional, "tdma": c.phase2_tdma, "slot": c.tdma_slot,
        "source_id": c.current_source_id, "tag": c.talkgroup_alpha_tag,
    })
}

impl Recorder {
    /// Events are stamped with `clock`'s time since it started (the
    /// capture's, in a paced replay).
    pub fn new(run_dir: &Path, clock: Clock) -> Result<Self> {
        let audio_dir = run_dir.join("audio");
        std::fs::create_dir_all(&audio_dir)?;
        Ok(Self {
            t0: Instant::now(),
            clock,
            events: BufWriter::new(File::create(run_dir.join("events.jsonl"))?),
            audio_dir,
            systems: BTreeMap::new(),
            open: HashMap::new(),
            orphan_samples: 0,
            freq_err_sum: 0.0,
            freq_err_n: 0,
            signal_sum: 0.0,
            signal_n: 0,
            noise_sum: 0.0,
            noise_n: 0,
            messages_total: 0,
        })
    }

    fn t(&self) -> f64 {
        round(self.clock.elapsed().as_secs_f64(), 3)
    }

    fn emit(&mut self, kind: &str, mut body: Value) {
        if let Value::Object(m) = &mut body {
            m.insert("t".into(), json!(self.t()));
            m.insert("type".into(), json!(kind));
        }
        let _ = writeln!(self.events, "{body}");
    }

    fn on_message(&mut self, msg: proto::StreamMessage) {
        match msg.message {
            Some(Message::Audio(a)) => self.on_audio(&a),
            Some(Message::Event(e)) => {
                if let Some(ev) = e.event {
                    self.on_event(ev);
                }
            }
            None => {}
        }
    }

    fn on_audio(&mut self, a: &proto::AudioSample) {
        let key = (a.short_name.clone(), a.talkgroup);
        let samples = (a.samples.len() / 2) as u64;
        if let Some(call) = self.open.get_mut(&key) {
            call.samples += samples;
            if call.wav.is_none() {
                call.rate = u32::try_from(a.sample_rate).unwrap_or(8000);
                call.wav = Wav::create(&call.path, call.rate).ok();
            }
            if let Some(w) = call.wav.as_mut() {
                let _ = w.write(&a.samples);
            }
        } else {
            self.orphan_samples += samples;
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::TrunkMessage(tm) => {
                let sys = tm
                    .system
                    .as_ref()
                    .map(|s| s.short_name.clone())
                    .unwrap_or_default();
                for m in &tm.messages {
                    let name = message_type_name(m.message_type);
                    *self
                        .systems
                        .entry(sys.clone())
                        .or_default()
                        .messages
                        .entry(name.clone())
                        .or_default() += 1;
                    self.messages_total += 1;
                    self.emit(
                        "trunk_message",
                        json!({
                            "system": sys, "message": name, "meta": m.meta, "freq": m.freq,
                            "talkgroup": m.talkgroup, "source": m.source, "encrypted": m.encrypted,
                            "emergency": m.emergency, "tdma": m.phase2_tdma, "slot": m.tdma_slot,
                            "opcode": m.opcode, "nac": m.nac, "sys_id": m.sys_id,
                            "rfss": m.rfss, "site_id": m.site_id,
                        }),
                    );
                }
            }
            Event::CallStart(cs) => {
                let Some(c) = cs.call else { return };
                self.systems
                    .entry(c.short_name.clone())
                    .or_default()
                    .calls_started += 1;
                self.emit("call_start", call_info_json(&c));
                let t = self.t();
                let name = format!(
                    "{t:09.3}_{}_{}_{:.4}MHz.wav",
                    c.short_name,
                    c.talkgroup,
                    c.freq / 1e6
                );
                self.open.insert(
                    (c.short_name, c.talkgroup),
                    OpenCall {
                        path: self.audio_dir.join(name),
                        wav: None,
                        rate: 8000,
                        samples: 0,
                    },
                );
            }
            Event::CallEnd(ce) => {
                let reason = ce.reason().as_str_name();
                let Some(d) = ce.call_data else { return };
                let (secs, rate) = self
                    .open
                    .remove(&(d.short_name.clone(), d.talkgroup))
                    .map_or((0.0, 8000), |o| {
                        if let Some(w) = o.wav {
                            let _ = w.finish();
                        }
                        #[allow(clippy::cast_precision_loss)]
                        (o.samples as f64 / f64::from(o.rate), o.rate)
                    });
                let sys = self.systems.entry(d.short_name.clone()).or_default();
                sys.calls_ended += 1;
                sys.encrypted_calls += u64::from(d.encrypted);
                sys.audio_seconds += secs;
                sys.decode_errors += d.error_count;
                sys.decode_spikes += d.spike_count;
                let tg = sys.talkgroups.entry(d.talkgroup).or_default();
                tg.calls += 1;
                tg.audio_seconds += secs;
                self.freq_err_sum += f64::from(d.freq_error.abs());
                self.freq_err_n += 1;
                if let Some(s) = d.signal {
                    self.signal_sum += s;
                    self.signal_n += 1;
                }
                if let Some(n) = d.noise {
                    self.noise_sum += n;
                    self.noise_n += 1;
                }
                self.emit(
                    "call_end",
                    json!({
                        "system": d.short_name, "talkgroup": d.talkgroup, "call_num": d.call_num,
                        "freq": d.freq, "freq_error": d.freq_error, "signal": d.signal,
                        "noise": d.noise, "length": round(d.length, 3), "audio_seconds": round(secs, 3),
                        "audio_rate": rate, "errors": d.error_count, "spikes": d.spike_count,
                        "encrypted": d.encrypted, "tdma": d.phase2_tdma, "slot": d.tdma_slot,
                        "reason": reason, "channel": d.channel, "kind": d.kind,
                        "transmissions": d.transmissions.len(),
                        "audio_shape": d.transmissions.iter().filter_map(|t| t.shape.as_ref()).map(|s| json!({
                            "onset_s": round(s.onset_s, 3), "trailing_silence_s": round(s.trailing_silence_s, 3),
                            "end_level_db": round(s.end_level_db, 1), "gaps": s.gaps,
                            "longest_gap_s": round(s.longest_gap_s, 3),
                        })).collect::<Vec<_>>(),
                        "signalling": d.signalling.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
                        "signalling_alerts": d.signalling.iter().filter(|s| s.alert() != proto::SignalAlert::None).map(|s| s.alert().as_str_name()).collect::<Vec<_>>(),
                    }),
                );
            }
            Event::Unit(u) => {
                let sys = u
                    .system
                    .as_ref()
                    .map(|s| s.short_name.clone())
                    .unwrap_or_default();
                self.emit(
                    "unit",
                    json!({"system": sys, "event": u.r#type, "source": u.source_id, "talkgroup": u.talkgroup}),
                );
            }
            _ => {}
        }
    }

    /// Drain the bridge until the sender side closes or `stop` fires.
    pub async fn run(
        &mut self,
        mut rx: broadcast::Receiver<proto::StreamMessage>,
        stop: std::sync::Arc<tokio::sync::Notify>,
    ) {
        loop {
            tokio::select! {
                () = stop.notified() => break,
                m = rx.recv() => match m {
                    Ok(m) => self.on_message(m),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "recorder lagged behind the event bridge");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        // Whatever is still queued.
        while let Ok(m) = rx.try_recv() {
            self.on_message(m);
        }
    }

    pub fn finish(
        mut self,
        speed: &str,
        overrides: Vec<String>,
        channelizer: Vec<Value>,
    ) -> Result<Summary> {
        for (_, o) in self.open.drain() {
            if let Some(w) = o.wav {
                let _ = w.finish();
            }
        }
        self.events.flush()?;
        let mean = |sum: f64, n: u64| (n > 0).then(|| round(sum / n as f64, 3));
        let calls_started_total = self.systems.values().map(|s| s.calls_started).sum();
        let calls_ended_total = self.systems.values().map(|s| s.calls_ended).sum();
        let audio_seconds_total = round(self.systems.values().map(|s| s.audio_seconds).sum(), 3);
        for s in self.systems.values_mut() {
            s.audio_seconds = round(s.audio_seconds, 3);
            for t in s.talkgroups.values_mut() {
                t.audio_seconds = round(t.audio_seconds, 3);
            }
        }
        Ok(Summary {
            wall_seconds: round(self.t0.elapsed().as_secs_f64(), 1),
            speed: speed.to_owned(),
            overrides,
            trunk_messages_total: self.messages_total,
            calls_started_total,
            calls_ended_total,
            audio_seconds_total,
            audio_samples_without_call: self.orphan_samples,
            mean_abs_freq_error_hz: mean(self.freq_err_sum, self.freq_err_n),
            mean_signal_db: mean(self.signal_sum, self.signal_n),
            mean_noise_db: mean(self.noise_sum, self.noise_n),
            channelizer,
            systems: self.systems,
            decoders: decoder_tallies(),
        })
    }
}
