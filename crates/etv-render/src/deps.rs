//! Which of the programs the renderer shells out to are present, and why not.
//!
//! The renderer is optional: the station runs without any of this, and a
//! channel whose chapters are not rendered airs its stand-in. So absence is a
//! reported state here, never a startup failure. `/api/health/renders` serves
//! [`detect`]'s result to the dashboard's Health tab.

use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::settings::Settings;
use crate::tools;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    Detected,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    pub status: Presence,
    /// The version found, or the reason it was not.
    pub detail: String,
}

impl Dependency {
    fn found(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Presence::Detected,
            detail: detail.into(),
        }
    }

    fn missing(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Presence::Missing,
            detail: detail.into(),
        }
    }
}

/// The encoder a chapter's final pass uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Encoder {
    pub name: &'static str,
    /// Arguments placed before the inputs (a VAAPI device).
    pub input_args: Vec<String>,
    /// Appended to the video filter chain (a hardware upload).
    pub filter_suffix: &'static str,
    /// The `-c:v` choice and its quality setting.
    pub codec_args: Vec<String>,
}

impl Encoder {
    pub fn software() -> Self {
        Self {
            name: "libx264",
            input_args: vec![],
            filter_suffix: "",
            codec_args: ["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"]
                .map(String::from)
                .to_vec(),
        }
    }
}

/// The hardware encoder for `ETV_ACCEL` when the ffmpeg lists it, else libx264.
pub fn pick_encoder(settings: &Settings, encoders: &str) -> Encoder {
    let has = |name: &str| {
        encoders
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(name))
    };
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    match settings.accel.as_deref() {
        Some("vaapi") if has("h264_vaapi") => match &settings.vaapi_device {
            Some(device) => Encoder {
                name: "h264_vaapi",
                input_args: args(&["-vaapi_device", device]),
                filter_suffix: ",format=nv12,hwupload",
                codec_args: args(&["-c:v", "h264_vaapi", "-qp", "22"]),
            },
            None => Encoder::software(),
        },
        Some("cuda") if has("h264_nvenc") => Encoder {
            name: "h264_nvenc",
            input_args: vec![],
            filter_suffix: "",
            codec_args: args(&["-c:v", "h264_nvenc", "-preset", "p4", "-cq", "23"]),
        },
        Some("qsv") if has("h264_qsv") => Encoder {
            name: "h264_qsv",
            input_args: vec![],
            filter_suffix: "",
            codec_args: args(&["-c:v", "h264_qsv", "-global_quality", "23"]),
        },
        Some("videotoolbox") if has("h264_videotoolbox") => Encoder {
            name: "h264_videotoolbox",
            input_args: vec![],
            filter_suffix: "",
            codec_args: args(&["-c:v", "h264_videotoolbox", "-b:v", "8M"]),
        },
        _ => Encoder::software(),
    }
}

/// `ffmpeg -encoders` output, or the reason it could not be read.
pub fn ffmpeg_encoders(settings: &Settings) -> Result<String, String> {
    let out = tools::run(Command::new(&settings.ffmpeg).args(["-hide_banner", "-encoders"]))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `ffmpeg -filters` output, or the reason it could not be read.
pub fn ffmpeg_filters_text(settings: &Settings) -> Result<String, String> {
    let out = tools::run(Command::new(&settings.ffmpeg).args(["-hide_banner", "-filters"]))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn has_rubberband_filter(filters: &str) -> bool {
    filters
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some("rubberband"))
}

const PYTHON_PROBE: &str = r#"
import importlib.util as u, importlib.metadata as m, json
out = {}
for n in ("demucs", "torch"):
    if u.find_spec(n) is None:
        out[n] = None
    else:
        try:
            out[n] = m.version(n)
        except Exception:
            out[n] = "unknown version"
print(json.dumps(out))
"#;

/// Whether Demucs and PyTorch import under the configured interpreter. Returns
/// the version or the reason for each, in that order.
pub fn python_modules(settings: &Settings) -> (Result<String, String>, Result<String, String>) {
    let missing_in = |name: &str| {
        format!(
            "no module named {name} for {} (set ETV_RENDER_PYTHON to the interpreter it is installed into)",
            settings.python
        )
    };
    let out = match tools::run(Command::new(&settings.python).args(["-c", PYTHON_PROBE])) {
        Ok(out) => out,
        Err(e) => return (Err(e.clone()), Err(e)),
    };
    let parsed: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("{} printed no module report: {e}", settings.python);
            return (Err(msg.clone()), Err(msg));
        }
    };
    let pick = |name: &str| match parsed.get(name).and_then(|v| v.as_str()) {
        Some(version) => Ok(version.to_string()),
        None => Err(missing_in(name)),
    };
    (pick("demucs"), pick("torch"))
}

