mod agent;
mod agent_profile;
mod artifact;
mod cli;
mod daemon;
mod domain;
mod gitutil;
mod ipc;
mod monitor;
mod paths;
mod pi_contract;
mod pi_event_journal;
mod state;
mod workflow;

fn main() {
    let mut args = std::env::args();
    let _executable = args.next();
    let internal_command = args.next();
    if internal_command.as_deref() == Some(workflow::COMMAND_SUPERVISOR_ARG) {
        let result_fd = args.next().and_then(|value| value.parse::<i32>().ok());
        let command = args.next();
        let (Some(result_fd), Some(command)) = (result_fd, command) else {
            eprintln!(
                "khazad-doom: verification command supervisor omitted its result descriptor or command"
            );
            std::process::exit(125);
        };
        if let Err(err) = workflow::protect_command_supervisor_result(result_fd) {
            eprintln!("khazad-doom: verification command supervisor result setup failed: {err:#}");
            std::process::exit(125);
        }
        match workflow::run_command_supervisor(&command) {
            Ok(code) => {
                if let Err(err) = workflow::write_command_supervisor_result(result_fd, None) {
                    eprintln!(
                        "khazad-doom: verification command supervision result failed: {err:#}"
                    );
                    std::process::exit(125);
                }
                std::process::exit(code)
            }
            Err(err) => {
                let message = format!("{err:#}");
                let _ = workflow::write_command_supervisor_result(result_fd, Some(&message));
                eprintln!("khazad-doom: verification command supervision failed: {message}");
                std::process::exit(125);
            }
        }
    }
    if internal_command.as_deref() == Some(pi_event_journal::PI_EVENT_RELAY_ARG) {
        let max_bytes = args.next().and_then(|value| value.parse::<u64>().ok());
        let stats_path = args.next();
        if max_bytes.is_none() || args.next().is_some() {
            eprintln!("khazad-doom: Pi event relay requires a byte limit and optional stats path");
            std::process::exit(125);
        }
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        match pi_event_journal::relay(
            stdin.lock(),
            stdout.lock(),
            max_bytes.expect("checked byte limit"),
        ) {
            Ok(stats) => {
                if let Some(path) = stats_path
                    && let Err(err) = artifact::write_json(path, &stats)
                {
                    eprintln!("khazad-doom: Pi event relay stats failed: {err:#}");
                    std::process::exit(120);
                }
            }
            Err(err) => {
                if let Some(path) = stats_path
                    && let Err(stats_err) = artifact::write_json(path, err.stats())
                {
                    eprintln!("khazad-doom: Pi event relay stats failed: {stats_err:#}");
                }
                eprintln!("khazad-doom: Pi event relay failed: {err}");
                std::process::exit(120);
            }
        }
        return;
    }
    if internal_command.as_deref() == Some(artifact::ATOMIC_JSON_WRITER_ARG) {
        let path = args.next();
        if path.is_none() || args.next().is_some() {
            eprintln!("khazad-doom: atomic JSON writer requires exactly one destination path");
            std::process::exit(125);
        }
        if let Err(err) = artifact::write_json_from_stdin(path.expect("checked destination path")) {
            eprintln!("khazad-doom: atomic JSON replacement failed: {err:#}");
            std::process::exit(125);
        }
        return;
    }
    if let Err(err) = cli::run(std::env::args().skip(1)) {
        eprintln!("khazad-doom: {err:#}");
        std::process::exit(1);
    }
}
