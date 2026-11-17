//! The plan's audio: every layer read from its source or stem, stretched,
//! shifted, filtered, levelled, faded, delayed to its place and summed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::RenderError;
use crate::plan::{Curve, FadeKind, Filter, Plan};
use crate::stems::{self, Region};
use crate::tools;

/// A stem quieter than this is silence, not music to be brought to a level.
const SILENT_DB: f64 = -70.0;
/// Rate the mix is written at.
pub const SAMPLE_RATE: u32 = 44_100;
/// Extra source seconds read past a layer's end, so a stretch never runs dry.
const READ_SLACK_S: f64 = 0.25;

/// Where one layer's samples come from: a file and the offset into it.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub file: PathBuf,
    pub start_s: f64,
}

/// The input for every layer: the source file for a `full` read, else the
/// separated stem, offset by where its region starts. `stem_dirs` maps a
/// source id to its region and stem directory.
pub fn inputs(plan: &Plan, stem_dirs: &BTreeMap<String, (Region, PathBuf)>) -> Vec<Input> {
    plan.layers
        .iter()
        .map(|l| match stems::file_name(l.stem) {
            None => Input {
                file: plan
                    .source_path(&l.source)
                    .expect("validated: every layer names a known source")
                    .clone(),
                start_s: l.src_start_s,
            },
            Some(name) => {
                let (region, dir) = &stem_dirs[&l.source];
                Input {
                    file: dir.join(name),
                    start_s: l.src_start_s - region.start_s,
                }
            }
        })
        .collect()
}

fn afade(kind: FadeKind, start_s: f64, len_s: f64, curve: Curve) -> String {
    let t = match kind {
        FadeKind::In => "in",
        FadeKind::Out => "out",
    };
    let c = match curve {
        Curve::Linear => "tri",
        Curve::EqualPower => "qsin",
    };
    format!("afade=t={t}:st={start_s:.4}:d={len_s:.4}:curve={c}")
}

fn filter(f: &Filter) -> String {
    match f {
        Filter::Highpass { hz } => format!("highpass=f={hz:.1}"),
        Filter::Lowpass { hz } => format!("lowpass=f={hz:.1}"),
        Filter::Echo {
            in_gain,
            out_gain,
            delay_ms,
            decay,
        } => format!("aecho={in_gain:.3}:{out_gain:.3}:{delay_ms:.0}:{decay:.3}"),
    }
}

/// The ffmpeg filter graph that mixes the plan's layers into `[aout]`. Input
/// `i` is layer `i`'s excerpt, already cut to its start; `gains[i]` is the dB
/// to apply to it.
pub fn filter_graph(plan: &Plan, gains: &[f64]) -> String {
    let mut parts = Vec::new();
    let mut labels = String::new();
    for (i, (l, gain)) in plan.layers.iter().zip(gains).enumerate() {
        let mut chain = format!(
            "[{i}:a]asetpts=PTS-STARTPTS,aresample={SAMPLE_RATE},aformat=channel_layouts=stereo"
        );
        match &l.ride {
            Some(ride) => {
                // The tempo steps once per step on the input's clock: the
                // source's own seconds, since the stretch comes after.
                let steps: Vec<String> = ride
                    .ratios
                    .iter()
                    .enumerate()
                    .skip(1)
                    .map(|(k, r)| {
                        format!(
                            "{:.4} rubberband@ride{i} tempo {r:.6}",
                            k as f64 * ride.step_src_s
                        )
                    })
                    .collect();
                if !steps.is_empty() {
                    chain.push_str(&format!(",asendcmd=c='{}'", steps.join(";")));
                }
                chain.push_str(&format!(
                    ",rubberband@ride{i}=tempo={:.6}:pitch={:.6}",
                    ride.ratios[0], l.pitch
                ));
            }
            None if (l.ratio - 1.0).abs() > 1e-9 || (l.pitch - 1.0).abs() > 1e-9 => {
                chain.push_str(&format!(
                    ",rubberband=tempo={:.6}:pitch={:.6}",
                    l.ratio, l.pitch
                ));
            }
            None => {}
        }
        chain.push_str(&format!(",atrim=end={:.4}", l.out_len_s));
        for f in &l.filters {
            chain.push(',');
            chain.push_str(&filter(f));
        }
        if gain.abs() > 1e-9 {
            chain.push_str(&format!(",volume={gain:.3}dB"));
        }
        for f in &l.fades {
            chain.push(',');
            chain.push_str(&afade(f.kind, f.start_s, f.len_s, f.curve));
        }
        let delay_ms = (l.out_start_s * 1000.0).round() as i64;
        chain.push_str(&format!(",adelay=delays={delay_ms}:all=1[a{i}]"));
        parts.push(chain);
        labels.push_str(&format!("[a{i}]"));
    }
    let total = plan.duration_s;
    parts.push(format!(
        "{labels}amix=inputs={}:normalize=0:duration=longest,apad=whole_dur={total:.4},atrim=end={total:.4},alimiter=limit=0.95[aout]",
        plan.layers.len()
    ));
    parts.join(";")
}

