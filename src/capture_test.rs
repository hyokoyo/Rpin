use std::ffi::OsString;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;

use crate::capture::{
    CaptureBackend, CaptureError,
    gdi::{GdiCapture, GdiMethod, GdiStages, milliseconds},
    monitor_at_cursor,
};
use crate::{
    logging::Level,
    platform::process::{log_memory, resources},
};

pub fn run(root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let dir = root.join(format!("rpin-capture-{timestamp}-{}", std::process::id()));
    fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    crate::logging::init_in(&dir, "debug")?;
    let result = capture_to_dir(&dir);
    if let Err(err) = &result {
        crate::logging::error(format!("{err:#}"));
    }
    result.with_context(|| format!("capture report in {}", dir.display()))
}

pub struct BenchOptions {
    runs: usize,
    monitor: bool,
    method: GdiMethod,
}

impl BenchOptions {
    pub fn parse(mut args: impl Iterator<Item = OsString>) -> Option<Self> {
        let mut runs = None;
        let mut monitor = false;
        let mut method = None;
        while let Some(arg) = args.next() {
            match arg.to_str()? {
                "--monitor" if !monitor => monitor = true,
                "--method" if method.is_none() => {
                    method = Some(match args.next()?.to_str()? {
                        "direct" => GdiMethod::Direct,
                        "copy" => GdiMethod::Copy,
                        "compatible" => GdiMethod::Compatible,
                        _ => return None,
                    });
                }
                value if runs.is_none() => runs = Some(value.parse().ok()?),
                _ => return None,
            }
        }
        Some(Self {
            runs: runs.unwrap_or(200),
            monitor,
            method: method.unwrap_or_default(),
        })
    }
}

struct BenchSample {
    total: Duration,
    stages: GdiStages,
}

/// Capture repeatedly in one process without exporting pixel data.
pub fn bench(options: BenchOptions) -> anyhow::Result<()> {
    let BenchOptions {
        runs,
        monitor,
        method,
    } = options;
    anyhow::ensure!(
        (2..=2000).contains(&runs),
        "capture runs must be between 2 and 2000"
    );
    let capture = GdiCapture::with_method(method);
    // Hold the target fixed for comparisons even if the cursor moves later.
    let target = if monitor {
        Some(monitor_at_cursor()?)
    } else {
        None
    };
    println!(
        "method={} target={}",
        method.name(),
        if monitor {
            "cursor monitor"
        } else {
            "all monitors"
        }
    );
    println!("before capture: {}", resources()?);
    let mut samples = Vec::with_capacity(runs);
    let mut single_4k = true;
    for iteration in 1..=runs {
        let start = Instant::now();
        let (frames, stages) = capture
            .capture_profiled(target)
            .with_context(|| format!("desktop capture {iteration}"))?;
        samples.push(BenchSample {
            total: start.elapsed(),
            stages,
        });
        single_4k &= matches!(frames.as_slice(), [monitor] if (monitor.frame.width, monitor.frame.height) == (3840, 2160));
        if iteration == 1 {
            for frame in &frames {
                println!("frame={}x{}", frame.frame.width, frame.frame.height);
            }
            println!("holding first capture: {}", resources()?);
        }
        drop(frames);
        if iteration == 1 || iteration % 20 == 0 || iteration == runs {
            println!("after capture {iteration}: {}", resources()?);
        }
    }

    print_percentiles("total", &samples, |sample| sample.total);
    print_percentiles("setup", &samples, |sample| sample.stages.setup);
    print_percentiles("bitblt", &samples, |sample| sample.stages.blit);
    print_percentiles("flush", &samples, |sample| sample.stages.flush);
    print_percentiles("readback", &samples, |sample| sample.stages.readback);
    println!(
        "completed={runs} layered_retries={}",
        samples
            .iter()
            .map(|sample| sample.stages.layered_retries)
            .sum::<usize>(),
    );
    if single_4k {
        let within_budget = samples
            .iter()
            .filter(|sample| sample.total <= Duration::from_millis(40))
            .count();
        println!("4k_within_40_ms={within_budget}/{runs}");
    }
    Ok(())
}

fn print_percentiles(
    name: &str,
    samples: &[BenchSample],
    select: impl Fn(&BenchSample) -> Duration,
) {
    let mut warm: Vec<Duration> = samples[1..].iter().map(&select).collect();
    warm.sort_unstable();
    let percentile = |p: usize| warm[(warm.len() * p).div_ceil(100) - 1];
    println!(
        "{name}: first_ms={:.2} warm_p50_ms={:.2} warm_p95_ms={:.2} warm_max_ms={:.2}",
        milliseconds(select(&samples[0])),
        milliseconds(percentile(50)),
        milliseconds(percentile(95)),
        milliseconds(warm[warm.len() - 1]),
    );
}

fn capture_to_dir(dir: &Path) -> anyhow::Result<()> {
    log_memory("before capture", Level::Info);
    let start = Instant::now();
    let captured = GdiCapture::new()
        .capture_all()
        .context("capturing desktop")?;
    let capture_ms = start.elapsed().as_secs_f64() * 1000.0;
    log_memory("with frozen frames", Level::Info);
    let desktop = captured
        .iter()
        .map(|monitor| monitor.info.rect)
        .reduce(|bounds, rect| bounds.union(rect))
        .ok_or(CaptureError::NoMonitor)?;
    crate::logging::info(format!(
        "Captured {} monitor(s) in {capture_ms:.2} ms including enumeration, desktop=[{}, {}, {}, {}]",
        captured.len(),
        desktop.left,
        desktop.top,
        desktop.right,
        desktop.bottom
    ));
    for (index, monitor) in captured.into_iter().enumerate() {
        let filename = format!("monitor-{}.png", index + 1);
        let path = dir.join(&filename);
        let write_start = Instant::now();
        let file =
            File::create_new(&path).with_context(|| format!("creating {}", path.display()))?;
        if let Err(err) = crate::output::png::write_rgb(&monitor.frame, BufWriter::new(file)) {
            let _ = fs::remove_file(&path);
            return Err(err).with_context(|| format!("writing {}", path.display()));
        }
        let info = monitor.info;
        crate::logging::info(format!(
            "{filename}: {}x{}, origin=({}, {}), work=[{}, {}, {}, {}], dpi={}, primary={}, PNG {:.2} ms",
            monitor.frame.width,
            monitor.frame.height,
            info.rect.left,
            info.rect.top,
            info.work.left,
            info.work.top,
            info.work.right,
            info.work.bottom,
            info.dpi,
            info.primary,
            write_start.elapsed().as_secs_f64() * 1000.0
        ));
    }
    log_memory("after output", Level::Info);
    Ok(())
}
