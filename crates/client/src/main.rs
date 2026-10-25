use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    // No arguments (double-clicked): the connect window.
    let args = client::Args::parse();
    let gui = std::env::args().len() == 1 || args.gui;
    ui::init_logging(gui, net::crypto::data_dir().ok(), "client");
    let r = client::run_main(args, gui);
    // Windowed runs have no console: show fatal errors.
    if let (Err(e), true) = (&r, gui) {
        ui::message(None, "Syncra client", &format!("{e:#}"), true);
    }
    r
}
