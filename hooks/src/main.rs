//! `collide-hook`: Collide's agent hooks as one native binary. The work is
//! in the library (lib.rs), which the free version's `collide` program
//! shares.

fn main() {
    collide_hooks::run(std::env::args().skip(1).collect())
}
