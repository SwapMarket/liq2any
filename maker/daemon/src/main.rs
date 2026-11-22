//! `liq2any-maker <config.json>`: run the maker's main loop until stopped.
//! `liq2any-maker clear-rollback <config.json>`: after checking the chain by hand, clear a rollback alarm.
//! `liq2any-maker test-page <config.json>`: send one test page to `page_to`, and show what each relay answered.
//! `liq2any-maker withdraw <config.json> <liquid|polygon> <usdt|gas> <amount|all>`: move funds to the signer's `withdraw_to`.
//! `liq2any-maker pause|resume <config.json>`: stop taking new swaps, or start again. Open swaps carry on.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["clear-rollback", path] => liq2any_maker::clear_rollback(std::path::Path::new(path)),
        ["report", path] => liq2any_maker::report(std::path::Path::new(path)),
        ["test-page", path] => liq2any_maker::test_page(std::path::Path::new(path)),
        ["withdraw", path, chain, asset, amount] => liq2any_maker::withdraw(std::path::Path::new(path), chain, asset, amount),
        ["pause", path] => liq2any_maker::set_paused(std::path::Path::new(path), true),
        ["resume", path] => liq2any_maker::set_paused(std::path::Path::new(path), false),
        [path] => liq2any_maker::run(std::path::Path::new(path)),
        _ => {
            eprintln!("usage: liq2any-maker [clear-rollback | report | test-page | pause | resume] <config.json>");
            eprintln!("       liq2any-maker withdraw <config.json> <liquid|polygon> <usdt|gas> <amount|all>");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("maker stopped: {e}");
        std::process::exit(1);
    }
}
