#![windows_subsystem = "windows"]

mod audio;
mod capture;
mod desktop;
mod engine;
mod library;
mod locale;
mod media;
mod platform;
mod ui;

fn main() {
    ui::run();
}
