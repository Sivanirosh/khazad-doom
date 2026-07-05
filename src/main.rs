mod agent;
mod agent_profile;
mod artifact;
mod cli;
mod daemon;
mod domain;
mod gitutil;
mod ipc;
mod paths;
mod pi_contract;
mod state;
mod workflow;

fn main() {
    if let Err(err) = cli::run(std::env::args().skip(1)) {
        eprintln!("khazad-doom: {err:#}");
        std::process::exit(1);
    }
}
