#!/usr/bin/env python3
"""Build the local Chinese engineering report from bounded committed receipts."""
import html
import json
from pathlib import Path


def main():
    root = Path(__file__).resolve().parents[1]
    evidence = root / "docs/evidence/pdk-import-results.json"
    if evidence.stat().st_size > 2 * 1024 * 1024:
        raise ValueError("receipt exceeds report input budget")
    data = json.loads(evidence.read_text())
    if data["status"] != "passed" or len(data["pdks"]) != 2:
        raise ValueError("report requires two explicitly passed PDK rows")
    rows = []
    for pdk in data["pdks"]:
        if pdk["opendb"].get("master_geometry", {}).get("status") != "passed":
            raise ValueError("report requires independent pin/OBS geometry evidence")
        placed = pdk["placed"]
        rows.append("<tr>" + "".join(f"<td>{html.escape(str(value))}</td>" for value in (
            pdk["pdk"], placed["cells"], placed["nets"], pdk["topology"]["endpoints"],
            placed["pins"], f'{pdk["timing_comparison"]["max_difference_ns"]:.7g} ns')) + "</tr>")
    measurement = data.get("measurement", {})
    wall = measurement.get("elapsed_seconds", measurement.get("wall_seconds"))
    if wall is None:
        wall = 0.0
        for part in measurement["wall_clock"].split(":"):
            wall = wall * 60 + float(part)
    wall = f"{wall:.2f}"
    text = r'''<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>RustROAD 0.2 · 真实 PDK 连接导入</title><style>
:root{color-scheme:dark;--bg:#10191c;--panel:#18262b;--line:#31474e;--ink:#e5eeee;--muted:#aac0c5;--teal:#7ee3cc;--amber:#ffcd8d}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:16px/1.75 system-ui,-apple-system,"Noto Sans CJK SC",sans-serif}main{max-width:1100px;margin:auto;padding:48px 24px 72px}h1{font-size:clamp(32px,6vw,60px);line-height:1.2;margin:16px 0 26px}h2{font-size:25px;margin:38px 0 16px}a{color:var(--teal);text-underline-offset:4px}code{font:14px ui-monospace,monospace;background:#0c1417;padding:3px 5px}small,.muted{color:var(--muted)}.eyebrow{letter-spacing:3px;color:var(--teal);font-size:13px}.lead{font-size:21px}.notice{border-left:3px solid var(--amber);padding:12px 20px;background:#242820}.grid{display:grid;grid-template-columns:repeat(3,1fr);gap:16px;margin:28px 0}.card{border:1px solid var(--line);background:var(--panel);padding:20px;border-radius:10px}.number{font-size:32px;font-weight:650;color:var(--teal)}.scroll{overflow:auto}table{border-collapse:collapse;width:100%;min-width:690px}th,td{padding:14px 12px;border-bottom:1px solid var(--line);text-align:left;vertical-align:top}th{color:var(--muted);font-size:14px}.flow{display:flex;gap:9px;align-items:stretch;flex-wrap:wrap}.flow div{border:1px solid var(--line);padding:14px 20px;border-radius:8px}.flow .later{border-style:dashed;color:var(--amber)}details{border:1px solid var(--line);padding:17px;border-radius:8px;margin:14px 0}summary{cursor:pointer;font-weight:600}pre{overflow:auto;background:#0c1417;padding:18px;line-height:1.6}footer{margin-top:40px;border-top:1px solid var(--line);padding-top:20px;color:var(--muted)}@media(max-width:720px){main{padding:28px 16px}.grid{grid-template-columns:1fr}.lead{font-size:18px}}
</style></head><body><main>
<div class="eyebrow">RUSTROAD / 0.2.0 / 2026-10-23</div>
<h1>真实 PDK 的连接数据，<br>已通过完整子流程验证。</h1>
<p class="lead">RustSys 的映射网表现在可以进入 RustROAD，与真实 LEF 的单元和引脚连接，完成行放置，再导出给 RustSTA。Sky130 和 GF180 的独立验证均通过。</p>
<p class="notice">当前完成的是 <strong>真实 PDK 导入 → 连接保持 → 放置 → 导出 → 理想线网 STA</strong>。电源网络、I/O 放置、CTS、布线、寄生提取、GDS 和 DRC/LVS 仍未完成，完整 RTL-to-GDS 状态为 <code>unverified</code>。</p>
<div class="grid"><div class="card"><div class="number">36 + 2</div>普通测试 + 真实 LEF 专测<small><br>另有独立外部 oracle</small></div><div class="card"><div class="number">512 × 2</div>RTL / 映射后仿真向量<small><br>每个 PDK 均含错误输出控制</small></div><div class="card"><div class="number">2 个 PDK</div>原始 LEF / Liberty 输入<small><br>文件哈希固定，生产库保持原样</small></div></div>
<h2>已连通的路径</h2><div class="flow"><div>RTL<br><small>RustSys + CEC</small></div><div>映射 Verilog + LEF<br><small>RustROAD 连接导入</small></div><div>行放置与快照<br><small>DEF PINS / NETS</small></div><div>导出 Verilog<br><small>RustSTA / OpenSTA</small></div><div class="later">CTS / route / SPEF<br><small>后续阶段</small></div></div>
<p>网表中的总线、转义标识符、位选择、拼接和 assign 别名会被解析并连接到真实 LEF pin。工作区保存原始逻辑 AST 与独立连接表，每次重新打开都会核对两者。逻辑名称与 shell-safe handle 分开处理，跨修订写入仍需要 <code>--base-rev</code>。</p>
<h2>本次双 PDK 结果</h2><div class="scroll"><table><thead><tr><th>工艺</th><th>单元</th><th>网</th><th>信号端点</th><th>全部存储引脚</th><th>STA 最大偏差</th></tr></thead><tbody>__ROWS__</tbody></table></div>
<p>使用四位寄存器、组合 XOR / AND 数据通路和输出别名，包含 18 个顶层端口位。全部存储引脚包括 LEF 电源/地引脚，其几何保留但尚未连接电源网络。STA 比较范围为 TT、理想时钟、理想线网，容差 <code>0.001 ns</code>，两个 PDK 均无未约束端点。</p>
<details open><summary>独立验证检查了什么</summary><ul>
<li>Yosys 独立读取原始与导出 Verilog，逐网比较端点集合；原生 CEC 和外部 CEC 均通过。</li>
<li>Verilator 在每个 PDK 上运行 512 组输入组合，验证 RTL 与映射后行为；错误输出被拒绝。</li>
<li>OpenROAD / OpenDB 使用真实 PDK LEF 重新打开 DEF，检查连接、端口方向、DBU、单元名称/尺寸和放置合法性；逐项核对信号/电源引脚方向、用途、PORT 分组矩形及 OBS。</li>
<li>仅改动逻辑引脚连线的 Verilog 与保持合法放置的错误 DEF，均被独立比较器检出。</li>
<li>RustSTA 完整/独立检查及无修改增量更新一致；OpenSTA 的 setup / hold 对照在既定容差内。</li></ul>
<p>无修改增量检查不代表已验证实际 ECO。脉宽、最小时钟周期和未建模的 LEF 工艺规则，仍保留显式诊断。</p></details>
<details><summary>资源预算与可复现性</summary><p>完整验证包含编译仿真模型、子进程启动、导入、mmap、放置、导出及独立检查；耗时 <strong>__WALL__</strong> 秒，记录的最大宿主进程 RSS 为 <strong>__RSS__ KiB</strong>。这是小设计功能基线，不能用于宣称工业规模的速度优势。</p>
<p>普通子进程使用 2 GiB 地址空间和 30 秒上限。OpenROAD 使用 8 GiB 虚拟地址空间以适应分配器保留空间，同时保持容器实际内存 2 GiB、无 swap、两 CPU 和超时保护。最大单进程 RSS 与进程组同时占用不同。</p>
<p>输入上限、图结构预算和收据：<a href="pdk-import-budget.md">内存/工作量契约</a> · <a href="evidence/pdk-import-results.json">完整验证结果</a> · <a href="evidence/pdk-import-sources.json">源码与工具身份</a> · <a href="evidence/pdk-validation.json">测试与构建</a>。</p></details>
<h2>格式与内存模型</h2><p>LEF 以精确十进制转换到 i64 DBU；越出格点直接报错。支持宏、site、层、信号/电源引脚、PORT、OBS、RECT/POLYGON 与固定 VIA。内核采用每个数据库独立的字符串表及 <code>InstId / CellId / NetId / PinId</code> arena，计算视图是独立 CSR。</p>
<p>版本 2 快照包含 <code>instances.rbin</code>、<code>pins.rbin</code>、<code>net_offsets.rbin</code>、<code>net_pins.rbin</code>，以只读 mmap 校验和访问。版本 1 几何快照仍可读取。测试覆盖连接表损坏、AST 与连接不一致、并发写入、资源上限、别名环和嵌套表达式持久化。</p>
<h2>下一阶段</h2><ol><li>物理 I/O 放置、供电/地与 tie cell 网络、行翻转及电源轨连续性。</li><li>网感知布局目标、位移受限的合法化和完整工艺约束验证。</li><li>CTS 与有界布线，再建立 SPEF → RustSTA 的真实寄生参数闭环。</li><li>真实 GDS 流出，结合独立 DRC / LVS 完成 RTL-to-GDS 验收。</li></ol>
<p>本阶段显式拒绝不支持的非零宏原点、多高度单元、需要 tie cell 的常量/供电连线及双向电气语义。复杂间距、天线、RC 等规则会产生诊断；当前结果不包含这些签核能力。</p>
<h2>运行入口</h2><pre><code>target/release/rustroad import-pdk --workspace .work/connected \
  --base-rev rev0 --tech-lef technology.tlef --cell-lef cells.lef \
  --netlist mapped.v --floorplan floorplan.json
target/release/rustroad place --workspace .work/connected --base-rev rev1
target/release/rustroad export-verilog --workspace .work/connected
target/release/rustroad export-def --workspace .work/connected</code></pre>
<p><a href="pdk-import.md">完整复现命令与实测范围</a> · <a href="architecture.md">架构</a> · <a href="../README.md">README</a> · <a href="gui-architecture.md">GUI 方案</a></p>
<footer>RustROAD 0.2.0 · Apache-2.0<br>参考工具仅通过公开接口与标准文件交互；未读取或翻译实现源码。RustSys / RustSTA 使用固定的既有二进制。</footer>
</main></body></html>'''
    text = text.replace("__ROWS__", "".join(rows)).replace("__WALL__", html.escape(str(wall))).replace("__RSS__", f'{data["peak_host_process_rss_kib"]:,}')
    (root / "docs/pdk-report.html").write_text(text)


if __name__ == "__main__":
    main()
