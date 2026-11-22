//! sac — MDCT audio codec with a CLI.
//!
//!   sac [PATH] [-q 1..9] [--encode] [--play] [--dry]
//!
//! * PATH = any audio file (mp3/flac/wav/ogg/m4a) -> compresses to "<PATH>.sac"
//!   (first run), plays "<PATH>.sac" by streaming decode (every run after).
//! * PATH = a .sac file -> plays it.
//! * No PATH -> asks for one.
//!
//! Codec: orthonormal MDCT (N=1024, sine window) -> per-bin quantizer
//! -> context-modelled adaptive binary range coder. Stereo is coded as M/S.
//! Playback decodes frame by frame into a small ring buffer; nothing decoded
//! is ever written to disk, so only the .sac remains afterwards.

#![cfg_attr(not(feature = "play"), allow(unused))]

use std::error::Error;
use std::f32::consts::FRAC_1_SQRT_2;
use std::f64::consts::PI as PI64;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

const MAGIC: &[u8; 4] = b"SAC1";
const HEADER_LEN: usize = 22;
const N: usize = 1024; // hop size = coefficients per frame
const LOG2_N: u8 = 10;
const BAND: usize = 16; // "last significant band" granularity
const ESC: usize = 15; // unary magnitude bins before escape
const NCTX: usize = 12;
const NPOS: usize = 5;
const RND: f32 = 0.4; // quantizer rounding offset (<0.5 = dead zone)

// ───────────────────────────── MDCT ─────────────────────────────

struct Mdct {
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    pre: Vec<Complex32>,
    post: Vec<Complex32>,
    ipre: Vec<Complex32>,
    ipost: Vec<Complex32>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    scale: f32,
}

impl Mdct {
    fn new() -> Self {
        let n2 = 2 * N;
        let nf = N as f64;
        let n0 = 0.5 + nf / 2.0;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n2);
        let ifft = planner.plan_fft_inverse(n2);
        let cis = |a: f64| Complex32::new(a.cos() as f32, a.sin() as f32);
        let scratch_len = fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len());
        Mdct {
            window: (0..n2).map(|n| (PI64 * (n as f64 + 0.5) / n2 as f64).sin() as f32).collect(),
            pre: (0..n2).map(|n| cis(-PI64 * n as f64 / n2 as f64)).collect(),
            post: (0..N).map(|k| cis(-PI64 * n0 * (k as f64 + 0.5) / nf)).collect(),
            ipre: (0..N).map(|k| cis(PI64 * n0 * k as f64 / nf)).collect(),
            ipost: (0..n2).map(|n| cis(PI64 * (n as f64 + n0) / n2 as f64)).collect(),
            buf: vec![Complex32::new(0.0, 0.0); n2],
            scratch: vec![Complex32::new(0.0, 0.0); scratch_len],
            scale: (2.0 / nf).sqrt() as f32,
            fft,
            ifft,
        }
    }

    /// 2N time samples -> N coefficients (windowed, orthonormal).
    fn forward(&mut self, x: &[f32], out: &mut [f32]) {
        for n in 0..2 * N {
            self.buf[n] = self.pre[n] * (x[n] * self.window[n]);
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        for k in 0..N {
            out[k] = (self.buf[k] * self.post[k]).re * self.scale;
        }
    }

    /// N coefficients -> 2N windowed time samples (overlap-add them).
    fn inverse(&mut self, c: &[f32], out: &mut [f32]) {
        for k in 0..N {
            self.buf[k] = self.ipre[k] * c[k];
        }
        for k in N..2 * N {
            self.buf[k] = Complex32::new(0.0, 0.0);
        }
        self.ifft.process_with_scratch(&mut self.buf, &mut self.scratch);
        for n in 0..2 * N {
            out[n] = (self.buf[n] * self.ipost[n]).re * self.scale * self.window[n];
        }
    }
}

fn step_table(base: f32) -> Vec<f32> {
    // coarser steps toward high frequencies
    (0..N).map(|m| base * (1.0 + 2.0 * m as f32 / N as f32)).collect()
}

fn base_step(quality: u32) -> f32 {
    let q = quality.clamp(1, 9) as f32;
    0.0004 * 2f32.powf((9.0 - q) * 0.75)
}

