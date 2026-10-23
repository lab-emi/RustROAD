//! Strict primary-clock SDC subset; data only, never a Tcl interpreter.
//! Budgets and scope: docs/routing-budget.md.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockConstraints {
    pub name: String,
    pub port: String,
    pub period_ns: f64,
    pub waveform_ns: [f64; 2],
    pub transition_ns: f64,
    pub uncertainty_ns: f64,
    pub max_transition_ns: Option<f64>,
    pub max_capacitance_pf: Option<f64>,
    pub max_fanout: Option<usize>,
    pub propagated: bool,
}
impl ClockConstraints {
    pub fn validate(&self) -> Result<(), String> {
        if !name(&self.name) || !name(&self.port) {
            return Err("SDC_UNSUPPORTED: literal scalar clock/port name required".into());
        }
        if !self.period_ns.is_finite()
            || self.period_ns <= 0.0
            || self.waveform_ns.iter().any(|v| !v.is_finite() || *v < 0.0)
            || self.waveform_ns[0] >= self.waveform_ns[1]
            || self.waveform_ns[1] >= self.waveform_ns[0] + self.period_ns
            || !self.transition_ns.is_finite()
            || self.transition_ns < 0.0
            || !self.uncertainty_ns.is_finite()
            || self.uncertainty_ns < 0.0
            || self
                .max_transition_ns
                .is_some_and(|v| !v.is_finite() || v <= 0.0)
            || self
                .max_capacitance_pf
                .is_some_and(|v| !v.is_finite() || v <= 0.0)
            || self.max_fanout.is_some_and(|v| !(2..=1024).contains(&v))
        {
            return Err("SDC_INVALID: clock value outside supported finite range".into());
        }
        Ok(())
    }
}
fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
fn value(s: &str) -> Result<f64, String> {
    let n = s
        .parse::<f64>()
        .map_err(|_| format!("SDC_INVALID: numeric literal required: {s}"))?;
    if !n.is_finite() || n < 0.0 {
        return Err("SDC_INVALID: nonfinite/negative value".into());
    }
    Ok(n)
}

