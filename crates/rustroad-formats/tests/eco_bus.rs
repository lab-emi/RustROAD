// SPDX-License-Identifier: Apache-2.0
//! Explicit bus ECOs preserve declaration order and every unselected sibling.
use rustroad_formats::eco::{
    self, BufferCell, EcoCells, EcoLimits, EcoOp, EcoPlan, PinRef, PinView,
};
use rustroad_formats::verilog::{self, Expr, VerilogLimits};

fn cells(range: [i64; 2]) -> EcoCells {
    let mut c = EcoCells::default();
    c.buffers.push(BufferCell {
        name: "BUF".into(),
        input_pin: "A".into(),
        output_pin: "Y".into(),
        function: "A".into(),
    });
    c.pin_buses.insert("M".into(), [("A".into(), range)].into());
    c.pin_directions.insert(
        "M".into(),
        (range[0].min(range[1])..=range[0].max(range[1]))
            .map(|i| (format!("A[{i}]"), "INPUT".into()))
            .chain([("Z".into(), "OUTPUT".into())])
            .collect(),
    );
    c.pin_directions.insert(
        "BUF".into(),
        [("A".into(), "INPUT".into()), ("Y".into(), "OUTPUT".into())].into(),
    );
    c
}
fn parse(source: &str) -> verilog::Netlist {
    verilog::parse_verilog(source, &VerilogLimits::default()).expect("valid fixture")
}
fn selected(n: &verilog::Netlist, c: &EcoCells, pin: &str) -> Expr {
    PinView::new(n, c, &VerilogLimits::default())
        .expect("valid metadata")
        .get(&n.instances[0], pin)
        .expect("connected bit")
}
#[test]
fn ascending_and_descending_formals_follow_original_expression_order() {
    let n = parse("module top(input [7:4] a,output z); M m(.A(a[7:4]),.Z(z)); endmodule");
    assert_eq!(
        selected(&n, &cells([3, 0]), "A[2]"),
        Expr::BitSelect {
            name: "a".into(),
            index: 6
        }
    );
    assert_eq!(
        selected(&n, &cells([0, 3]), "A[1]"),
        Expr::BitSelect {
            name: "a".into(),
            index: 6
        }
    );
    let n = parse("module top(input [4:7] a,output z); M m(.A(a),.Z(z)); endmodule");
    assert_eq!(
        selected(&n, &cells([3, 0]), "A[2]"),
        Expr::BitSelect {
            name: "a".into(),
            index: 5
        }
    );
}
#[test]
fn chained_bit_edits_retain_grouped_port_and_siblings() {
    let n = parse("module top(input [3:0] a,output z); M m(.A(a),.Z(z)); endmodule");
    let c = cells([3, 0]);
    let plan = EcoPlan {
        schema: eco::ECO_PLAN_SCHEMA.into(),
        module: "top".into(),
        operations: (0..2)
            .map(|i| EcoOp::InsertBuffer {
                cell: "BUF".into(),
                instance: format!("b{i}"),
                net: format!("b{i}_n"),
                sink: PinRef {
                    instance: "m".into(),
                    pin: "A[1]".into(),
                },
            })
            .collect(),
    };
    let (out, report) = eco::apply(
        &n,
        &plan,
        &c,
        &EcoLimits::default(),
        &VerilogLimits::default(),
    )
    .expect("bit ECO");
    assert_eq!(report.buffers_inserted, 2);
    assert_eq!(selected(&out, &c, "A[1]"), Expr::Ident("b1_n".into()));
    for i in [0, 2, 3] {
        assert_eq!(
            selected(&out, &c, &format!("A[{i}]")),
            Expr::BitSelect {
                name: "a".into(),
                index: i
            }
        );
    }
    assert_eq!(out.instances[0].connections[0].pin, "A");
    assert_eq!(
        out.instances[2].connections[0].signal,
        Expr::Ident("b0_n".into())
    );
    assert_eq!(
        n.instances[0].connections[0].signal,
        Expr::Ident("a".into()),
        "original AST remains immutable"
    );
}
#[test]
fn grouped_sinks_on_one_bus_share_one_identity_buffer() {
    let n = parse("module top(input a,output z); M m(.A({a,a,a,a}),.Z(z)); endmodule");
    let c = cells([3, 0]);
    let p = EcoPlan {
        schema: eco::ECO_PLAN_SCHEMA.into(),
        module: "top".into(),
        operations: vec![EcoOp::BufferSinks {
            cell: "BUF".into(),
            instance: "b".into(),
            net: "b_n".into(),
            sinks: [1, 2]
                .iter()
                .map(|i| PinRef {
                    instance: "m".into(),
                    pin: format!("A[{i}]"),
                })
                .collect(),
            ports: vec![],
        }],
    };
    let (out, _) = eco::apply(&n, &p, &c, &EcoLimits::default(), &VerilogLimits::default())
        .expect("two bits of one bus");
    assert_eq!(selected(&out, &c, "A[1]"), Expr::Ident("b_n".into()));
    assert_eq!(selected(&out, &c, "A[2]"), Expr::Ident("b_n".into()));
    assert_eq!(selected(&out, &c, "A[0]"), Expr::Ident("a".into()));
}
#[test]
fn concat_constants_and_four_state_siblings_are_preserved() {
    let c = cells([3, 0]);
    let n = parse(
        "module top(input s,input [3:0] a,output z); M m(.A({s,a[3:2],1'b0}),.Z(z)); endmodule",
    );
    assert_eq!(selected(&n, &c, "A[3]"), Expr::Ident("s".into()));
    assert_eq!(selected(&n, &c, "A[0]"), Expr::Constant("1'b0".into()));
    let n = parse("module top(output z); M m(.A(4'bx0z1),.Z(z)); endmodule");
    for (i, b) in [(3, "x"), (2, "0"), (1, "z"), (0, "1")] {
        assert_eq!(
            selected(&n, &c, &format!("A[{i}]")),
            Expr::Constant(format!("1'b{b}"))
        );
    }
    let n = parse("module top(output z); M m(.A('1),.Z(z)); endmodule");
    assert_eq!(selected(&n, &c, "A[3]"), Expr::Constant("1'b1".into()));
}
#[test]
fn widths_directions_unknown_schemas_and_budgets_fail_closed() {
    let c = cells([3, 0]);
    let n = parse("module top(input [2:0] a,output z); M m(.A(a),.Z(z)); endmodule");
    assert!(
        PinView::new(&n, &c, &VerilogLimits::default())
            .unwrap()
            .get(&n.instances[0], "A[1]")
            .is_err()
    );
    let mut mixed = c.clone();
    mixed
        .pin_directions
        .get_mut("M")
        .unwrap()
        .insert("A[2]".into(), "OUTPUT".into());
    assert!(PinView::new(&n, &mixed, &VerilogLimits::default()).is_err());
    let n = parse("module top(input [3:0] a,output z); M m(.A(a),.Z(z)); endmodule");
    let mut absent = c.clone();
    absent.pin_buses.clear();
    assert!(
        PinView::new(&n, &absent, &VerilogLimits::default())
            .unwrap()
            .get(&n.instances[0], "A[1]")
            .is_err()
    );
    let tiny = VerilogLimits {
        max_expr_nodes: 1,
        ..Default::default()
    };
    assert!(
        PinView::new(&n, &c, &tiny)
            .unwrap()
            .get(&n.instances[0], "A[1]")
            .is_err()
    );
    let mut huge = c.clone();
    huge.pin_buses
        .get_mut("M")
        .unwrap()
        .insert("A".into(), [0, 65536]);
    assert!(PinView::new(&n, &huge, &VerilogLimits::default()).is_err());
    assert!(c.check_sink("M", "m", "Z").is_err());
}
#[test]
fn literal_bracket_scalar_is_not_inferred_as_a_bus() {
    let n = parse("module top(input a,output z); LITERAL m(.\\A[0] (a),.Z(z)); endmodule");
    let c = EcoCells::default();
    assert_eq!(selected(&n, &c, "A[0]"), Expr::Ident("a".into()));
}

