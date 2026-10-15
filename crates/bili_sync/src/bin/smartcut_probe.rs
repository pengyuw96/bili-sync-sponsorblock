//! Diagnostic: run the SponsorBlock beta smart cut on a file copy.
//! Usage: smartcut_probe <in> <out> <s1-e1> [<s2-e2> ...]   (seconds; keep ranges)
#![allow(dead_code)]
mod downloader {
    pub fn resolve_media_tool_path(n: &str) -> std::path::PathBuf {
        n.into()
    }
}
#[path = "../sponsorblock/smart_cut.rs"]
mod smart_cut;

use std::path::Path;

fn children_cpu() -> f64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let f: Vec<&str> = s.rsplit_once(')').map(|x| x.1).unwrap_or("").split_whitespace().collect();
    let g = |i: usize| f.get(i).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
    (g(13) + g(14)) / 100.0
}

async fn ffprobe(args: &[&str], p: &Path) -> String {
    let o = tokio::process::Command::new("ffprobe").args(args).arg(p).output().await.unwrap();
    String::from_utf8_lossy(&o.stdout).to_string()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let a: Vec<String> = std::env::args().collect();
    let (input, output) = (Path::new(&a[1]), Path::new(&a[2]));
    let keep: Vec<(f64, f64)> = a[3..]
        .iter()
        .map(|r| {
            let (s, e) = r.split_once('-').unwrap();
            (s.parse().unwrap(), e.parse().unwrap())
        })
        .collect();
    let start: f64 = ffprobe(&["-v", "error", "-show_entries", "format=start_time", "-of", "default=nw=1:nk=1"], input)
        .await.trim().parse().unwrap_or(0.0);
    let dur: f64 = ffprobe(&["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"], input)
        .await.trim().parse()?;
    let mut kfs: Vec<f64> = ffprobe(&["-v", "error", "-select_streams", "v:0", "-skip_frame", "nokey", "-show_entries", "frame=pts_time", "-of", "csv=p=0"], input)
        .await.lines().filter_map(|l| l.trim().parse::<f64>().ok()).map(|t| (t - start).max(0.0)).collect();
    kfs.sort_by(|x, y| x.partial_cmp(y).unwrap());
    kfs.dedup();
    let gaps: Vec<f64> = kfs.windows(2).map(|w| w[1] - w[0]).collect();
    println!("PROBE duration={:.3} keyframes={} gop_avg={:.2}s", dur, kfs.len(),
        if gaps.is_empty() { 0.0 } else { gaps.iter().sum::<f64>() / gaps.len() as f64 });
    let cpu0 = children_cpu();
    let t = std::time::Instant::now();
    let res = smart_cut::smart_cut(input, output, &keep, &kfs, dur).await;
    let (wall, cpu) = (t.elapsed().as_secs_f64(), children_cpu() - cpu0);
    let expected: f64 = keep.iter().map(|(s, e)| e - s).sum();
    match res {
        Ok(st) => println!(
            "RESULT ok encoder={} pieces={} encoded={:.2}s copied={:.2}s expected={:.3}s got={:.3}s diff={:+.3}s wall={:.2}s cpu={:.2}s",
            st.encoder, st.pieces, st.encoded_secs, st.copied_secs, expected, st.output_duration_secs,
            st.output_duration_secs - expected, wall, cpu
        ),
        Err(e) => println!("RESULT FALLBACK (stable would run) err={:#} wall={:.2}s cpu={:.2}s", e, wall, cpu),
    }
    Ok(())
}