/// Split only literal words, braces and public collection expressions. Tcl
/// substitutions/quoting/escapes are rejected, rather than partly evaluated.
fn words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut brace = 0i32;
    let mut bracket = 0i32;
    for ch in line.chars() {
        if matches!(ch, '$' | '\\' | '"') {
            return Err("SDC_UNSUPPORTED: Tcl substitution/escape/quote".into());
        }
        if ch == '{' {
            brace += 1;
        }
        if ch == '[' {
            bracket += 1;
        }
        if brace > 1 || bracket > 1 {
            return Err("SDC_UNSUPPORTED: nested expression".into());
        }
        if ch == '}' {
            brace -= 1;
        }
        if ch == ']' {
            bracket -= 1;
        }
        if brace < 0 || bracket < 0 {
            return Err("SDC_INVALID: unmatched delimiter".into());
        }
        if ch.is_whitespace() && brace == 0 && bracket == 0 {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if brace != 0 || bracket != 0 {
        return Err("SDC_INVALID: unmatched delimiter".into());
    }
    if !word.is_empty() {
        out.push(word);
    }
    if out.len() > 32 {
        return Err("SDC_BUDGET: statement tokens".into());
    }
    Ok(out)
}
fn collection(s: &str, query: &str, object: &str) -> bool {
    let Some(inner) = s.strip_prefix('[').and_then(|v| v.strip_suffix(']')) else {
        return false;
    };
    let parts: Vec<_> = inner.split_whitespace().collect();
    parts.len() == 2 && parts[0] == query && parts[1].trim_matches(['{', '}']) == object
}
fn design(s: &str) -> bool {
    s == "[current_design]"
}

pub fn parse(input: &str) -> Result<ClockConstraints, String> {
    if input.len() > 1024 * 1024 {
        return Err("SDC_BUDGET: input exceeds 1 MiB".into());
    }
    let mut clock: Option<ClockConstraints> = None;
    let mut count = 0usize;
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // This subset deliberately requires one complete command per line.
        if line.len() > 8192 || line.contains(';') {
            return Err("SDC_UNSUPPORTED: one bounded command per line required".into());
        }
        count += 1;
        if count > 10_000 {
            return Err("SDC_BUDGET: command count".into());
        }
        let w = words(line)?;
        let Some(command) = w.first() else {
            continue;
        };
        if command == "set_units" {
            if clock.is_some() || w.len() != 5 || w[1..] != ["-time", "ns", "-capacitance", "pF"] {
                return Err(
                    "SDC_UNSUPPORTED: use set_units -time ns -capacitance pF before create_clock"
                        .into(),
                );
            }
            continue;
        }
        if command == "create_clock" {
            if clock.is_some() {
                return Err("SDC_UNSUPPORTED: one primary clock supported".into());
            }
            let mut n = None;
            let mut period = None;
            let mut waveform = None;
            let mut port = None;
            let mut i = 1;
            while i < w.len() {
                match w[i].as_str() {
                    "-name" if n.is_none() && i + 1 < w.len() => {
                        n = Some(w[i + 1].clone());
                        i += 2;
                    }
                    "-period" if period.is_none() && i + 1 < w.len() => {
                        period = Some(value(&w[i + 1])?);
                        i += 2;
                    }
                    "-waveform" if waveform.is_none() && i + 1 < w.len() => {
                        let a = w[i + 1]
                            .strip_prefix('{')
                            .and_then(|s| s.strip_suffix('}'))
                            .ok_or("SDC_INVALID: waveform braces required")?;
                        let a: Vec<_> = a.split_whitespace().collect();
                        if a.len() != 2 {
                            return Err("SDC_UNSUPPORTED: two clock waveform edges required".into());
                        }
                        waveform = Some([value(a[0])?, value(a[1])?]);
                        i += 2;
                    }
                    p if port.is_none() && p.starts_with("[get_ports ") => {
                        let inner = p
                            .strip_prefix("[get_ports ")
                            .and_then(|s| s.strip_suffix(']'))
                            .unwrap()
                            .trim()
                            .trim_matches(['{', '}']);
                        if !name(inner) {
                            return Err("SDC_UNSUPPORTED: one literal clock port required".into());
                        }
                        port = Some(inner.to_owned());
                        i += 1;
                    }
                    _ => return Err(format!("SDC_UNSUPPORTED: create_clock option {}", w[i])),
                }
            }
            let period = period.ok_or("SDC_INVALID: missing period")?;
            let port = port.ok_or("SDC_INVALID: missing clock port")?;
            clock = Some(ClockConstraints {
                name: n.unwrap_or_else(|| port.clone()),
                port,
                period_ns: period,
                waveform_ns: waveform.unwrap_or([0.0, period / 2.0]),
                transition_ns: 0.0,
                uncertainty_ns: 0.0,
                max_transition_ns: None,
                max_capacitance_pf: None,
                max_fanout: None,
                propagated: false,
            });
            continue;
        }
        let c = clock
            .as_mut()
            .ok_or("SDC_INVALID: create_clock must precede clock constraints")?;
        if command == "set_propagated_clock"
            && w.len() == 2
            && collection(&w[1], "get_clocks", &c.name)
        {
            c.propagated = true;
            continue;
        }
        if w.len() != 3 {
            return Err(format!("SDC_UNSUPPORTED: {command} command form"));
        }
        let v = value(&w[1])?;
        let clock_target = collection(&w[2], "get_clocks", &c.name);
        match command.as_str() {
            "set_clock_transition" if clock_target => c.transition_ns = v,
            "set_input_transition" if collection(&w[2], "get_ports", &c.port) => {
                c.transition_ns = v
            }
            "set_clock_uncertainty" if clock_target => c.uncertainty_ns = v,
            "set_max_transition" if clock_target || design(&w[2]) => c.max_transition_ns = Some(v),
            "set_max_capacitance" if design(&w[2]) => c.max_capacitance_pf = Some(v),
            "set_max_fanout"
                if design(&w[2]) && v.fract() == 0.0 && (2.0..=1024.0).contains(&v) =>
            {
                c.max_fanout = Some(v as usize)
            }
            _ => return Err(format!("SDC_UNSUPPORTED: {command} or collection scope")),
        }
    }
    let c = clock.ok_or("SDC_INVALID: no primary clock")?;
    c.validate()?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_primary_clock_scope() {
        let c=parse("set_units -time ns -capacitance pF\ncreate_clock -name core -period 10 -waveform {0 4} [get_ports {clk}]\nset_clock_transition 0.1 [get_clocks core]\nset_clock_uncertainty 0.05 [get_clocks core]\nset_max_fanout 2 [current_design]\nset_max_capacitance 0.5 [current_design]\nset_max_transition 1 [get_clocks core]\nset_propagated_clock [get_clocks core]\n").unwrap();
        assert_eq!(c.waveform_ns, [0.0, 4.0]);
        assert_eq!(c.max_fanout, Some(2));
        assert!(c.propagated);
    }
    #[test]
    fn unsupported_or_ambiguous_semantics_fail_closed() {
        for s in [
            "create_generated_clock -name c",
            "create_clock -period NaN [get_ports clk]",
            "create_clock -period 0 [get_ports clk]",
            "create_clock -period 5 [get_ports *]",
            "create_clock -period 5 [get_ports clk]\nset_false_path -from [get_ports clk]",
            "create_clock -period 5 [get_ports clk]\nset_max_transition 1 [get_clocks other]",
            "source file.sdc",
            "create_clock -period $p [get_ports clk]",
            "create_clock -period 5 [get_ports clk];",
        ] {
            assert!(parse(s).is_err(), "{s}");
        }
        assert!(
            parse(&"a".repeat(1024 * 1024 + 1))
                .unwrap_err()
                .contains("BUDGET")
        );
    }
}
