//! Sound: the PDM microphone (I2S0) streamed to the plane, and the speaker (I2S1, its amplifier
//! switched by an expander pin) playing the plane's PCM stream or the robot's own recorded
//! phrases. 16 kHz, 16-bit, mono both ways.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use esp_idf_svc::sys::{self, esp, EspError};

use crate::link::Link;
use crate::pins::{self, exp};
use crate::tca::Tca;
use crate::util::{self, millis, Stack};

const RATE: u32 = 16_000;
/// Microphone samples per read (32 ms, one datagram).
const MIC_CHUNK: usize = 512;
const HIGH_PASS_HZ: f32 = 120.0;
/// Dropped after the microphone starts.
const MIC_SETTLE_MS: u64 = 100;
/// Speaker audio buffered before a stream starts playing (network jitter).
const PREBUFFER: usize = RATE as usize / 10;
/// At most this much is buffered (the plane paces its stream in real time).
const MAX_BUFFER: usize = RATE as usize * 8;
/// A stream that sends nothing for this long has ended (its spk_end got lost).
const STREAM_SILENT_MS: u64 = 2000;
const BLOCK: usize = 256;

fn chan_config(port: sys::i2s_port_t, auto_clear: bool) -> sys::i2s_chan_config_t {
    let mut c: sys::i2s_chan_config_t = unsafe { core::mem::zeroed() };
    c.id = port;
    c.role = sys::i2s_role_t_I2S_ROLE_MASTER;
    c.dma_desc_num = 6;
    c.dma_frame_num = 240;
    c.__bindgen_anon_1.auto_clear_after_cb = auto_clear;
    c
}

// ---------------------------------------------------------------- microphone