// ─────────────────────── adaptive binary range coder ───────────────────────

#[derive(Clone, Copy)]
struct Prob {
    a: u16, // fast estimate of P(bit=0), 16-bit
    b: u16, // slow estimate
}

impl Prob {
    const INIT: Prob = Prob { a: 32768, b: 32768 };
    #[inline]
    fn p(&self) -> u32 {
        ((self.a as u32 + self.b as u32) >> 5).clamp(1, 4095)
    }
    #[inline]
    fn update(&mut self, bit: u32) {
        if bit == 0 {
            self.a += (65535 - self.a) >> 4;
            self.b += (65535 - self.b) >> 7;
        } else {
            self.a -= self.a >> 4;
            self.b -= self.b >> 7;
        }
    }
}

trait Coder {
    const ENC: bool;
    /// Encoder: codes `b` and returns it. Decoder: ignores `b`, returns the decoded bit.
    fn bit(&mut self, p: &mut Prob, b: u32) -> u32;
    /// Same, for an equiprobable bit.
    fn raw(&mut self, b: u32) -> u32;
}

struct RcEnc {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
}

impl RcEnc {
    fn new() -> Self {
        RcEnc { low: 0, range: 0xFFFF_FFFF, cache: 0, cache_size: 1, out: Vec::new() }
    }
    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low >> 24) & 0xFF) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low & 0x00FF_FFFF) << 8;
    }
    fn norm(&mut self) {
        while self.range < (1 << 24) {
            self.range <<= 8;
            self.shift_low();
        }
    }
    fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

impl Coder for RcEnc {
    const ENC: bool = true;
    fn bit(&mut self, p: &mut Prob, b: u32) -> u32 {
        let bound = (self.range >> 12) * p.p();
        if b == 0 {
            self.range = bound;
        } else {
            self.low += bound as u64;
            self.range -= bound;
        }
        p.update(b);
        self.norm();
        b
    }
    fn raw(&mut self, b: u32) -> u32 {
        self.range >>= 1;
        if b != 0 {
            self.low += self.range as u64;
        }
        self.norm();
        b
    }
}

struct RcDec<R: Read> {
    r: R,
    code: u32,
    range: u32,
    over: u32, // bytes requested past EOF (truncation detector)
}

impl<R: Read> RcDec<R> {
    fn new(mut r: R) -> io::Result<Self> {
        let mut b = [0u8; 5];
        r.read_exact(&mut b)?;
        Ok(RcDec { r, code: u32::from_be_bytes([b[1], b[2], b[3], b[4]]), range: 0xFFFF_FFFF, over: 0 })
    }
    fn byte(&mut self) -> u8 {
        let mut b = [0u8; 1];
        match self.r.read(&mut b) {
            Ok(1) => b[0],
            _ => {
                self.over += 1;
                0
            }
        }
    }
    fn norm(&mut self) {
        while self.range < (1 << 24) {
            self.range <<= 8;
            self.code = (self.code << 8) | self.byte() as u32;
        }
    }
}

impl<R: Read> Coder for RcDec<R> {
    const ENC: bool = false;
    fn bit(&mut self, p: &mut Prob, _b: u32) -> u32 {
        let bound = (self.range >> 12) * p.p();
        let bit = if self.code < bound {
            self.range = bound;
            0
        } else {
            self.code -= bound;
            self.range -= bound;
            1
        };
        p.update(bit);
        self.norm();
        bit
    }
    fn raw(&mut self, _b: u32) -> u32 {
        self.range >>= 1;
        let bit = if self.code >= self.range {
            self.code -= self.range;
            1
        } else {
            0
        };
        self.norm();
        bit
    }
}

// ─────────────────────── coefficient modelling (shared enc/dec) ───────────────────────

struct Models {
    nb: Vec<[Prob; 128]>,
    mag: Vec<[Prob; ESC]>,
    esc: Vec<[Prob; 32]>,
}

impl Models {
    fn new(ch: usize) -> Self {
        Models {
            nb: vec![[Prob::INIT; 128]; ch],
            mag: vec![[Prob::INIT; ESC]; ch * NPOS * NCTX],
            esc: vec![[Prob::INIT; 32]; ch],
        }
    }
}