/// Every dependency the renderer has, as the Health tab lists them.
pub fn detect(settings: &Settings) -> Vec<Dependency> {
    let mut deps = Vec::new();

    match tools::run(Command::new(&settings.ffmpeg).arg("-version")) {
        Ok(out) => {
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            deps.push(Dependency::found("ffmpeg", first));
        }
        Err(e) => deps.push(Dependency::missing("ffmpeg", e)),
    }

    deps.push(match ffmpeg_filters_text(settings) {
        Ok(filters) if has_rubberband_filter(&filters) => {
            Dependency::found("librubberband", "ffmpeg lists the rubberband filter")
        }
        Ok(_) => Dependency::missing(
            "librubberband",
            format!(
                "{} was built without librubberband: its filter list has no rubberband",
                settings.ffmpeg
            ),
        ),
        Err(e) => Dependency::missing("librubberband", e),
    });

    deps.push(match ffmpeg_encoders(settings) {
        Ok(list) => {
            let enc = pick_encoder(settings, &list);
            if enc.name == "libx264"
                && !list
                    .lines()
                    .any(|l| l.split_whitespace().nth(1) == Some("libx264"))
            {
                Dependency::missing(
                    "encoder",
                    "no hardware encoder for ETV_ACCEL and no libx264",
                )
            } else {
                Dependency::found("encoder", enc.name)
            }
        }
        Err(e) => Dependency::missing("encoder", e),
    });

    let (demucs, torch) = python_modules(settings);
    deps.push(match demucs {
        Ok(v) => Dependency::found("demucs", v),
        Err(e) => Dependency::missing("demucs", e),
    });
    deps.push(match torch {
        Ok(v) => Dependency::found("pytorch", v),
        Err(e) => Dependency::missing("pytorch", e),
    });
    deps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(accel: Option<&str>, vaapi: Option<&str>) -> Settings {
        Settings {
            cache_dir: "/c".into(),
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            python: "python3".into(),
            accel: accel.map(String::from),
            vaapi_device: vaapi.map(String::from),
        }
    }

    const LIST: &str = " V....D libx264              libx264 H.264\n V....D h264_vaapi           H.264/AVC (VAAPI)\n";

    #[test]
    fn the_hardware_encoder_is_used_only_when_ffmpeg_lists_it() {
        let s = settings(Some("vaapi"), Some("/dev/dri/renderD128"));
        assert_eq!(pick_encoder(&s, LIST).name, "h264_vaapi");
        assert_eq!(pick_encoder(&s, " V....D libx264 x\n").name, "libx264");
    }

    #[test]
    fn vaapi_without_a_device_falls_back_to_software() {
        assert_eq!(
            pick_encoder(&settings(Some("vaapi"), None), LIST).name,
            "libx264"
        );
    }

    #[test]
    fn no_accel_means_software() {
        assert_eq!(pick_encoder(&settings(None, None), LIST).name, "libx264");
    }

    #[test]
    fn the_rubberband_filter_is_found_by_name() {
        assert!(has_rubberband_filter(
            " .. rubberband        A->A       Apply time-stretching.\n"
        ));
        assert!(!has_rubberband_filter(
            " .. atempo        A->A       Tempo.\n"
        ));
    }

    #[test]
    fn a_missing_interpreter_reports_both_python_modules_missing() {
        let mut s = settings(None, None);
        s.python = "/nonexistent/python".into();
        let (demucs, torch) = python_modules(&s);
        assert!(demucs.unwrap_err().contains("/nonexistent/python"));
        assert!(torch.is_err());
    }
}
