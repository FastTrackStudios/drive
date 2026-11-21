//! Video: a frame near the middle, read by ffmpeg when this computer has
//! it (`$DRIVE_FFMPEG`, or `ffmpeg` on the `PATH`). Without it a video
//! gets no thumbnail (`Error::NoGenerator`), and its icon stands in.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::Error;
use super::picture::Picture;

/// How long ffmpeg may take for one frame.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The ffmpeg this computer has, if any.
pub fn ffmpeg() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DRIVE_FFMPEG").map(PathBuf::from)
        && p.is_file()
    {
        return Some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("ffmpeg"))
        .find(|p| p.is_file())
}

fn ffprobe(ffmpeg: &Path) -> PathBuf {
    let p = ffmpeg.with_file_name("ffprobe");
    if p.is_file() {
        p
    } else {
        PathBuf::from("ffprobe")
    }
}

/// Run a command to its end, at most [`TIMEOUT`]; its standard output.
fn run(mut cmd: Command) -> Result<Vec<u8>, Error> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::NoGenerator(format!("ffmpeg: {e}")))?;
    let mut out = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut v);
        v
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Failed("ffmpeg took too long".into()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(Error::Failed(e.to_string())),
        }
    }
    reader
        .join()
        .map_err(|_| Error::Failed("ffmpeg's output".into()))
}

/// A frame from near the middle of a video.
pub fn frame(path: &Path) -> Result<Picture, Error> {
    let ff = ffmpeg().ok_or_else(|| Error::NoGenerator("no ffmpeg on this computer".into()))?;
    let mut probe = Command::new(ffprobe(&ff));
    probe
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path);
    let secs: f64 = run(probe)
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o).trim().parse().ok())
        .filter(|s: &f64| s.is_finite() && *s > 0.0)
        .unwrap_or(0.0);
    let at = format!("{:.3}", secs / 2.0);
    let mut cmd = Command::new(&ff);
    cmd.args(["-v", "error", "-nostdin", "-ss", &at, "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-an",
            "-sn",
            "-vf",
            "scale='min(1024,iw)':-2",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "-",
        ]);
    let png = run(cmd)?;
    if png.is_empty() {
        return Err(Error::Failed("ffmpeg gave no frame".into()));
    }
    Picture::from_bytes(&png)
}
