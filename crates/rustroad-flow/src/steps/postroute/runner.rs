//! The screening loop, its child processes and its records.
use super::super::{
    StepError, StepResult, dict_of, fail, file_id, key, peak_rss_kib, read_json, read_text, render,
    require, str_of,
};
use super::policy::{CORNERS, Families, Thresholds, families, plan};
use super::{lef, liberty, netlist};
use rustroad_pyjson::{Dict, FileId, Layout, Value, list, pure_path, sha256_hex};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CODE: &str = "POSTROUTE";
const SCHEMA: &str = "rusteda.native-postroute-inputs.v3";
const MAX_ROUNDS: i128 = 3;
const MAX_RESIZES: i128 = 1024;
const MAX_ROWS: usize = 100_000;

fn need(ok: bool, message: impl Into<String>) -> StepResult<()> {
    require(ok, CODE, message)
}

fn identity(path: &Path) -> StepResult<FileId> {
    file_id(path, u64::MAX, CODE)
}

fn id_value(path: &Path) -> StepResult<Value> {
    Ok(identity(path)?.to_value())
}

fn read(path: &Path, cap: u64) -> StepResult<Value> {
    read_json(path, cap, CODE)
}

fn text<'a>(value: &'a Value, what: &str) -> StepResult<&'a str> {
    str_of(value, CODE, what)
}

fn get<'a>(dict: &'a Dict, name: &str) -> StepResult<&'a Value> {
    key(dict, name, CODE)
}

fn sub<'a>(dict: &'a Dict, name: &str) -> StepResult<&'a Dict> {
    dict_of(get(dict, name)?, CODE, name)
}

/// `tmp.write_text(json.dumps(value, indent=2) + '\n'); os.replace(tmp, path)`.
fn save(path: &Path, value: &Value) -> StepResult<()> {
    let mut rendered = render(value, Layout::Indent(2), CODE)?;
    rendered.push('\n');
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, rendered)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn head(workspace: &Path) -> StepResult<String> {
    let path = workspace.join("HEAD");
    if !path.exists() {
        return Ok("rev0".into());
    }
    Ok(super::super::text::strip(&read_text(&path, 4096, CODE)?).to_owned())
}

/// `round(seconds, 3)`.
fn round3(seconds: f64) -> f64 {
    format!("{seconds:.3}").parse().unwrap_or(seconds)
}

fn strings(value: &Value, what: &str) -> StepResult<Vec<String>> {
    let items = value
        .as_list()
        .ok_or_else(|| fail(CODE, format!("{what}: list required")))?;
    items
        .iter()
        .map(|item| text(item, what).map(str::to_owned))
        .collect()
}

struct Job {
    name: String,
    argv: Vec<String>,
    child: Child,
    started: Instant,
    wall: Option<f64>,
}

struct Screen {
    source: &'static str,
    workspace: PathBuf,
    revision: String,
    spef: FileId,
    antenna: Value,
    extraction: Value,
    record: Value,
}

struct Runner {
    c: Dict,
    out: PathBuf,
    inputs: FileId,
    started: Instant,
    commands: Vec<Value>,
    final_workspace: PathBuf,
    final_spef: PathBuf,
    threads: String,
    deadline: f64,
    libs: Option<[liberty::Library; 2]>,
    families: Families,
}

