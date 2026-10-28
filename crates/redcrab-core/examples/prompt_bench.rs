//! How long the console prompt takes per keystroke in the worst cases: the
//! hint and the completion for the longest usages in the command table,
//! with many words typed. Run with `--release` for what users see:
//!
//!     cargo run --release -p redcrab-core --example prompt_bench

use std::time::{Duration, Instant};

use redcrab_core::console::{Prompt, complete, hint};

fn time(label: &str, line: &str, mut f: impl FnMut(&str)) {
    const RUNS: u32 = 200;
    let started = Instant::now();
    for _ in 0..RUNS {
        f(line);
    }
    let each: Duration = started.elapsed() / RUNS;
    println!(
        "{label:<10} {:>9.1} µs  {line:.70}",
        each.as_secs_f64() * 1e6
    );
}

fn main() {
    let lines = [
        "s",
        "SET k v EX 10 NX ",
        "FT.AGGREGATE idx * LOAD 2 @a @b GROUPBY 1 @a REDUCE COUNT 0 AS n SORTBY 2 @n DESC LIMIT 0 10 ",
        "FT.SEARCH idx q NOCONTENT VERBATIM WITHSCORES INKEYS 2 a b RETURN 1 f LIMIT 0 10 DIALECT 2 ",
        "CLIENT KILL ID 1 TYPE normal USER u ADDR a:1 LADDR b:2 SKIPME yes MAXAGE 1 ",
        "FT.AGGREGATE idx * GROUPBY 1 @a REDUCE COUNT 0 GROUPBY 1 @b REDUCE COUNT 0 GROUPBY 1 @c REDUCE COUNT 0 ",
    ];
    for line in lines {
        time("hint", line, |line| {
            let _ = hint(line);
        });
        time("complete", line, |line| {
            let _ = complete(line, line.len());
        });
        time("edited", line, |line| {
            Prompt::default().edited(line, line.len());
        });
    }
}
