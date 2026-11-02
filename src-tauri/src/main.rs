#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let mut arguments = std::env::args().skip(1);
    if arguments.next().as_deref() == Some("--agentdeck-permission-server") {
        if let (Some(address), Some(token)) = (arguments.next(), arguments.next()) {
            agentdeck_lib::run_permission_server(address, token);
            return;
        }
    }
    agentdeck_lib::run()
}
