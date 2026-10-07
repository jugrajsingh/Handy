use handy_tts_core::device::Device;
use handy_tts_core::performance::{Timing, gate, replay_with_lanes};
use serde_json::Value;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("timing JSONL path required")?;
    let words: usize = args
        .next()
        .ok_or("measured input word count required")?
        .parse()?;
    let lanes: usize = args.next().map_or(Ok(1), |value| value.parse())?;
    let rows = std::fs::read_to_string(path)?;
    let mut timings = Vec::new();
    let mut rtfs = Vec::new();
    for line in rows.lines() {
        let row: Value = serde_json::from_str(line)?;
        if row["kind"] != "dsp" {
            continue;
        }
        let samples = row["samples"].as_u64().ok_or("sample count missing")? as usize;
        let audio = row["audio_s"].as_f64().ok_or("duration missing")?;
        let elapsed = row["synth_s"].as_f64().ok_or("synthesis missing")?
            + row["dsp_s"].as_f64().ok_or("DSP missing")?;
        timings.push(Timing {
            synthesis_seconds: elapsed,
            audio_seconds: audio,
            samples,
            sample_rate: (samples as f64 / audio).round() as u32,
        });
        rtfs.push(elapsed / audio);
    }
    rtfs.sort_by(f64::total_cmp);
    let median = *rtfs.get(rtfs.len() / 2).ok_or("no timings")?;
    let result = replay_with_lanes(&timings, 2.0, lanes)?;
    let pass = gate(
        "pocket-en",
        Device::Cpu,
        Device::Cpu,
        median,
        words,
        result.buffering_pauses,
    );
    println!(
        "{}",
        serde_json::json!({"replay":result,"words":words,"median_rtf":median,"pass":pass})
    );
    if !pass {
        return Err("Pocket continuity gate failed".into());
    }
    Ok(())
}