fn ctx_of(s: u32) -> usize {
    match s {
        0..=4 => s as usize,
        5..=6 => 5,
        7..=8 => 6,
        9..=11 => 7,
        12..=15 => 8,
        16..=22 => 9,
        23..=35 => 10,
        _ => 11,
    }
}

fn pos_of(k: usize) -> usize {
    match k {
        0..=3 => 0,
        4..=31 => 1,
        32..=127 => 2,
        128..=383 => 3,
        _ => 4,
    }
}

/// Codes (encoder) or reconstructs (decoder) one channel-frame of quantized ints.
/// `prev[k+1]` holds |q| of the previous frame at bin k (len N+2, zero padded).
fn code_frame<C: Coder>(c: &mut C, m: &mut Models, ch: usize, q: &mut [i32], prev: &mut [u32]) {
    // number of active 16-bin bands, as a 7-bit tree
    let mut nb = 0usize;
    if C::ENC {
        nb = q.iter().rposition(|&v| v != 0).map_or(0, |l| l / BAND + 1);
    }
    let mut node = 1usize;
    for i in (0..7).rev() {
        let b = c.bit(&mut m.nb[ch][node], ((nb >> i) & 1) as u32);
        node = node * 2 + b as usize;
    }
    let end = ((node - 128) * BAND).min(N);

    let (mut a1, mut a2) = (0u32, 0u32);
    for k in 0..end {
        let s = 2 * a1 + a2 + 2 * prev[k + 1] + prev[k] + prev[k + 2];
        let probs = &mut m.mag[(ch * NPOS + pos_of(k)) * NCTX + ctx_of(s)];
        let abs_in = if C::ENC { q[k].unsigned_abs() } else { 0 };

        let mut mag = 0u32;
        while (mag as usize) < ESC {
            if c.bit(&mut probs[mag as usize], (abs_in > mag) as u32) == 0 {
                break;
            }
            mag += 1;
        }
        if mag as usize == ESC {
            // Exp-Golomb escape for the remainder
            let v1 = if C::ENC { abs_in - ESC as u32 + 1 } else { 1 };
            let e_in = 31 - v1.leading_zeros();
            let esc = &mut m.esc[ch];
            let mut e = 0u32;
            while e < 31 {
                if c.bit(&mut esc[e as usize], (e_in > e) as u32) == 0 {
                    break;
                }
                e += 1;
            }
            let mut acc = 1u32;
            for j in (0..e).rev() {
                acc = (acc << 1) | c.raw((v1 >> j) & 1);
            }
            mag = ESC as u32 + acc - 1;
        }
        let mut val = mag as i32;
        if mag > 0 && c.raw((q[k] < 0) as u32) == 1 {
            val = -val;
        }
        q[k] = val;
        a2 = a1;
        a1 = mag.min(255);
    }
    for v in &mut q[end..] {
        *v = 0;
    }
    for k in 0..N {
        prev[k + 1] = q[k].unsigned_abs().min(255);
    }
}

// ───────────────────────────── encoder ─────────────────────────────

fn encode(pcm: &[f32], sr: u32, ch: usize, base: f32) -> Vec<u8> {
    let len = pcm.len() / ch;
    let mut chans: Vec<Vec<f32>> = (0..ch).map(|c| pcm.iter().skip(c).step_by(ch).copied().collect()).collect();
    if ch == 2 {
        for i in 0..len {
            let (l, r) = (chans[0][i], chans[1][i]);
            chans[0][i] = (l + r) * FRAC_1_SQRT_2;
            chans[1][i] = (l - r) * FRAC_1_SQRT_2;
        }
    }
    let frames = (len + N - 1) / N + 1;
    let mut mdct = Mdct::new();
    let steps = step_table(base);
    let mut models = Models::new(ch);
    let mut rc = RcEnc::new();
    let mut prev = vec![vec![0u32; N + 2]; ch];
    let mut blk = vec![0f32; 2 * N];
    let mut coef = vec![0f32; N];
    let mut q = vec![0i32; N];

    for k in 0..frames {
        let start = k as isize * N as isize - N as isize;
        for c in 0..ch {
            for n in 0..2 * N {
                let i = start + n as isize;
                blk[n] = if i >= 0 && (i as usize) < len { chans[c][i as usize] } else { 0.0 };
            }
            mdct.forward(&blk, &mut coef);
            for m in 0..N {
                let v = (coef[m].abs() / steps[m] + RND).floor() as i32;
                q[m] = if coef[m] < 0.0 { -v } else { v };
            }
            code_frame(&mut rc, &mut models, c, &mut q, &mut prev[c]);
        }
    }

    let mut out = Vec::with_capacity(HEADER_LEN + rc.out.len() + 8);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&sr.to_le_bytes());
    out.push(ch as u8);
    out.push(LOG2_N);
    out.extend_from_slice(&base.to_le_bytes());
    out.extend_from_slice(&(len as u64).to_le_bytes());
    out.extend_from_slice(&rc.finish());
    out
}

