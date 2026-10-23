#!/usr/bin/env python3
"""Render the local constrained-CTS report from actual retained evidence."""
import html
import json
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]


def read(name):
    p=ROOT/'docs/evidence'/name
    if p.stat().st_size>4*1024**2:raise ValueError('report input budget')
    return json.loads(p.read_text())


def main():
    s=read('routing-results.json');v=read('routing-validation.json')
    if s['functional_status']!='passed' or s['status']!='partial':raise ValueError('expected honest partial integration result')
    rows=[]
    for p in s['pdks']:
        ref=p['opensta'];hold=min(r['opensta_ns'] for r in ref['comparisons'] if r['kind']=='hold')
        values=[p['pdk'],p['geometry']['routed_nets'],p['geometry']['wires'],p['geometry']['vias'],
                f'{max(ref["clock_skew_rise_fall_ns"])*1000:.2f}',f'{ref["max_difference_ns"]*1000:.2f}',
                f'{p["timing"]["metrics"]["worst_hold_slack_ns"]:.6f}',f'{hold:.6f}']
        rows.append('<tr>'+''.join('<td>'+html.escape(str(x))+'</td>' for x in values)+'</tr>')
    layouts={p['pdk']:read('routing-layout-'+p['pdk']+'.json') for p in s['pdks']}
    template='''<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>RustROAD 0.5 · CTS / Routing / SPEF</title><style>
:root{color-scheme:dark;--bg:#10191f;--panel:#182730;--text:#edf4f4;--muted:#aec0c8;--line:#334953;--green:#8bdec5;--amber:#ffcc84}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:16px/1.75 system-ui,-apple-system,sans-serif}main{max-width:1160px;margin:auto;padding:44px 24px 68px}h1{font-size:clamp(31px,4.5vw,52px);line-height:1.22;margin:16px 0 24px}h2{font-size:25px;margin:38px 0 16px}a{color:var(--green)}.tag{letter-spacing:2px;font-size:12px;color:var(--green)}.lead{font-size:21px}.muted{color:var(--muted)}.notice{border-left:3px solid var(--amber);background:#302a20;padding:14px 20px}.cards{display:grid;grid-template-columns:repeat(3,1fr);gap:16px;margin:25px 0}.card{padding:20px;border:1px solid var(--line);border-radius:10px;background:var(--panel)}.big{font-size:31px;font-weight:650;color:var(--green);line-height:1.4}.scroll{overflow:auto}table{border-collapse:collapse;width:100%;min-width:900px}th,td{text-align:left;border-bottom:1px solid var(--line);padding:11px}th{font-size:13px;color:var(--muted)}.toolbar{display:flex;align-items:center;flex-wrap:wrap;gap:16px;margin:16px 0}select{font:inherit;background:var(--panel);color:var(--text);padding:6px 12px;border:1px solid var(--line);border-radius:6px}input{accent-color:var(--green)}canvas{width:100%;height:480px;display:block;border:1px solid var(--line);background:#0c151b;border-radius:10px}.columns{display:grid;grid-template-columns:1fr 1fr;gap:18px}code{background:#0b141a;padding:2px 5px;font:14px ui-monospace,monospace}pre{padding:18px;overflow:auto;background:#0b141a}pre code{padding:0}summary{cursor:pointer;font-weight:650}details{border:1px solid var(--line);padding:18px;margin:18px 0;border-radius:8px}footer{border-top:1px solid var(--line);margin-top:35px;padding-top:18px;font-size:14px;color:var(--muted)}@media(max-width:720px){main{padding:26px 16px}.cards,.columns{grid-template-columns:1fr}canvas{height:330px}.lead{font-size:18px}}
</style></head><body><main><div class="tag">RUSTROAD / 0.5.0 / 2026-10-23</div>
<h1>时钟约束、物理布线与 SPEF<br>已进入真实双 PDK 验证。</h1>
<p class="lead">四寄存器流水线已从 RTL 经过综合、CTS、时钟 / 信号布线和 OpenRCX 提取，进入传播时钟 STA。几何、连通和偏斜检查通过；时序精度与 hold 闭合还有明确缺口。</p>
<p class="notice"><strong>当前结论：功能链路通过，整体结果 partial。</strong>RustSTA / OpenSTA 的最大差异约 3.15 / 6.79 ps，超过保持不变的 1 ps 门槛。hold 结果也未全部满足约束。验证命令返回 3，不把这些问题包装为签核成功。</p>
<div class="cards"><div class="card"><div class="big">19 / 19</div>每套 PDK 的信号网全部布通<br><span class="muted">包含 4 个 sink 的物理时钟树</span></div><div class="card"><div class="big">__TESTS__ + 2</div>Rust 测试与真实 LEF 专测<br><span class="muted">__RECEIPTS__ 份外部过程收据</span></div><div class="card"><div class="big">__WALL__</div>完整双 PDK 验证耗时<br><span class="muted">峰值宿主进程 __RSS__ KiB</span></div></div>
<h2>独立读回的路由几何</h2><div class="toolbar"><label>工艺 <select id="pdk"><option value="sky130">Sky130</option><option value="gf180">GF180</option></select></label><label>层 <select id="layer"></select></label><label><input id="clocks" type="checkbox"> 仅时钟</label><label>放大 <input id="zoom" type="range" min="1" max="4" step="0.25" value="1"></label></div>
<canvas id="layout" role="img" aria-label="从 OpenDB 读回的实际线段和单元放置"></canvas><p class="muted">金色是时钟线，绿色是信号线，粉色单元是 tap，灰色是 filler。图形来自实际 DEF 的 OpenDB 读回；这是报告视图，不是新的 RustROAD GUI。不同层投影交叉不等于电气连接。</p>
<h2>结果与未满足项</h2><div class="scroll"><table><thead><tr><th>工艺</th><th>信号网</th><th>线段</th><th>VIA</th><th>最大偏斜 ps</th><th>最大 STA 差异 ps</th><th>RustSTA 最差 hold ns</th><th>OpenSTA 最差 hold ns</th></tr></thead><tbody>__ROWS__</tbody></table></div>
<p>偏斜门槛为 Sky130 0.5 ns、GF180 1 ns。两者均满足当前门槛。Sky130 的近零 hold 结果会因当前精度差异改变正负号，仍需核查；GF180 存在明显 hold 违例。当前没有声称已完成时序闭合。</p>
<div class="columns"><section class="card"><h3>本轮已实现</h3><ul><li>严格主时钟 SDC：period、waveform、transition、uncertainty 与明确的约束作用域。</li><li>预留引脚接入、固定 VIA、带障碍的有界 CPU 布线和版本 5 路由快照。</li><li>真实 PDK 标称 OpenRCX 提取与详细 SPEF 传递。</li><li>传播时钟、完整重算 / 无修改更新一致性及独立 OpenSTA 比较。</li></ul></section><section class="card"><h3>验证保持分层</h3><ul><li>OpenDB / KLayout 独立检查连通、部分间距、阻挡和短接。</li><li>原始 GDS 继续验证 tap 与井 / 衬底接触。</li><li>每套工艺 CEC 与 512 组仿真向量，含流水线历史值检查。</li><li>8 个时序端点全部受约束，未约束端点为 0。</li></ul></section></div>
<details open><summary>反例确实会失败</summary><p>移除一条网络的路由、引入电源短接、删除 SPEF 标注和给一条时钟分支增加 10,000 倍电阻，都会触发对应检查失败。重复布线的 DEF 哈希一致；失败的命令不改变原工作区 HEAD，重新放置会使旧路由失效。</p></details>
<h2>边界与下一步</h2><p>布线器采用有限坐标网格与显式工作预算。四寄存器样例通过；较密集的六寄存器样例仍可能触发引脚接入或搜索预算，不能据此宣称工业规模能力。RC 提取目前由外部 OpenRCX 完成，原生校准 RC 提取器尚未实现。</p>
<p>GF180 的技术 LEF 明确提示其寄生值不适用于提取，本轮采用 PDK 专用规则文件。完整 DRC、天线、LVS、IR/EM、GDS streamout 均未签核。后续优先核查 STA 差异与 hold 修复，再改进密集引脚接入、布局与布线优化。</p>
<details><summary>文件、工具和复现</summary><p>参考工具只通过公开命令和标准格式调用。RustSTA / RustEDA 使用本机单独固定的 postroute 候选二进制，未修改或覆盖其他工作区的源代码和发布目标。</p>
<pre><code>free -h
ulimit -v 8388608
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 RAYON_NUM_THREADS=2
cargo build --release --locked --offline --bin rustroad
python3 scripts/verify_routing.py \
  --workspace .work/routing-check --base-rev empty \
  --rusteda /path/to/postroute/rusteda --ruststa /path/to/postroute/ruststa
# 当前预期：functional_status=passed，status=partial，退出码 3</code></pre>
<p><a href="evidence/routing-results.json">完整结果与收据索引</a> · <a href="evidence/routing-validation.json">构建 / 测试记录</a> · <a href="cts-routing.md">实现与复现说明</a> · <a href="routing-budget.md">内存预算</a></p>
<p>SPEF（gzip，解压后为原始提取文件）：<a href="evidence/routing-sky130.spef.gz">Sky130</a> · <a href="evidence/routing-gf180.spef.gz">GF180</a></p></details>
<footer>RustROAD 0.5.0 · Apache-2.0 · <a href="../README.md">README</a> · <a href="architecture.md">架构</a><br>HTML 源码、链接和脚本语法已检查；未声称浏览器渲染验收。完整 RTL-to-GDS 签核仍未完成。</footer>
</main><script type="application/json" id="data">__DATA__</script><script>
const data=JSON.parse(document.getElementById('data').textContent);const pdk=document.getElementById('pdk'), layer=document.getElementById('layer'), clocks=document.getElementById('clocks'),zoom=document.getElementById('zoom'),canvas=document.getElementById('layout');
function options(){layer.replaceChildren();['all',...new Set(data[pdk.value].wires.map(w=>w[1]))].forEach(v=>{const o=document.createElement('option');o.value=v;o.textContent=v==='all'?'所有层':v;layer.append(o)});draw()}
function draw(){const d=data[pdk.value],scale=window.devicePixelRatio||1;canvas.width=canvas.clientWidth*scale;canvas.height=canvas.clientHeight*scale;const c=canvas.getContext('2d');c.scale(scale,scale);c.fillStyle='#0c151b';c.fillRect(0,0,canvas.clientWidth,canvas.clientHeight);const b=d.die,s=Math.min((canvas.clientWidth-30)/(b[2]-b[0]),(canvas.clientHeight-30)/(b[3]-b[1]))*Number(zoom.value),cx=(b[0]+b[2])/2,cy=(b[1]+b[3])/2;function box(x,y,w,h,color,alpha=1){c.globalAlpha=alpha;c.fillStyle=color;c.fillRect(canvas.clientWidth/2+(x-cx)*s,canvas.clientHeight/2-(y+h-cy)*s,Math.max(.5,w*s),Math.max(.5,h*s))}d.cells.forEach(v=>box(v.x,v.y,v.width,v.height,({tap:'#e588b9',clock:'#ffcb75',filler:'#293c47',logic:'#41786f'})[v.role],.8));d.wires.forEach(w=>{const clk=w[0]==='clk'||w[0].startsWith('rr_clock_');if((layer.value==='all'||w[1]===layer.value)&&(!clocks.checked||clk))box(w[2],w[3],w[4]-w[2],w[5]-w[3],clk?'#ffd18e':'#8adbc8',clk?.95:.65)});c.globalAlpha=1}
pdk.addEventListener('change',options);[layer,clocks,zoom].forEach(e=>e.addEventListener('input',draw));window.addEventListener('resize',draw);options();
</script></body></html>'''
    replacements={'__TESTS__':str(v['unit_tests']['passed']),'__RECEIPTS__':str(len(s['receipts'])),'__WALL__':s['measurement']['wall_clock'],'__RSS__':f'{s["peak_rss_kib"]:,}','__ROWS__':''.join(rows),'__DATA__':json.dumps(layouts,separators=(',',':')).replace('</',r'<\/')}
    for k,val in replacements.items():template=template.replace(k,val)
    (ROOT/'docs/cts-routing-report.html').write_text(template)


if __name__=='__main__':main()
