//! The music visualizer's ears: listens to whatever the PC is playing (a "loopback" of the
//! speakers, nothing is recorded or saved) and turns it into bar heights.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustfft::{num_complex::Complex, FftPlanner};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

/// Number of bars, from bass to treble.
pub const BANDS: usize = 48;
const FFT: usize = 2048;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::SeqCst);
}

/// Runs in the background; `on_frame` gets BANDS levels (0-255) about 30 times a second
/// while something plays, and one last all-zero frame when it goes quiet.
pub fn start(on_frame: impl Fn(Vec<u8>) + Send + 'static) {
    std::thread::spawn(move || loop {
        if ENABLED.load(Ordering::SeqCst) {
            let _ = listen(&on_frame);
        }
        std::thread::sleep(Duration::from_millis(500));
    });
}

/// Records `seconds` of whatever the PC is playing (not the microphone) as a WAV file, for
/// recognizing in-game music. Returns the WAV bytes and when the recording started (unix ms).
pub fn record_clip(seconds: u32) -> Option<(Vec<u8>, i64)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let client: IAudioClient = en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?.Activate(CLSCTX_ALL, None).ok()?;
        let fmt = client.GetMixFormat().ok()?;
        let (channels, rate, bits, tag) = ((*fmt).nChannels as usize, (*fmt).nSamplesPerSec, (*fmt).wBitsPerSample, (*fmt).wFormatTag);
        let float = tag == 3 || (tag == 0xFFFE && bits == 32);
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, fmt, None).ok()?;
        CoTaskMemFree(Some(fmt as *const _));
        let capture: IAudioCaptureClient = client.GetService().ok()?;
        let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        client.Start().ok()?;
        let want = (rate * seconds) as usize;
        let mut mono: Vec<i16> = Vec::with_capacity(want);
        let deadline = Instant::now() + Duration::from_secs(seconds as u64 + 2);
        while mono.len() < want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(15));
            while capture.GetNextPacketSize().ok()? > 0 {
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).ok()?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                for f in 0..frames as usize {
                    let mut s = 0.0f32;
                    if !silent {
                        for c in 0..channels {
                            let i = f * channels + c;
                            s += if float { *(data as *const f32).add(i) } else if bits == 16 { *(data as *const i16).add(i) as f32 / 32768.0 } else { 0.0 };
                        }
                        s /= channels as f32;
                    }
                    mono.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                }
                capture.ReleaseBuffer(frames).ok()?;
            }
        }
        let _ = client.Stop();
        if mono.len() < rate as usize * 3 {
            return None; // (almost) nothing played
        }
        // a plain 16-bit mono WAV file
        let data_len = (mono.len() * 2) as u32;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend(b"RIFF");
        wav.extend((36 + data_len).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(rate.to_le_bytes());
        wav.extend((rate * 2).to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(data_len.to_le_bytes());
        for s in mono {
            wav.extend(s.to_le_bytes());
        }
        Some((wav, started))
    }
}

fn listen(on_frame: &dyn Fn(Vec<u8>)) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let device_id = device.GetId()?.to_string().unwrap_or_default();
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let fmt = client.GetMixFormat()?;
        let (channels, rate, bits, tag) = ((*fmt).nChannels as usize, (*fmt).nSamplesPerSec as f32, (*fmt).wBitsPerSample, (*fmt).wFormatTag);
        let float = tag == 3 || (tag == 0xFFFE && bits == 32);
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, fmt, None)?;
        CoTaskMemFree(Some(fmt as *const _));
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;

        let fft = FftPlanner::<f32>::new().plan_fft_forward(FFT);
        let window: Vec<f32> = (0..FFT).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT as f32).cos()).collect();
        // log-spaced band edges, 35 Hz to 16 kHz, as FFT bin numbers
        let edges: Vec<usize> = (0..=BANDS)
            .map(|b| (35.0 * (16000.0f32 / 35.0).powf(b as f32 / BANDS as f32) / (rate / FFT as f32)) as usize)
            .collect();
        // circular buffer of the latest samples; `pos` is where the next one goes
        let mut ring = vec![0f32; FFT];
        let mut pos = 0usize;
        let mut levels = [0f32; BANDS];
        let mut quiet_sent = true;
        let (mut last_frame, mut last_check) = (Instant::now(), Instant::now());

        loop {
            std::thread::sleep(Duration::from_millis(10));
            if !ENABLED.load(Ordering::SeqCst) {
                break;
            }
            let mut got_sound = false;
            loop {
                let n = capture.GetNextPacketSize()?;
                if n == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                for f in 0..frames as usize {
                    let mut s = 0.0;
                    if !silent {
                        for c in 0..channels {
                            let i = f * channels + c;
                            s += if float { *(data as *const f32).add(i) } else if bits == 16 { *(data as *const i16).add(i) as f32 / 32768.0 } else { 0.0 };
                        }
                        s /= channels as f32;
                    }
                    ring[pos] = s;
                    pos = (pos + 1) % FFT;
                }
                got_sound |= !silent && frames > 0;
                capture.ReleaseBuffer(frames)?;
            }

            if last_frame.elapsed() >= Duration::from_millis(33) {
                last_frame = Instant::now();
                let mut buf: Vec<Complex<f32>> = (0..FFT).map(|i| Complex::new(ring[(pos + i) % FFT] * window[i], 0.0)).collect();
                fft.process(&mut buf);
                let mut any = false;
                for b in 0..BANDS {
                    let (lo, hi) = (edges[b].max(1), edges[b + 1].max(edges[b].max(1) + 1).min(FFT / 2));
                    let peak = buf[lo..hi].iter().map(|c| c.norm()).fold(0.0, f32::max) / (FFT as f32 / 4.0);
                    // -60 dB .. -6 dB mapped onto 0..1, treble gets a small boost so it isn't flat
                    let db = 20.0 * peak.max(1e-9).log10() + 4.0 * (b as f32 / BANDS as f32);
                    let target = ((db + 60.0) / 54.0).clamp(0.0, 1.0);
                    let l = &mut levels[b];
                    *l = if target > *l { *l + (target - *l) * 0.65 } else { *l * 0.82 };
                    any |= *l > 0.02;
                }
                if !got_sound && !any {
                    // nothing playing: let the ring empty out so old sound doesn't linger
                    ring.iter_mut().for_each(|s| *s *= 0.5);
                }
                if any || !quiet_sent {
                    on_frame(levels.iter().map(|l| (l * 255.0) as u8).collect());
                    quiet_sent = !any;
                }
            }

            // follow you when you switch speakers / headphones
            if last_check.elapsed() >= Duration::from_secs(2) {
                last_check = Instant::now();
                let now = en.GetDefaultAudioEndpoint(eRender, eConsole)?.GetId()?.to_string().unwrap_or_default();
                if now != device_id {
                    break;
                }
            }
        }
        let _ = client.Stop();
        Ok(())
    }
}
