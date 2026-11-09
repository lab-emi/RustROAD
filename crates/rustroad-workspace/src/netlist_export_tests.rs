use super::*;
use rustroad_core::connectivity::{ConnectedDesign, Master, MasterPin, PinUse};

fn bundle(source: &str) -> PhysicalBundle {
    let netlist = verilog::parse_verilog(source, &VerilogLimits::default()).unwrap();
    let pin = |name: &str, direction, use_| MasterPin {
        name: name.into(),
        direction: Some(direction),
        use_,
        shape: None,
        ports: Vec::new(),
    };
    let master = |name: &str, pins| Master {
        buses: Default::default(),
        obstruction_spacing: Default::default(),
        name: name.into(),
        class: Some("CORE".into()),
        symmetry: Vec::new(),
        site: "s".into(),
        size: [1, 1],
        pins,
        obstructions: Vec::new(),
    };
    let instance_masters = netlist
        .instances
        .iter()
        .map(|i| u32::from(i.cell == "FILL"))
        .collect();
    PhysicalBundle {
        import_version: 1,
        technology: crate::pdk::Technology {
            dbu_per_micron: 1000,
            manufacturing_grid: Some(1),
            layers: Vec::new(),
            sites: Vec::new(),
            vias: Vec::new(),
        },
        netlist,
        connectivity: ConnectedDesign {
            layers: Vec::new(),
            masters: vec![
                master(
                    "BUF",
                    vec![
                        pin(
                            "A",
                            rustroad_core::connectivity::Direction::Input,
                            PinUse::Signal,
                        ),
                        pin(
                            "Y",
                            rustroad_core::connectivity::Direction::Output,
                            PinUse::Signal,
                        ),
                        pin(
                            "OPEN",
                            rustroad_core::connectivity::Direction::Output,
                            PinUse::Signal,
                        ),
                        pin(
                            "OTHER",
                            rustroad_core::connectivity::Direction::Output,
                            PinUse::Signal,
                        ),
                    ],
                ),
                master("FILL", Vec::new()),
            ],
            instance_masters,
            ports: Vec::new(),
            nets: Vec::new(),
            pins: Vec::new(),
        },
        sources: Vec::new(),
        diagnostics: Vec::new(),
        config: None,
        ties: None,
        generated_fillers: vec!["f".into()],
        generated_taps: Vec::new(),
        port_buffers: Vec::new(),
        input_buffers: None,
        clock_tree: None,
        routing: None,
        state: None,
        placement: None,
        net_weights: None,
        placement_seed: None,
    }
}

#[test]
fn view_matches_legacy_alias_cycles_open_pins_and_filtering() {
    for text in [
        "assign x=a; assign z=x;",
        "assign x=z; assign z=x;",
        "assign x=a; assign z={x};",
    ] {
        let source = format!(
            "module m(input a, output y); wire x,z,rr_open_0; assign rr_open_0=a; {text} BUF u(.A(z),.Y(y),.OPEN()); FILL f(); endmodule"
        );
        let bundle = bundle(&source);
        for mode in [SupplyMode::Logical, SupplyMode::Powered] {
            let mut old = Vec::new();
            crate::physical::export_verilog_legacy(&bundle, mode == SupplyMode::Powered, &mut old)
                .unwrap();
            let mut new = Vec::new();
            write(&bundle, mode, &ExportLimits::default(), &mut new).unwrap();
            assert_eq!(old, new, "mode {mode:?}, aliases {text}");
        }
    }
}

#[test]
fn alias_resolution_is_linear_and_retains_cycles() {
    let mut source = String::from("module m(input a,output y); wire x0;");
    for i in 1..5000 {
        source.push_str(&format!(" wire x{i}; assign x{i}=x{};", i - 1));
    }
    source.push_str(" assign x0=a; BUF u(.A(x4999),.Y(y)); FILL f(); endmodule");
    let bundle = bundle(&source);
    let mut work = 0;
    let folded = aliases(&bundle, &mut work, 100_000).unwrap();
    assert_eq!(folded.len(), 5000);
    assert_eq!(folded["x4999"], "a");
    assert!(work < 100_000);
    assert!(aliases(&bundle, &mut 0, 1).is_err());
}
