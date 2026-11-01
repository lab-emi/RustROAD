use super::*;
use crate::verilog::{parse_verilog, write_verilog};

#[test]
fn records_match_canonical_output_and_refuse_bad_references() {
    let limits = VerilogLimits::default();
    let netlist = parse_verilog("module m(input [1:0] a, output y); wire x; assign x=a[0]; BUF u(.A(x),.Y(y),.OPEN()); endmodule", &limits).unwrap();
    let mut expected = Vec::new();
    write_verilog(&netlist, &mut expected).unwrap();
    let mut actual = Vec::new();
    let mut writer = StreamWriter::new(&mut actual, &limits, 1 << 20, 10000);
    writer
        .module(
            &netlist.module,
            netlist
                .ports
                .iter()
                .map(|p| (p.name.as_str(), p.direction, p.range)),
        )
        .unwrap();
    for w in &netlist.wires {
        writer.wire(Cow::Borrowed(&w.name), w.range).unwrap();
    }
    for a in &netlist.assigns {
        writer.assign(&a.lhs, &a.rhs).unwrap();
    }
    for i in &netlist.instances {
        writer
            .instance(
                &i.cell,
                Cow::Borrowed(&i.name),
                i.connections
                    .iter()
                    .map(|c| (c.pin.as_str(), Cow::Borrowed(&c.signal))),
            )
            .unwrap();
    }
    writer.finish().unwrap();
    drop(writer);
    assert_eq!(actual, expected);
    let mut sink = std::io::sink();
    let mut writer = StreamWriter::new(&mut sink, &limits, 1 << 20, 10000);
    writer.module("m", [("a", Direction::Input, None)]).unwrap();
    assert!(
        writer
            .assign(&Expr::Ident("missing".into()), &Expr::Ident("a".into()))
            .is_err()
    );
}

#[test]
fn output_work_and_state_limits_are_errors() {
    let mut sink = std::io::sink();
    let tiny = VerilogLimits {
        max_input_bytes: 4,
        ..Default::default()
    };
    assert!(
        StreamWriter::new(&mut sink, &tiny, 1 << 20, 1000)
            .module("module", [])
            .is_err()
    );
    let limits = VerilogLimits::default();
    assert!(
        StreamWriter::new(&mut sink, &limits, 1, 1000)
            .module("m", [])
            .is_err()
    );
    assert!(
        StreamWriter::new(&mut sink, &limits, 1 << 20, 1)
            .module("m", [])
            .is_err()
    );
}

#[test]
fn explicit_instance_limit_exceeds_the_ordinary_writer_default() {
    // 300001 names plus borrowed-name state: <64MiB; output is discarded.
    let count = 300_001;
    let names: Vec<_> = (0..count).map(|i| format!("u{i}")).collect();
    let limits = VerilogLimits {
        max_instances: count,
        max_connections: count,
        max_expr_nodes: count,
        max_input_bytes: 64 << 20,
        ..Default::default()
    };
    let mut sink = std::io::sink();
    let expr = Expr::Ident("a".into());
    let mut writer = StreamWriter::new(&mut sink, &limits, 64 << 20, 20_000_000);
    writer.module("m", [("a", Direction::Input, None)]).unwrap();
    for name in &names {
        writer
            .instance("BUF", Cow::Borrowed(name), [("A", Cow::Borrowed(&expr))])
            .unwrap();
    }
    assert!(
        writer
            .instance("BUF", Cow::Borrowed("extra"), [("A", Cow::Borrowed(&expr))])
            .is_err()
    );
    assert!(
        writer.finish().is_err(),
        "a failed record poisons completion"
    );
}