// ───────────────────────── streaming decoder ─────────────────────────

struct Decoder<R: Read> {
    rc: RcDec<R>,
    sr: u32,
    ch: usize,
    len: u64,
    emitted: u64,
    k: u64,
    mdct: Mdct,
    steps: Vec<f32>,
    models: Models,
    prev: Vec<Vec<u32>>,
    overlap: Vec<Vec<f32>>,
    pcm: Vec<Vec<f32>>,
    q: Vec<i32>,
    coef: Vec<f32>,
    y: Vec<f32>,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl<R: Read> Decoder<R> {
    fn open(mut r: R) -> io::Result<Self> {
        let mut h = [0u8; HEADER_LEN];
        r.read_exact(&mut h)?;
        if &h[0..4] != MAGIC {
            return Err(bad("not a SAC1 file"));
        }
        let sr = u32::from_le_bytes(h[4..8].try_into().unwrap());
        let ch = h[8] as usize;
        let base = f32::from_le_bytes(h[10..14].try_into().unwrap());
        let len = u64::from_le_bytes(h[14..22].try_into().unwrap());
        if h[9] != LOG2_N || !(1..=2).contains(&ch) || sr == 0 || !base.is_finite() || base <= 0.0 {
            return Err(bad("unsupported or corrupt header"));
        }
        Ok(Decoder {
            rc: RcDec::new(r)?,
            sr,
            ch,
            len,
            emitted: 0,
            k: 0,
            mdct: Mdct::new(),
            steps: step_table(base),
            models: Models::new(ch),
            prev: vec![vec![0u32; N + 2]; ch],
            overlap: vec![vec![0f32; N]; ch],
            pcm: vec![vec![0f32; N]; ch],
            q: vec![0i32; N],
            coef: vec![0f32; N],
            y: vec![0f32; 2 * N],
        })
    }

    /// Next block of interleaved f32 PCM (<= N frames), or None at the end.
    fn next_block(&mut self) -> io::Result<Option<Vec<f32>>> {
        loop {
            if self.emitted >= self.len {
                return Ok(None);
            }
            for c in 0..self.ch {
                code_frame(&mut self.rc, &mut self.models, c, &mut self.q, &mut self.prev[c]);
                for m in 0..N {
                    self.coef[m] = self.q[m] as f32 * self.steps[m];
                }
                self.mdct.inverse(&self.coef, &mut self.y);
                for n in 0..N {
                    self.pcm[c][n] = self.overlap[c][n] + self.y[n];
                    self.overlap[c][n] = self.y[N + n];
                }
            }
            if self.rc.over > 8 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated .sac file"));
            }
            self.k += 1;
            if self.k == 1 {
                continue; // first frame only produces the leading pad region
            }
            let take = ((self.len - self.emitted) as usize).min(N);
            let mut out = Vec::with_capacity(take * self.ch);
            for n in 0..take {
                if self.ch == 2 {
                    let (m, s) = (self.pcm[0][n], self.pcm[1][n]);
                    out.push(((m + s) * FRAC_1_SQRT_2).clamp(-1.0, 1.0));
                    out.push(((m - s) * FRAC_1_SQRT_2).clamp(-1.0, 1.0));
                } else {
                    out.push(self.pcm[0][n].clamp(-1.0, 1.0));
                }
            }
            self.emitted += take as u64;
            return Ok(Some(out));
        }
    }
}

// ───────────────────────── input loading (symphonia) ─────────────────────────

