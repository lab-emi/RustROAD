"""Narrow declaration policy tests; synthetic records are never signoff evidence."""
from pathlib import Path
import copy
import hashlib
import json
import sys
import subprocess
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import rtl2gds_deviation as V
import rtl2gds_design as D
import rtl2gds_timing as T
import rtl2gds_evidence as E
from mpw_probe import driver_waveforms, scale_resistance


class DeviationPolicy(unittest.TestCase):
    def setUp(self):
        work = Path(__file__).resolve().parents[2] / ".work"
        work.mkdir(exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=work)
        self.root = Path(self.temporary.name)
        self.binding = dict(pdk="sky130", corner="ss", pin="r1/CLK", kind="min_pulse_width_high",
                            liberty_sha256="a"*64, spef_sha256="b"*64, netlist_sha256="c"*64)
        self.original = dict(ruststa_required_ns=1.005, opensta_required_ns=1.0038,
                             ruststa_slack_ns=4.0, opensta_slack_ns=4.0012)
        artifacts = []
        for i in range(5):
            file = self.root / f"record{i}"
            file.write_text("synthetic test data\n")
            artifacts.append(dict(path=str(file), sha256=hashlib.sha256(file.read_bytes()).hexdigest()))
        self.proof = dict(
            schema=V.SCHEMA, binding=self.binding, original=self.original,
            nearby=dict(scale=.9, ruststa_required_ns=1.004, opensta_required_ns=1.00395,
                        ruststa_slack_ns=4.001, opensta_slack_ns=4.00105),
            step=dict(
                lo=dict(scale=.94995, ruststa_required_ns=1.0045, opensta_required_ns=1.00445,
                        ruststa_slack_ns=4.0, opensta_slack_ns=4.00005, opensta_driver_slew_ns=.102),
                hi=dict(scale=.95002, ruststa_required_ns=1.00450001, opensta_required_ns=1.0033,
                        ruststa_slack_ns=4.0, opensta_slack_ns=4.0012, opensta_driver_slew_ns=.101)),
            circuit=dict(model="transistor_rc_with_independent_driver_characterization",
                         points=[dict(ruststa_required_ns=1.002, opensta_required_ns=1.001,
                                      spice_required_ns=1.0025, uncertainty_ns=.0001)]),
            artifacts=artifacts)

    def tearDown(self):
        self.temporary.cleanup()

    def check(self, proof):
        return V.validate(proof, self.binding, self.original, self.root)

    def test_all_conditions_and_artifact_bindings_are_required(self):
        self.assertTrue(self.check(self.proof)["valid"])
        for change in ("binding", "nearby", "step", "circuit", "artifact"):
            p = copy.deepcopy(self.proof)
            if change == "binding":
                p["binding"]["spef_sha256"] = "d"*64
            elif change == "nearby":
                p["nearby"]["opensta_required_ns"] = 1.0
            elif change == "step":
                p["step"]["hi"]["opensta_required_ns"] = p["step"]["lo"]["opensta_required_ns"]
                p["step"]["hi"]["opensta_slack_ns"] = p["step"]["lo"]["opensta_slack_ns"]
            elif change == "circuit":
                p["circuit"]["points"][0]["spice_required_ns"] = 1.001
            else:
                p["artifacts"][0]["sha256"] = "f"*64
            self.assertFalse(self.check(p)["valid"], change)

    def test_declaration_is_visible_and_restricted_to_timing_accuracy(self):
        value = D.overall({"sky130":{"timing_accuracy":D.item("deviation_declared")}},
                          ["timing_accuracy"], {}, ["sky130"])
        self.assertEqual((value["status"], value["exit_code"]), ("deviation_declared", 0))
        self.assertEqual(value["declared_deviations"], ["sky130:timing_accuracy"])
        self.assertEqual(D.worst(["passed", "deviation_declared"]), "deviation_declared")
        self.assertEqual(D.worst(["deviation_declared", "unverified"]), "unverified")
        wrong = D.overall({"sky130":{"drv":D.item("deviation_declared")}}, ["drv"], {}, ["sky130"])
        self.assertEqual(wrong["status"], "unverified")

    def test_missing_and_malformed_proofs_fail_closed(self):
        for key in ("original", "nearby", "step", "circuit", "artifacts"):
            for wrong in (None, True, "bad", 3):
                p = copy.deepcopy(self.proof)
                p[key] = wrong
                self.assertFalse(self.check(p)["valid"], (key, wrong))
        p = copy.deepcopy(self.proof)
        p["step"]["hi"]["opensta_driver_slew_ns"] = p["step"]["lo"]["opensta_driver_slew_ns"]
        self.assertFalse(self.check(p)["valid"])

    def test_pulse_gate_keeps_original_error_and_rejects_other_failures(self):
        row = dict(pin="r1/CLK", kind="min_pulse_width_high", corner="ss", net="clock",
                   limit=1.005, slack=4.0)
        compat = dict(reports=dict(pulse_width=dict(rows=[row]), min_period=dict(rows=[])))
        ref = dict(mpw={"r1/CLK:high":dict(required=1.0038, slack=4.0012)}, period={})
        self.assertEqual(T.pulse_parity(compat, ref, "ss", .001)["status"], "failed")
        proof = self.check(self.proof)
        report = T.pulse_parity(compat, ref, "ss", .001, {"r1/CLK:high":proof})
        self.assertEqual(report["status"], "deviation_declared")
        self.assertGreater(report["max_difference_ns"], .001)
        self.assertEqual(report["declared_deviations"][0]["check"], "r1/CLK:high")
        ref["mpw"]["extra/CLK:high"] = dict(required=1.0, slack=4.0)
        self.assertEqual(T.pulse_parity(compat, ref, "ss", .001, {"r1/CLK:high":proof})["status"], "failed")

    def test_resistance_probe_changes_only_the_selected_net(self):
        source = self.root / "source.spef"
        source.write_text('*NAME_MAP\n*1 clock\n*2 data\n*PORTS\n*D_NET *1 0.1\n'
                          '*RES\n1 a b 100\n*END\n*D_NET *2 0.2\n*RES\n1 c d 200\n*END\n')
        target = self.root / "probe.spef"
        scale_resistance(source, target, "clock", .9)
        self.assertIn('1 a b 90\n', target.read_text())
        self.assertIn('1 c d 200\n', target.read_text())
        with self.assertRaises(D.GateError):
            scale_resistance(source, target, "clock", .5)

    def test_reference_waveform_observation_requires_both_transitions(self):
        text = ('A ^ -> X ^\ninput_net_transition = 0.1\nDriver waveform slew = 0.2\n...\n'
                'A v -> X v\ninput_net_transition = 0.11\nDriver waveform slew = 0.21\n...\n')
        self.assertEqual(driver_waveforms(text)["fall"]["slew_ns"], .21)
        with self.assertRaises(D.GateError):
            driver_waveforms(text.split('A v')[0])

    def test_model_hash_helper_loads_in_the_main_gate_import_environment(self):
        scripts = str(Path(__file__).resolve().parents[1])
        code = (f"import sys,types; sys.path.insert(0,{scripts!r}); "
                "sys.modules['signoff']=types.ModuleType('signoff'); "
                "from mpw_circuit import model_tree_digest; "
                f"assert model_tree_digest(__import__('pathlib').Path({str(self.root)!r}))[1] == 5")
        subprocess.run([sys.executable, "-c", code], check=True, timeout=10)

    def test_compact_report_preserves_declaration_and_unmeasured_values(self):
        proof = dict(self.check(self.proof), proof=self.proof)
        pulse = dict(status="deviation_declared", max_difference_ns=.0012,
                     declared_deviations=[dict(check="r1/CLK:high", difference_ns=.0012, evidence=proof)])
        row = dict(details={}, statuses={"timing_accuracy":dict(by_spef={"native":dict(corners={
            "ss":dict(endpoints=dict(max_difference_ns=None), pulse=pulse, drv=dict(status="passed"))})})})
        compact = E.timing(row)["parity"]["native"]["ss"]
        self.assertIsNone(compact["endpoints_ps"])
        self.assertEqual(compact["pulse_ps"], 1.2)
        self.assertEqual(compact["pulse_status"], "deviation_declared")
        self.assertEqual(compact["declared_deviations"][0]["original"], self.original)


if __name__ == "__main__":
    unittest.main()
