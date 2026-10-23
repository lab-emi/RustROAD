#!/usr/bin/env python3
"""Generate the local physical-preparation report from bounded verified data."""
import html
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(name):
    path = ROOT / "docs/evidence" / name
    if path.stat().st_size > 2 * 1024 * 1024:
        raise ValueError("report input exceeds 2 MiB")
    return json.loads(path.read_text())


def drawing(layout):
    x0, y0, x1, y1 = map(int, layout["die"])
    height = y1 - y0
    parts = [f'<svg viewBox="{x0} {y0} {x1-x0} {height}" role="img" aria-label="独立 OpenDB 读回的放置和供电布局"><g transform="translate(0 {y0+y1}) scale(1 -1)">']
    parts.append(f'<rect x="{x0}" y="{y0}" width="{x1-x0}" height="{height}" fill="#101a20"/>')
    for cell in layout["cells"]:
        color = "#345159" if cell["role"] == "filler" else "#efbf70" if cell["role"] == "tie" else "#72cfbb" if cell["orientation"] in ("R0", "N") else "#81aee5"
        x, y, w, h = (int(cell[k]) for k in ("x", "y", "width", "height"))
        parts.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{color}" stroke="#101a20" stroke-width="{max(1,height//450)}"><title>{html.escape(cell["name"]+" / "+cell["orientation"])}</title></rect>')
    for wire in layout["wires"]:
        a, b, c, d = map(int, wire["rect"])
        color = "#ff856f" if wire["net"] == "VDD" else "#71b7ff"
        parts.append(f'<rect x="{a}" y="{b}" width="{c-a}" height="{d-b}" fill="{color}" opacity=".85"/>')
    for io in layout["io"]:
        for shape in io["shapes"]:
            a, b, c, d = map(int, shape["rect"])
            parts.append(f'<rect x="{a}" y="{b}" width="{c-a}" height="{d-b}" fill="#fff1a8"><title>{html.escape(io["name"])}</title></rect>')
    parts.append('</g></svg>')
    return "".join(parts)


