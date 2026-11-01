use rustroad_formats::verilog::{VerilogError, VerilogLimits, parse_verilog_reader};
use std::io::{self, BufReader, Cursor, Read};

const DESIGN: &str = "module top(input a, output y); /* λ🙂 */ wire n; BUF u0(.A(a),.Y(n)); BUF u1(.A(n),.Y(y)); endmodule // final comment";

#[test]
fn tokens_and_utf8_comments_can_cross_every_reader_boundary() {
    for capacity in 1..=32 {
        let parsed = parse_verilog_reader(
            BufReader::with_capacity(capacity, DESIGN.as_bytes()),
            &VerilogLimits::default(),
        )
        .expect("legal structural Verilog must parse across buffer boundaries");
        assert_eq!(parsed.module, "top", "capacity {capacity}");
        assert_eq!(parsed.instances.len(), 2, "capacity {capacity}");
        assert_eq!(parsed.instances[1].name, "u1", "capacity {capacity}");
        assert_eq!(parsed.ports.len(), 2, "capacity {capacity}");
    }
}

#[test]
fn streaming_checks_trailing_pragmas_and_encoding() {
    for suffix in [
        b" /* SYNOPSYS translate_off */".as_slice(),
        b" /* unfinished",
        b" /* \xff */",
        b" /* \xed\xa0\x80 */",
        b" // \xf0\x9f",
    ] {
        let input = Cursor::new(b"module top(); endmodule").chain(Cursor::new(suffix));
        assert!(
            parse_verilog_reader(
                BufReader::with_capacity(1, input),
                &VerilogLimits::default()
            )
            .is_err(),
            "invalid trailing input must not be skipped: {suffix:?}",
        );
    }
}

#[test]
fn byte_and_token_budgets_apply_to_the_entire_stream() {
    let input = b"module top(); endmodule";
    let exact = VerilogLimits {
        max_input_bytes: input.len(),
        max_tokens: 6,
        ..VerilogLimits::default()
    };
    assert!(parse_verilog_reader(input.as_slice(), &exact).is_ok());
    for limits in [
        VerilogLimits {
            max_tokens: 5,
            ..exact.clone()
        },
        VerilogLimits {
            max_input_bytes: input.len() - 1,
            ..exact.clone()
        },
    ] {
        assert!(matches!(
            parse_verilog_reader(input.as_slice(), &limits),
            Err(VerilogError::Budget(_))
        ));
    }
    let trailing = Cursor::new(input).chain(io::repeat(b' ').take(1024));
    assert!(matches!(
        parse_verilog_reader(BufReader::new(trailing), &exact),
        Err(VerilogError::Budget(_))
    ));
}

#[test]
fn large_comments_do_not_need_a_retained_comment_buffer() {
    let reader = Cursor::new(b"module top(); /*")
        .chain(io::repeat(b'a').take(2 * 1024 * 1024))
        .chain(Cursor::new(b"*/ endmodule"));
    let parsed = parse_verilog_reader(
        BufReader::with_capacity(7, reader),
        &VerilogLimits::default(),
    )
    .expect("large ordinary comments remain bounded lexer work");
    assert!(parsed.instances.is_empty());
}

struct FailingReader;
impl Read for FailingReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("injected read failure"))
    }
}

#[test]
fn io_failure_after_endmodule_is_not_accepted_as_eof() {
    let reader = Cursor::new(b"module top(); endmodule").chain(FailingReader);
    assert!(matches!(
        parse_verilog_reader(
            BufReader::with_capacity(1, reader),
            &VerilogLimits::default()
        ),
        Err(VerilogError::Io(_))
    ));
}
