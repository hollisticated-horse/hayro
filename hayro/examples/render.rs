//! This example shows you how you can render a PDF file to PNG.

use hayro::{Pdf, RenderSettings, render};
use hayro_interpret::InterpreterSettings;
use std::sync::Arc;

fn main() {
    if let Ok(()) = log::set_logger(&LOGGER) {
        log::set_max_level(log::LevelFilter::Trace);
    }

    let args = CliArgs::parse().unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });

    let file = std::fs::read(&args.pdf_path).expect("failed to read PDF");
    let data = Arc::new(file);
    let pdf = Pdf::new(data).expect("failed to load PDF");

    let interpreter_settings = InterpreterSettings::default();

    let render_settings = RenderSettings::default();

    for (idx, page) in pdf.pages().iter().enumerate().filter(|(idx, _)| args.matches_page(*idx)) {
        let pixmap = render(page, &interpreter_settings, &render_settings);
        std::fs::write(format!("rendered_{idx}.png"), pixmap.take_png()).unwrap();
    }
}

/// A simple stderr logger.
static LOGGER: SimpleLogger = SimpleLogger;
struct SimpleLogger;
impl log::Log for SimpleLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::LevelFilter::Warn
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let target = if !record.target().is_empty() {
                record.target()
            } else {
                record.module_path().unwrap_or_default()
            };

            let line = record.line().unwrap_or(0);
            let args = record.args();

            match record.level() {
                log::Level::Error => eprintln!("Error (in {target}:{line}): {args}"),
                log::Level::Warn => eprintln!("Warning (in {target}:{line}): {args}"),
                log::Level::Info => eprintln!("Info (in {target}:{line}): {args}"),
                log::Level::Debug => eprintln!("Debug (in {target}:{line}): {args}"),
                log::Level::Trace => eprintln!("Trace (in {target}:{line}): {args}"),
            }
        }
    }

    fn flush(&self) {}
}

struct CliArgs {
    pdf_path: String,
    page: Option<usize>,
    range: Option<std::ops::RangeInclusive<usize>>,
}

impl CliArgs {
    fn parse() -> Result<Self, String> {
        let mut args = std::env::args().skip(1);
        let pdf_path = args
            .next()
            .ok_or_else(|| Self::usage("missing PDF path"))?;

        let mut page = None;
        let mut range = None;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--page" => {
                    let value = args
                        .next()
                        .ok_or_else(|| Self::usage("expected value after --page"))?;
                    if page.is_some() {
                        return Err(Self::usage("duplicate --page argument"));
                    }
                    if range.is_some() {
                        return Err(Self::usage("--page conflicts with --range"));
                    }
                    let parsed = value.parse::<usize>().map_err(|_| {
                        Self::usage("--page expects a positive integer (1-based)")
                    })?;
                    if parsed == 0 {
                        return Err(Self::usage("--page expects a positive integer (1-based)"));
                    }
                    page = Some(parsed - 1);
                }
                "--range" => {
                    let value = args
                        .next()
                        .ok_or_else(|| Self::usage("expected value after --range"))?;
                    if range.is_some() {
                        return Err(Self::usage("duplicate --range argument"));
                    }
                    if page.is_some() {
                        return Err(Self::usage("--range conflicts with --page"));
                    }
                    let (start, end) = value
                        .split_once('-')
                        .ok_or_else(|| Self::usage("--range expects START-END (1-based)"))?;
                    let start = start.trim().parse::<usize>().map_err(|_| {
                        Self::usage("--range expects START-END (1-based)")
                    })?;
                    let end = end.trim().parse::<usize>().map_err(|_| {
                        Self::usage("--range expects START-END (1-based)")
                    })?;
                    if start == 0 || end == 0 || start > end {
                        return Err(Self::usage(
                            "--range expects START-END with START <= END and both >= 1",
                        ));
                    }
                    range = Some(start - 1..=end - 1);
                }
                other => {
                    return Err(Self::usage(&format!("unknown argument '{other}'")));
                }
            }
        }

        Ok(Self {
            pdf_path,
            page,
            range,
        })
    }

    fn usage(msg: &str) -> String {
        format!(
            "{msg}\n\nUsage: cargo run --example render -- <PDF_PATH> [--page N] [--range START-END]\n  --page N        Render only page N (1-based)\n  --range S-E     Render inclusive page range S through E (1-based)\n\nNote: --page and --range are mutually exclusive."
        )
    }

    fn matches_page(&self, page_index: usize) -> bool {
        match (self.page, &self.range) {
            (Some(p), _) => page_index == p,
            (None, Some(r)) => r.contains(&page_index),
            (None, None) => true,
        }
    }
}
