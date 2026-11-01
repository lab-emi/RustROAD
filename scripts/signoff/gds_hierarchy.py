# SPDX-License-Identifier: Apache-2.0
"""Bounded exact hierarchy counts without expanding every placed cell shape."""
from collections import Counter, defaultdict, deque
import heapq


def shape_counts(layout, top, *, max_cells=200_000, max_references=64_000_000,
                 max_layers=4096, max_count=(1 << 63) - 1,
                 max_edges=1_000_000, max_entries=1_000_000, max_work=100_000_000):
    layers = list(layout.layer_indexes())
    if len(layers) > max_layers:
        raise ValueError('GDS hierarchy layer budget exceeded')
    root = top.cell_index()
    pending = deque([root])
    queued = {root}
    graph, own, parents = {}, {}, defaultdict(list)
    references = edges = entries = work = 0
    while pending:
        index = pending.popleft()
        if index in graph:
            continue
        if len(graph) >= max_cells:
            raise ValueError('GDS hierarchy cell budget exceeded')
        cell = layout.cell(index)
        if cell is None:
            raise ValueError("missing GDS hierarchy cell")
        children = Counter()
        for instance in cell.each_inst():
            references += 1
            if references > max_references:
                raise ValueError('GDS hierarchy reference budget exceeded')
            work += 1
            if work > max_work:
                raise ValueError("GDS hierarchy work budget exceeded")
            child = instance.cell.cell_index()
            if child not in children:
                edges += 1
                if edges > max_edges:
                    raise ValueError("GDS hierarchy edge budget exceeded")
            count = instance.size() if instance.is_regular_array() else 1
            if count < 0 or count > max_count or children[child] + count > max_count:
                raise ValueError('GDS hierarchy multiplicity budget exceeded')
            children[child] += count
        graph[index] = children
        counts = {}
        for layer in layers:
            work += 1
            if work > max_work:
                raise ValueError('GDS hierarchy work budget exceeded')
            n = cell.shapes(layer).size()
            if n:
                if n > max_count:
                    raise ValueError('GDS hierarchy shape-count budget exceeded')
                counts[layer] = n
                entries += 1
                if entries > max_entries:
                    raise ValueError('GDS hierarchy count-entry budget exceeded')
        own[index] = counts
        for child in children:
            parents[child].append(index)
            if child not in queued:
                pending.append(child)
                queued.add(child)
    remaining = {index: len(children) for index, children in graph.items()}
    ready = [index for index, count in remaining.items() if count == 0]
    heapq.heapify(ready)
    totals = {}
    while ready:
        index = heapq.heappop(ready)
        counts = own[index]
        for child, multiplier in graph[index].items():
            for layer, n in totals[child].items():
                work += 1
                if work > max_work:
                    raise ValueError('GDS hierarchy work budget exceeded')
                if layer not in counts:
                    entries += 1
                    if entries > max_entries:
                        raise ValueError('GDS hierarchy count-entry budget exceeded')
                count = counts.get(layer, 0) + multiplier * n
                if count > max_count:
                    raise ValueError('GDS hierarchy shape-count budget exceeded')
                counts[layer] = count
        totals[index] = counts
        for parent in parents[index]:
            remaining[parent] -= 1
            if remaining[parent] == 0:
                heapq.heappush(ready, parent)
    if len(totals) != len(graph):
        raise ValueError('cyclic GDS hierarchy')
    return totals[root]
