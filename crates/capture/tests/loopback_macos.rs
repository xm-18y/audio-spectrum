//! macOS Loopback integration test: real ScreenCaptureKit capture.
//!
//! Plays a 5 s sine through the system output (`afplay`), opens the Loopback
//! Source and asserts that non-silent samples actually arrive. Requires a
//! working audio output device; the 5 s wav bounds the test even if a panic
//! skips cleanup.

#![cfg(target_os = "macos")]

use audio_spectrum_capture::{CaptureSystem, SourceKind};
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn loopback_delivers_samples_while_system_plays() {
    // CI runners have no audio hardware (and no audible output device at
    // all) — this test is only meaningful on a real machine.
    if std::env::var_os("CI").is_some() {
        eprintln!("skipping: CI runner has no audio output");
        return;
    }

    let wav_path = "/tmp/audio-spectrum-loopback-test.wav";
    std::fs::write(wav_path, sine_wav(440.0, 0.5, 5.0)).expect("write test wav");

    let mut player = Command::new("afplay")
        .arg(wav_path)
        .spawn()
        .expect("afplay is available on macOS");

    let system = CaptureSystem::new();
    let devices = system.list_devices(SourceKind::Loopback);
    assert!(!devices.is_empty(), "Loopback 声源应被列出(macOS 13+)");

    let source = devices.into_iter().next().unwrap();
    let open_result = system.open(&source);
    let mut stream = match open_result {
        Ok(s) => s,
        Err(e) => {
            let _ = player.kill();
            panic!("打开 Loopback 失败:{e}");
        }
    };

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut samples: Vec<f32> = Vec::new();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        stream.pop_samples(&mut samples);
        if samples.len() > 9_600 {
            break; // > 200 ms of audio captured
        }
    }

    let _ = player.kill();
    let _ = player.wait();

    assert!(
        samples.len() > 4_800,
        "应捕获到系统音频样本,实际 {} 个",
        samples.len()
    );
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.05, "音频应为非静音(440Hz 半幅正弦),峰值 {peak}");
}

/// Minimal 16-bit PCM mono WAV.
fn sine_wav(freq: f32, amp: f32, secs: f32) -> Vec<u8> {
    let rate: usize = 44_100;
    let n = (rate as f32 * secs) as usize;
    let mut v = Vec::with_capacity(44 + n * 2);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + n as u32 * 2).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&(rate as u32).to_le_bytes());
    v.extend_from_slice(&(rate as u32 * 2).to_le_bytes()); // byte rate
    v.extend_from_slice(&2u16.to_le_bytes()); // block align
    v.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(n as u32 * 2).to_le_bytes());
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let s = (amp * (std::f32::consts::TAU * freq * t).sin() * 32_000.0) as i16;
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}
