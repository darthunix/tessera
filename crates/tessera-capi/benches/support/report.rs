//! Presentation shared by all benchmarks; acceptance arithmetic lives elsewhere.

use super::{
    baseline::Run,
    measurement::{Assessment, Comparison, Measurement, Policy, Status},
};
use anyhow::Result;
use std::collections::BTreeMap;
use std::io::Write;

fn comparison(value: Comparison, status: Status, policy: Policy) -> String {
    let ratios = value.ratios;
    let delta = if policy == Policy::Filter {
        format!(", delta={:+.3}ns", value.delta_ns)
    } else {
        String::new()
    };
    format!(
        "{status} {:+.2}% [{:+.2}%, {:+.2}%]{delta}",
        (ratios.median - 1.) * 100.,
        (ratios.low - 1.) * 100.,
        (ratios.high - 1.) * 100.
    )
}

pub fn print(
    out: &mut impl Write,
    results: &BTreeMap<String, Measurement>,
    previous: Option<&Run>,
    paths: &[&'static str],
    policy: Policy,
) -> Result<bool> {
    let mut counts = [0; 3];
    let mut bad_controls = 0;
    writeln!(
        out,
        "\nPaired-ratio median [min, max series medians], not confidence intervals."
    )?;
    for (name, data) in results {
        let control = data.summary("control")?;
        let control_ok = control.ratios.control_ok();
        bad_controls += usize::from(!control_ok);
        writeln!(
            out,
            "CONTROL {name}: {} ref={:.3}ns self={:.3}ns ratio={:.4} [{:.4}, {:.4}]",
            if control_ok {
                Status::Pass
            } else {
                Status::Unstable
            },
            control.reference_ns,
            control.measured_ns,
            control.ratios.median,
            control.ratios.low,
            control.ratios.high
        )?;
        for &path in paths.iter().filter(|&&path| path != "control") {
            let summary = data.summary(path)?;
            let reference = data.comparison(path, None, policy)?;
            let change = previous
                .map(|old| data.comparison(path, Some(&old.measurements[name]), policy))
                .transpose()?;
            let assessment =
                Assessment::new(reference.status, change.map(|c| c.status), control_ok);
            counts[match assessment.status {
                Status::Pass => 0,
                Status::Fail => 1,
                Status::Unstable => 2,
            }] += 1;
            let old = change.map_or_else(
                || "not-compared".to_owned(),
                |c| comparison(c, assessment.previous.unwrap(), policy),
            );
            writeln!(
                out,
                "{} {name}/{path}: time={:.3}ns ref={:.3}ns reference={}; baseline={old}; {}",
                assessment.status,
                summary.measured_ns,
                summary.reference_ns,
                comparison(reference, assessment.reference, policy),
                assessment.reason
            )?;
        }
    }
    writeln!(
        out,
        "\n{} PASS, {} FAIL, {} UNSTABLE paths in {} cases; {bad_controls} unstable controls.",
        counts[0],
        counts[1],
        counts[2],
        results.len()
    )?;
    Ok(counts[1] == 0 && counts[2] == 0)
}
