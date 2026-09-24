//! Test stand-in: a binary that only acts as the confinement helper.

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(claustrum_confine::HELPER_ARG.as_ref()) {
        claustrum_confine::helper_main();
    }
    eprintln!(
        "usage: claustrum-confine-helper {} <profile> <program> [args...]",
        claustrum_confine::HELPER_ARG
    );
    std::process::exit(2);
}