fn load_audio(path: &Path) -> Result<(u32, usize, Vec<f32>), Box<dyn Error>> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
    use symphonia::core::errors::Error as SymErr;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let mss = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("no audio track found")?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let (mut sr, mut ch) = (0u32, 0usize);
    let mut out: Vec<f32> = Vec::new();
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymErr::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(SymErr::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = *buf.spec();
                sr = spec.rate;
                ch = spec.channels.count();
                let need = buf.capacity() * ch;
                if sbuf.as_ref().map_or(true, |s| s.capacity() < need) {
                    sbuf = Some(SampleBuffer::new(buf.capacity() as u64, spec));
                }
                let sb = sbuf.as_mut().unwrap();
                sb.copy_interleaved_ref(buf);
                out.extend_from_slice(sb.samples());
            }
            Err(SymErr::DecodeError(_)) => continue, // skip a bad packet
            Err(e) => return Err(e.into()),
        }
    }
    if out.is_empty() || sr == 0 {
        return Err("no audio samples decoded".into());
    }
    if !(1..=2).contains(&ch) {
        return Err(format!("{ch}-channel audio not supported (mono/stereo only)").into());
    }
    Ok((sr, ch, out))
}

// ───────────────────────────── CLI actions ─────────────────────────────

fn encode_file(src: &Path, dst: &Path, quality: u32) -> Result<(), Box<dyn Error>> {
    let t = Instant::now();
    let (sr, ch, pcm) = load_audio(src)?;
    let bytes = encode(&pcm, sr, ch, base_step(quality));
    let tmp = PathBuf::from(format!("{}.part", dst.display()));
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, dst)?;

    let secs = (pcm.len() / ch) as f64 / sr as f64;
    let pcm16 = (pcm.len() * 2) as f64;
    let orig = fs::metadata(src)?.len() as f64;
    println!("compressed  {}", src.display());
    println!("        ->  {}", dst.display());
    println!(
        "  {:.1}s, {} Hz, {}ch | {:.2} MB -> {:.2} MB | {:.1}x vs 16-bit PCM, {:.1}x vs source file | {:.0} kbps | q{} | {:.1}s",
        secs,
        sr,
        ch,
        orig / 1e6,
        bytes.len() as f64 / 1e6,
        pcm16 / bytes.len() as f64,
        orig / bytes.len() as f64,
        bytes.len() as f64 * 8.0 / secs / 1000.0,
        quality,
        t.elapsed().as_secs_f32()
    );
    Ok(())
}

fn dry_decode(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut dec = Decoder::open(BufReader::new(File::open(path)?))?;
    let t = Instant::now();
    let mut frames = 0u64;
    while let Some(b) = dec.next_block()? {
        frames += (b.len() / dec.ch) as u64;
    }
    let audio = frames as f64 / dec.sr as f64;
    let took = t.elapsed().as_secs_f64();
    println!("decoded {:.1}s of audio in {:.2}s ({:.0}x realtime), nothing written to disk", audio, took, audio / took);
    Ok(())
}