def main():
    result = read("power-io-results.json")
    if result["status"] != "passed" or {p["pdk"] for p in result["pdks"]} != {"sky130", "gf180"}:
        raise ValueError("report requires both passed PDKs")
    rows, scenes, timing = [], [], []
    for p in result["pdks"]:
        if p["power_geometry"]["status"] != "passed" or p["timing"]["status"] != "passed":
            raise ValueError("physical or timing verification absent")
        placed, geometry = p["placed"], p["power_geometry"]
        rows.append("<tr>" + "".join(f"<td>{html.escape(str(x))}</td>" for x in (
            p["pdk"], placed["cells"], placed["inserted_tie_cells"], placed["filler_cells"],
            geometry["flipped_instances"], geometry["pg_pins"], geometry["components"]["VDD"], geometry["components"]["VSS"])) + "</tr>")
        scenes.append(f'<section class="scene"><h3>{html.escape(p["pdk"])}</h3>{drawing(read("power-io-layout-"+p["pdk"]+".json"))}</section>')
        timing.append(f'<li>{html.escape(p["pdk"])}：最大 setup/hold 偏差 <code>{p["timing"]["opensta"]["max_difference_ns"]:.7g} ns</code>；14 个约束端点，2 个常量驱动 D 引脚缺少数据到达路径，已明确列出。</li>')
    text = r'''<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>RustROAD 0.3 · I/O、供电与 tie-cell</title><style>
:root{color-scheme:dark;--bg:#10191c;--panel:#18262b;--line:#31474e;--ink:#e5eeee;--muted:#aac0c5;--teal:#7ee3cc}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:16px/1.75 system-ui,-apple-system,sans-serif}main{max-width:1120px;margin:auto;padding:48px 24px 70px}h1{font-size:clamp(30px,5vw,56px);line-height:1.15;margin:16px 0 24px}h2{font-size:25px;margin:38px 0 16px}h3{margin:0 0 12px}a{color:var(--teal)}code{background:#0c1417;padding:3px 5px;font:14px ui-monospace,monospace}.tag{font-size:13px;letter-spacing:3px;color:var(--teal)}.lead{font-size:21px}.notice{border-left:3px solid #ffc987;padding:12px 18px;background:#282921}.stats,.scenes{display:grid;grid-template-columns:repeat(2,1fr);gap:18px;margin:24px 0}.card,.scene{padding:20px;border:1px solid var(--line);border-radius:10px;background:var(--panel)}.big{color:var(--teal);font-size:32px;font-weight:650}.scene svg{display:block;width:100%;height:auto;border:1px solid var(--line)}.muted{color:var(--muted)}.scroll{overflow:auto}table{width:100%;min-width:720px;border-collapse:collapse}th,td{border-bottom:1px solid var(--line);padding:12px;text-align:left}th{font-size:13px;color:var(--muted)}details{border:1px solid var(--line);border-radius:8px;padding:18px;margin:14px 0}summary{cursor:pointer;font-weight:600}pre{overflow:auto;padding:18px;background:#0c1417}footer{margin-top:38px;border-top:1px solid var(--line);padding-top:20px;color:var(--muted)}@media(max-width:740px){main{padding:28px 16px}.stats,.scenes{grid-template-columns:1fr}.lead{font-size:18px}}</style></head><body><main>
<div class="tag">RUSTROAD / 0.3.0 / 2026-10-23</div><h1>物理 I/O、主供电网络<br>与 tie-cell 准备已连通。</h1>
<p class="lead">在真实 Sky130、GF180 数据上完成边界 I/O 放置、常量 tie-cell 插入、filler 补齐、N/FS 行翻转，以及水平电源轨—固定 VIA—垂直供电主干的生成与独立校验。</p>
<p class="notice">当前检查的是<strong>主金属供电网络和物理准备</strong>。体偏置引脚已逻辑连接，但井/衬底接触、tap cell、闩锁效应、DRC/LVS、IR/EM 仍未签核。CTS、信号布线和寄生提取仍是后续阶段。</p>
<div class="stats"><div class="card"><div class="big">65 + 2</div>普通测试与真实 LEF 专测通过<br><span class="muted">另有双 PDK 外部语义/几何/时序 oracle</span></div><div class="card"><div class="big">每网 1 个连通分量</div>VDD 与 VSS 独立连通，无检测到的金属短接<br><span class="muted">连接按实际 OpenDB 线段与 VIA 几何检查</span></div></div>
<h2>独立读回的真实布局</h2><p class="muted">绿色/蓝色表示 N/FS 逻辑单元，灰色为 filler，金色为 tie cell；红/蓝细线为 VDD/VSS，淡黄色为 I/O。图形由 OpenDB 读回数据生成，不是已完成的 GUI；多层投影中的交叉不等于电气连接。</p><div class="scenes">__SCENES__</div>
<h2>双 PDK 结果</h2><div class="scroll"><table><thead><tr><th>工艺</th><th>物理单元</th><th>新增 tie</th><th>filler</th><th>FS 单元</th><th>PG 引脚</th><th>VDD 分量</th><th>VSS 分量</th></tr></thead><tbody>__ROWS__</tbody></table></div>
<p>每套工艺均有 20 个信号 I/O 和 2 个供电端口，全部具有固定物理矩形。8 行主电源轨的原生 pin 矩形连续，邻行共享边界极性一致。供电系统含水平轨、分别属于 VDD/VSS 的两条垂直主干及固定 VIA，导出的带电源 Verilog 与 OpenDB 完整端点分区一致。</p>
<details open><summary>哪些错误被验证器检出</summary><ul><li>相邻行电源极性错误。</li><li>删去 filler 后，原生单元电源轨形成缺口。</li><li>切断垂直主干，供电网络分裂。</li><li>引入 VDD/VSS 金属短接。</li><li>把 I/O 从芯片边界移入内部。</li></ul><p>验证器从独立的 OpenDB 矩形、实际 VIA 和 N/FS 坐标出发，使用有上限的几何连通检查；没有把 RustROAD 自己的通过标记当作 oracle。</p></details>
<h2>逻辑和时序仍有独立校验</h2><p>原生与 Yosys CEC、每套工艺 512 组仿真向量均通过。tie 高低输出由真实 Liberty 的函数和反向错误控制确认。普通 Verilog 导出保留逻辑接口，供电版导出保留 filler、PG 端口和引脚。</p><ul>__TIMING__</ul>
<p>时序采用理想线网、理想时钟，容差 <code>0.001 ns</code>；常量端点的覆盖限制没有被改写为零。增量检查为无修改更新，尚不代表完成实际 ECO 或寄生参数时序验证。</p>
<details><summary>实现、资源和收据</summary><p>所有权威几何为 i64 DBU。I/O、常量展开、filler、轨道区间和生成线段都有硬上限。版本 3 快照在原有连接表之外增加 <code>physical.rbin</code>；重新打开时重建并核对物理状态。写入仍由锁与 <code>--base-rev</code> 保护。</p>
<p>完整双 PDK 验证耗时 <strong>__WALL__</strong>，最大宿主进程 RSS 为 <strong>__RSS__ KiB</strong>。这包括导入、放置、导出、仿真构建、独立检查和时序分析；是小设计功能基线，不是工业级吞吐量或功耗结论。</p>
<p><a href="evidence/power-io-results.json">外部验证收据</a> · <a href="evidence/power-validation.json">构建/测试收据</a> · <a href="power-io-budget.md">资源预算</a> · <a href="power-pdk-notes.md">PDK 角色与几何依据</a></p></details>
<h2>继续推进 RTL-to-GDS</h2><ol><li>补齐井/衬底 tap 和相应的几何/电气检查。</li><li>建立时钟树和受时序约束的布局优化。</li><li>实现有界信号布线、完整设计规则检查与 SPEF 提取。</li><li>对比提取后 RustSTA/OpenSTA，并完成 GDS、DRC/LVS 验收。</li></ol>
<pre><code>rustroad import-pdk --workspace .work/chip --base-rev rev0 \
  --tech-lef tech.tlef --cell-lef cells.lef --netlist mapped.v \
  --floorplan floorplan.json --physical-config power-io.json
rustroad place --workspace .work/chip --base-rev rev1
rustroad export-def --workspace .work/chip
rustroad export-verilog --workspace .work/chip --include-power</code></pre>
<p><a href="power-io-integration.md">完整复现说明</a> · <a href="architecture.md">架构</a> · <a href="../README.md">README</a></p><footer>RustROAD 0.3.0 · Apache-2.0<br>参考工具只通过公开接口与标准格式使用；未读取或翻译其实现源码。</footer></main></body></html>'''
    text = text.replace("__ROWS__", "".join(rows)).replace("__SCENES__", "".join(scenes)).replace("__TIMING__", "".join(timing)).replace("__WALL__", html.escape(result["measurement"]["wall_clock"])).replace("__RSS__", f'{result["peak_host_process_rss_kib"]:,}')
    (ROOT / "docs/power-io-report.html").write_text(text)


if __name__ == "__main__":
    main()
