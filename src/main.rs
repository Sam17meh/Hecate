pub mod agent;
mod analysis;
mod application;
mod cli;
mod contribution_cli;
mod discovery;
mod github;
mod inspection;
mod planning;
mod terminal;
mod workspace;

fn main() {
    cli::run();
}