fn fmt_time(s: f64) -> String {
    let s = s as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn map_channels(x: &[f32], in_ch: usize, out_ch: usize) -> Vec<f32> {
    if in_ch == out_ch {
        return x.to_vec();
    }
    let mut out = Vec::with_capacity(x.len() / in_ch * out_ch);
    for f in x.chunks_exact(in_ch) {
        for o in 0..out_ch {
            out.push(if in_ch > out_ch { f.iter().sum::<f32>() / in_ch as f32 } else { f[o.min(in_ch - 1)] });
        }
    }
    out
}

/// Streaming linear resampler (only used if the device can't run at the file's rate).
struct Resampler {
    step: f64,
    pos: f64,
    last: Vec<f32>,
    ch: usize,
    bypass: bool,
}

impl Resampler {
    fn new(in_sr: u32, out_sr: u32, ch: usize) -> Self {
        Resampler { step: in_sr as f64 / out_sr as f64, pos: 0.0, last: Vec::new(), ch, bypass: in_sr == out_sr }
    }
    fn process(&mut self, x: &[f32]) -> Vec<f32> {
        if self.bypass {
            return x.to_vec();
        }
        let ch = self.ch;
        let n = x.len() / ch;
        if n == 0 {
            return Vec::new();
        }
        if self.last.is_empty() {
            self.last = x[..ch].to_vec();
        }
        let last = self.last.clone();
        let get = |i: usize, c: usize| if i == 0 { last[c] } else { x[(i - 1) * ch + c] };
        let mut out = Vec::new();
        while self.pos < n as f64 {
            let i = self.pos as usize;
            let f = (self.pos - i as f64) as f32;
            for c in 0..ch {
                out.push(get(i, c) * (1.0 - f) + get(i + 1, c) * f);
            }
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.last = x[(n - 1) * ch..n * ch].to_vec();
        out
    }
}

#[cfg(feature = "play")]
struct Feed {
    rx: Receiver<Vec<f32>>,
    cur: Vec<f32>,
    pos: usize,
    done: Arc<AtomicBool>,
}

#[cfg(feature = "play")]
impl Feed {
    fn next(&mut self) -> f32 {
        loop {
            if self.pos < self.cur.len() {
                let v = self.cur[self.pos];
                self.pos += 1;
                return v;
            }
            match self.rx.try_recv() {
                Ok(b) => {
                    self.cur = b;
                    self.pos = 0;
                }
                Err(TryRecvError::Empty) => return 0.0, // underrun -> silence
                Err(TryRecvError::Disconnected) => {
                    self.done.store(true, Ordering::Relaxed);
                    return 0.0;
                }
            }
        }
    }
}

#[cfg(feature = "play")]
fn build_stream<T>(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    mut feed: Feed,
    played: Arc<AtomicU64>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    device.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            for s in data.iter_mut() {
                *s = T::from_sample(feed.next());
            }
            played.fetch_add(data.len() as u64, Ordering::Relaxed);
        },
        |e| eprintln!("audio error: {e}"),
        None,
    )
}

/// Streams the .sac to the sound card. Decoded PCM only ever exists in a small
/// in-memory buffer, so when playback ends nothing but the .sac is left on disk.
#[cfg(feature = "play")]
fn play(path: &Path) -> Result<(), Box<dyn Error>> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{SampleFormat, SampleRate};

    let dec = Decoder::open(BufReader::new(File::open(path)?))?;
    let (in_sr, in_ch, len) = (dec.sr, dec.ch, dec.len);

    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no audio output device found")?;
    let mut pick: Option<cpal::SupportedStreamConfigRange> = None;
    for cfg in device.supported_output_configs()? {
        if cfg.min_sample_rate().0 <= in_sr && in_sr <= cfg.max_sample_rate().0 {
            let better = match &pick {
                None => true,
                Some(p) => cfg.channels() as usize == in_ch && p.channels() as usize != in_ch,
            };
            if better {
                pick = Some(cfg);
            }
        }
    }
    let supported = match pick {
        Some(c) => c.with_sample_rate(SampleRate(in_sr)),
        None => device.default_output_config()?,
    };
    let (out_sr, out_ch, fmt) = (supported.sample_rate().0, supported.channels() as usize, supported.sample_format());
    let config: cpal::StreamConfig = supported.into();

    let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(16);
    let producer = std::thread::spawn(move || -> io::Result<()> {
        let mut dec = dec;
        let mut rs = Resampler::new(in_sr, out_sr, out_ch);
        while let Some(block) = dec.next_block()? {
            let out = rs.process(&map_channels(&block, in_ch, out_ch));
            if tx.send(out).is_err() {
                break;
            }
        }
        Ok(())
    });

    let done = Arc::new(AtomicBool::new(false));
    let played = Arc::new(AtomicU64::new(0));
    let feed = Feed { rx, cur: Vec::new(), pos: 0, done: done.clone() };
    let stream = match fmt {
        SampleFormat::F32 => build_stream::<f32>(&device, &config, feed, played.clone())?,
        SampleFormat::I16 => build_stream::<i16>(&device, &config, feed, played.clone())?,
        SampleFormat::U16 => build_stream::<u16>(&device, &config, feed, played.clone())?,
        SampleFormat::I32 => build_stream::<i32>(&device, &config, feed, played.clone())?,
        other => return Err(format!("unsupported output sample format {other:?}").into()),
    };

    let total = len as f64 / in_sr as f64;
    println!("playing {}  ({} Hz, {}ch, {})  Ctrl-C to stop", path.display(), in_sr, in_ch, fmt_time(total));
    std::thread::sleep(Duration::from_millis(150)); // let the buffer prefill
    stream.play()?;
    while !done.load(Ordering::Relaxed) {
        let t = (played.load(Ordering::Relaxed) as f64 / (out_ch as f64 * out_sr as f64)).min(total);
        print!("\r  {} / {}", fmt_time(t), fmt_time(total));
        io::stdout().flush().ok();
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(400)); // drain device buffer
    drop(stream);
    println!("\r  {} / {}  done", fmt_time(total), fmt_time(total));
    producer.join().map_err(|_| "decoder thread panicked")??;
    Ok(())
}

