mod app;
mod audio;
mod ui;

use std::path::PathBuf;

fn parse_screenshot() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--screenshot" {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = argument.strip_prefix("--screenshot=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn main() -> eframe::Result<()> {
    let screenshot = parse_screenshot();
    if std::env::args().any(|argument| argument == "--screenshot") && screenshot.is_none() {
        eprintln!("Usage: rigwerk --screenshot <file.png>");
        std::process::exit(2);
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 820.0])
            .with_min_inner_size([1040.0, 700.0])
            .with_title("RigWerk - independent audio bridge"),
        ..Default::default()
    };

    eframe::run_native(
        "RigWerk",
        options,
        Box::new(move |cc| Ok(Box::new(app::RigWerkApp::new(cc, screenshot.clone())))),
    )
}
