#!/usr/bin/env python3
"""Build a local HTML report from bounded, independently verified evidence."""
import html
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(name):
    path = ROOT / 'docs/evidence' / name
    if path.stat().st_size > 2 * 1024**2:
        raise ValueError('report input byte budget')
    return json.loads(path.read_text())


def drawing(layout):
    x0, y0, x1, y1 = layout['die']
    height = y1 - y0
    result = [f'<svg viewBox="{x0} {y0} {x1-x0} {height}" role="img" aria-label="OpenDB 独立读回的 {layout["pdk"]} 物理布局"><g transform="translate(0 {y0+y1}) scale(1 -1)">']
    result.append(f'<rect x="{x0}" y="{y0}" width="{x1-x0}" height="{height}" fill="#101c25"/>')
    colors = dict(tap='#ff80bd', clock='#ffca6b', filler='#2b4456', tie='#b9a0f7', logic='#79d8c3')
    for c in layout['cells']:
        x, y, w, h = [c[k] for k in ('x', 'y', 'width', 'height')]
        color = colors[c['role']]
        tip = html.escape(f"{c['name']} · {c['master']} · {c['orientation']} · {c['role']}")
        result.append(f'<rect class="cell {c["role"]}" x="{x}" y="{y}" width="{w}" height="{h}" fill="{color}" stroke="#101c25" stroke-width="{height/800:.4f}"><title>{tip}</title></rect>')
        if c['orientation'] in ('MX', 'FS'):
            result.append(f'<path class="orientation" d="M{x} {y+h*.8}h{w}" stroke="#101c25" stroke-width="{height/950:.4f}" opacity=".55"/>')
    result.append('<g class="pg">')
    for wire in layout['wires']:
        a, b, c, d = wire['rect']
        color = '#fa9c82' if wire['net'] == 'VDD' else '#78baff'
        result.append(f'<rect x="{a}" y="{b}" width="{c-a}" height="{d-b}" fill="{color}" opacity=".8"/>')
    result.append('</g>')
    for io in layout['io']:
        for shape in io['shapes']:
            a, b, c, d = shape['rect']
            result.append(f'<rect x="{a}" y="{b}" width="{c-a}" height="{d-b}" fill="#e9edf1"><title>{html.escape(io["name"])}</title></rect>')
    result.append('</g></svg>')
    return ''.join(result)