#[cfg(not(feature = "play"))]
fn play(_: &Path) -> Result<(), Box<dyn Error>> {
    Err("built without audio output (feature \"play\"); use --dry or rebuild with default features".into())
}

const HELP: &str = "sac [PATH] [-q 1..9] [--encode] [--play] [--dry]\n\
  PATH       audio file (mp3/flac/wav/ogg/m4a) or a .sac file; asked for if omitted\n\
  -q N       quality 1 (smallest) .. 9 (best), default 5\n\
  --encode   force re-compress even if PATH.sac exists\n\
  --play     also play right after compressing\n\
  --dry      decode without audio output (speed test)\n\
First run on a file compresses it to PATH.sac; later runs stream-play the .sac.";

fn run() -> Result<(), Box<dyn Error>> {
    let (mut quality, mut force, mut play_after, mut dry) = (5u32, false, false, false);
    let mut path_arg: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-q" | "--quality" => quality = args.next().and_then(|v| v.parse().ok()).ok_or("-q needs a number 1-9")?,
            "--encode" | "-e" => force = true,
            "--play" | "-p" => play_after = true,
            "--dry" => dry = true,
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            _ => path_arg = Some(a),
        }
    }
    let raw = match path_arg {
        Some(p) => p,
        None => {
            print!("Audio path: ");
            io::stdout().flush()?;
            let mut s = String::new();
            io::stdin().read_line(&mut s)?;
            s
        }
    };
    let path = PathBuf::from(raw.trim().trim_matches(|c| c == '"' || c == '\''));
    if !path.is_file() {
        return Err(format!("not a file: {}", path.display()).into());
    }

    let is_sac = path.extension().map_or(false, |e| e.eq_ignore_ascii_case("sac"));
    let sac = if is_sac {
        path.clone()
    } else {
        let mut n = path.as_os_str().to_owned();
        n.push(".sac");
        PathBuf::from(n)
    };

    let mut just_encoded = false;
    if !is_sac {
        let stale = match (fs::metadata(&path).and_then(|m| m.modified()), fs::metadata(&sac).and_then(|m| m.modified())) {
            (Ok(s), Ok(d)) => s > d,
            _ => false,
        };
        if force || stale || !sac.exists() {
            encode_file(&path, &sac, quality)?;
            just_encoded = true;
        }
    }
    if just_encoded && !play_after {
        println!("done. run again on the same path to play it.");
        return Ok(());
    }
    if dry {
        dry_decode(&sac)
    } else {
        play(&sac)
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use std::io::Cursor;

    fn synth(sr: u32, secs: f32) -> Vec<f32> {
        let n = (sr as f32 * secs) as usize;
        let mut seed = 12345u32;
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let env = 0.5 + 0.5 * (2.0 * PI * 0.5 * t).sin();
            let tone = 0.3 * (2.0 * PI * 220.0 * t).sin()
                + 0.2 * (2.0 * PI * 440.0 * t).sin() * env
                + 0.1 * (2.0 * PI * 3000.0 * t).sin();
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = ((seed >> 16) as f32 / 32768.0 - 1.0) * 0.01;
            out.push(tone + noise);
            out.push(0.8 * tone - noise);
        }
        out
    }

    fn snr_db(a: &[f32], b: &[f32]) -> f64 {
        let (mut s, mut e) = (0f64, 0f64);
        for (x, y) in a.iter().zip(b) {
            s += (*x as f64).powi(2);
            e += ((*x - *y) as f64).powi(2);
        }
        10.0 * (s / e.max(1e-30)).log10()
    }

    fn roundtrip(pcm: &[f32], sr: u32, ch: usize, base: f32) -> (Vec<f32>, usize) {
        let bytes = encode(pcm, sr, ch, base);
        let mut dec = Decoder::open(Cursor::new(&bytes)).unwrap();
        let mut out = Vec::new();
        while let Some(b) = dec.next_block().unwrap() {
            out.extend(b);
        }
        (out, bytes.len())
    }

    #[test]
    fn mdct_perfect_reconstruction() {
        let mut m = Mdct::new();
        let len = N * 6;
        let mut seed = 1u32;
        let x: Vec<f32> = (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 16) as f32 / 32768.0 - 1.0
            })
            .collect();
        let mut out = vec![0f32; len + 2 * N];
        let (mut c, mut y) = (vec![0f32; N], vec![0f32; 2 * N]);
        for k in 0..(len / N + 1) {
            let mut blk = vec![0f32; 2 * N];
            for n in 0..2 * N {
                let i = (k * N + n) as isize - N as isize;
                if i >= 0 && (i as usize) < len {
                    blk[n] = x[i as usize];
                }
            }
            m.forward(&blk, &mut c);
            m.inverse(&c, &mut y);
            for n in 0..2 * N {
                out[k * N + n] += y[n];
            }
        }
        // frame k covers [(k-1)N,(k+1)N) -> out index shifted by N
        let rec = &out[N..N + len];
        assert!(snr_db(&x, rec) > 100.0, "snr {}", snr_db(&x, rec));
    }

    #[test]
    fn roundtrip_stereo_quality() {
        let pcm = synth(44100, 3.0);
        let (hi, hi_bytes) = roundtrip(&pcm, 44100, 2, base_step(9));
        let (mid, mid_bytes) = roundtrip(&pcm, 44100, 2, base_step(5));
        assert_eq!(hi.len(), pcm.len());
        assert_eq!(mid.len(), pcm.len());
        let (s9, s5) = (snr_db(&pcm, &hi), snr_db(&pcm, &mid));
        println!("q9: {:.1} dB, {:.1}x | q5: {:.1} dB, {:.1}x", s9, pcm.len() as f64 * 2.0 / hi_bytes as f64, s5, pcm.len() as f64 * 2.0 / mid_bytes as f64);
        assert!(s9 > 45.0 && s5 > 28.0 && s9 > s5);
        assert!(mid_bytes < hi_bytes);
    }

    #[test]
    fn roundtrip_mono_loud_escape_path() {
        // loud + very fine step forces large magnitudes through the escape coder
        let pcm: Vec<f32> = synth(48000, 1.0).chunks(2).map(|f| f[0] * 3.0).collect();
        let (out, _) = roundtrip(&pcm, 48000, 1, 0.00002);
        assert_eq!(out.len(), pcm.len());
        let clipped: Vec<f32> = pcm.iter().map(|v| v.clamp(-1.0, 1.0)).collect();
        assert!(snr_db(&clipped, &out) > 60.0, "snr {}", snr_db(&clipped, &out));
    }

    #[test]
    fn odd_length_and_truncation() {
        let pcm: Vec<f32> = synth(44100, 0.5).into_iter().take(2 * 12345).collect();
        let (out, _) = roundtrip(&pcm, 44100, 2, base_step(7));
        assert_eq!(out.len(), pcm.len());
        let bytes = encode(&pcm, 44100, 2, base_step(7));
        let cut = &bytes[..bytes.len() / 2];
        let mut dec = Decoder::open(Cursor::new(cut)).unwrap();
        let mut err = false;
        while let Ok(Some(_)) = dec.next_block().map_err(|_| err = true) {}
        assert!(err, "truncated file must error, not loop or panic");
    }

    #[test]
    fn resampler_length() {
        let mut r = Resampler::new(44100, 48000, 2);
        let x = vec![0.5f32; 2 * 1024];
        let mut total = 0;
        for _ in 0..100 {
            total += r.process(&x).len() / 2;
        }
        let want = 100.0 * 1024.0 * 48000.0 / 44100.0;
        assert!((total as f64 - want).abs() < 4.0, "{total} vs {want}");
    }
}