/// Biquad high-pass (RBJ cookbook, Q = 1/sqrt 2): the PDM mic has a large DC offset and rumble.
struct HighPass {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl HighPass {
    fn new(hz: f32) -> HighPass {
        let w0 = 2.0 * core::f32::consts::PI * hz / RATE as f32;
        let alpha = w0.sin() / (2.0 * core::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let c = w0.cos();
        HighPass {
            b: [(1.0 + c) / 2.0 / a0, -(1.0 + c) / a0, (1.0 + c) / 2.0 / a0],
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

pub struct Mic {
    on: AtomicBool,
    gate: AtomicBool,
    gain: AtomicU32,
    changed: (Mutex<()>, Condvar),
}

struct Chan(sys::i2s_chan_handle_t);
unsafe impl Send for Chan {}

impl Mic {
    pub fn start(link: Arc<Link>, speaker: Arc<Speaker>) -> Result<Arc<Mic>, EspError> {
        let mut rx = core::ptr::null_mut();
        let cfg = chan_config(sys::i2s_port_t_I2S_NUM_0, false);
        esp!(unsafe { sys::i2s_new_channel(&cfg, core::ptr::null_mut(), &mut rx) })?;
        let mut pdm: sys::i2s_pdm_rx_config_t = unsafe { core::mem::zeroed() };
        pdm.clk_cfg.sample_rate_hz = RATE;
        pdm.clk_cfg.clk_src = sys::soc_periph_i2s_clk_src_t_I2S_CLK_SRC_DEFAULT;
        pdm.clk_cfg.mclk_multiple = sys::i2s_mclk_multiple_t_I2S_MCLK_MULTIPLE_256;
        pdm.clk_cfg.dn_sample_mode = sys::i2s_pdm_dsr_t_I2S_PDM_DSR_8S;
        pdm.clk_cfg.bclk_div = 8;
        pdm.slot_cfg.data_bit_width = sys::i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_16BIT;
        pdm.slot_cfg.slot_bit_width = sys::i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_AUTO;
        pdm.slot_cfg.slot_mode = sys::i2s_slot_mode_t_I2S_SLOT_MODE_STEREO;
        pdm.slot_cfg.slot_mask = sys::i2s_pdm_slot_mask_t_I2S_PDM_SLOT_BOTH;
        pdm.slot_cfg.data_fmt = sys::i2s_pdm_data_fmt_t_I2S_PDM_DATA_FMT_PCM;
        pdm.gpio_cfg.clk = pins::MIC_CLK;
        pdm.gpio_cfg.__bindgen_anon_1.din = pins::MIC_DIN;
        esp!(unsafe { sys::i2s_channel_init_pdm_rx_mode(rx, &pdm) })?;
        let mic = Arc::new(Mic {
            on: AtomicBool::new(false),
            gate: AtomicBool::new(true),
            gain: AtomicU32::new(1.0f32.to_bits()),
            changed: (Mutex::new(()), Condvar::new()),
        });
        let m = mic.clone();
        let rx = Chan(rx);
        util::spawn(c"mic", 6144, 9, Stack::Internal, move || m.run(rx, &link, &speaker));
        Ok(mic)
    }

    pub fn set(&self, on: bool, gate: bool, gain: f32) {
        self.gate.store(gate, Ordering::Relaxed);
        self.gain.store(gain.clamp(0.0, 8.0).to_bits(), Ordering::Relaxed);
        self.on.store(on, Ordering::Relaxed);
        self.changed.1.notify_one();
    }

    fn run(&self, rx: Chan, link: &Link, speaker: &Speaker) {
        let rx = rx.0;
        let mut enabled = false;
        let mut hp = HighPass::new(HIGH_PASS_HZ);
        let mut raw = vec![0i16; MIC_CHUNK * 2];
        let mut out = vec![0i16; MIC_CHUNK];
        let mut settle_until = 0;
        loop {
            if !self.on.load(Ordering::Relaxed) {
                if enabled {
                    unsafe { sys::i2s_channel_disable(rx) };
                    enabled = false;
                }
                let g = self.changed.0.lock().unwrap();
                let _ = self.changed.1.wait_timeout(g, Duration::from_millis(500));
                continue;
            }
            if !enabled {
                unsafe { sys::i2s_channel_enable(rx) };
                enabled = true;
                hp = HighPass::new(HIGH_PASS_HZ);
                settle_until = millis() + MIC_SETTLE_MS;
            }
            let mut read = 0usize;
            let err = unsafe {
                sys::i2s_channel_read(rx, raw.as_mut_ptr() as *mut _, raw.len() * 2, &mut read, 200)
            };
            if err != sys::ESP_OK || read == 0 {
                continue;
            }
            let frames = read / 4;
            if millis() < settle_until {
                // The filter settles on the microphone's offset (a loud click otherwise).
                for i in 0..frames {
                    hp.run((raw[2 * i] as f32 + raw[2 * i + 1] as f32) * 0.5);
                }
                continue;
            }
            let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
            // No echo cancellation: silence while the robot itself is talking.
            let muted = self.gate.load(Ordering::Relaxed) && speaker.playing();
            for i in 0..frames {
                let x = (raw[2 * i] as f32 + raw[2 * i + 1] as f32) * 0.5;
                let y = hp.run(x) * gain;
                out[i] = if muted { 0 } else { y.clamp(-32768.0, 32767.0) as i16 };
            }
            link.send_mic(&out[..frames]);
        }
    }
}

// ---------------------------------------------------------------- speaker

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Feed {
    Idle,
    /// The plane's stream: more may come.
    Streaming,
    /// Play out what is buffered, then stop.
    Finishing,
}

struct Queue {
    pcm: VecDeque<i16>,
    feed: Feed,
    abort: bool,
    last_push: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum SpeakerEvent {
    Playing { on: bool, underruns: u32 },
}

pub struct Speaker {
    q: Mutex<Queue>,
    wake: Condvar,
    playing: AtomicBool,
    gain: AtomicU32,
    underruns: AtomicU32,
}

impl Speaker {
    pub fn start<E: From<SpeakerEvent> + Send + 'static>(
        tca: Arc<Tca>,
        events: Sender<E>,
    ) -> Result<Arc<Speaker>, EspError> {
        let mut tx = core::ptr::null_mut();
        let cfg = chan_config(sys::i2s_port_t_I2S_NUM_1, true);
        esp!(unsafe { sys::i2s_new_channel(&cfg, &mut tx, core::ptr::null_mut()) })?;
        let mut std: sys::i2s_std_config_t = unsafe { core::mem::zeroed() };
        std.clk_cfg.sample_rate_hz = RATE;
        std.clk_cfg.clk_src = sys::soc_periph_i2s_clk_src_t_I2S_CLK_SRC_DEFAULT;
        std.clk_cfg.mclk_multiple = sys::i2s_mclk_multiple_t_I2S_MCLK_MULTIPLE_256;
        std.slot_cfg.data_bit_width = sys::i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_16BIT;
        std.slot_cfg.slot_bit_width = sys::i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_AUTO;
        std.slot_cfg.slot_mode = sys::i2s_slot_mode_t_I2S_SLOT_MODE_MONO;
        std.slot_cfg.slot_mask = sys::i2s_std_slot_mask_t_I2S_STD_SLOT_BOTH;
        std.slot_cfg.ws_width = 16;
        std.slot_cfg.ws_pol = false;
        std.slot_cfg.bit_shift = false;
        std.slot_cfg.left_align = true;
        std.slot_cfg.big_endian = false;
        std.slot_cfg.bit_order_lsb = false;
        std.gpio_cfg.mclk = -1;
        std.gpio_cfg.bclk = pins::SPK_BCK;
        std.gpio_cfg.ws = pins::SPK_WS;
        std.gpio_cfg.dout = pins::SPK_DOUT;
        std.gpio_cfg.din = -1;
        esp!(unsafe { sys::i2s_channel_init_std_mode(tx, &std) })?;
        let s = Arc::new(Speaker {
            q: Mutex::new(Queue { pcm: VecDeque::new(), feed: Feed::Idle, abort: false, last_push: 0 }),
            wake: Condvar::new(),
            playing: AtomicBool::new(false),
            gain: AtomicU32::new(0.8f32.to_bits()),
            underruns: AtomicU32::new(0),
        });
        let me = s.clone();
        let tx = Chan(tx);
        util::spawn(c"speaker", 6144, 10, Stack::Internal, move || me.run(tx, &tca, &events));
        Ok(s)
    }

    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn set_gain(&self, g: f32) {
        self.gain.store(g.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// The plane starts a stream (`spk_begin`).
    pub fn begin(&self) {
        let mut q = self.q.lock().unwrap();
        q.pcm.clear();
        q.abort = q.feed != Feed::Idle;
        q.feed = Feed::Streaming;
        q.last_push = millis();
        self.underruns.store(0, Ordering::Relaxed);
        self.wake.notify_all();
    }

    /// Stream audio from the plane: s16le samples.
    pub fn push(&self, bytes: &[u8]) {
        let mut q = self.q.lock().unwrap();
        if q.feed != Feed::Streaming {
            return;
        }
        for c in bytes.chunks_exact(2) {
            q.pcm.push_back(i16::from_le_bytes([c[0], c[1]]));
        }
        while q.pcm.len() > MAX_BUFFER {
            q.pcm.pop_front();
        }
        q.last_push = millis();
        self.wake.notify_all();
    }

    pub fn end(&self) {
        let mut q = self.q.lock().unwrap();
        if q.feed == Feed::Streaming {
            q.feed = Feed::Finishing;
            self.wake.notify_all();
        }
    }

    pub fn abort(&self) {
        let mut q = self.q.lock().unwrap();
        q.pcm.clear();
        if q.feed != Feed::Idle {
            q.abort = true;
            q.feed = Feed::Idle;
        }
        self.wake.notify_all();
    }

    /// One of the robot's own phrases (cuts off whatever plays).
    pub fn say(&self, p: Phrase) {
        let pcm = p.decode();
        let mut q = self.q.lock().unwrap();
        q.abort = q.feed != Feed::Idle;
        q.pcm = pcm.into();
        q.feed = Feed::Finishing;
        self.wake.notify_all();
    }

    /// Wait until nothing plays (at most `ms`).
    pub fn wait_idle(&self, ms: u64) {
        let deadline = millis() + ms;
        let mut q = self.q.lock().unwrap();
        while (q.feed != Feed::Idle || self.playing()) && millis() < deadline {
            q = self.wake.wait_timeout(q, Duration::from_millis(20)).unwrap().0;
        }
    }

    fn run<E: From<SpeakerEvent>>(&self, tx: Chan, tca: &Tca, events: &Sender<E>) {
        let tx = tx.0;
        let mut block = [0i16; BLOCK];
        loop {
            // Something to play: a phrase, or a stream with enough buffered.
            {
                let mut q = self.q.lock().unwrap();
                loop {
                    q.abort = false;
                    let ready = match q.feed {
                        Feed::Idle => false,
                        Feed::Finishing => !q.pcm.is_empty(),
                        Feed::Streaming => q.pcm.len() >= PREBUFFER,
                    };
                    if ready {
                        break;
                    }
                    if q.feed == Feed::Finishing {
                        q.feed = Feed::Idle; // nothing came
                        self.wake.notify_all();
                    }
                    q = self.wake.wait_timeout(q, Duration::from_millis(100)).unwrap().0;
                }
            }
            tca.write(exp::SPK_EN, true);
            unsafe { sys::i2s_channel_enable(tx) };
            self.playing.store(true, Ordering::Relaxed);
            let _ = events.send(SpeakerEvent::Playing { on: true, underruns: 0 }.into());
            let mut had_data = false;
            loop {
                let (n, done) = {
                    let mut q = self.q.lock().unwrap();
                    if q.abort {
                        (0, true)
                    } else {
                        let n = q.pcm.len().min(BLOCK);
                        for (i, s) in q.pcm.drain(..n).enumerate() {
                            block[i] = s;
                        }
                        let done = n == 0
                            && (q.feed != Feed::Streaming || millis() - q.last_push > STREAM_SILENT_MS);
                        if done {
                            q.feed = Feed::Idle;
                        }
                        (n, done)
                    }
                };
                if done {
                    break;
                }
                if n == 0 {
                    // Network jitter: keep the speaker fed with silence.
                    if had_data {
                        self.underruns.fetch_add(1, Ordering::Relaxed);
                        had_data = false;
                    }
                    block.fill(0);
                } else {
                    had_data = true;
                    let g = f32::from_bits(self.gain.load(Ordering::Relaxed));
                    for s in block[..n].iter_mut() {
                        *s = (*s as f32 * g) as i16;
                    }
                    block[n..].fill(0);
                }
                let mut written = 0usize;
                unsafe {
                    sys::i2s_channel_write(tx, block.as_ptr() as *const _, BLOCK * 2, &mut written, 200);
                }
            }
            // Let the DMA play out its last buffers (silence after them), then off.
            block.fill(0);
            for _ in 0..6 {
                let mut written = 0usize;
                unsafe {
                    sys::i2s_channel_write(tx, block.as_ptr() as *const _, BLOCK * 2, &mut written, 200);
                }
            }
            unsafe { sys::i2s_channel_disable(tx) };
            tca.write(exp::SPK_EN, false);
            self.playing.store(false, Ordering::Relaxed);
            self.wake.notify_all();
            let underruns = self.underruns.load(Ordering::Relaxed);
            let _ = events.send(SpeakerEvent::Playing { on: false, underruns }.into());
        }
    }
}

// ---------------------------------------------------------------- the robot's own phrases

/// Recorded with flite's kal16 voice (the stock robot's) by `tools/phrases.py`, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Phrase {
    Hello,
    AiOn,
    AiOff,
    NoPlane,
    LostPlane,
    NoSettings,
    Goodbye,
    BatteryCritical,
    ManualOverride,
    BackToAi,
    CameraFailure,
    MotorBoardFailure,
}

static PHRASES: &[u8] = include_bytes!("../assets/phrases.bin");

const STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];
const INDEX: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

impl Phrase {
    /// IMA ADPCM, 4 bits a sample. The file: u16 count, then per phrase u32 offset and u32
    /// samples; each phrase starts with its first sample (i16) and step index (u8, then a pad).
    fn decode(self) -> Vec<i16> {
        let p = PHRASES;
        let u32_at = |i: usize| u32::from_le_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]]) as usize;
        let count = u16::from_le_bytes([p[0], p[1]]) as usize;
        let i = self as usize;
        if i >= count {
            return Vec::new();
        }
        let off = u32_at(2 + i * 8);
        let samples = u32_at(2 + i * 8 + 4);
        let mut pred = i16::from_le_bytes([p[off], p[off + 1]]) as i32;
        let mut idx = p[off + 2] as i32;
        let data = &p[off + 4..];
        let mut out = Vec::with_capacity(samples);
        for n in 0..samples {
            let code = (data[n / 2] >> ((n % 2) * 4)) as i32 & 0xf;
            let step = STEPS[idx as usize];
            let mut delta = step >> 3;
            if code & 4 != 0 {
                delta += step;
            }
            if code & 2 != 0 {
                delta += step >> 1;
            }
            if code & 1 != 0 {
                delta += step >> 2;
            }
            pred = if code & 8 != 0 { pred - delta } else { pred + delta }.clamp(-32768, 32767);
            idx = (idx + INDEX[code as usize]).clamp(0, 88);
            out.push(pred as i16);
        }
        out
    }
}
