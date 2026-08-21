mod bot;
mod crypto;
mod media;
mod net;
mod room;
mod store;
mod sys;
mod tui;
mod update;

/// Anything the app can be asked to do without opening the interface.
///
/// Kept deliberately tiny. This is a chat window first; the command line is
/// here so somebody can be handed one line to run, not so the app grows a
/// second personality.
const USAGE: &str = "\
local-llm

  local-llm            open the chat
  local-llm update     install a new build and exit
  local-llm version    print the version

  local-llm bot --room <PIN> [--nick <name>]
                       join a room as a program: one json object per line
                       out, one json object per line in
";

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Skips the program name, and anything the relaunch after an update adds.
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with(update::JUST_UPDATED))
        .collect();

    match args.first().map(String::as_str) {
        None => rt.block_on(tui::run()),
        Some("update") => rt.block_on(update::run_cli()),
        Some("bot") => {
            let flag = |name: &str| -> Option<String> {
                let at = args.iter().position(|a| a == name)?;
                args.get(at + 1).cloned()
            };
            match flag("--room").or_else(|| flag("--pin")) {
                Some(pin) => rt.block_on(bot::run(&pin, flag("--nick").as_deref())),
                None => {
                    eprintln!("local-llm bot: --room <PIN> is required
");
                    eprint!("{USAGE}");
                    std::process::exit(2);
                }
            }
        }
        Some("version" | "--version" | "-V") => {
            println!("local-llm {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("help" | "--help" | "-h") => {
            print!("{USAGE}");
            Ok(())
        }
        // Opening the chat on an argument we do not understand would look
        // like the argument worked.
        Some(other) => {
            eprintln!("local-llm: no such command: {other}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}