impl Runner {
    fn new(inputs_path: &Path, out: &Path) -> StepResult<Self> {
        let config = read(inputs_path, 4 << 20)?;
        let c = dict_of(&config, CODE, "inputs")?.clone();
        need(
            c.get("schema").and_then(Value::as_str) == Some(SCHEMA)
                && c.has_exactly(&[
                    "schema",
                    "top",
                    "trial",
                    "final_workspace",
                    "prects_netlist",
                    "recipe",
                    "profiles",
                    "timing",
                    "tools",
                    "policy",
                    "limits",
                    "final_spef",
                    "screen",
                    "antenna",
                ])
                && sub(&c, "tools")?.has_exactly(&["rustroad", "rusteda", "ruststa"]),
            "post-route input schema",
        )?;
        let p = sub(&c, "policy")?;
        let number = |name: &str| -> StepResult<f64> {
            get(p, name)?
                .as_f64()
                .ok_or_else(|| fail(CODE, "post-route policy budget"))
        };
        let (near, target) = (number("near_limit_ratio")?, number("target_ratio")?);
        let whole = |name: &str| -> StepResult<i128> {
            get(p, name)?
                .as_int()
                .ok_or_else(|| fail(CODE, "post-route policy budget"))
        };
        need(
            (0.5..=1.0).contains(&near)
                && 0.3 <= target
                && target < near
                && (1..=MAX_ROUNDS).contains(&whole("max_rounds")?)
                && (0..=MAX_RESIZES).contains(&whole("max_resizes")?),
            "post-route policy budget",
        )?;
        let recipe = sub(&c, "recipe")?;
        for name in ["import", "place", "cts", "route", "extract"] {
            let args = get(recipe, name)?.as_list().unwrap_or_default();
            need(
                (1..=128).contains(&args.len()) && args.iter().all(|a| a.as_str().is_some()),
                format!("recipe {name}"),
            )?;
        }
        let count = |name: &str, marker: &str| -> StepResult<usize> {
            Ok(strings(get(recipe, name)?, name)?
                .iter()
                .filter(|a| *a == marker)
                .count())
        };
        need(
            count("import", "@NETLIST@")? == 1
                && count("extract", "@SPEF@")? == 1
                && count("route", "--prior-workspace")? == 0,
            "recipe markers",
        )?;
        for group in ["tools", "timing"] {
            for (_, record) in sub(&c, group)?.iter() {
                let path = record
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(CODE, "file identity schema"))?;
                need(
                    &identity(Path::new(path))?.to_value() == record,
                    format!("input changed: {path}"),
                )?;
            }
        }
        let limits = sub(&c, "limits")?;
        let threads = get(limits, "threads")?
            .as_int()
            .ok_or_else(|| fail(CODE, "limits.threads"))?
            .to_string();
        let deadline = get(limits, "deadline_seconds")?
            .as_f64()
            .ok_or_else(|| fail(CODE, "limits.deadline_seconds"))?;
        let final_workspace = PathBuf::from(pure_path(text(
            get(&c, "final_workspace")?,
            "final_workspace",
        )?));
        need(
            !final_workspace.exists() && final_workspace.parent() == Some(out),
            "final workspace must be fresh, inside the runner",
        )?;
        let mut runner = Self {
            inputs: identity(inputs_path)?,
            out: out.to_owned(),
            started: Instant::now(),
            commands: Vec::new(),
            final_workspace,
            final_spef: PathBuf::new(),
            threads,
            deadline,
            libs: None,
            families: Families::default(),
            c,
        };
        runner.screen_inputs()?;
        Ok(runner)
    }

    fn tool(&self, name: &str) -> StepResult<&str> {
        text(get(sub(sub(&self.c, "tools")?, name)?, "path")?, name)
    }

    fn trial(&self) -> StepResult<(&str, &str)> {
        let trial = sub(&self.c, "trial")?;
        Ok((
            text(get(trial, "workspace")?, "trial.workspace")?,
            text(get(trial, "revision")?, "trial.revision")?,
        ))
    }

    /// The screened trial evidence, rendered by the producer as file identities.
    fn screen_inputs(&mut self) -> StepResult<()> {
        let (trial, revision) = self.trial()?;
        need(
            Path::new(trial).is_absolute() && revision == "rev4",
            "trial route binding",
        )?;
        let final_spef = PathBuf::from(pure_path(text(get(&self.c, "final_spef")?, "final_spef")?));
        need(
            final_spef.is_absolute()
                && final_spef.parent() == self.out.parent()
                && std::fs::symlink_metadata(&final_spef).is_err(),
            "final SPEF must be a fresh path beside the runner workspace",
        )?;
        let (screen, antenna) = (sub(&self.c, "screen")?, sub(&self.c, "antenna")?);
        need(
            screen.has_exactly(&["spef", "extract_report", "extract_command"])
                && antenna.has_exactly(&["inputs", "lineage", "verifier_inputs", "verification"]),
            "post-route screen input schema",
        )?;
        need(
            get(antenna, "verifier_inputs")?.is_null() == get(antenna, "verification")?.is_null(),
            "antenna verifier inputs and result go together",
        )?;
        for item in screen
            .values()
            .chain(antenna.values().filter(|v| !v.is_null()))
        {
            self.record(item)?;
        }
        self.final_spef = final_spef;
        Ok(())
    }

    /// A file identity rendered earlier in this run; the bytes must be unchanged.
    fn record(&self, item: &Value) -> StepResult<PathBuf> {
        let id = FileId::from_value(item)
            .filter(|id| Path::new(&id.path).is_absolute())
            .ok_or_else(|| fail(CODE, "file identity schema"))?;
        need(
            identity(Path::new(&id.path))? == id,
            format!("input changed: {}", id.path),
        )?;
        Ok(PathBuf::from(id.path))
    }

    fn remaining(&self) -> StepResult<f64> {
        let left = self.deadline - self.started.elapsed().as_secs_f64();
        need(left > 30.0, "post-route deadline")?;
        Ok(left)
    }

    /// Start one bounded child; `finish` records its row.
    fn spawn(&self, name: &str, argv: Vec<String>) -> StepResult<Job> {
        let log = self.out.join("logs");
        std::fs::create_dir_all(&log)?;
        let create = |suffix: &str| -> StepResult<File> {
            Ok(OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(log.join(format!("{name}.{suffix}")))?)
        };
        let (stdout, stderr) = (create("stdout")?, create("stderr")?);
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .env("RAYON_NUM_THREADS", &self.threads)
            .spawn()
            .map_err(|e| fail(CODE, format!("{name}: cannot start {}: {e}", argv[0])))?;
        Ok(Job {
            name: name.to_owned(),
            argv,
            child,
            started: Instant::now(),
            wall: None,
        })
    }

    fn abandon(jobs: &mut [Job]) {
        for job in jobs {
            if matches!(job.child.try_wait(), Ok(None)) {
                let _ = job.child.kill();
            }
            let _ = job.child.wait();
        }
    }

    /// Wait for every job under the deadline, then record rows in job order.
    fn finish(&mut self, mut jobs: Vec<Job>, cap: u64) -> StepResult<Vec<PathBuf>> {
        let mut pause = Duration::from_micros(50);
        loop {
            let mut pending = false;
            for job in &mut jobs {
                if job.wall.is_none() {
                    match job.child.try_wait() {
                        Ok(Some(_)) => job.wall = Some(round3(job.started.elapsed().as_secs_f64())),
                        Ok(None) => pending = true,
                        Err(error) => {
                            Self::abandon(&mut jobs);
                            return Err(error.into());
                        }
                    }
                }
            }
            if !pending {
                break;
            }
            if let Err(error) = self.remaining() {
                Self::abandon(&mut jobs);
                return Err(error);
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(2));
        }
        let log = self.out.join("logs");
        let mut outputs = Vec::new();
        for job in &mut jobs {
            let status = job.child.wait()?;
            // A signal death has no exit code; Python reports it as -signal.
            let code = status.code().unwrap_or_else(|| {
                use std::os::unix::process::ExitStatusExt;
                -status.signal().unwrap_or(0)
            });
            let stdout = log.join(format!("{}.stdout", job.name));
            let stderr = log.join(format!("{}.stderr", job.name));
            let (stdout_id, stderr_id) = (identity(&stdout)?, identity(&stderr)?);
            self.commands.push(Value::Dict(
                Dict::new()
                    .with("name", job.name.as_str())
                    .with("argv", list(job.argv.iter()))
                    .with("exit_code", code)
                    .with("wall_seconds", job.wall.unwrap_or(0.0))
                    .with("stdout", stdout_id.to_value())
                    .with("stderr", stderr_id.to_value()),
            ));
            need(
                stdout_id.bytes <= cap && stderr_id.bytes <= 64 << 20,
                format!("command output budget: {}", job.name),
            )?;
            if code != 0 {
                let tail = String::from_utf8_lossy(&std::fs::read(&stderr)?).into_owned();
                let start = tail.char_indices().rev().nth(1999).map_or(0, |(at, _)| at);
                return Err(fail(
                    CODE,
                    format!("{} exited {code}: {}", job.name, &tail[start..]),
                ));
            }
            outputs.push(stdout);
        }
        Ok(outputs)
    }

    fn run(&mut self, name: &str, argv: Vec<String>, cap: u64) -> StepResult<PathBuf> {
        self.remaining()?;
        let job = self.spawn(name, argv)?;
        let mut outputs = self.finish(vec![job], cap)?;
        outputs
            .pop()
            .ok_or_else(|| fail(CODE, "child process record"))
    }

    fn road(
        &mut self,
        name: &str,
        workspace: &Path,
        args: &[String],
        cap: u64,
    ) -> StepResult<PathBuf> {
        let mut argv = vec![
            self.tool("rustroad")?.to_owned(),
            "--workspace".into(),
            workspace.display().to_string(),
        ];
        argv.extend(args.iter().cloned());
        self.run(name, argv, cap)
    }

    fn libs(&self) -> StepResult<&[liberty::Library; 2]> {
        self.libs
            .as_ref()
            .ok_or_else(|| fail(CODE, "Liberty tables not loaded"))
    }

    /// Stub Liberty cells for LEF macros that are physical only.
    fn physical_models(
        &self,
        netlist: &Path,
        root: &Path,
    ) -> StepResult<([PathBuf; 2], Vec<String>)> {
        let profiles = sub(&self.c, "profiles")?;
        let lef_text = read_text(
            Path::new(text(get(profiles, "cell_lef")?, "cell_lef")?),
            256 << 20,
            CODE,
        )?;
        let logical = read_text(netlist, 256 << 20, CODE)?;
        let names = lef::physical_only_cells(&lef_text, self.libs()?, &logical)?;
        let mut paths = Vec::new();
        for corner in CORNERS {
            let path = root.join(format!("{corner}-physical.lib"));
            std::fs::write(&path, lef::stub_library(corner, &names))?;
            paths.push(path);
        }
        let [tt, ss] =
            <[PathBuf; 2]>::try_from(paths).map_err(|_| fail(CODE, "physical stub libraries"))?;
        Ok(([tt, ss], names))
    }

    /// RustSTA TT/SS (propagated clocks, SPEF RC) on one netlist and SPEF.
    /// The four read-only report queries of rev2 run at once; their rows are
    /// recorded in the fixed sequential order.
    #[allow(clippy::type_complexity)]
    fn sta(
        &mut self,
        tag: &str,
        root: &Path,
        spef: &Path,
        netlist: &Path,
    ) -> StepResult<(Value, Vec<(&'static str, Vec<Value>)>)> {
        let (stubs, names) = self.physical_models(netlist, root)?;
        let sta = root.join("sta").display().to_string();
        let (rusteda, ruststa) = (
            self.tool("rusteda")?.to_owned(),
            self.tool("ruststa")?.to_owned(),
        );
        let th = self.threads.clone();
        let s = |items: &[&str]| -> Vec<String> { items.iter().map(|i| (*i).to_owned()).collect() };
        self.run(
            &format!("{tag}-sta-init"),
            s(&[
                &rusteda,
                "init",
                "-w",
                &sta,
                "--format",
                "json",
                "--threads",
                &th,
            ]),
            256 << 20,
        )?;
        let mut argv = s(&[
            &rusteda,
            "import",
            "-w",
            &sta,
            "--base-rev",
            "rev0",
            "--threads",
            &th,
            "--format",
            "json",
            "--max-spef-mib",
            "128",
            "--max-rc-name-mib",
            "128",
            "--max-rc-nodes",
            "2000000",
            "--max-rc-elements",
            "4000000",
            "--max-rc-nets",
            "100000",
        ]);
        let timing = sub(&self.c, "timing")?;
        let timing_path = |name: &str| -> StepResult<String> {
            Ok(text(get(sub(timing, name)?, "path")?, name)?.to_owned())
        };
        let spef_text = spef.display().to_string();
        for (corner, stub) in CORNERS.iter().zip(&stubs) {
            for lib in [
                timing_path(&format!("std_{corner}"))?,
                timing_path(&format!("ram_{corner}"))?,
                stub.display().to_string(),
            ] {
                argv.extend(s(&["--lib", &lib, "--corner", corner]));
            }
            argv.extend(s(&["--spef", &spef_text, "--spef-corner", corner]));
        }
        argv.extend(s(&[
            "--netlist",
            &netlist.display().to_string(),
            "--sdc",
            &timing_path("sdc")?,
        ]));
        self.run(&format!("{tag}-sta-import"), argv, 256 << 20)?;
        self.run(
            &format!("{tag}-sta-run"),
            s(&[
                &ruststa,
                "run",
                "-w",
                &sta,
                "--base-rev",
                "rev1",
                "--threads",
                &th,
                "--backend",
                "cpu",
                "--precision",
                "f64",
                "--max-state-gib",
                "2",
                "--max-paths",
                "32",
                "--max-limit-rows",
                &MAX_ROWS.to_string(),
                "--require-wire-model",
                "spef_rc",
                "--format",
                "json",
            ]),
            256 << 20,
        )?;
        self.remaining()?;
        let mut jobs = Vec::new();
        for corner in CORNERS {
            let common = [
                &ruststa,
                "report",
                "-w",
                &sta,
                "--rev",
                "rev2",
                "--corner",
                corner,
                "--threads",
                &th,
                "--section",
            ];
            let mut summary = s(&common);
            summary.extend(s(&["summary", "--format", "json"]));
            let mut drv = s(&common);
            drv.extend(s(&[
                "drv",
                "--filter",
                "slack<0.2",
                "--limit",
                &MAX_ROWS.to_string(),
                "--format",
                "json",
            ]));
            for (kind, argv) in [("summary", summary), ("drv", drv)] {
                match self.spawn(&format!("{tag}-{corner}-{kind}"), argv) {
                    Ok(job) => jobs.push(job),
                    Err(error) => {
                        Self::abandon(&mut jobs);
                        return Err(error);
                    }
                }
            }
        }
        let outputs = self.finish(jobs, 256 << 20)?;
        let mut corners = Vec::new();
        let mut metrics_by_corner = Dict::new();
        for (index, corner) in CORNERS.into_iter().enumerate() {
            let summary = read(&outputs[2 * index], 64 << 20)?;
            let drv = read(&outputs[2 * index + 1], 64 << 20)?;
            let drv = dict_of(&drv, CODE, "DRV report")?;
            let scope = sub(drv, "scope")?;
            let field = |name: &str| scope.get(name).and_then(Value::as_str);
            need(
                field("wire_model") == Some("spef_rc")
                    && field("clock_model") == Some("propagated")
                    && field("corner") == Some(corner),
                "post-route STA scope",
            )?;
            let section = sub(sub(drv, "sections")?, "drv")?;
            let rows = get(section, "rows")?
                .as_list()
                .ok_or_else(|| fail(CODE, "DRV rows"))?;
            let shown = get(section, "shown")?;
            need(
                shown == get(section, "matched")?
                    && *shown == Value::Int(rows.len() as i128)
                    && rows.len() <= MAX_ROWS
                    && !section.get("truncated").is_some_and(Value::truthy)
                    && !section.get("unevaluated").is_some_and(Value::truthy),
                "post-route DRV report truncated/unevaluated",
            )?;
            // `{**drv.metrics, **summary.metrics}`: the summary wins.
            let lookup = |name: &str| -> Value {
                [summary.get("metrics"), drv.get("metrics")]
                    .into_iter()
                    .flatten()
                    .find_map(|metrics| metrics.get(name))
                    .cloned()
                    .unwrap_or(Value::Null)
            };
            let mut metrics = Dict::new();
            for name in [
                "worst_setup_slack_ns",
                "worst_hold_slack_ns",
                "setup_violating_endpoints",
                "hold_violating_endpoints",
                "max_transition_violating",
                "max_capacitance_violating",
                "max_fanout_violating",
            ] {
                metrics.insert(name, lookup(name));
            }
            metrics_by_corner.insert(corner, metrics);
            corners.push((corner, rows.to_vec()));
        }
        let summary = Value::Dict(
            Dict::new()
                .with("spef", id_value(spef)?)
                .with("netlist", id_value(netlist)?)
                .with("physical_only", list(names))
                .with("corners", metrics_by_corner),
        );
        Ok((summary, corners))
    }

    /// Equal-width sizing families from both corners' Liberty and the cell LEF.
    fn prepare(&mut self) -> StepResult<PathBuf> {
        let timing = sub(&self.c, "timing")?;
        let mut libs = Vec::new();
        let mut sources = Vec::new();
        for corner in CORNERS {
            let record = sub(timing, &format!("std_{corner}"))?;
            libs.push(liberty::read(Path::new(text(
                get(record, "path")?,
                "path",
            )?))?);
            sources.push(Value::Dict(
                Dict::new()
                    .with("role", format!("liberty_{corner}"))
                    .with("path", get(record, "path")?)
                    .with("sha256", get(record, "sha256")?)
                    .with("bytes", get(record, "bytes")?),
            ));
        }
        let libs = <[liberty::Library; 2]>::try_from(libs)
            .map_err(|_| fail(CODE, "Liberty corner tables"))?;
        let cell_lef = PathBuf::from(text(
            get(sub(&self.c, "profiles")?, "cell_lef")?,
            "cell_lef",
        )?);
        let sizes = lef::sizes(&read_text(&cell_lef, 256 << 20, CODE)?)?;
        self.families = families(&libs, &sizes)?;
        self.libs = Some(libs);
        let lef_id = identity(&cell_lef)?;
        sources.push(Value::Dict(
            Dict::new()
                .with("role", "cell_lef")
                .with("path", lef_id.path.as_str())
                .with("sha256", lef_id.sha256.as_str())
                .with("bytes", lef_id.bytes),
        ));
        let top = text(get(&self.c, "top")?, "top")?;
        let profile = Value::Dict(
            Dict::new()
                .with("schema", "rustroad.sizing-profile.v1")
                .with("pdk", "ihp-sg13g2")
                .with("library", format!("{top}-equal-width"))
                .with("generator", "native_postroute.py")
                .with("sources", list(sources))
                .with("dbu_per_um", 1000)
                .with(
                    "rule",
                    "equal LEF width/height, identical pins/directions/output functions in TT and SS; \
                     no sequential, bus, bundle, clock-gating or dont_use cell",
                )
                .with(
                    "families",
                    list(self.families.list.iter().map(super::policy::Family::to_value)),
                )
                .with("excluded", Value::List(Vec::new())),
        );
        let sizing = self.out.join("equal-width-sizing.json");
        save(&sizing, &profile)?;
        Ok(sizing)
    }

    fn plan(
        &self,
        corners: &[(&'static str, Vec<Value>)],
        netlist: &Path,
        resized: &Dict,
    ) -> StepResult<super::policy::Plan> {
        let p = sub(&self.c, "policy")?;
        let value = |name: &str| get(p, name).map(|v| v.as_f64().unwrap_or(0.0));
        let source = read_text(netlist, 256 << 20, CODE)?;
        let instances = netlist::instances(&source);
        need(
            !instances.is_empty(),
            format!("no instances parsed: {}", netlist.display()),
        )?;
        let prects_path = text(get(&self.c, "prects_netlist")?, "prects_netlist")?;
        let prects_text = read_text(Path::new(prects_path), 256 << 20, CODE)?;
        let prects = netlist::instances(&prects_text);
        need(
            !prects.is_empty(),
            format!("no instances parsed: {prects_path}"),
        )?;
        let known: HashSet<&str> = prects.0.iter().map(|(name, _)| name.as_str()).collect();
        let rows: Vec<(&str, &[Value])> = corners
            .iter()
            .map(|(corner, rows)| (*corner, rows.as_slice()))
            .collect();
        plan(
            &rows,
            &instances.0,
            &known,
            self.libs()?,
            &self.families,
            resized,
            &Thresholds {
                near_limit_ratio: value("near_limit_ratio")?,
                target_ratio: value("target_ratio")?,
                max_resizes: get(p, "max_resizes")?
                    .as_int()
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(0),
            },
        )
    }

    fn recipe(&self, name: &str) -> StepResult<Vec<String>> {
        strings(get(sub(&self.c, "recipe")?, name)?, name)
    }

    fn build(
        &mut self,
        tag: &str,
        netlist: &Path,
        prior: (&Path, &str),
    ) -> StepResult<(PathBuf, Value)> {
        let ws = self.out.join(tag);
        need(!ws.exists(), "fresh rebuild workspace")?;
        let netlist_text = netlist.display().to_string();
        let import: Vec<String> = self
            .recipe("import")?
            .into_iter()
            .map(|a| {
                if a == "@NETLIST@" {
                    netlist_text.clone()
                } else {
                    a
                }
            })
            .collect();
        let mut route = self.recipe("route")?;
        route.extend([
            "--prior-workspace".to_owned(),
            prior.0.display().to_string(),
            "--prior-rev".to_owned(),
            prior.1.to_owned(),
        ]);
        let mut stages: Vec<(&str, Value)> = Vec::new();
        for (stage, args) in [
            ("import", import),
            ("place", self.recipe("place")?),
            ("cts", self.recipe("cts")?),
            ("route", route),
        ] {
            let output = self.road(&format!("{tag}-{stage}"), &ws, &args, 256 << 20)?;
            stages.push((stage, read(&output, 64 << 20)?));
        }
        let revisions: Vec<&str> = stages
            .iter()
            .map(|(_, report)| report.get("revision").and_then(Value::as_str).unwrap_or(""))
            .collect();
        need(
            revisions == ["rev1", "rev2", "rev3", "rev4"] && head(&ws)? == "rev4",
            "rebuild revision chain",
        )?;
        let eco = stages[3].1.get("eco_prior").cloned().unwrap_or(Value::Null);
        need(
            eco.truthy()
                && eco.get("workspace").and_then(Value::as_str)
                    == Some(&prior.0.display().to_string())
                && eco.get("revision").and_then(Value::as_str) == Some(prior.1),
            "route did not use its prior",
        )?;
        let export = ["export-def".to_owned()];
        let mine = self.road(&format!("{tag}-def"), &ws, &export, 256 << 20)?;
        let theirs = self.road(&format!("{tag}-prior-def"), prior.0, &export, 256 << 20)?;
        let (mine, theirs) = (identity(&mine)?.sha256, identity(&theirs)?.sha256);
        let mut stage_stdout = Dict::new();
        for (stage, _) in &stages {
            stage_stdout.insert(
                *stage,
                id_value(&self.out.join("logs").join(format!("{tag}-{stage}.stdout")))?,
            );
        }
        let record = Value::Dict(
            Dict::new()
                .with("workspace", ws.display().to_string())
                .with("revisions", list(revisions))
                .with("eco_prior", eco)
                .with("stage_stdout", stage_stdout)
                .with("def_identical_to_prior", mine == theirs)
                .with("def_sha256", mine),
        );
        Ok((ws, record))
    }

    /// Apply the cumulative resizes to the pre-CTS netlist and rebuild with
    /// `prior` as ECO route prior. Returns the workspace and the round's
    /// `build` and `netlist` records.
    fn rebuild(
        &mut self,
        index: usize,
        ops: &[Dict],
        resized: &mut Dict,
        sizing: &Path,
        prior: (&Path, &str),
    ) -> StepResult<(PathBuf, Value, Value)> {
        for op in ops {
            let instance = text(get(op, "instance")?, "instance")?;
            resized.insert(instance, get(op, "cell")?);
        }
        let mut names: Vec<&str> = resized.keys().collect();
        names.sort_unstable();
        let operations = list(names.iter().map(|name| {
            Dict::new()
                .with("op", "resize_cell")
                .with("instance", *name)
                .with("cell", resized.get(name))
        }));
        let plan = self.out.join(format!("plan{index}.json"));
        save(
            &plan,
            &Value::Dict(
                Dict::new()
                    .with("schema", "rustroad.eco-plan.v1")
                    .with("module", get(&self.c, "top")?)
                    .with("operations", operations),
            ),
        )?;
        let source = self.out.join(format!("netlist{index}.v"));
        let profiles = sub(&self.c, "profiles")?;
        let profile =
            |name: &str| -> StepResult<String> { Ok(text(get(profiles, name)?, name)?.to_owned()) };
        let mut argv = vec![
            self.tool("rustroad")?.to_owned(),
            "apply-eco".into(),
            "--netlist".into(),
            text(get(&self.c, "prects_netlist")?, "prects_netlist")?.to_owned(),
            "--plan".into(),
            plan.display().to_string(),
            "--delay-cells".into(),
            profile("delay")?,
            "--cell-lef".into(),
            profile("cell_lef")?,
            "--sizing-profile".into(),
            sizing.display().to_string(),
            "--max-eco-operations".into(),
            resized.len().to_string(),
            "--max-input-mib".into(),
            "16".into(),
            "--eco-memory-mib".into(),
            "2048".into(),
            "--out".into(),
            source.display().to_string(),
        ];
        if profiles.get("cell_buses").is_some_and(Value::truthy) {
            argv.extend(["--cell-buses".to_owned(), profile("cell_buses")?]);
        }
        self.run(&format!("apply{index}"), argv, 256 << 20)?;
        let (ws, build) = self.build(&format!("build{index}"), &source, prior)?;
        Ok((ws, build, id_value(&source)?))
    }

    fn extract_argv(&self, workspace: &Path, spef: &str) -> StepResult<Vec<String>> {
        let mut argv = vec![
            self.tool("rustroad")?.to_owned(),
            "--workspace".into(),
            workspace.display().to_string(),
        ];
        argv.extend(
            self.recipe("extract")?
                .into_iter()
                .map(|a| if a == "@SPEF@" { spef.to_owned() } else { a }),
        );
        Ok(argv)
    }

    /// Bind one extraction to (workspace path, HEAD, design/routes bytes, exact argv).
    #[allow(clippy::too_many_arguments)]
    fn screened(
        &self,
        source: &'static str,
        workspace: &Path,
        lineage: &Value,
        antenna: Value,
        spef: &Value,
        report_path: &Path,
        argv: &[String],
    ) -> StepResult<Screen> {
        let revision = head(workspace)?;
        let workspace_text = workspace.display().to_string();
        let field = |name: &str| lineage.get(name).and_then(Value::as_str);
        need(
            field("workspace") == Some(&workspace_text)
                && field("initial_revision") == Some("rev4")
                && field("final_revision") == Some(&revision)
                && lineage.get("tool") == Some(get(sub(&self.c, "tools")?, "rustroad")?),
            "antenna lineage does not end at the screened HEAD",
        )?;
        let folder = workspace.join("revisions").join(&revision);
        let design = id_value(&folder.join("design.json"))?;
        let routes = id_value(&folder.join("routes.rbin"))?;
        let last = lineage.get("final");
        need(
            last.and_then(|f| f.get("revision")).and_then(Value::as_str) == Some(&revision)
                && last.and_then(|f| f.get("design_file")) == Some(&design),
            "screened revision differs from the antenna lineage final design",
        )?;
        let spef_id = FileId::from_value(spef).ok_or_else(|| fail(CODE, "file identity schema"))?;
        let expected = self.extract_argv(workspace, &spef_id.path)?;
        need(
            argv == expected.as_slice(),
            "extraction argv differs from the recipe template",
        )?;
        let report = read(report_path, 2 << 20)?;
        let profile = expected
            .iter()
            .position(|a| a == "--rc-profile")
            .and_then(|at| expected.get(at + 1))
            .ok_or_else(|| fail(CODE, "extraction recipe has no --rc-profile"))?;
        let field = |name: &str| report.get(name).and_then(Value::as_str);
        need(
            field("schema") == Some("rustroad.extract.v1")
                && field("status") == Some("ok")
                && field("workspace_revision") == Some(&revision)
                && report.get("output") == Some(spef)
                && report.get("rc_profile") == Some(&id_value(Path::new(profile))?),
            "extraction report does not bind its SPEF to the screened HEAD",
        )?;
        need(head(workspace)? == revision, "screened HEAD moved")?;
        let argv_text = render(&list(expected.iter()), Layout::Line, CODE)?;
        let extraction = Value::Dict(
            Dict::new()
                .with("argv_sha256", sha256_hex(argv_text.as_bytes()))
                .with("report", id_value(report_path)?)
                .with("spef", spef),
        );
        let record = Value::Dict(
            Dict::new()
                .with("source", source)
                .with("workspace", workspace_text)
                .with("revision", revision.as_str())
                .with(
                    "revision_files",
                    Dict::new()
                        .with("design.json", design)
                        .with("routes.rbin", routes),
                )
                .with("antenna", &antenna)
                .with("extraction", &extraction),
        );
        Ok(Screen {
            source,
            workspace: workspace.to_owned(),
            revision,
            spef: spef_id,
            antenna,
            extraction,
            record,
        })
    }

    /// The trial as the producer left it: antenna-repaired (and verified),
    /// extracted by the producer's extraction step.
    fn screen_trial(&self, trial: &Path) -> StepResult<Screen> {
        let a = sub(&self.c, "antenna")?;
        let lineage = read(&self.record(get(a, "lineage")?)?, 64 << 20)?;
        let verification = get(a, "verification")?;
        if !verification.is_null() {
            let review = read(&self.record(verification)?, 64 << 20)?;
            need(
                review.get("workspace").and_then(Value::as_str)
                    == Some(&trial.display().to_string())
                    && review.get("final_revision").and_then(Value::as_str) == Some(&head(trial)?)
                    && review.get("lineage") == Some(get(a, "lineage")?),
                "antenna verification does not bind the trial HEAD",
            )?;
        }
        let screen = sub(&self.c, "screen")?;
        let command = read(&self.record(get(screen, "extract_command")?)?, 64 << 20)?;
        // The producer runs every command under GNU time: time -v -o FILE argv...
        let argv = command.get("argv").and_then(Value::as_list);
        let argv = match argv {
            Some(argv)
                if argv.len() > 4
                    && argv[1].as_str() == Some("-v")
                    && argv[2].as_str() == Some("-o") =>
            {
                strings(&Value::List(argv[4..].to_vec()), "extraction command")?
            }
            _ => return Err(fail(CODE, "extraction command record")),
        };
        let antenna = Value::Dict(
            Dict::new()
                .with("inputs", get(a, "inputs")?)
                .with("lineage", get(a, "lineage")?)
                .with("verification", verification),
        );
        let report = self.record(get(screen, "extract_report")?)?;
        self.screened(
            "trial",
            trial,
            &lineage,
            antenna,
            get(screen, "spef")?,
            &report,
            &argv,
        )
    }

    /// Antenna-repair (and verify) an ECO rebuild with the producer's own
    /// session inputs; only the workspace differs.
    fn antenna(&mut self, index: usize, workspace: &Path) -> StepResult<(Value, Value)> {
        let a = sub(&self.c, "antenna")?.clone();
        let (trial, _) = self.trial()?;
        let trial = trial.to_owned();
        let rustroad = self.tool("rustroad")?.to_owned();
        let workspace_text = workspace.display().to_string();
        let template = read(&self.record(get(&a, "inputs")?)?, 64 << 20)?;
        let template = dict_of(&template, CODE, "antenna inputs")?;
        need(
            template.get("design_workspace").and_then(Value::as_str) == Some(&trial)
                && template.get("initial_revision").and_then(Value::as_str) == Some("rev4")
                && template.get("tool") == Some(get(sub(&self.c, "tools")?, "rustroad")?),
            "antenna input template differs",
        )?;
        need(
            head(workspace)? == "rev4",
            "rebuild HEAD before antenna repair",
        )?;
        let inputs = self.out.join(format!("antenna{index}-inputs.json"));
        save(
            &inputs,
            &Value::Dict(
                template
                    .clone()
                    .with("design_workspace", workspace_text.as_str()),
            ),
        )?;
        let work = self.out.join(format!("antenna{index}"));
        self.run(
            &format!("antenna{index}"),
            vec![
                rustroad.clone(),
                "antenna-session".into(),
                "--inputs".into(),
                inputs.display().to_string(),
                "--out-dir".into(),
                work.display().to_string(),
            ],
            16 << 20,
        )?;
        let lineage_path = work.join("lineage.json");
        let lineage = read(&lineage_path, 64 << 20)?;
        let (inputs_id, lineage_id) = (id_value(&inputs)?, id_value(&lineage_path)?);
        let mut verification = Value::Null;
        if !get(&a, "verifier_inputs")?.is_null() {
            let template = read(&self.record(get(&a, "verifier_inputs")?)?, 64 << 20)?;
            let template = dict_of(&template, CODE, "antenna verifier inputs")?;
            need(
                template.get("design_workspace").and_then(Value::as_str) == Some(&trial)
                    && template.get("lineage") == Some(get(&a, "lineage")?)
                    && template
                        .get("controller_inputs")
                        .unwrap_or(get(&a, "inputs")?)
                        == get(&a, "inputs")?,
                "antenna verifier input template differs",
            )?;
            let mut config = template
                .clone()
                .with("lineage", &lineage_id)
                .with("design_workspace", workspace_text.as_str());
            if template.contains_key("controller_inputs") {
                config.insert("controller_inputs", &inputs_id);
            }
            let path = self
                .out
                .join(format!("antenna{index}-verification-inputs.json"));
            save(&path, &Value::Dict(config))?;
            let review = self.out.join(format!("antenna{index}-verification"));
            self.run(
                &format!("antenna{index}-verify"),
                vec![
                    rustroad,
                    "verify-antenna-lineage".into(),
                    "--inputs".into(),
                    path.display().to_string(),
                    "--out-dir".into(),
                    review.display().to_string(),
                ],
                16 << 20,
            )?;
            let result = review.join("result.json");
            let record = read(&result, 64 << 20)?;
            need(
                record.get("workspace").and_then(Value::as_str) == Some(&workspace_text)
                    && record.get("final_revision").and_then(Value::as_str)
                        == Some(&head(workspace)?)
                    && record.get("lineage") == Some(&lineage_id),
                "rebuild antenna verification differs",
            )?;
            verification = id_value(&result)?;
        }
        let ids = Value::Dict(
            Dict::new()
                .with("inputs", inputs_id)
                .with("lineage", lineage_id)
                .with("verification", verification),
        );
        Ok((lineage, ids))
    }

    fn screen_rebuild(&mut self, index: usize, workspace: &Path) -> StepResult<Screen> {
        let (lineage, ids) = self.antenna(index, workspace)?;
        let root = self.out.join(format!("check{index}"));
        std::fs::create_dir(&root)?;
        let spef = root.join("native.nom.spef");
        let argv = self.extract_argv(workspace, &spef.display().to_string())?;
        let report = self.road(
            &format!("check{index}-extract"),
            workspace,
            &argv[3..],
            1 << 20,
        )?;
        self.screened(
            "eco_rebuild",
            workspace,
            &lineage,
            ids,
            &id_value(&spef)?,
            &report,
            &argv,
        )
    }

    /// The final workspace: every file of `source` hard-linked, fresh HEAD and lock.
    fn link_workspace(&self, source: &Path, revision: &str) -> StepResult<usize> {
        use std::os::unix::fs::MetadataExt;
        let target = &self.final_workspace;
        need(
            head(source)? == revision,
            "screened HEAD moved before linking",
        )?;
        let lock = File::open(source.join("workspace.lock"))?;
        // As a RustROAD reader: no commit meanwhile.
        fs2::FileExt::lock_shared(&lock)?;
        need(
            head(source)? == revision,
            "screened HEAD moved before linking",
        )?;
        std::fs::create_dir(target)?;
        let mut linked: Vec<PathBuf> = Vec::new();
        let mut folders = vec![PathBuf::new()];
        while let Some(relative) = folders.pop() {
            let here = source.join(&relative);
            let (mut dirs, mut files) = (Vec::new(), Vec::new());
            for entry in std::fs::read_dir(&here)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                // `os.walk`: a symlink to a directory is listed with the directories.
                if kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()) {
                    dirs.push(entry.file_name());
                } else {
                    files.push(entry.file_name());
                }
            }
            dirs.sort();
            files.sort();
            for name in &dirs {
                need(
                    !std::fs::symlink_metadata(here.join(name))?
                        .file_type()
                        .is_symlink(),
                    "workspace directory symlink",
                )?;
                std::fs::create_dir(target.join(&relative).join(name))?;
            }
            for name in files {
                let path = relative.join(&name);
                if path == Path::new("HEAD") || path == Path::new("workspace.lock") {
                    continue;
                }
                need(
                    std::fs::symlink_metadata(source.join(&path))?
                        .file_type()
                        .is_file(),
                    format!("workspace entry is not a regular file: {}", path.display()),
                )?;
                std::fs::hard_link(source.join(&path), target.join(&path))?;
                linked.push(path);
            }
            // Depth first, directories in sorted order.
            for name in dirs.into_iter().rev() {
                folders.push(relative.join(name));
            }
        }
        File::create(target.join("workspace.lock"))?;
        std::fs::write(target.join("HEAD"), format!("{revision}\n"))?;
        let mut same = true;
        for path in &linked {
            let (a, b) = (
                std::fs::metadata(source.join(path))?,
                std::fs::metadata(target.join(path))?,
            );
            same &= a.ino() == b.ino() && a.dev() == b.dev();
        }
        need(
            same && head(target)? == revision && head(source)? == revision,
            "final workspace links differ",
        )?;
        Ok(linked.len())
    }

    fn execute(&mut self) -> StepResult<String> {
        use std::os::unix::fs::MetadataExt;
        let (trial_text, trial_revision) = {
            let (workspace, revision) = self.trial()?;
            (workspace.to_owned(), revision.to_owned())
        };
        let trial = PathBuf::from(&trial_text);
        need(
            std::fs::canonicalize(&trial)? == trial,
            "trial workspace path must be canonical",
        )?;
        let p = sub(&self.c, "policy")?;
        let max_rounds = usize::try_from(get(p, "max_rounds")?.as_int().unwrap_or(0))
            .map_err(|_| fail(CODE, "post-route policy budget"))?;
        // The round-0 netlist export starts first; it overlaps input hashing and Liberty parsing.
        let mut job = vec![self.spawn(
            "check0-netlist",
            vec![
                self.tool("rustroad")?.to_owned(),
                "--workspace".into(),
                trial_text.clone(),
                "export-verilog".into(),
            ],
        )?];
        let prepared = self
            .screen_trial(&trial)
            .and_then(|screen| Ok((screen, self.prepare()?)));
        let (mut current, sizing) = match prepared {
            Ok(value) => value,
            Err(error) => {
                Self::abandon(&mut job);
                return Err(error);
            }
        };
        let mut netlist = self
            .finish(job, 256 << 20)?
            .pop()
            .ok_or_else(|| fail(CODE, "child process record"))?;
        need(
            head(&trial)? == current.revision,
            "trial HEAD moved during the netlist export",
        )?;
        let mut screens = vec![current.record.clone()];
        let mut resized = Dict::new();
        let mut rounds: Vec<Dict> = Vec::new();
        let status;
        let mut index = 0;
        loop {
            let tag = format!("check{index}");
            let root = self.out.join(&tag);
            if index > 0 {
                let export = ["export-verilog".to_owned()];
                let workspace = current.workspace.clone();
                netlist = self.road(&format!("{tag}-netlist"), &workspace, &export, 256 << 20)?;
            } else {
                std::fs::create_dir(&root)?;
            }
            let spef = PathBuf::from(&current.spef.path);
            let (summary, corners) = self.sta(&tag, &root, &spef, &netlist)?;
            let decided = self.plan(&corners, &netlist, &resized)?;
            let dicts = |items: &[Dict]| list(items.iter().cloned());
            let mut row = Dict::new()
                .with("check", summary)
                .with("workspace", current.workspace.display().to_string())
                .with("revision", current.revision.as_str())
                .with("source", current.source)
                .with("resizes", dicts(&decided.ops))
                .with("residual", dicts(&decided.residual))
                .with("near_limit_without_upsize", dicts(&decided.skipped));
            if decided.ops.is_empty() {
                status = if !decided.residual.is_empty() {
                    "residual_violations"
                } else if !resized.is_empty() {
                    "repaired"
                } else {
                    "clean"
                };
                rounds.push(row);
                break;
            }
            if index == max_rounds {
                status = "round_budget_exhausted";
                rounds.push(row);
                break;
            }
            let prior_workspace = current.workspace.clone();
            let prior_revision = current.revision.clone();
            let (workspace, build, source) = self.rebuild(
                index,
                &decided.ops,
                &mut resized,
                &sizing,
                (&prior_workspace, &prior_revision),
            )?;
            row.insert("build", build);
            row.insert("netlist", source);
            rounds.push(row);
            current = self.screen_rebuild(index + 1, &workspace)?;
            screens.push(current.record.clone());
            index += 1;
        }
        let last = current;
        need(
            rounds.last().and_then(|row| row.get("workspace"))
                == Some(&Value::Str(last.workspace.display().to_string())),
            "last screen differs from the last round",
        )?;
        let linked = self.link_workspace(&last.workspace, &last.revision)?;
        let spef = PathBuf::from(&last.spef.path);
        need(identity(&spef)? == last.spef, "screened SPEF changed")?;
        std::fs::hard_link(&spef, &self.final_spef)?;
        let (a, b) = (
            std::fs::metadata(&spef)?,
            std::fs::metadata(&self.final_spef)?,
        );
        need(
            a.ino() == b.ino() && a.dev() == b.dev(),
            "final SPEF link differs",
        )?;
        let executable = std::env::current_exe()?;
        let last_corners = rounds
            .last()
            .and_then(|row| row.get("check"))
            .and_then(|check| check.get("corners"))
            .cloned()
            .unwrap_or(Value::Null);
        let result = Dict::new()
            .with("schema", "rusteda.native-postroute-result.v2")
            .with("status", status)
            .with("top", get(&self.c, "top")?)
            .with("input_manifest", self.inputs.to_value())
            .with("policy_source", id_value(&executable)?)
            .with("tools", get(&self.c, "tools")?)
            .with("order", "screen_after_extract")
            .with(
                "trial",
                Dict::new()
                    .with("workspace", trial_text.as_str())
                    .with("revision", trial_revision.as_str()),
            )
            .with("screens", Value::List(screens))
            .with(
                "final_workspace",
                self.final_workspace.display().to_string(),
            )
            .with("final_revision", last.revision.as_str())
            .with("resized", Value::Dict(resized.clone()))
            .with(
                "final_source",
                if last.source == "trial" {
                    "trial_link"
                } else {
                    "eco_rebuild"
                },
            )
            .with(
                "final_link",
                Dict::new()
                    .with("method", "hard_link")
                    .with("source", last.workspace.display().to_string())
                    .with("linked_files", linked)
                    .with("fresh", list(["HEAD", "workspace.lock"])),
            )
            .with("final_spef", id_value(&self.final_spef)?)
            .with("final_extraction", &last.extraction)
            .with("final_antenna", &last.antenna)
            .with("sizing_profile", id_value(&sizing)?)
            .with("equal_width_families", self.families.list.len())
            .with("rounds", list(rounds))
            .with("last_check_corners", last_corners)
            .with("commands", Value::List(self.commands.clone()))
            .with(
                "scope",
                "native nominal SPEF, RustSTA TT/SS propagated; repair selection only, not signoff",
            )
            .with(
                "unverified",
                list(["signoff_timing", "openrcx_timing", "drc", "lvs", "antenna"]),
            )
            .with(
                "resources",
                Dict::new()
                    .with("wall_seconds", round3(self.started.elapsed().as_secs_f64()))
                    .with("peak_child_rss_kib", peak_rss_kib(true)),
            );
        save(&self.out.join("result.json"), &Value::Dict(result.clone()))?;
        let mut summary = Dict::new();
        for name in [
            "schema",
            "status",
            "final_workspace",
            "final_revision",
            "resized",
        ] {
            summary.insert(name, get(&result, name)?);
        }
        render(&Value::Dict(summary), Layout::Line, CODE)
    }
}

/// Run the step in a fresh `out_dir` and return its one-line result. A
/// failure leaves `result.json` with `status: "error"` beside the logs.
///
/// # Errors
/// `REVISION_CONFLICT` if `out_dir` exists; `POSTROUTE` for every schema,
/// binding, budget, child-process or deadline failure.
pub fn run(inputs: &Path, out_dir: &Path) -> StepResult<String> {
    if out_dir.exists() {
        return Err(fail(
            "REVISION_CONFLICT",
            "post-route workspace must be fresh",
        ));
    }
    std::fs::create_dir_all(out_dir)?;
    let out = std::fs::canonicalize(out_dir)?;
    let outcome = Runner::new(inputs, &out).and_then(|mut runner| runner.execute());
    if let Err(error) = &outcome {
        let record = Dict::new()
            .with("schema", "rusteda.native-postroute-result.v1")
            .with("status", "error")
            .with("error", error.to_string());
        save(&out.join("result.json"), &Value::Dict(record))?;
    }
    outcome.map_err(|error| StepError {
        code: if error.code == "STEP_IO" || error.code == "STEP_JSON" {
            CODE
        } else {
            error.code
        },
        message: error.message,
    })
}
