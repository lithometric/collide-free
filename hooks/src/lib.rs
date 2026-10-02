//! Collide's agent hooks: the library behind `collide-hook` and, in the
//! free version, the single `collide` program (collide-rs links it in).
//!
//! Collide's agent hooks as one native binary.
//!
//! Same contract as the Python hooks it replaces, and a differential test
//! drives both with identical stdin and filesystem and asserts they agree.
//! What changes is the cost of getting there: these fire on every tool call
//! an agent makes, and a Python interpreter spends ~33ms booting before it
//! can decide anything, out of a 200ms gate budget.
//!
//! Usage:
//!   collide-hook gate                    < hook.json     (PreToolUse)
//!   collide-hook report                  < hook.json     (PostToolUse / Stop)
//!   collide-hook pre-commit                              (git pre-commit)
//!   collide-hook land [--test CMD]                       (rebase, test, push; see land.rs)
//!   collide-hook login [--server URL] [--repo ID]        (browser sign-in; see login.rs)
//!   collide-hook watch [--once] [...]                    (the disk watcher)
//!   collide-hook check-credentials [cmd]                 (SessionStart)
//!   collide-hook add-credentials [file]
//!   collide-hook self-update <server>                    (detached; see selfupdate.rs)
//!   collide-hook setup <server> <code>                   (one-command setup; see setup.rs)
//!   collide mcp                                          (MCP over stdio for the free version; see mcpbridge.rs)

pub mod harness;
pub mod check;
pub mod verify;
pub mod prompt;
pub mod config;
pub mod gate;
pub mod land;
pub mod login;
pub mod machine;
pub mod mcpbridge;
pub mod git;
pub mod http;
pub mod pre_commit;
pub mod apply;
pub mod report;
pub mod selfupdate;
pub mod setup;
pub mod shell;
pub mod spool;
pub mod supersede;
pub mod transcript;
pub mod watch;

use std::io::Read;

fn read_stdin() -> String {
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

/// The whole command line, after the program name. Exits the process.
pub fn run(args: Vec<String>) -> ! {
    // before anything is started in the background (see machine.rs)
    machine::no_inherit_std();
    let command = args.first().map(String::as_str).unwrap_or("");
    let env = config::env_map();
    // which agent harness is calling; named by the generated config, never sniffed
    harness::set(&harness::of(&args));

    // The generated settings command runs this binary BEFORE the committed
    // script and exits, so a binary older than the repo's hooks would shadow
    // them silently and forever. Read stdin once, here, and hand the whole
    // event to the script when the script is the newer of the two.
    let stdin_data = matches!(command, "gate" | "report").then(read_stdin);
    if let Some(ref data) = stdin_data {
        // the machine's hooks stand down in a repo that runs Collide's own,
        // and start the local server when a session or a prompt begins
        if let Some(code) = machine::before_event(data, &env) {
            std::process::exit(code);
        }
        if let Some(code) = supersede::delegate(command, &args[1..], data, &env) {
            std::process::exit(code);
        }
    }
    let stdin_data = stdin_data.unwrap_or_default();

    // Fail open is the whole contract, so a panic anywhere must still exit 0
    // (or 2 only where the gate deliberately blocks). This is the Rust
    // equivalent of the Python hooks' bare `except`.
    let code = std::panic::catch_unwind(|| match command {
        "gate" => {
            // a message to the other agents: signed by the agent that sends it
            if let Some(rewritten) = machine::message_rewrite(&stdin_data) {
                println!("{rewritten}");
                return 0;
            }
            // a plain push becomes a landing: rebase, test and push in one step
            if let Some(rewritten) = land::rewrite(&stdin_data, &env) {
                println!("{rewritten}");
                return 0;
            }
            let (code, stderr_text) = gate::run(&stdin_data, &env);
            let (stdout_text, stderr_text, code) = harness::render_decision(code == 0, &stderr_text);
            if !stdout_text.is_empty() {
                println!("{stdout_text}");
            }
            if !stderr_text.is_empty() {
                eprint!("{stderr_text}");
            }
            code
        }
        "report" => report::run(&stdin_data, &env),
        "index" => report::index_command(args.get(1).map(String::as_str).unwrap_or(""), &env),
        "verify-tests" => verify::run(
            args.get(1).map(String::as_str).unwrap_or(""),
            args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).map(String::as_str).unwrap_or(""),
            &env,
        ),
        "check" => check::check_command(
            args.get(1).map(String::as_str).unwrap_or(""), args.get(2).map(String::as_str).unwrap_or(""),
            args.get(3).map(String::as_str).unwrap_or(""), args.get(4).map(String::as_str).unwrap_or("[]"), &env),
        "apply" => apply::run_args(&args[1..], &env),
        "land" => land::run(&args[1..], &env),
        "login" => login::run(&args[1..], &env),
        "install" => machine::install(&args[1..], &env),
        "upgrade" => machine::upgrade(&args[1..], &env),
        "add" => machine::add(&env),
        "message" => machine::message(&args[1..], &env),
        "mcp" => mcpbridge::run(&env),
        "agents-md" => machine::agents_md(&env),
        "status" => machine::status(&env),
        "sync" => machine::sync(&env),
        "uninstall" => machine::uninstall(&env),
        "pre-commit" => {
            let (code, stderr_text) = pre_commit::run(&env);
            if !stderr_text.is_empty() {
                eprint!("{stderr_text}");
            }
            code
        }
        "watch" => match watch::parse_args(&args[1..], &env) {
            Ok(options) => watch::run(options, &env),
            Err(problem) => {
                eprintln!("collide-hook watch: {problem}");
                2
            }
        },
        "check-credentials" => {
            report::check_credentials(args.get(1).map(String::as_str).unwrap_or("python"), &env)
        }
        "add-credentials" => {
            report::add_credentials(args.get(1).map(String::as_str).unwrap_or(""), &env)
        }
        "self-update" => selfupdate::run(args.get(1).map(String::as_str).unwrap_or(""), &env),
        "setup" => setup::run(
            args.get(1).map(String::as_str).unwrap_or(""), args.get(2).map(String::as_str).unwrap_or(""), &env),
        "--version" | "version" => {
            // the hook-artifact version is the one that decides staleness, so
            // print it next to the crate version rather than making anyone
            // guess which number the server is comparing against
            println!(
                "collide-hook {} (hook artifacts v{})",
                env!("CARGO_PKG_VERSION"),
                report::HOOK_VERSION
            );
            0
        }
        _ if machine::serves_local_pub() => {
            println!(
                "collide: Collide for every repo on this machine.\n\n\
  collide status      what Collide did here, and whether it is running\n\
  collide message     tell the other agents in this repo something\n\
  collide login       link this machine to your Collide account\n\
  collide upgrade     bring in your team (Team, 14 days free)\n\
  collide add         share this repo with your team (on Team)\n\
  collide uninstall   take Collide's hooks out (your data stays)\n\n\
The agents' hooks run it as: collide report | collide gate"
            );
            0
        }
        _ => {
            eprintln!(
                "usage: collide-hook \
<gate|report|pre-commit|watch|check-credentials|add-credentials|--version>"
            );
            0
        }
    })
    .unwrap_or(0);

    std::process::exit(code);
}