/// The dB each layer is turned by: what brings its excerpt to `level_db`, when
/// it has one, plus its `gain_db`.
fn gains(ffmpeg: &str, plan: &Plan, inputs: &[Input]) -> Result<Vec<f64>, RenderError> {
    let mut out = Vec::with_capacity(plan.layers.len());
    for (l, input) in plan.layers.iter().zip(inputs) {
        let level = match l.level_db {
            None => 0.0,
            Some(target) => {
                let measured =
                    tools::mean_volume_db(ffmpeg, &input.file, input.start_s, l.src_len_s())
                        .map_err(|e| RenderError::new(&l.name, e))?;
                if measured < SILENT_DB {
                    return Err(RenderError::new(
                        &l.name,
                        format!(
                            "{} is silent at {:.1}s (mean {measured:.1} dB), so there is nothing to level",
                            input.file.display(),
                            input.start_s
                        ),
                    ));
                }
                target - measured
            }
        };
        out.push(level + l.gain_db);
    }
    Ok(out)
}

/// Mix the plan's audio into `out_wav`. Each layer is its own input, read from
/// its start for its length, so no file is decoded past the part it plays.
pub fn mix(ffmpeg: &str, plan: &Plan, inputs: &[Input], out_wav: &Path) -> Result<(), RenderError> {
    let gains = gains(ffmpeg, plan, inputs)?;
    let graph = filter_graph(plan, &gains);
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-v", "error"]);
    for (l, input) in plan.layers.iter().zip(inputs) {
        cmd.arg("-ss")
            .arg(format!("{:.4}", input.start_s))
            .arg("-t")
            .arg(format!("{:.4}", l.src_len_s() + READ_SLACK_S))
            .arg("-i")
            .arg(&input.file);
    }
    cmd.args(["-filter_complex", &graph, "-map", "[aout]", "-ar"])
        .arg(SAMPLE_RATE.to_string())
        .args(["-ac", "2"])
        .arg(out_wav);
    tools::run(&mut cmd).map_err(|e| RenderError::new("audio mix", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::sample;
    use crate::plan::{Ride, Stem};

    fn dirs(p: &Plan) -> BTreeMap<String, (Region, PathBuf)> {
        stems::regions(p)
            .into_iter()
            .map(|r| (r.source.clone(), (r, PathBuf::from("/s/a"))))
            .collect()
    }

    #[test]
    fn a_stem_layer_reads_its_stem_file_offset_into_the_region() {
        let p = sample();
        let inputs = inputs(&p, &dirs(&p));
        assert_eq!(inputs[0].file, PathBuf::from("/s/a/vocals.wav"));
        // The read starts at 30 s; the region starts 8 s before it.
        assert!((inputs[0].start_s - 8.0).abs() < 1e-9);
        // `b` is read whole, from its own file.
        assert_eq!(inputs[1].file, PathBuf::from("/m/b.mp4"));
        assert!((inputs[1].start_s - 10.0).abs() < 1e-9);
    }

    #[test]
    fn the_graph_stretches_filters_levels_fades_and_places_every_layer() {
        let p = sample();
        let g = filter_graph(&p, &[2.0, -3.0]);
        assert!(
            g.contains("rubberband=tempo=1.200000:pitch=1.000000"),
            "{g}"
        );
        assert!(g.contains("highpass=f=250.0"), "{g}");
        assert!(g.contains("volume=-3.000dB"), "{g}");
        assert!(
            g.contains("afade=t=in:st=0.0000:d=2.0000:curve=qsin"),
            "{g}"
        );
        assert!(g.contains("adelay=delays=6000:all=1[a1]"), "{g}");
        assert!(g.contains("amix=inputs=2:normalize=0"), "{g}");
        assert!(g.contains("apad=whole_dur=16.0000"), "{g}");
        // `b` plays at its own speed and pitch: no stretch on its branch.
        let b = g.split(';').nth(1).unwrap();
        assert!(!b.contains("rubberband"), "{b}");
    }

    #[test]
    fn a_ride_steps_its_tempo_on_the_sources_clock_then_holds_the_last() {
        let mut p = sample();
        p.layers[1].stem = Stem::Full;
        p.layers[1].ride = Some(Ride {
            step_src_s: 2.0,
            ratios: vec![0.9, 0.95, 1.0],
        });
        let g = filter_graph(&p, &[0.0, 0.0]);
        assert!(g.contains("asendcmd=c='2.0000 rubberband@ride1 tempo 0.950000;4.0000 rubberband@ride1 tempo 1.000000'"), "{g}");
        assert!(g.contains("rubberband@ride1=tempo=0.900000"), "{g}");
    }
}