def main():
    result, tests = read('tap-results.json'), read('tap-validation.json')
    if result['status'] != 'passed' or {p['pdk'] for p in result['pdks']} != {'sky130', 'gf180'}:
        raise ValueError('report requires both PDKs to pass')
    scenes, rows, timing = [], [], []
    for p in result['pdks']:
        c, tag = p['clock_tree'], p['pdk']
        if p['tap_verification']['status'] != 'passed' or c['status'] != 'passed':
            raise ValueError('tap or clock evidence missing')
        g, n = c['gds']['result'], c['native']
        layout = read('tap-layout-' + tag + '.json')
        scenes.append(f'<div class="scene" id="scene-{tag}" {"hidden" if tag != "sky130" else ""}>{drawing(layout)}</div>')
        values = [tag, n['cells'], n['tap_cells'], n['clock_buffers'], n['filler_cells'],
                  f'{max(r["max_gap"] for r in c["tap_rows"])/g["dbu_per_micron"]:.2f}',
                  f'{g["contact_counts_per_tap"]["nwell"]} / {g["contact_counts_per_tap"]["substrate"]}',
                  c['graph']['levels'], c['graph']['actual_max_fanout']]
        rows.append('<tr>' + ''.join('<td>' + html.escape(str(v)) + '</td>' for v in values) + '</tr>')
        timing.append(f'<li><strong>{tag}</strong>：RustSTA/OpenSTA 最大差值 {c["timing"]["opensta"]["max_difference_ns"]:.7g} ns；{c["gds"]["result"]["active_input_cells_checked"]} 个含输入的逻辑/时钟单元纳入 GDS 活性区检查。</li>')
    template = '''<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>RustROAD 0.4 · Tap 与时钟树</title>
<style>
:root{color-scheme:dark;--bg:#0d171f;--panel:#14242f;--ink:#ebf1f4;--muted:#a9bdc8;--line:#304652;--mint:#79d8c3}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:16px/1.75 system-ui,-apple-system,sans-serif}main{max-width:1120px;margin:auto;padding:44px 26px 64px}h1{font-size:clamp(32px,5vw,54px);line-height:1.18;margin:16px 0 24px;letter-spacing:-1px}h2{font-size:26px;margin:40px 0 16px}h3{font-size:19px;margin:0 0 12px}p{margin:12px 0}a{color:var(--mint)}code{font:14px ui-monospace,monospace;background:#091219;padding:3px 6px;border-radius:3px}.eyebrow{color:var(--mint);letter-spacing:2px;font-size:12px}.lead{font-size:21px;max-width:900px}.muted{color:var(--muted)}.scope{border-left:3px solid #ffca6b;background:#282820;padding:14px 20px;margin-top:24px}.metrics{display:grid;grid-template-columns:repeat(3,1fr);gap:16px;margin:28px 0}.card{border:1px solid var(--line);border-radius:12px;padding:20px;background:var(--panel)}.number{font-size:32px;font-weight:650;color:var(--mint);line-height:1.4}.toolbar{display:flex;flex-wrap:wrap;gap:10px;align-items:center;margin:12px 0}button{font:inherit;padding:7px 16px;border-radius:6px;border:1px solid var(--line);background:var(--panel);color:var(--ink);cursor:pointer}button[aria-pressed=true]{border-color:var(--mint);color:var(--mint)}label{margin-left:auto;color:var(--muted)}input{accent-color:var(--mint)}.viewport{border:1px solid var(--line);border-radius:12px;overflow:hidden;background:#101c25}.scene svg{display:block;width:100%;height:420px}.scene[hidden]{display:none}.legend{display:flex;flex-wrap:wrap;gap:20px;font-size:14px;margin:10px 0}.dot{display:inline-block;width:10px;height:10px;border-radius:2px;margin-right:7px}.scroll{overflow:auto}table{border-collapse:collapse;width:100%;min-width:820px}th,td{padding:11px 12px;text-align:left;border-bottom:1px solid var(--line)}th{color:var(--muted);font-size:13px}.columns{display:grid;grid-template-columns:1fr 1fr;gap:18px}.columns p{color:var(--muted)}.columns li{margin-bottom:7px}details{border:1px solid var(--line);border-radius:8px;padding:18px;margin:18px 0}summary{cursor:pointer;font-weight:600}pre{background:#091219;padding:18px;overflow:auto;line-height:1.65}pre code{padding:0}footer{margin-top:36px;padding-top:20px;border-top:1px solid var(--line);color:var(--muted);font-size:14px}@media(max-width:720px){main{padding:28px 16px}.metrics,.columns{grid-template-columns:1fr}.number{font-size:29px}.scene svg{height:310px}.lead{font-size:18px}label{margin-left:0}}
</style></head><body><main>
<div class="eyebrow">RUSTROAD · 0.4.0 · 2026-10-23</div>
<h1>井 / 衬底接触已验证。<br>时钟缓冲树进入闭环。</h1>
<p class="lead">从真实 LEF、映射网表出发插入 tap；用原始 PDK GDS 独立核查接触与活性区覆盖，再插入有界时钟树，重新运行逻辑、物理与时序检查。</p>
<div class="scope">当前完成 <strong>tap 插入与独立验证、未布线的时钟拓扑</strong>。时钟偏斜收敛、信号布线、寄生提取和完整 DRC/LVS 尚未完成；距离策略也不等于 foundry 闩锁效应签核。</div>
<div class="metrics"><div class="card"><div class="number">32 / 32</div>Sky130 / GF180 tap 单元<br><span class="muted">每行两端与内部均有接触</span></div><div class="card"><div class="number">6 → 3 级</div>每套工艺的时钟缓冲器与深度<br><span class="muted">6 个 sink，最大扇出 2</span></div><div class="card"><div class="number">__TESTS__ + 2</div>Rust 测试 + 真实 LEF 专测<br><span class="muted">__RECEIPTS__ 份外部过程收据</span></div></div>
<h2>时钟插入后的实际放置</h2>
<div class="toolbar"><button type="button" data-pdk="sky130" aria-pressed="true">Sky130</button><button type="button" data-pdk="gf180" aria-pressed="false">GF180</button><label><input type="checkbox" id="show-pg" checked> 显示主供电线</label></div>
<div class="viewport">__SCENES__</div>
<div class="legend"><span><i class="dot" style="background:#ff80bd"></i>tap</span><span><i class="dot" style="background:#ffca6b"></i>时钟 buffer</span><span><i class="dot" style="background:#79d8c3"></i>逻辑</span><span><i class="dot" style="background:#b9a0f7"></i>tie</span><span><i class="dot" style="background:#2b4456"></i>filler</span></div>
<p class="muted">图形来自 OpenDB 重新读取 DEF 后的坐标。单元上的细线表示翻转行，悬停查看实例；红 / 蓝线为主供电。这里未绘制信号或时钟路由，也不是完成的 RustROAD GUI。</p>
<div class="scroll"><table><thead><tr><th>工艺</th><th>总单元</th><th>tap</th><th>时钟 buffer</th><th>filler</th><th>最大 tap 净间隙 µm</th><th>每 tap 井 / 衬底接触</th><th>时钟级数</th><th>最大扇出</th></tr></thead><tbody>__ROWS__</tbody></table></div>
<h2>两个独立的物理判断</h2>
<div class="columns"><section class="card"><h3>LEF / OpenDB：放在哪里，连到哪里</h3><p>检查 N/FS 合法放置、每行 tap 边界与间距、VDD/VSS 引脚、原生电源轨连续性，以及带电源 Verilog 与 DEF 的端点分区一致性。</p><p>插入时钟树后，tap、filler、PG 和 I/O 全部重建；再次放置所得 DEF 的 SHA256 完全相同。</p></section><section class="card"><h3>原始 GDS / KLayout：接触确实存在</h3><p>用 nwell、掺杂、活性区、contact 与金属多边形核查：Sky130 的 licon → li1 → mcon → met1；GF180 的 Contact → Metal1。</p><p>同一井 / 衬底连通区域内，活性区到正确供电接触的 Manhattan 距离不超过 15 µm。此为明确的工程验收策略。</p></section></div>
<details open><summary>主动破坏后，验证器是否拒绝？</summary><p><strong>已拒绝：</strong>删除接触、移除全部 tap、仅保留行两端 tap、把 tap 接到错误电源、错误的时钟 buffer 极性断言、sink 旁路时钟树、断开时钟 buffer 输出，以及过期 revision 写入。</p><p class="muted">原有电源轨反极性、filler 缺口、主干断裂、VDD/VSS 短路和内部 I/O 反例继续通过。失败控制使用实际图形或连接数据，不依赖 RustROAD 自报结果。</p></details>
<h2>RustSys → RustROAD → RustSTA</h2>
<p>真实库函数的 Yosys 证明、原 RTL 对导出网表的等价检查、每套工艺 512 组 Verilator 仿真均通过。时钟树通过独立 OpenDB sink-to-driver 追踪，覆盖全部六个 sink 与新 buffer。</p><ul>__TIMING__</ul>
<p class="scope">时序为 <strong>TT、理想时钟、理想线网</strong>，容差 0.001 ns。14 个约束端点与 2 个常量驱动、无数据到达的 D 引脚按真实结果保留。脉宽 / 最小周期限制仍有显式诊断。增量检查仅验证无修改更新，不代表完成 ECO 或提取后时序。</p>
<details><summary>规则依据、资源和复现</summary><p>Sky130 的 <a href="https://skywater-pdk.readthedocs.io/en/main/rules/periphery.html#nwell">nwell.4</a> 描述井接触要求；GF180 有 <a href="https://gf180mcu-pdk.readthedocs.io/en/latest/physical_verification/design_manual/drm_14_3_1.html">条件化闩锁规则</a>与 <a href="https://gf180mcu-pdk.readthedocs.io/en/latest/physical_verification/design_manual/drm_07_06.html">DF.13/14 距离规则</a>。当前检查不涵盖所有标记、深井、I/O guard ring、enclosure 或可靠性规则。</p>
<p>完整验证耗时 <strong>__WALL__</strong>；最大宿主进程 RSS <strong>__RSS__ KiB</strong>。包含综合、导入、tap / clock 插入、重复放置、导出、仿真、GDS、OpenDB 和 STA。小设计功能基线不代表大规模吞吐量。</p><p>每个核心算法有实例 / 工作预算；Rust 构建 2 workers，外部检查逐工艺执行，带内存、时间和输出上限。原生核心不依赖 Python 或 KLayout。</p>
<pre><code>cargo build --release --locked --offline --bin rustroad
python3 scripts/verify_taps.py \
  --workspace .work/tap-check --base-rev empty

rustroad clock-tree --workspace .work/chip --base-rev rev2</code></pre>
<p><a href="evidence/tap-results.json">完整验证摘要</a> · <a href="evidence/tap-validation.json">构建 / 测试收据</a> · <a href="tap-integration.md">复现与检查边界</a> · <a href="tap-budget.md">内存 / 工作预算</a></p></details>
<h2>后续闭环</h2><ol><li>从 SDC 导入时钟目标，加入 transition / capacitance 约束、布局优化及 propagated-clock 比较。</li><li>实现有界障碍感知的信号 / 时钟布线，并核查端点连通与设计规则。</li><li>提取 corner-tagged RC / SPEF，完成 RustSTA / OpenSTA 提取后时序差分与实际 ECO 验证。</li><li>GDS 输出与完整 DRC / LVS 验收。</li></ol>
<footer>RustROAD 0.4.0 · Apache-2.0 · <a href="../README.md">README</a> · <a href="architecture.md">架构</a><br>参考工具只通过公开接口与标准数据格式调用；未读取、移植或翻译参考实现源码。HTML 源码与链接已检查，未声称浏览器渲染验收。</footer>
</main><script>
document.querySelectorAll('[data-pdk]').forEach(button=>button.addEventListener('click',()=>{document.querySelectorAll('[data-pdk]').forEach(b=>b.setAttribute('aria-pressed',String(b===button)));document.querySelectorAll('.scene').forEach(scene=>scene.hidden=scene.id!=='scene-'+button.dataset.pdk)}));
document.getElementById('show-pg').addEventListener('change',event=>document.querySelectorAll('.pg').forEach(group=>group.style.display=event.target.checked?'':'none'));
</script></body></html>'''
    replacements = {'__SCENES__': ''.join(scenes), '__ROWS__': ''.join(rows), '__TIMING__': ''.join(timing),
                    '__TESTS__': str(tests['unit_integration']['passed']), '__RECEIPTS__': str(len(result['receipts'])),
                    '__WALL__': html.escape(result['measurement']['wall_clock']), '__RSS__': f'{result["peak_host_process_rss_kib"]:,}'}
    for key, value in replacements.items():
        template = template.replace(key, value)
    (ROOT / 'docs/tap-report.html').write_text(template)


if __name__ == '__main__':
    main()
