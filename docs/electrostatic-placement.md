# Bounded electrostatic target prototype

The optional `placer.electrostatic` configuration adds unseeded CPU target
generation after the existing B2B/bisection placement has produced a legal
baseline. The common retargeting, legalizer, detailed placement and legality
check process the proposed targets. The final exact integer raw and weighted
center HPWL must both be no worse than that baseline. A rejected proposal keeps
the baseline and reports its reason.

Omitting the option preserves the legacy serialization and execution branch.
Seeded placement retains its existing algorithm and records
`not_applied_seeded` when this option is present. Seed refinement is outside
this prototype. Combining `anchor_isolated` with the option is an explicit
configuration error; `retarget_isolated` keeps its existing final targeting
behavior.

For example, an experimental configuration can contain:

```json
{
  "electrostatic": {
    "target_density_milli": 600,
    "max_iterations": 200,
    "max_evaluations": 512,
    "max_backtracks": 4,
    "workers": 4
  }
}
```

These parameters are heuristics requiring component and physical qualification.
The target density is bounded by 700 milli-units. It is a numerical parameter;
physical density, routing, timing and foundry rules have separate acceptance
gates.

## Numerical model

Stored design geometry and published positions remain integer DBU. The
workspace adapter supplies the actual integer core rectangle. A bounded local
integer view subtracts the core origin before any floating-point conversion.
Both axes then use the same scale `L=max(core_width,core_height)`. Supported
integer differences are at most 2^52 DBU. Candidate legalization uses that local
view and translates checked integer positions back to the original core.

Movable wire pins use physical cell centers and fixed pins retain the existing
adapter coordinates. Net weights, repeated pins and the existing fanout cutoff
keep their original meanings. The wire objective is weighted-average
wirelength with shifted exponentials, shifted weighted means and analytic
derivatives. Every inner reduction has a fixed order.

The grid is 64 by 64. Exact integer rectangle/bin intersections compute free
capacity from the existing intervals, which already exclude taps, fixed
halos and placement blockages. Cell spacing is enforced by the common legalizer.
It is not subtracted again from capacity or applied between disconnected
intervals.

Physical area is deposited with a uniform rectangle convolved with a
unit-integral hat of one bin radius in each axis. Bin weights integrate that
kernel. Truncated weights are normalized with the quotient-rule derivative,
which preserves each object's total mass. Extremely thin normalized kernel
extents are rejected explicitly. Numerical fillers carry the positive slack
between target free capacity and real-cell area; they have no pins, are placed
deterministically in capacity-bin order, and never enter the physical output.

Let `q=M-target*C`, `rho=q/bin_area`, with the arithmetic mean removed.
The symmetric discrete finite-volume Neumann operator is solved by direct
separable orthonormal cosine transforms. The constant mode is zero and every
other eigenvalue is positive. Energy and force are

```text
D = 0.5 * sum(q * phi)
dD/dcenter_i = area_i * sum(phi * dweight_i/dcenter_i)
```

The force is the adjoint of the same normalized deposition used in the energy.
With normalized lengths, energy has length^4 units, its gradient length^3,
and the weight in `WA + lambda*D` has length^-3. The three stages freeze
smoothing and density weight separately. The initial density weight uses a
guarded wire/density gradient-norm ratio. Stage changes restart momentum and
secant history.

Projected accelerated steps use an area/weighted-degree diagonal
preconditioner, bounded secant estimates, a majorization check and at most four
backtracks. Objective increases restart momentum. Nonfinite state, failed
mass/residual checks and insufficient trial budgets produce explicit fallback
diagnostics. A bounded line-search stall after a
strictly improved exact-target checkpoint retains that checkpoint for the
common legalizer and final no-worse gate; a stall without it falls back. The iteration horizon is
reported as a horizon, without a convergence-rate claim.

The mathematical basis is the published formulation in
[Lu et al., ePlace, TODAES 2015](https://doi.org/10.1145/2699873). This prototype
uses its own fixed-grid discrete operator and differentiated deposition; it
does not reproduce the paper's implementation or performance claims.

## Admission and work

| Quantity | Hard ceiling |
| --- | ---: |
| Movable cells | 100000 |
| Numerical fillers | 4096 |
| Pin occurrences | 1000000 |
| Nets | 100000 |
| Free intervals | 250000 |
| Grid bins | 4096 |
| Touched bins/object | 256 |
| Forward plus adjoint interactions/evaluation | 2000000 |
| Accepted iterations | 200 |
| Objective evaluations | 512 |
| Backtracks per attempt | 4 |
| Numerical workers | 4 |
| New-state allowance | 256 MiB |
| Total original placement work | 20 billion |

Additional counters bound cumulative pin evaluations, density interactions,
transform terms and capacity intersections. Admission checks the requested
evaluation envelope against each remaining budget. Common retargeting,
legalization and detailed-placement work shares the additional-work ceiling.
The opt-in unseeded baseline and refinement also share a hard 20-billion
total ceiling even when a core caller supplies a larger placement limit.
Maximum-sized inputs are not promised the maximum horizon. In particular, the
conservative worst-case support admission may reject a design whose actual
deposition would use fewer bins; the report identifies this budget fallback.
Every trial recomputes the wire objective,
deposition and full Poisson potential.

A conservative state estimate is
`256*(cells+fillers)+64*pins+96*nets+112*intervals+1MiB`, including the retained
legal baseline, translated view, exact checkpoint, numerical histories,
pin/net scratch and fixed grids. It is about 124 MiB at the simultaneous input
ceilings, within the unchanged 256 MiB allowance. Loaded design, PDK and snapshot memory are separate. The intended
Ibex experiment is estimated below 1 GiB total and requires a 2 GiB/512 MiB/180 s
process guard. No cell-by-bin matrix or per-iteration transcript is retained.

Independent transform output jobs preserve fixed serial inner sums.
Deposition and global scalar reductions use object/bin order. The caller's
thread pool is bounded further to four active numerical jobs.

## Qualification scope

Numerical acceptance requires an independently assembled finite-volume dense
solve, independent high-precision wire/deposition expectations and
step-refined finite differences, including boundary truncation, conservation
and large common translations. These tests are separate from the EDA oracle.

Default physical outputs must retain exact dual-PDK identity. Opt-in physical
acceptance requires an independent actual-pin DEF/LEF checker for site/row,
orientation, overlaps, obstacles, displacement and density, followed by the
original full foundry and timing gates. Surrogate energy and unlegalized
wirelength do not establish physical QoR or the 1.0M um final HPWL goal.
