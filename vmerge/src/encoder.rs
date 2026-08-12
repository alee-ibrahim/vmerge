//! Picking an encoder and translating a quality name into encoder flags.
//! Ported from Test-Encoder / Select-VideoEncoder / Get-QualityArgs.

use std::path::Path;

use clap::ValueEnum;

use crate::proc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Quality {
    VisuallyLossless,
    High,
    Medium,
    Small,
}

impl Quality {
    pub const ALL: [Quality; 4] =
        [Quality::VisuallyLossless, Quality::High, Quality::Medium, Quality::Small];

    pub fn crf(self) -> u32 {
        match self {
            Quality::VisuallyLossless => 16,
            Quality::High => 20,
            Quality::Medium => 23,
            Quality::Small => 27,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::VisuallyLossless => "visually-lossless",
            Quality::High => "high",
            Quality::Medium => "medium",
            Quality::Small => "small",
        }
    }

    /// What choosing this costs, for the picker.
    pub fn note(self) -> &'static str {
        match self {
            Quality::VisuallyLossless => "biggest file, slowest",
            Quality::High => "the sensible default",
            Quality::Medium => "noticeably smaller",
            Quality::Small => "smallest, softest picture",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum EncoderPref {
    /// Use the GPU if one works.
    Auto,
    /// Always libx264.
    Cpu,
    Nvenc,
    Qsv,
    Amf,
}

impl EncoderPref {
    pub fn label(self) -> &'static str {
        match self {
            EncoderPref::Auto => "auto",
            EncoderPref::Cpu => "cpu",
            EncoderPref::Nvenc => "nvenc",
            EncoderPref::Qsv => "qsv",
            EncoderPref::Amf => "amf",
        }
    }

    pub fn note(self) -> &'static str {
        match self {
            EncoderPref::Auto => "use the GPU when possible",
            EncoderPref::Cpu => "always libx264",
            _ => "forced",
        }
    }

    /// The UI toggles between the two choices that matter.
    pub fn toggled(self) -> EncoderPref {
        match self {
            EncoderPref::Auto => EncoderPref::Cpu,
            _ => EncoderPref::Auto,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncoderChoice {
    pub name: String,
    pub label: String,
}

/// Does this encoder actually work here? Having ffmpeg list it is not enough;
/// a laptop can report h264_nvenc with no usable NVIDIA card behind it, so the
/// only reliable test is encoding a fraction of a second and checking the code.
fn works(ffmpeg: &Path, name: &str) -> bool {
    let mut cmd = proc::command(ffmpeg);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "quiet",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=320x240:r=25:d=0.2",
        "-c:v",
        name,
        "-f",
        "null",
        "-",
    ]);
    proc::run_captured(cmd).map(|o| o.status.success()).unwrap_or(false)
}

pub fn select(ffmpeg: &Path, preference: EncoderPref) -> EncoderChoice {
    let cpu = || EncoderChoice { name: "libx264".into(), label: "CPU (libx264)".into() };

    match preference {
        EncoderPref::Cpu => cpu(),
        EncoderPref::Auto => {
            let listed = {
                let mut cmd = proc::command(ffmpeg);
                cmd.args(["-hide_banner", "-nostdin", "-loglevel", "quiet", "-encoders"]);
                proc::run_captured(cmd)
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                    .unwrap_or_default()
            };
            for candidate in ["h264_nvenc", "h264_qsv", "h264_amf"] {
                if listed.contains(candidate) && works(ffmpeg, candidate) {
                    return EncoderChoice {
                        name: candidate.into(),
                        label: format!("GPU ({candidate})"),
                    };
                }
            }
            cpu()
        }
        forced => {
            let name = format!("h264_{}", forced.label());
            if works(ffmpeg, &name) {
                EncoderChoice { label: format!("GPU ({name})"), name }
            } else {
                cpu()
            }
        }
    }
}

pub fn quality_args(encoder: &str, quality: Quality) -> Vec<String> {
    let crf = quality.crf().to_string();
    let args: Vec<&str> = match encoder {
        "libx264" => vec!["-preset", "veryfast", "-crf", &crf],
        "h264_nvenc" => vec!["-preset", "p5", "-rc", "vbr", "-cq", &crf, "-b:v", "0"],
        "h264_qsv" => vec!["-preset", "veryfast", "-global_quality", &crf, "-look_ahead", "0"],
        "h264_amf" => vec!["-quality", "speed", "-rc", "cqp", "-qp_i", &crf, "-qp_p", &crf],
        _ => vec!["-crf", &crf],
    };
    args.into_iter().map(String::from).collect()
}
