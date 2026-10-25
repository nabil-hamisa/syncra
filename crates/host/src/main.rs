use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    // No arguments (double-clicked): share this PC through the saved signal
    // server and show the host window.
    let args = host::Args::parse();
    let gui = std::env::args().len() == 1 || args.gui;
    ui::init_logging(gui, net::crypto::data_dir().ok(), "host");
    let r = host::run_main(args, gui, false);
    // Windowed runs have no console: show fatal errors.
    if let (Err(e), true) = (&r, gui) {
        ui::message(None, "Syncra host", &format!("{e:#}"), true);
    }
    r
}
