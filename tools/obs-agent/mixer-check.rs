//! Diagnostic entry point for the production integer mixer solver.
#[path = "maia-httpd/src/control_runtime/mixer_math.rs"]
mod mixer_math;
fn main() {
    let values = std::env::args()
        .skip(1)
        .map(|v| v.parse::<i64>())
        .collect::<Result<Vec<_>, _>>();
    let Ok(v) = values else {
        eprintln!("integer Hz required");
        std::process::exit(2)
    };
    if v.len() != 5 {
        eprintln!("usage: mixer-check RF_LOW RF_HIGH MULT PLL_MIN PLL_MAX");
        std::process::exit(2)
    }
    let Some(t) = mixer_math::choose([v[0], v[1]], v[2], [v[3], v[4]], None, None) else {
        eprintln!("no solution for [{},{})", v[0], v[1]);
        std::process::exit(1)
    };
    println!(
        "{} {} {} {} {}",
        t.pll, t.mixer_lo, t.side, t.if_edges[0], t.if_edges[1]
    );
}