fn grouped_plan() -> EcoPlan {
    EcoPlan {
        schema: eco::ECO_PLAN_SCHEMA.into(),
        module: "top".into(),
        operations: vec![EcoOp::BufferSinks {
            cell: "BUF".into(),
            instance: "b".into(),
            net: "b_n".into(),
            sinks: [1, 2]
                .iter()
                .map(|i| PinRef {
                    instance: "m".into(),
                    pin: format!("A[{i}]"),
                })
                .collect(),
            ports: vec![],
        }],
    }
}
#[test]
fn grouped_bus_aliases_follow_declared_vectors_and_preserve_assigns() {
    let c = cells([3, 0]);
    let n = parse(
        "module top(input a,output z); wire [0:3] v; wire [7:4] w; assign v={a,a,a,a}; assign w=v; M m(.A(w),.Z(z)); endmodule",
    );
    let (out, _) = eco::apply(
        &n,
        &grouped_plan(),
        &c,
        &EcoLimits::default(),
        &VerilogLimits::default(),
    )
    .expect("vector aliases denote the same physical scalar net");
    assert_eq!(
        out.assigns, n.assigns,
        "alias assignments must remain unchanged"
    );
    assert_eq!(selected(&out, &c, "A[1]"), Expr::Ident("b_n".into()));
    assert_eq!(selected(&out, &c, "A[2]"), Expr::Ident("b_n".into()));
    assert_eq!(
        selected(&out, &c, "A[0]"),
        Expr::BitSelect {
            name: "w".into(),
            index: 4
        }
    );
    let n = parse(
        "module top(input a,output z); wire [3:0] v; assign v[0]=a; assign v[1]=a; assign v[2]=a; assign v[3]=a; M m(.A(v[3:0]),.Z(z)); endmodule",
    );
    assert!(
        eco::apply(
            &n,
            &grouped_plan(),
            &c,
            &EcoLimits::default(),
            &VerilogLimits::default()
        )
        .is_ok()
    );
}
#[test]
fn bit_alias_cycles_multiple_drivers_and_literal_names_fail_closed() {
    let c = cells([3, 0]);
    for source in [
        "module top(output z); wire [3:0] a; wire [3:0] b; assign a=b; assign b=a; M m(.A(a),.Z(z)); endmodule",
        "module top(input a,output z); wire [3:0] v; assign v={a,a,a,a}; assign v[1]=a; M m(.A(v),.Z(z)); endmodule",
        "module top(input a,input b,output z); wire [3:0] v; wire \\v[1] ; assign v={a,a,a,a}; assign \\v[1] =b; M m(.A({v[3],v[2],\\v[1] ,v[0]}),.Z(z)); endmodule",
    ] {
        let n = parse(source);
        assert!(
            eco::apply(
                &n,
                &grouped_plan(),
                &c,
                &EcoLimits::default(),
                &VerilogLimits::default()
            )
            .is_err(),
            "must reject {source}"
        );
    }
    let n = parse(
        "module top(input a,output z); wire [3:0] v; assign v={a,a,a,a}; M m(.A(v),.Z(z)); endmodule",
    );
    let tiny = VerilogLimits {
        max_expr_nodes: 8,
        ..Default::default()
    };
    assert!(eco::apply(&n, &grouped_plan(), &c, &EcoLimits::default(), &tiny).is_err());
}
